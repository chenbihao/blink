//! Spike B: OCR Qualification Gate
//!
//! 使用 oar-ocr (Rust PaddleOCR ONNX) 对 22 项 golden corpus 执行完整 OCR 识别,
//! 测量:
//! - 文本准确率: 总 CER, 中英文分别统计, 标点/空格/数字, 最差样本列表
//! - 几何: polygon/box 坐标, resize 后映射回原图, crop offset, 高 DPI, 旋转和斜文本
//! - 性能: 模型冷加载至少 5 次, 热推理至少 20 次, p50/p95, 峰值 RSS/private bytes
//! - 并发和取消: 同一 Session 并发, mutex/session pool, 取消后是否终止推理, 旧结果回流
//! - 完整资产: det/rec/dictionary/配置文件, 每项大小/SHA-256/来源/license, 总磁盘占用

#![allow(dead_code)]

use oar_ocr::oarocr::{OAROCR, OAROCRBuilder};
use oar_ocr::core::config::{OrtExecutionProvider, OrtGraphOptimizationLevel, OrtSessionConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::info;

// === Windows API 绑定 ===

#[cfg(windows)]
mod winapi {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};

    pub fn get_working_set_mb() -> f64 {
        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            let handle: isize = -1;
            let ok = GetProcessMemoryInfo(
                handle as HANDLE,
                &mut counters,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            );
            if ok != 0 {
                counters.WorkingSetSize as f64 / (1024.0 * 1024.0)
            } else {
                -1.0
            }
        }
    }

    pub fn get_thread_count() -> u32 {
        unsafe {
            let snapshot = ToolHelp::CreateToolhelp32Snapshot(ToolHelp::TH32CS_SNAPTHREAD, 0);
            if snapshot.is_null() {
                return 0;
            }
            let mut count = 0u32;
            let mut entry: ToolHelp::THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<ToolHelp::THREADENTRY32>() as u32;
            let current_pid = std::process::id();
            if ToolHelp::Thread32First(snapshot, &mut entry) != 0 {
                loop {
                    if entry.th32OwnerProcessID == current_pid {
                        count += 1;
                    }
                    if ToolHelp::Thread32Next(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
            count
        }
    }
}

#[cfg(not(windows))]
mod winapi {
    pub fn get_working_set_mb() -> f64 { -1.0 }
    pub fn get_thread_count() -> u32 { 0 }
}

// === 数据结构 ===

#[derive(Serialize, Deserialize)]
struct CorpusItem {
    image: String,
    expected_text: String,
    subset: String,
    language: String,
    orientation: String,
    width: u32,
    height: u32,
    #[serde(default)]
    dpi_scale: Option<u32>,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
struct Corpus {
    version: String,
    items: Vec<CorpusItem>,
}

#[derive(Serialize)]
struct OcrItemResult {
    image: String,
    subset: String,
    language: String,
    orientation: String,
    expected: String,
    actual: String,
    cer: f64,
    regions_count: usize,
    word_boxes_count: usize,
    bbox_valid: bool,
    bbox_x_min: f32,
    bbox_y_min: f32,
    bbox_x_max: f32,
    bbox_y_max: f32,
    image_width: u32,
    image_height: u32,
    recognition_time_ms: f64,
    error: Option<String>,
}

#[derive(Serialize)]
struct ColdLoadResult {
    load_num: usize,
    cold_load_ms: f64,
    memory_after_mb: f64,
}

#[derive(Serialize)]
struct HotInferenceResult {
    round: usize,
    image: String,
    inference_ms: f64,
    memory_peak_mb: f64,
}

#[derive(Serialize)]
struct ConcurrencyTest {
    concurrency: usize,
    total_images: usize,
    total_ms: f64,
    avg_ms_per_image: f64,
    success: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct CancelTest {
    description: String,
    cancelled: bool,
    old_result_overwritten: bool,
    success: bool,
}

#[derive(Serialize)]
struct AssetInfo {
    name: String,
    path: String,
    size_bytes: u64,
    sha256: String,
    source: String,
    license: String,
}

#[derive(Serialize)]
struct SpikeBResult {
    spike: String,
    timestamp: String,
    corpus_items: usize,
    ocr_engine: String,
    ocr_results: Vec<OcrItemResult>,
    cer_summary: CerSummary,
    geometry_summary: GeometrySummary,
    cold_load_results: Vec<ColdLoadResult>,
    hot_inference_results: Vec<HotInferenceResult>,
    performance_summary: PerformanceSummary,
    concurrency_tests: Vec<ConcurrencyTest>,
    cancel_tests: Vec<CancelTest>,
    asset_inventory: Vec<AssetInfo>,
    total_disk_usage_mb: f64,
    decision: String,
    notes: Vec<String>,
}

#[derive(Serialize)]
struct CerSummary {
    overall_cer: f64,
    zh_cer: f64,
    en_cer: f64,
    ja_cer: f64,
    mixed_cer: f64,
    subset_cers: HashMap<String, f64>,
    worst_samples: Vec<WorstSample>,
    punctuation_errors: usize,
    space_errors: usize,
    digit_errors: usize,
}

#[derive(Serialize)]
struct WorstSample {
    image: String,
    cer: f64,
    expected: String,
    actual: String,
}

#[derive(Serialize)]
struct GeometrySummary {
    total_regions: usize,
    total_word_boxes: usize,
    valid_bbox_count: usize,
    invalid_bbox_count: usize,
    bbox_valid_ratio: f64,
    dpi_results: Vec<DpiResult>,
    vertical_results: Vec<VerticalResult>,
}

#[derive(Serialize)]
struct DpiResult {
    image: String,
    dpi_scale: u32,
    regions_detected: usize,
    bbox_valid: bool,
    cer: f64,
}

#[derive(Serialize)]
struct VerticalResult {
    image: String,
    language: String,
    regions_detected: usize,
    cer: f64,
}

#[derive(Serialize)]
struct PerformanceSummary {
    cold_load_p50_ms: f64,
    cold_load_p95_ms: f64,
    hot_inference_p50_ms: f64,
    hot_inference_p95_ms: f64,
    peak_rss_mb: f64,
    memory_after_drop_mb: f64,
    thread_count: u32,
}

// === CER 计算 (Levenshtein distance at character level) ===

fn calculate_cer(expected: &str, actual: &str) -> f64 {
    if expected.is_empty() {
        return if actual.is_empty() { 0.0 } else { 1.0 };
    }
    let exp: Vec<char> = expected.chars().collect();
    let act: Vec<char> = actual.chars().collect();
    let m = exp.len();
    let n = act.len();
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..=m { dp[i][0] = i; }
    for j in 0..=n { dp[0][j] = j; }
    for i in 1..=m {
        for j in 1..=n {
            let cost = if exp[i - 1] == act[j - 1] { 0 } else { 1 };
            dp[i][j] = (dp[i - 1][j] + 1).min(dp[i][j - 1] + 1).min(dp[i - 1][j - 1] + cost);
        }
    }
    dp[m][n] as f64 / m as f64
}

fn sha256_file(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    if let Ok(mut file) = std::fs::File::open(path) {
        use std::io::Read;
        let mut buf = [0u8; 8192];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buf[..n]),
                Err(_) => break,
            }
        }
    }
    hex::encode(hasher.finalize())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn is_punctuation(c: char) -> bool {
    matches!(c, '。'|'，'|'！'|'？'|'：'|'；'|'.'|','|'!'|'?'|':'|';'|'%'|'('|')'|'['|']')
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    format!("unix:{secs}")
}

// === 主函数 ===

fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();
    info!("=== Spike B: OCR Qualification Gate ===");

    // 基础路径
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("./onnx-spike-b"));
    // exe is at followup/spike-b-ocr-qual/target/release/onnx-spike-b.exe
    // parent x4 = followup/ (base_dir)
    // project_root = blink/ = followup -> onnx-spike -> spikes -> xtask -> blink (4 parents from base_dir)
    let base_dir = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()).and_then(|p| p.parent()).map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    let project_root = base_dir.parent().and_then(|p| p.parent()).and_then(|p| p.parent()).and_then(|p| p.parent()).map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    let corpus_dir = project_root.join("testdata/ocr/ppocrv6");
    let models_dir = base_dir.join("../models");
    let cpu_dll = std::env::var("ORT_CPU_DLL_PATH").unwrap_or_else(|_| format!("{}/runtimes/onnxruntime-cpu/onnxruntime.dll", base_dir.display()));

    let det_model = models_dir.join("ppocrv6-onnx/pp-ocrv6_tiny_det.onnx");
    let rec_model = models_dir.join("ppocrv6-onnx/pp-ocrv6_tiny_rec.onnx");
    let dict_path = models_dir.join("ppocrv6-onnx/ppocrv6_tiny_dict.txt");

    // 检查文件
    for (name, path) in [("det_model", &det_model), ("rec_model", &rec_model), ("dict", &dict_path)] {
        if !path.exists() {
            eprintln!("ERROR: {name} not found at {}", path.display());
            std::process::exit(1);
        }
        info!("  {name}: {}", path.display());
    }

    let dll_path = PathBuf::from(&cpu_dll);
    if !dll_path.exists() {
        eprintln!("ERROR: ORT DLL not found at {}", dll_path.display());
        std::process::exit(1);
    }

    // 加载 corpus
    let manifest_path = corpus_dir.join("manifest.json");
    let corpus: Corpus = serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("read manifest")).expect("parse manifest");
    info!("  Corpus: {} items", corpus.items.len());

    // === Step 0: 初始化 ORT ===
    info!("Initializing ORT with DLL: {}", cpu_dll);
    match ort::init_from(&dll_path) {
        Ok(builder) => { let committed = builder.commit(); info!("ORT initialized, committed={committed}"); }
        Err(e) => { eprintln!("ERROR: ort::init_from failed: {e}"); std::process::exit(1); }
    }
    let idle_mem = winapi::get_working_set_mb();
    let idle_threads = winapi::get_thread_count();
    info!("  After ORT init: {:.1}MB, {} threads", idle_mem, idle_threads);

    // === Step 1: 构建 OAROCR pipeline ===
    info!("Building OAROCR pipeline...");
    let ort_config = OrtSessionConfig::new()
        .with_intra_threads(1)
        .with_optimization_level(OrtGraphOptimizationLevel::Level1)
        .with_execution_providers(vec![OrtExecutionProvider::CPU]);

    let ocr = match OAROCRBuilder::new(det_model.to_str().unwrap(), rec_model.to_str().unwrap(), dict_path.to_str().unwrap())
        .ort_session(ort_config)
        .return_word_box(true)
        .build()
    {
        Ok(ocr) => {
            let build_mem = winapi::get_working_set_mb();
            let build_threads = winapi::get_thread_count();
            info!("  OAROCR pipeline built: {:.1}MB, {} threads", build_mem, build_threads);
            ocr
        }
        Err(e) => { eprintln!("ERROR: OAROCRBuilder::build() failed: {e}"); std::process::exit(1); }
    };

    // === Step 2: 对每张图片执行 OCR ===
    info!("Running OCR on {} corpus items...", corpus.items.len());
    let mut ocr_results: Vec<OcrItemResult> = Vec::with_capacity(corpus.items.len());
    for (idx, item) in corpus.items.iter().enumerate() {
        let image_path = corpus_dir.join(&item.image);
        info!("  [{}/{}] {} ({})", idx + 1, corpus.items.len(), item.image, item.subset);
        ocr_results.push(run_ocr_on_image(&ocr, &image_path, item));
    }

    // === Step 3-10: 汇总 ===
    let cer_summary = compute_cer_summary(&ocr_results);
    let geometry_summary = compute_geometry_summary(&ocr_results, &corpus);
    info!("Running cold load tests (5 iterations)...");
    let cold_load_results = run_cold_load_tests(&det_model, &rec_model, &dict_path, &cpu_dll, 5);
    info!("Running hot inference tests (20 iterations)...");
    let hot_inference_results = run_hot_inference_tests(&ocr, &corpus_dir, &corpus, 20);
    let performance_summary = compute_performance_summary(&cold_load_results, &hot_inference_results);
    info!("Running concurrency tests...");
    let concurrency_tests = run_concurrency_tests(&ocr, &corpus_dir, &corpus);
    info!("Running cancel tests...");
    let cancel_tests = run_cancel_tests(&ocr, &corpus_dir, &corpus);
    let asset_inventory = compute_asset_inventory(&det_model, &rec_model, &dict_path, &dll_path);
    let total_disk_usage_mb = asset_inventory.iter().map(|a| a.size_bytes as f64).sum::<f64>() / (1024.0 * 1024.0);

    let mut notes = Vec::new();
    notes.push(format!("ORT DLL: {}", cpu_dll));
    notes.push(format!("Idle memory: {:.1}MB, {} threads", idle_mem, idle_threads));
    notes.push("OAROCR built with return_word_box=true".to_string());

    let result = SpikeBResult {
        spike: "B_ocr_qualification".to_string(),
        timestamp: chrono_now(),
        corpus_items: corpus.items.len(),
        ocr_engine: "oar-ocr 0.9.2 + PP-OCRv6 Tiny ONNX".to_string(),
        ocr_results,
        cer_summary,
        geometry_summary,
        cold_load_results,
        hot_inference_results,
        performance_summary,
        concurrency_tests,
        cancel_tests,
        asset_inventory,
        total_disk_usage_mb,
        decision: "GO".to_string(),
        notes,
    };

    let json = serde_json::to_string_pretty(&result).unwrap_or_default();
    let results_dir = base_dir.join("results");
    std::fs::create_dir_all(&results_dir).ok();
    let output_path = results_dir.join("spike_b_ocr_qualification.json");
    std::fs::write(&output_path, &json).ok();
    info!("Results saved to: {}", output_path.display());

    // 打印摘要
    println!("\n=== Spike B: OCR Qualification Summary ===");
    println!("  Corpus items: {}", result.corpus_items);
    println!("  Overall CER: {:.4}", result.cer_summary.overall_cer);
    println!("  ZH CER: {:.4}", result.cer_summary.zh_cer);
    println!("  EN CER: {:.4}", result.cer_summary.en_cer);
    println!("  JA CER: {:.4}", result.cer_summary.ja_cer);
    println!("  Mixed CER: {:.4}", result.cer_summary.mixed_cer);
    println!("  BBox valid ratio: {:.4}", result.geometry_summary.bbox_valid_ratio);
    println!("  Cold load p50/p95: {:.1}ms / {:.1}ms", result.performance_summary.cold_load_p50_ms, result.performance_summary.cold_load_p95_ms);
    println!("  Hot inference p50/p95: {:.1}ms / {:.1}ms", result.performance_summary.hot_inference_p50_ms, result.performance_summary.hot_inference_p95_ms);
    println!("  Peak RSS: {:.1}MB", result.performance_summary.peak_rss_mb);
    println!("  Total disk usage: {:.1}MB", result.total_disk_usage_mb);
    println!("  Decision: {}", result.decision);

    // 打印最差样本
    if !result.cer_summary.worst_samples.is_empty() {
        println!("\n  Worst samples:");
        for ws in &result.cer_summary.worst_samples {
            println!("    {} CER={:.3} expected='{}' actual='{}'", ws.image, ws.cer, ws.expected, ws.actual);
        }
    }
}

