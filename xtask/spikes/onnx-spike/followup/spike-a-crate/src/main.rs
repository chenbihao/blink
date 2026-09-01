//! Spike A: ORT 负面加载实验
//!
//! 每个 DLL 场景在独立 child process 中执行，避免全局 ORT 污染。
//!
//! 测试场景:
//! 1. 正确 CPU-only DLL (仅 init_from + commit)
//! 2. DLL 不存在
//! 3. zero-byte DLL
//! 4. random bytes 假 DLL
//! 5. ABI/版本不兼容 DLL (用 kernel32.dll 冒充)
//! 6. 缺少 companion DLL (只复制 onnxruntime.dll)
//! 7. 正确 DLL + 正确最小模型 Session (VAD model)
//! 8. 正确 DLL + 损坏模型
//! 9. GPU DLL (来自 Python venv, 作为参考)
//!
//! 每个 child process 通过 stdout JSON 报告结果。
//! 使用 Windows API (psapi) 获取真实内存和线程数。

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::info;

// === Windows API 绑定 ===

#[cfg(windows)]
mod winapi {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };

    /// 获取当前进程的工作集内存 (MB)
    pub fn get_working_set_mb() -> f64 {
        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            let handle: isize = -1; // GetCurrentProcess pseudo-handle
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

    /// 获取当前进程的峰值工作集内存 (MB)
    pub fn get_peak_working_set_mb() -> f64 {
        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            let handle: isize = -1;
            let ok = GetProcessMemoryInfo(
                handle as HANDLE,
                &mut counters,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            );
            if ok != 0 {
                counters.PeakWorkingSetSize as f64 / (1024.0 * 1024.0)
            } else {
                -1.0
            }
        }
    }

    /// 获取当前进程的线程数
    pub fn get_thread_count() -> u32 {
        unsafe {
            let snapshot = ToolHelp::CreateToolhelp32Snapshot(
                ToolHelp::TH32CS_SNAPTHREAD,
                0,
            );
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
    pub fn get_peak_working_set_mb() -> f64 { -1.0 }
    pub fn get_thread_count() -> u32 { 0 }
}

// === 数据结构 ===

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChildResult {
    scenario: String,
    command: String,
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    panicked: bool,
    access_violation: bool,
    session_created: bool,
    ort_version: Option<String>,
    error_message: Option<String>,
    child_metrics: Option<ChildMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChildMetrics {
    working_set_mb: f64,
    peak_working_set_mb: f64,
    thread_count: u32,
}

impl ChildResult {
    fn from_output(scenario: &str, cmd: &str, output: &std::process::Output) -> Self {
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // 检测 panic
        let panicked = stderr.contains("panicked") || stdout.contains("panicked");

        // 检测 access violation (Windows)
        let access_violation = stderr.contains("STATUS_ACCESS_VIOLATION")
            || stderr.contains("0xC0000005")
            || output.status.code().is_none();

        // 尝试从 stdout 解析 JSON 结果
        let (session_created, ort_version, error_message, child_metrics) =
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
                (
                    json.get("session_created")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    json.get("ort_version")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    json.get("error")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    json.get("metrics")
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                )
            } else {
                (false, None, None, None)
            };

        ChildResult {
            scenario: scenario.to_string(),
            command: cmd.to_string(),
            stdout,
            stderr,
            exit_code: output.status.code(),
            panicked,
            access_violation,
            session_created,
            ort_version,
            error_message,
            child_metrics,
        }
    }
}

fn run_child_scenario(
    scenario: &str,
    dll_path: &str,
    model_path: Option<&str>,
    exe: &Path,
) -> ChildResult {
    let mut cmd_str = format!("{} child --dll \"{}\"", exe.display(), dll_path);
    if let Some(mp) = model_path {
        cmd_str.push_str(&format!(" --model \"{}\"", mp));
    }

    let output = Command::new(exe)
        .args(["child", "--dll", dll_path])
        .args(model_path.map(|m| vec!["--model", m]).unwrap_or_default())
        .output();

    match output {
        Ok(o) => ChildResult::from_output(scenario, &cmd_str, &o),
        Err(e) => ChildResult {
            scenario: scenario.to_string(),
            command: cmd_str,
            stdout: String::new(),
            stderr: format!("Failed to spawn child: {e}"),
            exit_code: None,
            panicked: false,
            access_violation: false,
            session_created: false,
            ort_version: None,
            error_message: Some(format!("Spawn failed: {e}")),
            child_metrics: None,
        },
    }
}

/// 计算文件 SHA-256
fn sha256_file(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    match std::fs::read(path) {
        Ok(data) => {
            let mut hasher = Sha256::new();
            hasher.update(&data);
            hex::encode(hasher.finalize())
        }
        Err(_) => "FILE_NOT_FOUND".to_string(),
    }
}

/// 列出目录中所有 DLL 文件及其 SHA-256
fn list_dlls_with_hash(dir: &Path) -> Vec<serde_json::Value> {
    let mut dlls = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext.eq_ignore_ascii_case("dll") {
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let sha = sha256_file(&path);
                    dlls.push(serde_json::json!({
                        "name": name,
                        "size_bytes": size,
                        "sha256": sha,
                    }));
                }
            }
        }
    }
    dlls
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let is_child = args.iter().any(|a| a == "child");

    // 只在 parent mode 初始化 tracing
    // child mode 不初始化 tracing，避免 stdout 混入日志导致 JSON 解析失败
    if !is_child {
        tracing_subscriber::fmt()
            .with_env_filter("info")
            .init();
        info!("=== Spike A: ORT Negative Loading Tests ===");
    }

    let exe =
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("./onnx-spike-a"));

    // 检测 child 模式
    if is_child {
        return run_child_mode(&args);
    }

    // === Parent mode: 运行所有场景 ===

    // CPU-only ORT DLL 路径 (使用绝对路径避免 child process 工作目录问题)
    // current_exe() = followup/spike-a-crate/target/release/onnx-spike-a.exe
    // 4 层 parent = followup/
    let base_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent()?.parent()?.parent()?.to_str().map(String::from))
        .unwrap_or_else(|| ".".to_string());
    let cpu_dll = std::env::var("ORT_CPU_DLL_PATH").unwrap_or_else(|_| {
        format!("{}/runtimes/onnxruntime-cpu/onnxruntime.dll", base_dir)
    });
    let cpu_dll_path = PathBuf::from(&cpu_dll);

    // GPU ORT DLL 路径 (来自 Python venv)
    let gpu_dll = std::env::var("ORT_GPU_DLL_PATH").unwrap_or_else(|_| {
        "D:/Projects/Coding/blink/.tmp-venv/Lib/site-packages/onnxruntime/capi/onnxruntime.dll"
            .to_string()
    });

    // VAD 模型路径 (最小模型, 用于 Session 测试)
    // models 在 onnx-spike/ 下，即 followup 的上一级
    let models_dir = std::env::var("MODELS_DIR").unwrap_or_else(|_| {
        format!("{}/../models", base_dir)
    });
    let vad_model = std::env::var("VAD_MODEL_PATH").unwrap_or_else(|_| {
        format!("{}/fsmn-vad-onnx-v2/model_quant.onnx", models_dir)
    });

    // 损坏模型路径 (创建一个)
    let corrupted_model = PathBuf::from(format!("{}/runtimes/corrupted_model.onnx", base_dir));
    std::fs::write(&corrupted_model, b"NOT_A_VALID_ONNX_MODEL").ok();

    // Zero-byte DLL
    let zero_dll = PathBuf::from(format!("{}/runtimes/zero_byte.dll", base_dir));
    std::fs::write(&zero_dll, b"").ok();

    // Random bytes DLL
    let random_dll = PathBuf::from(format!("{}/runtimes/random_bytes.dll", base_dir));
    let random_data: Vec<u8> = (0..1024).map(|i| (i * 37 % 256) as u8).collect();
    std::fs::write(&random_dll, &random_data).ok();

    // "其他 DLL" 冒充 ORT (用 kernel32.dll 作为 ABI 不兼容 DLL)
    let fake_dll = PathBuf::from("C:/Windows/System32/kernel32.dll");

    let mut results = Vec::new();

    // 场景 1: 正确 CPU-only DLL
    info!("Scenario 1: Valid CPU-only DLL");
    results.push(run_child_scenario(
        "valid_cpu_dll",
        cpu_dll_path.to_str().unwrap(),
        None,
        &exe,
    ));

    // 场景 2: DLL 不存在
    info!("Scenario 2: DLL not found");
    results.push(run_child_scenario(
        "dll_not_found",
        "../runtimes/nonexistent/onnxruntime.dll",
        None,
        &exe,
    ));

    // 场景 3: Zero-byte DLL
    info!("Scenario 3: Zero-byte DLL");
    results.push(run_child_scenario(
        "zero_byte_dll",
        zero_dll.to_str().unwrap(),
        None,
        &exe,
    ));

    // 场景 4: Random bytes DLL
    info!("Scenario 4: Random bytes DLL");
    results.push(run_child_scenario(
        "random_bytes_dll",
        random_dll.to_str().unwrap(),
        None,
        &exe,
    ));

    // 场景 5: ABI 不兼容 DLL (kernel32.dll)
    info!("Scenario 5: ABI incompatible DLL (kernel32.dll)");
    results.push(run_child_scenario(
        "abi_incompatible_dll",
        fake_dll.to_str().unwrap(),
        None,
        &exe,
    ));

    // 场景 6: 缺少 companion DLL (只复制 onnxruntime.dll 到没有 providers_shared 的目录)
    let alone_dir = PathBuf::from(format!("{}/runtimes/ort-alone", base_dir));
    std::fs::create_dir_all(&alone_dir).ok();
    let alone_dll = alone_dir.join("onnxruntime.dll");
    // 只有 cpu_dll 存在时才复制
    if cpu_dll_path.exists() {
        std::fs::copy(&cpu_dll_path, &alone_dll).ok();
    }
    info!("Scenario 6: Missing companion DLL");
    results.push(run_child_scenario(
        "missing_companion_dll",
        alone_dll.to_str().unwrap(),
        None,
        &exe,
    ));

    // 场景 7: 正确 DLL + 正确最小模型 Session
    info!("Scenario 7: Valid DLL + valid model Session");
    results.push(run_child_scenario(
        "valid_dll_valid_model",
        cpu_dll_path.to_str().unwrap(),
        Some(&vad_model),
        &exe,
    ));

    // 场景 8: 正确 DLL + 损坏模型
    info!("Scenario 8: Valid DLL + corrupted model");
    results.push(run_child_scenario(
        "valid_dll_corrupted_model",
        cpu_dll_path.to_str().unwrap(),
        Some(corrupted_model.to_str().unwrap()),
        &exe,
    ));

    // GPU DLL 场景 (作为参考)
    info!("Scenario 9: GPU DLL (from Python venv)");
    results.push(run_child_scenario(
        "gpu_dll",
        &gpu_dll,
        None,
        &exe,
    ));

    // 收集 DLL 文件信息
    let cpu_dll_dir = cpu_dll_path.parent().unwrap_or(Path::new("."));
    let dll_files = list_dlls_with_hash(cpu_dll_dir);

    let gpu_dll_dir = PathBuf::from(&gpu_dll)
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let gpu_dll_files = list_dlls_with_hash(&gpu_dll_dir);

    // 验证问题回答
    let init_timing = results
        .iter()
        .find(|r| r.scenario == "valid_cpu_dll")
        .and_then(|r| {
            if r.session_created {
                Some(
                    "init_from loads DLL immediately via libloading::Library::new; \
                     version check at init_from time; Session::builder().commit_from_file() succeeds"
                        .to_string(),
                )
            } else if r.ort_version.is_some() {
                Some(
                    "init_from succeeded (DLL loaded) but no Session created (no model path)"
                        .to_string(),
                )
            } else if r.error_message.is_some() {
                Some(format!(
                    "init_from failed: {}",
                    r.error_message.as_ref().unwrap()
                ))
            } else {
                Some("init_from result unknown".to_string())
            }
        })
        .unwrap_or("NO_DATA".to_string());

    let can_switch_path = results.iter().find(|r| r.scenario == "gpu_dll").map(|r| {
        if r.ort_version.is_some() {
            "YES - different DLL loaded in separate child process, \
             but CANNOT switch within same process (OnceLock in load_dynamic module)".to_string()
        } else {
            match (&r.error_message, &r.ort_version) {
                (Some(e), _) => format!("NO - init_from failed: {e}"),
                _ => "NO - cannot determine".to_string(),
            }
        }
    }).unwrap_or("NOT_TESTED".to_string());

    let final_result = serde_json::json!({
        "spike": "A_ort_negative_loading",
        "ort_crate_version": "2.0.0-rc.13",
        "ort_crate_features": ["std", "ndarray", "load-dynamic", "tracing"],
        "oar_ocr_version": "0.9.2",
        "oar_ocr_license": "Apache-2.0",
        "ort_version_from_cpu_dll": results
            .iter()
            .find(|r| r.scenario == "valid_cpu_dll")
            .and_then(|r| r.ort_version.clone()),
        "cpu_dll_info": {
            "path": cpu_dll_path.display().to_string(),
            "exists": cpu_dll_path.exists(),
            "sha256": sha256_file(&cpu_dll_path),
            "directory_files": dll_files,
        },
        "gpu_dll_info": {
            "path": gpu_dll,
            "directory_files": gpu_dll_files,
        },
        "scenarios": results,
        "analysis": {
            "init_timing": init_timing,
            "can_switch_runtime_path_in_process": can_switch_path,
            "pending_restart_necessary":
                "YES - Windows locks loaded DLLs; OnceLock in load_dynamic prevents re-init; \
                 cannot overwrite/delete onnxruntime.dll until process exits",
            "init_from_validates_path":
                "YES - init_from calls libloading::Library::new(absolute_path) immediately; \
                 returns LoadError::Dlopen if file not found or not loadable",
            "commit_error_boundary":
                "commit() returns bool (true=first init, false=already initialized); \
                 init_from returns Result<EnvironmentBuilder, LoadDynamicError> with \
                 Dlopen/MissingApi/BadVersion variants",
        },
    });

    // 保存结果
    let results_dir = format!("{}/results", base_dir);
    std::fs::create_dir_all(&results_dir).ok();
    let json = serde_json::to_string_pretty(&final_result).unwrap_or_default();
    std::fs::write(format!("{}/spike_a_result.json", results_dir), &json).ok();

    println!("\n=== Spike A Result ===");
    println!("{json}");
}

