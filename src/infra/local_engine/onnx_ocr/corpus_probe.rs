//! OCR corpus 真实推理探针（`#[ignore]` 手动跑，不进 CI）。
//!
//! 对 `testdata/ocr/corpus/` 下的图片用真实 PP-OCRv6 tiny 模型做识别，
//! 把含 `lines` + `char_boxes` 的 `OcrResult` JSON 落到
//! `testdata/ocr/corpus/results/<stem>.ocr.json`，供前端颜色采样
//! （`sampleAverageBackgroundColorFromPixels` / `sampleOriginalInkColorsFromPixels`）
//! 离线评估脚本消费。
//!
//! 运行：
//! `cargo test --bin blink ocr_corpus_color_probe -- --ignored --nocapture`
//!
//! 模型位置与 GUI 安装布局一致：`%APPDATA%\blink\runtimes\engines\paddleocr\slot-a\`。

use std::path::PathBuf;

use super::pipeline::{OcrPipeline, OrtocrPipeline};

/// 定位 slot-a 模型目录（与生产安装布局一致）。
fn slot_a_dir() -> Option<PathBuf> {
    let base = std::env::var("APPDATA").ok()?;
    let dir = PathBuf::from(base)
        .join("blink")
        .join("runtimes")
        .join("engines")
        .join("paddleocr")
        .join("slot-a");
    dir.is_dir().then_some(dir)
}

#[test]
#[ignore]
fn ocr_corpus_color_probe() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/ocr/corpus");
    let out_dir = corpus_dir.join("results");
    std::fs::create_dir_all(&out_dir).expect("创建 results 目录失败");

    let slot = slot_a_dir().expect("PaddleOCR slot-a 模型目录不存在（%APPDATA%\\blink\\runtimes）");
    let dll = slot.join("onnxruntime.dll");
    let det = slot.join("pp-ocrv6_tiny_det.onnx");
    let rec = slot.join("pp-ocrv6_tiny_rec.onnx");
    let dict = slot.join("ppocrv6_tiny_dict.txt");
    for p in [&dll, &det, &rec, &dict] {
        assert!(p.exists(), "缺少模型文件: {}", p.display());
    }

    // build 会阻塞加载 DLL/模型——测试线程是独立线程，可直接 build
    //（与 executor 的专用阻塞线程同语义，不在 tokio runtime 上）。
    let mut pipeline =
        OrtocrPipeline::build(&det, &rec, &dict, &dll, 1, 1).expect("pipeline 构建失败");

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&corpus_dir)
        .expect("读取 corpus 目录失败")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("png"))
        })
        .collect();
    entries.sort();

    assert!(!entries.is_empty(), "corpus 目录没有 PNG 用例");

    for img_path in entries {
        let stem = img_path.file_stem().unwrap().to_string_lossy().to_string();
        let png = std::fs::read(&img_path).expect("读取图片失败");
        let t0 = std::time::Instant::now();
        let result = pipeline.recognize(&png).expect("识别失败");
        println!(
            "{stem}: {} 行 / {} 字符框 / {} ms",
            result.lines.len(),
            result.char_boxes.len(),
            t0.elapsed().as_millis()
        );
        let json = serde_json::to_string_pretty(&result).expect("序列化失败");
        std::fs::write(out_dir.join(format!("{stem}.ocr.json")), json).expect("写结果失败");
    }
}