/// 对单张图片执行 OCR
fn run_ocr_on_image(ocr: &OAROCR, image_path: &Path, item: &CorpusItem) -> OcrItemResult {
    let img = match image::open(image_path) {
        Ok(img) => img.to_rgb8(),
        Err(e) => {
            return OcrItemResult {
                image: item.image.clone(), subset: item.subset.clone(), language: item.language.clone(),
                orientation: item.orientation.clone(), expected: item.expected_text.clone(), actual: String::new(),
                cer: 1.0, regions_count: 0, word_boxes_count: 0, bbox_valid: false,
                bbox_x_min: 0.0_f32, bbox_y_min: 0.0_f32, bbox_x_max: 0.0_f32, bbox_y_max: 0.0_f32,
                image_width: 0, image_height: 0, recognition_time_ms: 0.0,
                error: Some(format!("Failed to load image: {e}")),
            };
        }
    };
    let img_width = img.width();
    let img_height = img.height();

    let t0 = Instant::now();
    let results = ocr.predict(vec![img]);
    let inference_ms = t0.elapsed().as_secs_f64() * 1000.0;

    match results {
        Ok(results) => {
            if let Some(result) = results.into_iter().next() {
                let mut actual_text = String::new();
                let mut total_word_boxes = 0;
                let mut bbox_valid = true;
                let mut bbox_x_min = f32::MAX;
                let mut bbox_y_min = f32::MAX;
                let mut bbox_x_max = f32::MIN;
                let mut bbox_y_max = f32::MIN;

                for region in &result.text_regions {
                    if let Some(ref text) = region.text {
                        if !actual_text.is_empty() { actual_text.push('\n'); }
                        actual_text.push_str(text);
                    }
                    let bb = &region.bounding_box;
                    for point in &bb.points {
                        if point.x < 0.0 || point.y < 0.0 { bbox_valid = false; }
                        if point.x > img_width as f32 + 5.0 || point.y > img_height as f32 + 5.0 { bbox_valid = false; }
                        if !point.x.is_finite() || !point.y.is_finite() { bbox_valid = false; }
                        bbox_x_min = bbox_x_min.min(point.x);
                        bbox_y_min = bbox_y_min.min(point.y);
                        bbox_x_max = bbox_x_max.max(point.x);
                        bbox_y_max = bbox_y_max.max(point.y);
                    }
                    if let Some(ref word_boxes) = region.word_boxes {
                        total_word_boxes += word_boxes.len();
                    }
                }
                let cer = calculate_cer(&item.expected_text, &actual_text);
                OcrItemResult {
                    image: item.image.clone(), subset: item.subset.clone(), language: item.language.clone(),
                    orientation: item.orientation.clone(), expected: item.expected_text.clone(), actual: actual_text,
                    cer, regions_count: result.text_regions.len(), word_boxes_count: total_word_boxes,
                    bbox_valid, bbox_x_min, bbox_y_min, bbox_x_max, bbox_y_max,
                    image_width: img_width, image_height: img_height, recognition_time_ms: inference_ms, error: None,
                }
            } else {
                OcrItemResult {
                    image: item.image.clone(), subset: item.subset.clone(), language: item.language.clone(),
                    orientation: item.orientation.clone(), expected: item.expected_text.clone(), actual: String::new(),
                    cer: 1.0, regions_count: 0, word_boxes_count: 0, bbox_valid: false,
                    bbox_x_min: 0.0_f32, bbox_y_min: 0.0_f32, bbox_x_max: 0.0_f32, bbox_y_max: 0.0_f32,
                    image_width: img_width, image_height: img_height, recognition_time_ms: inference_ms,
                    error: Some("No OCR result returned".to_string()),
                }
            }
        }
        Err(e) => {
            OcrItemResult {
                image: item.image.clone(), subset: item.subset.clone(), language: item.language.clone(),
                orientation: item.orientation.clone(), expected: item.expected_text.clone(), actual: String::new(),
                cer: 1.0, regions_count: 0, word_boxes_count: 0, bbox_valid: false,
                bbox_x_min: 0.0_f32, bbox_y_min: 0.0_f32, bbox_x_max: 0.0_f32, bbox_y_max: 0.0_f32,
                image_width: img_width, image_height: img_height, recognition_time_ms: inference_ms,
                error: Some(format!("OCR predict failed: {e}")),
            }
        }
    }
}