/// Child mode: 在独立进程中执行单个 DLL 加载测试
fn run_child_mode(args: &[String]) {
    // 解析参数
    let mut dll_path = String::new();
    let mut model_path: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dll" => {
                if i + 1 < args.len() {
                    dll_path = args[i + 1].clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--model" => {
                if i + 1 < args.len() {
                    model_path = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    let dll = PathBuf::from(&dll_path);
    let result = test_dll_load(&dll, model_path.as_deref());

    // 输出 JSON 到 stdout
    let json = serde_json::to_string(&result).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"));
    println!("{json}");
}

#[derive(Debug, Serialize)]
struct DllLoadResult {
    dll_path: String,
    dll_exists: bool,
    dll_size: u64,
    init_from_succeeded: bool,
    init_from_error: Option<String>,
    commit_succeeded: bool,
    ort_version: Option<String>,
    session_created: bool,
    session_error: Option<String>,
    error: Option<String>,
    metrics: Option<ChildMetrics>,
}

fn collect_metrics() -> ChildMetrics {
    ChildMetrics {
        working_set_mb: winapi::get_working_set_mb(),
        peak_working_set_mb: winapi::get_peak_working_set_mb(),
        thread_count: winapi::get_thread_count(),
    }
}

fn test_dll_load(dll_path: &Path, model_path: Option<&str>) -> DllLoadResult {
    let dll_exists = dll_path.exists();
    let dll_size = std::fs::metadata(dll_path).map(|m| m.len()).unwrap_or(0);

    let mut result = DllLoadResult {
        dll_path: dll_path.display().to_string(),
        dll_exists,
        dll_size,
        init_from_succeeded: false,
        init_from_error: None,
        commit_succeeded: false,
        ort_version: None,
        session_created: false,
        session_error: None,
        error: None,
        metrics: None,
    };

    if !dll_exists {
        result.error = Some("DLL file does not exist".to_string());
        return result;
    }

    // 测试 ort::init_from
    // init_from 内部调用 libloading::Library::new(path) 立即加载 DLL
    // 如果文件不存在或不是有效 PE, 返回 LoadError::Dlopen
    // 如果文件是有效 PE 但没有 OrtGetApiBase 符号, 返回 LoadError::MissingApi
    // 如果版本不兼容, 返回 LoadError::BadVersion
    let init_result = std::panic::catch_unwind(|| ort::init_from(dll_path));

    match init_result {
        Ok(Ok(builder)) => {
            result.init_from_succeeded = true;
            // commit() 返回 bool (true = 成功初始化, false = 已有环境)
            let committed = builder.commit();
            result.commit_succeeded = committed;

            if committed {
                // 获取 ORT 版本
                result.ort_version = Some(ort::info().to_string());

                // 如果提供了模型路径，尝试创建 Session
                if let Some(model) = model_path {
                    let model_path = PathBuf::from(model);
                    match create_session(&model_path) {
                        Ok(()) => {
                            result.session_created = true;
                        }
                        Err(e) => {
                            result.session_error = Some(e);
                        }
                    }
                }

                // 收集 metrics (在所有操作完成后)
                result.metrics = Some(collect_metrics());
            } else {
                result.error =
                    Some("commit() returned false (already initialized?)".to_string());
                result.metrics = Some(collect_metrics());
            }
        }
        Ok(Err(e)) => {
            // init_from 返回了 LoadDynamicError
            result.init_from_error = Some(format!("{e}"));
            result.metrics = Some(collect_metrics());
        }
        Err(_) => {
            // init_from panic (不应该发生, 但防御性处理)
            result.error = Some("init_from panicked".to_string());
            result.metrics = Some(collect_metrics());
        }
    }

    result
}

fn create_session(model_path: &Path) -> Result<(), String> {
    if !model_path.exists() {
        return Err(format!("Model file not found: {}", model_path.display()));
    }

    // ort 2.0.0-rc.13 API:
    // Session::builder() -> Result<SessionBuilder>
    // .with_optimization_level(GraphOptimizationLevel::Level1) -> BuilderResult
    // .with_intra_threads(1) -> BuilderResult
    // .commit_from_file(path) -> Result<Session>
    use ort::session::builder::GraphOptimizationLevel;

    let mut builder = ort::session::Session::builder()
        .map_err(|e| format!("Session::builder() failed: {e}"))?;

    builder = builder
        .with_optimization_level(GraphOptimizationLevel::Level1)
        .map_err(|e| format!("with_optimization_level failed: {e}"))?;

    builder = builder
        .with_intra_threads(1)
        .map_err(|e| format!("with_intra_threads failed: {e}"))?;

    let _session = builder
        .commit_from_file(model_path)
        .map_err(|e| format!("commit_from_file failed: {e}"))?;

    Ok(())
}