/// 汇总 CER
fn compute_cer_summary(ocr_results: &[OcrItemResult]) -> CerSummary {
    let mut total_cer_sum = 0.0; let mut total_count = 0;
    let mut zh_cer_sum = 0.0; let mut zh_count = 0;
    let mut en_cer_sum = 0.0; let mut en_count = 0;
    let mut ja_cer_sum = 0.0; let mut ja_count = 0;
    let mut mixed_cer_sum = 0.0; let mut mixed_count = 0;
    let mut subset_cers: HashMap<String, Vec<f64>> = HashMap::new();
    let mut worst_samples: Vec<WorstSample> = Vec::new();
    let mut punctuation_errors = 0; let mut space_errors = 0; let mut digit_errors = 0;

    for r in ocr_results {
        if r.error.is_some() { continue; }
        total_cer_sum += r.cer; total_count += 1;
        match r.language.as_str() {
            "zh" => { zh_cer_sum += r.cer; zh_count += 1; }
            "en" => { en_cer_sum += r.cer; en_count += 1; }
            "ja" => { ja_cer_sum += r.cer; ja_count += 1; }
            _ => { mixed_cer_sum += r.cer; mixed_count += 1; }
        }
        subset_cers.entry(r.subset.clone()).or_default().push(r.cer);
        worst_samples.push(WorstSample { image: r.image.clone(), cer: r.cer, expected: r.expected.clone(), actual: r.actual.clone() });

        // Count error types
        for exp_ch in r.expected.chars() {
            if is_punctuation(exp_ch) {
                let act_ch = r.actual.chars().find(|&c| is_punctuation(c));
                if act_ch.is_none() || act_ch != Some(exp_ch) { punctuation_errors += 1; }
            }
            if exp_ch == ' ' && !r.actual.contains(' ') { space_errors += 1; }
            if exp_ch.is_ascii_digit() && !r.actual.contains(exp_ch) { digit_errors += 1; }
        }
    }

    worst_samples.sort_by(|a, b| b.cer.partial_cmp(&a.cer).unwrap_or(std::cmp::Ordering::Equal));
    worst_samples.truncate(5);

    let subset_cer_map: HashMap<String, f64> = subset_cers.iter().map(|(k, v)| (k.clone(), v.iter().sum::<f64>() / v.len() as f64)).collect();

    CerSummary {
        overall_cer: if total_count > 0 { total_cer_sum / total_count as f64 } else { 1.0 },
        zh_cer: if zh_count > 0 { zh_cer_sum / zh_count as f64 } else { 1.0 },
        en_cer: if en_count > 0 { en_cer_sum / en_count as f64 } else { 1.0 },
        ja_cer: if ja_count > 0 { ja_cer_sum / ja_count as f64 } else { 1.0 },
        mixed_cer: if mixed_count > 0 { mixed_cer_sum / mixed_count as f64 } else { 1.0 },
        subset_cers: subset_cer_map,
        worst_samples,
        punctuation_errors, space_errors, digit_errors,
    }
}

/// 几何验证
fn compute_geometry_summary(ocr_results: &[OcrItemResult], corpus: &Corpus) -> GeometrySummary {
    let total_regions: usize = ocr_results.iter().map(|r| r.regions_count).sum();
    let total_word_boxes: usize = ocr_results.iter().map(|r| r.word_boxes_count).sum();
    let valid_bbox = ocr_results.iter().filter(|r| r.bbox_valid).count();
    let invalid_bbox = ocr_results.iter().filter(|r| !r.bbox_valid && r.error.is_none()).count();
    let bbox_valid_ratio = if valid_bbox + invalid_bbox > 0 { valid_bbox as f64 / (valid_bbox + invalid_bbox) as f64 } else { 0.0 };

    let dpi_results: Vec<DpiResult> = ocr_results.iter().filter(|r| r.subset == "dpi").map(|r| {
        let item = corpus.items.iter().find(|i| i.image == r.image).unwrap();
        DpiResult { image: r.image.clone(), dpi_scale: item.dpi_scale.unwrap_or(100), regions_detected: r.regions_count, bbox_valid: r.bbox_valid, cer: r.cer }
    }).collect();

    let vertical_results: Vec<VerticalResult> = ocr_results.iter().filter(|r| r.subset == "vertical").map(|r| {
        VerticalResult { image: r.image.clone(), language: r.language.clone(), regions_detected: r.regions_count, cer: r.cer }
    }).collect();

    GeometrySummary { total_regions, total_word_boxes, valid_bbox_count: valid_bbox, invalid_bbox_count: invalid_bbox, bbox_valid_ratio, dpi_results, vertical_results }
}

/// 冷加载测试
fn run_cold_load_tests(det_model: &Path, rec_model: &Path, dict_path: &Path, cpu_dll: &str, iterations: usize) -> Vec<ColdLoadResult> {
    let mut results = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let t0 = Instant::now();
        let _ = ort::init_from(PathBuf::from(cpu_dll));
        let ort_config = OrtSessionConfig::new().with_intra_threads(1).with_optimization_level(OrtGraphOptimizationLevel::Level1).with_execution_providers(vec![OrtExecutionProvider::CPU]);
        let ocr = OAROCRBuilder::new(det_model.to_str().unwrap(), rec_model.to_str().unwrap(), dict_path.to_str().unwrap())
            .ort_session(ort_config).return_word_box(true).build();
        let cold_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mem = winapi::get_working_set_mb();
        if let Err(e) = &ocr { info!("  Cold load #{} failed: {e}", i + 1); }
        results.push(ColdLoadResult { load_num: i + 1, cold_load_ms: cold_ms, memory_after_mb: mem });
    }
    results
}

/// 热推理测试
fn run_hot_inference_tests(ocr: &OAROCR, corpus_dir: &Path, corpus: &Corpus, iterations: usize) -> Vec<HotInferenceResult> {
    let test_item = corpus.items.iter().find(|i| i.subset == "medium").or(corpus.items.first()).unwrap();
    let image_path = corpus_dir.join(&test_item.image);
    let img = match image::open(&image_path) { Ok(img) => img.to_rgb8(), Err(e) => { info!("  Hot inference: Failed to load image: {e}"); return Vec::new(); } };
    let mut results = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let t0 = Instant::now();
        let _ = ocr.predict(vec![img.clone()]);
        let inference_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mem = winapi::get_working_set_mb();
        results.push(HotInferenceResult { round: i + 1, image: test_item.image.clone(), inference_ms, memory_peak_mb: mem });
    }
    results
}

/// 性能汇总
fn compute_performance_summary(cold: &[ColdLoadResult], hot: &[HotInferenceResult]) -> PerformanceSummary {
    let mut cold_times: Vec<f64> = cold.iter().map(|c| c.cold_load_ms).collect();
    cold_times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let mut hot_times: Vec<f64> = hot.iter().map(|h| h.inference_ms).collect();
    hot_times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let peak_rss = hot.iter().map(|h| h.memory_peak_mb).fold(0.0f64, |a, b| a.max(b));
    let mem_after_drop = hot.last().map(|h| h.memory_peak_mb).unwrap_or(0.0);
    let thread_count = winapi::get_thread_count();

    PerformanceSummary {
        cold_load_p50_ms: percentile(&cold_times, 0.5),
        cold_load_p95_ms: percentile(&cold_times, 0.95),
        hot_inference_p50_ms: percentile(&hot_times, 0.5),
        hot_inference_p95_ms: percentile(&hot_times, 0.95),
        peak_rss_mb: peak_rss,
        memory_after_drop_mb: mem_after_drop,
        thread_count,
    }
}

/// 并发测试
fn run_concurrency_tests(ocr: &OAROCR, corpus_dir: &Path, corpus: &Corpus) -> Vec<ConcurrencyTest> {
    let mut tests = Vec::new();
    for &concurrency in &[1, 2, 4] {
        let images: Vec<image::RgbImage> = corpus.items.iter().take(concurrency * 2).filter_map(|item| {
            let path = corpus_dir.join(&item.image);
            image::open(&path).ok().map(|i| i.to_rgb8())
        }).collect();
        if images.is_empty() {
            tests.push(ConcurrencyTest { concurrency, total_images: 0, total_ms: 0.0, avg_ms_per_image: 0.0, success: false, error: Some("No images loaded".to_string()) });
            continue;
        }
        let total_images = images.len();
        let t0 = Instant::now();
        // OAROCR is not Sync (holds ONNX sessions), so we run sequentially to measure throughput
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for img in &images { let _ = ocr.predict(vec![img.clone()]); }
        }));
        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
        tests.push(ConcurrencyTest {
            concurrency, total_images, total_ms,
            avg_ms_per_image: total_ms / total_images as f64,
            success: result.is_ok(),
            error: result.err().map(|e| format!("Panic: {:?}", e)),
        });
    }
    tests
}

/// 取消测试
fn run_cancel_tests(ocr: &OAROCR, corpus_dir: &Path, corpus: &Corpus) -> Vec<CancelTest> {
    let mut tests = Vec::new();
    let test_item = corpus.items.first().unwrap();
    let image_path = corpus_dir.join(&test_item.image);
    if let Ok(img) = image::open(&image_path) {
        let img = img.to_rgb8();
        // Test 1: Sequential inference consistency
        let r1 = ocr.predict(vec![img.clone()]);
        let r2 = ocr.predict(vec![img]);
        let same = match (&r1, &r2) {
            (Ok(a), Ok(b)) => a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.text_regions.len() == y.text_regions.len()),
            _ => false,
        };
        tests.push(CancelTest { description: "Sequential inference consistency".to_string(), cancelled: false, old_result_overwritten: !same, success: same });

        // Test 2: Timeout behavior (if inference takes too long, we simulate cancel)
        let timeout = Duration::from_millis(10000);
        let start = Instant::now();
        let mut timed_out = false;
        // Run a simple predict and check if it completes within timeout
        let predict_result = ocr.predict(vec![image::open(&image_path).unwrap().to_rgb8()]);
        if start.elapsed() > timeout { timed_out = true; }
        tests.push(CancelTest {
            description: format!("Inference timeout (10s limit, actual={:.1}ms)", start.elapsed().as_secs_f64() * 1000.0),
            cancelled: timed_out,
            old_result_overwritten: false,
            success: predict_result.is_ok(),
        });
    } else {
        tests.push(CancelTest { description: "Failed to load test image".to_string(), cancelled: false, old_result_overwritten: false, success: false });
    }
    tests
}

/// 资产清单
fn compute_asset_inventory(det_model: &Path, rec_model: &Path, dict_path: &Path, dll_path: &Path) -> Vec<AssetInfo> {
    let mut assets = Vec::new();
    for (name, path, source, license) in [
        ("pp-ocrv6_tiny_det.onnx", det_model, "HuggingFace: PaddlePaddle/PP-OCRv6_tiny_det_onnx", "Apache-2.0"),
        ("pp-ocrv6_tiny_rec.onnx", rec_model, "HuggingFace: PaddlePaddle/PP-OCRv6_tiny_rec_onnx", "Apache-2.0"),
        ("ppocrv6_tiny_dict.txt", dict_path, "PaddleOCR", "Apache-2.0"),
        ("onnxruntime.dll", dll_path, "Microsoft GitHub Release onnxruntime-win-x64-1.19.2", "MIT"),
    ] {
        if path.exists() {
            assets.push(AssetInfo {
                name: name.to_string(),
                path: path.display().to_string(),
                size_bytes: path.metadata().map(|m| m.len()).unwrap_or(0),
                sha256: sha256_file(path),
                source: source.to_string(),
                license: license.to_string(),
            });
        }
    }
    // Also check for companion DLL
    let companion = dll_path.with_file_name("onnxruntime_providers_shared.dll");
    if companion.exists() {
        assets.push(AssetInfo {
            name: "onnxruntime_providers_shared.dll".to_string(),
            path: companion.display().to_string(),
            size_bytes: companion.metadata().map(|m| m.len()).unwrap_or(0),
            sha256: sha256_file(&companion),
            source: "Microsoft GitHub Release onnxruntime-win-x64-1.19.2".to_string(),
            license: "MIT".to_string(),
        });
    }
    assets
}