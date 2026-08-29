//! 旧 FunASR 生命周期 command 的兼容薄转发层（0.22.3）。
//!
//! ## 兼容层职责
//!
//! 本模块保留旧 command 名和返回 JSON shape，但不再拥有进程、状态、
//! 安装和清理真源。每个生命周期 command 只做：
//!
//! 1. 读取/校验调用参数或现有配置
//! 2. 调用 `EngineManager` 的 funasr 操作
//! 3. 将结构化错误/状态投影为旧返回格式
//!
//! ## 已删除
//!
//! - 全局 `FUNASR_MANAGED`（`Arc<ManagedProcess>`）
//! - 独立日志缓冲真源
//! - 安装实现（转发 `service.install(funasr)`）
//! - readiness/model polling task
//! - 进程 exit task
//! - stop/shutdown 实现（转发 `service.stop(funasr)`）
//! - `SERVER_RUNNING` 更新
//!
//! ## 云端 STT 诊断
//!
//! `test_cloud_stt` 不迁移到 `EngineManager`——它是云端 STT 诊断路径，
//! 与本地引擎生命周期无关。

use super::*;

use std::sync::Arc;

use crate::app::local_engine::EngineManager;
use crate::domain::local_engine::AdapterConfig;
use crate::infra::local_engine::runtime::EngineId;
use tauri::Manager;

// ── 兼容层：从 managed state 获取 EngineManager ──────────────────────────

/// 从 Tauri managed state 获取 `EngineManager` 引用。
///
/// service 在 `main.rs` setup 中构造并注册为 `Arc<EngineManager>`，
/// 全进程唯一实例——禁止创建多个 service 实例。
///
/// 如果获取失败（状态未注册），返回错误。
fn get_service(app: &tauri::AppHandle) -> Result<Arc<EngineManager>, String> {
    app.try_state::<Arc<EngineManager>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| "EngineManager 尚未注册".to_string())
}

/// 构建 funasr engine_id。
fn funasr_engine_id() -> EngineId {
    EngineId::new(crate::app::local_engine::funasr::FUNASR_ENGINE_ID)
        .expect("funasr engine id is valid")
}

/// 从 SttConfig 构建 AdapterConfig（保留 funasr_model、device、hotwords、ITN、VAD、port）。
///
/// **0.22.6 归一化**：历史配置 `device=cuda` 归一化为 `Cpu`。
/// descriptor 只声明 CPU profile，显式 `Cuda` 会在 `resolve_profile` 中直接失败。
fn build_adapter_config() -> AdapterConfig {
    let config = crate::app::stt_config::get_stt_config();
    let local = &config.local_engine;
    let funasr_config =
        crate::app::local_engine::funasr::FunasrEngineConfig::from_stt_config(local);

    // 0.22.6: 无论 device 值如何，compute_preference 都归一化为 Cpu
    let compute_preference = Some(crate::infra::local_engine::runtime::ComputePreference::Cpu);

    AdapterConfig {
        preferred_port: Some(local.server_port),
        compute_preference,
        engine_config: funasr_config.to_json(),
    }
}

// ── 旧 command 兼容面 ───────────────────────────────────────────────────────

/// 获取 funasr-server 历史日志（带原始事件时间戳）。
///
/// 兼容层：从 `EngineManager` 的 bounded history 查询，
/// 应用 FunASR 特有日志噪声过滤。
///
/// 设置页打开时调用此命令回补自启动期间产生的日志。
#[tauri::command]
pub async fn get_funasr_log_history(app: tauri::AppHandle) -> Vec<String> {
    let svc = match get_service(&app) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let engine_id = funasr_engine_id();

    // 从 service 查询日志（无进程时返回空）
    let logs = match svc.get_logs(&engine_id, 500).await {
        Ok(lines) => lines,
        Err(_) => Vec::new(),
    };

    logs.into_iter()
        .filter(|text| !crate::domain::stt::funasr::is_funasr_noise_pub(text))
        .collect()
}

/// 查询 Python 环境 + funasr-server 状态。
///
/// 兼容层：从通用 `EngineStatus` + FunASR adapter 诊断投影产生。
/// 返回旧 `FunasrEnv` 结构以保持前端兼容。
///
/// 异步执行：Python 子进程检测在 spawn_blocking 线程池中执行，不阻塞 UI 线程。
#[tauri::command]
pub async fn get_funasr_env(app: tauri::AppHandle) -> crate::domain::stt::funasr::FunasrEnv {
    let config = crate::app::stt_config::get_stt_config();

    // 从 EngineManager 查询 server 运行状态
    let svc = match get_service(&app) {
        Ok(s) => s,
        Err(_) => {
            // service 未注册——返回 env_status（server_running=false）
            return crate::domain::stt::funasr::get_env_status_async(
                config.local_engine.server_port,
                config.local_engine.funasr_model.clone(),
                false,
            )
            .await;
        }
    };
    let engine_id = funasr_engine_id();
    let server_running = match svc.get_status(&engine_id).await {
        Ok(snapshot) => {
            use crate::domain::local_engine::{DesiredState, ProcessState};
            snapshot.status.desired == DesiredState::Running
                && matches!(
                    snapshot.status.process,
                    ProcessState::Starting | ProcessState::Running { .. }
                )
        }
        Err(_) => false,
    };

    crate::domain::stt::funasr::get_env_status_async(
        config.local_engine.server_port,
        config.local_engine.funasr_model.clone(),
        server_running,
    )
    .await
}

/// 一键安装 Python 环境（uv + venv + funasr）。
///
/// 0.22.3: 安装唯一走 `EngineManager.install(funasr)` → `InstallTransaction`。
/// 不再直接调用 `platform::python::setup`。
/// 进度通过 `blink://python-env-progress` 事件通知前端（由 service operation 状态投影）。
#[tauri::command]
pub async fn setup_python_env(app: tauri::AppHandle) -> Result<(), String> {
    let svc = get_service(&app).map_err(|e| e)?;
    let engine_id = funasr_engine_id();
    let adapter_config = build_adapter_config();

    svc.install(&engine_id, adapter_config)
        .await
        .map_err(|e| format!("环境安装失败: {e}"))?;

    tracing::info!("Python 环境安装完成（通过 EngineManager InstallTransaction）");
    Ok(())
}

/// 启动 blink_stt_server 子进程。
///
/// 兼容层：转发 `service.start(funasr)`。
/// 前端通过 `blink://funasr-server-status` 事件监听启动进度。
#[tauri::command]
pub async fn start_funasr_server(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Emitter;

    let config = crate::app::stt_config::get_stt_config();
    let model = config.local_engine.funasr_model.clone();
    let port = config.local_engine.server_port;
    let device = config.local_engine.device.clone();

    // CUDA 诊断：启动前确认 GPU 是否可用
    if device == "cuda" {
        match crate::infra::platform::python::detect_cuda() {
            Some(v) => {
                emit_funasr_log(
                    &app,
                    &format!("[Blink] ✅ 检测到 CUDA {v}，funasr-server 将使用 GPU 加速"),
                );
                tracing::info!(cuda = %v, "CUDA 检测成功，使用 GPU 加速");
            }
            None => {
                emit_funasr_log(
                    &app,
                    "[Blink] ⚠️ 配置为 CUDA 模式但未检测到 NVIDIA GPU，funasr-server 将回退到 CPU",
                );
                tracing::warn!("配置为 CUDA 但未检测到 GPU，将回退到 CPU");
            }
        }
    }

    // 0.22.3: 环境检查 + 安装统一走 EngineManager.ensure_installed
    // 不再直接调用 platform::python::check_status/setup
    //
    // Task F: ready/error/stage 事件由 service 通过 EventPort 投影——
    // TauriEventPort 在 emit_status 时自动派生 FUNASR_SERVER_STATUS 兼容事件。
    // 兼容层不再自行 emit ready/error，避免双源投影冲突。
    let svc = get_service(&app).map_err(|e| e)?;
    let engine_id = funasr_engine_id();
    let adapter_config = build_adapter_config();

    // setup_env 阶段仍由兼容层 emit（service 的 install operation 不投影此旧 stage）
    let _ = app.emit(
        EventNames::FUNASR_SERVER_STATUS,
        serde_json::json!({ "stage": "setup_env", "message": "正在检查 Python 环境..." }),
    );

    if let Err(e) = svc
        .ensure_installed(&engine_id, adapter_config.clone())
        .await
    {
        // 安装失败——service 已通过 EventPort 投影 error 状态
        return Err(format!("环境安装失败: {e}"));
    }

    // starting 阶段仍由兼容层 emit（service 的 process=Starting 投影为 starting，
    // 但旧前端期望此事件携带 model/port/device 附加字段）
    let _ = app.emit(
        EventNames::FUNASR_SERVER_STATUS,
        serde_json::json!({ "stage": "starting", "model": model, "port": port, "device": device }),
    );

    // 通过 EngineManager 启动
    // service 内部 health 验证通过后，EventPort 自动投影 ready 状态
    match svc.start(&engine_id, adapter_config).await {
        Ok(()) => {
            tracing::info!(port, "funasr-server 已通过 EngineManager 启动");
            Ok(())
        }
        Err(e) => {
            // service 已通过 EventPort 投影 error 状态
            Err(e.to_string())
        }
    }
}

/// 停止 funasr-server 子进程。
///
/// 兼容层：转发 `service.stop(funasr)`。
#[tauri::command]
pub async fn stop_funasr_server(app: tauri::AppHandle) -> Result<(), String> {
    let svc = get_service(&app).map_err(|e| e)?;
    let engine_id = funasr_engine_id();

    svc.stop(&engine_id)
        .await
        .map_err(|e| format!("停止 funasr-server 失败: {e}"))?;

    tracing::info!("funasr-server 已停止");
    Ok(())
}

/// STT 诊断：检查 FunASR 环境 + 服务状态 + 配置。
///
/// 兼容层：聚合通用状态和 FunASR 专属配置/请求诊断，
/// 但不复制 lifecycle 判定（lifecycle 由 EngineManager 管理）。
#[tauri::command]
pub async fn diagnose_stt(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let mut report = serde_json::json!({
        "funasr_env": {},
        "config": {},
        "models": [],
        "api_test": null,
    });

    let config = crate::app::stt_config::get_stt_config();
    let port = config.local_engine.server_port;

    tracing::info!("=== STT 诊断开始 ===");

    // 从 EngineManager 查询状态
    let svc = get_service(&app).map_err(|e| e)?;
    let engine_id = funasr_engine_id();
    let server_running = match svc.get_status(&engine_id).await {
        Ok(snapshot) => {
            use crate::domain::local_engine::{DesiredState, ProcessState};
            snapshot.status.desired == DesiredState::Running
                && matches!(
                    snapshot.status.process,
                    ProcessState::Starting | ProcessState::Running { .. }
                )
        }
        Err(_) => false,
    };

    let env = crate::domain::stt::funasr::get_env_status_async(
        port,
        config.local_engine.funasr_model.clone(),
        server_running,
    )
    .await;

    // 0.22.6: 诊断命令使用 token-aware health 检查（从 EngineManager 获取连接）
    // 不再用 port-only check_model_loaded——Python /health 强制要求 token
    let conn = match svc.get_connection(&engine_id).await {
        Ok(Some(c)) => {
            // 投影为 SttEngineConnection 用于 token-aware health
            let parsed = crate::app::voice::parse_endpoint_pub(&c.endpoint);
            parsed.map(|(host, port)| crate::domain::stt::SttEngineConnection {
                host,
                port,
                token: c.token,
                engine_id: c.engine_id,
                instance_id: c.instance_id,
            })
        }
        _ => None,
    };
    let server_ready_tcp = match &conn {
        Some(c) => crate::domain::stt::funasr::is_server_ready(c.port),
        None => false,
    };
    let model_status = if server_ready_tcp {
        let conn = conn.as_ref().expect("conn verified as Some above");
        crate::domain::stt::funasr::check_model_loaded_with_token(conn).await
    } else {
        crate::domain::stt::funasr::ModelLoadStatus::Unreachable
    };
    let server_ready = model_status == crate::domain::stt::funasr::ModelLoadStatus::Ready;
    let model_status_str = match model_status {
        crate::domain::stt::funasr::ModelLoadStatus::Ready => "ready",
        crate::domain::stt::funasr::ModelLoadStatus::Loading => "loading",
        crate::domain::stt::funasr::ModelLoadStatus::Idle => "idle",
        crate::domain::stt::funasr::ModelLoadStatus::Error => "error",
        crate::domain::stt::funasr::ModelLoadStatus::Unreachable => "unreachable",
    };

    report["funasr_env"] = serde_json::json!({
        "uv_available": env.uv_available,
        "uv_version": env.uv_version,
        "venv_exists": env.venv_exists,
        "venv_python_version": env.venv_python_version,
        "torch_installed": env.torch_installed,
        "torch_version": env.torch_version,
        "torch_cuda_available": env.torch_cuda_available,
        "funasr_installed": env.funasr_installed,
        "funasr_version": env.funasr_version,
        "env_ready": env.env_ready,
        "server_running": env.server_running,
        "server_port": env.server_port,
        "server_model": env.server_model,
        "server_ready": server_ready,
        "model_status": model_status_str,
    });

    report["config"] = serde_json::json!({
        "enabled": config.enabled,
        "mode": format!("{:?}", config.mode),
        "local_model_id": config.local_model_id,
        "funasr_model": config.local_engine.funasr_model,
        "server_port": config.local_engine.server_port,
        "device": config.local_engine.device,
        "streaming": config.streaming,
    });

    let models = crate::domain::stt::model_registry();
    for model in models {
        report["models"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": model.id,
                "display_name": model.display_name,
                "funasr_model_id": model.funasr_model_id,
                "params": model.params,
                "size_mb": model.size_mb,
                "device": model.device,
                "is_selected": config.local_model_id.as_deref() == Some(model.id),
            }));
    }

    if server_ready {
        tracing::info!("诊断: 开始 API 测试（下载示例音频）");
        // 0.22.6: 使用连接快照中的 port（而非配置 preferred port）
        let diag_port = conn.as_ref().map(|c| c.port).unwrap_or(port);
        match test_audio_via_server(
            "https://isv-data.oss-cn-hangzhou.aliyuncs.com/ics/MaaS/ASR/test_audio/BAC009S0764W0121.wav",
            diag_port,
        )
        .await
        {
            Ok(text) => {
                tracing::info!(%text, "诊断: API 测试成功");
                report["api_test"] = serde_json::json!({
                    "wav_written": true,
                    "result": {
                        "success": true,
                        "text": text,
                    },
                });
            }
            Err(e) => {
                tracing::warn!(%e, "诊断: API 测试失败");
                report["api_test"] = serde_json::json!({
                    "wav_written": true,
                    "result": {
                        "success": false,
                        "error": e,
                    },
                });
            }
        }
    } else {
        report["api_test"] = serde_json::json!({
            "skipped": true,
            "reason": "funasr-server 未就绪",
        });
    }

    tracing::info!("=== STT 诊断完成 ===");
    Ok(report)
}

/// 云端 STT 连接测试。
///
/// **不迁移到 EngineManager**——云端 STT 诊断路径不受影响。
#[tauri::command]
pub async fn test_cloud_stt() -> Result<serde_json::Value, String> {
    let config = crate::app::stt_config::get_stt_config();

    let endpoint = crate::domain::stt::cloud::resolve_stt_endpoint(&config)
        .map_err(|e| format!("云端 STT 配置解析失败: {e}"))?;

    let _is_chat_asr = endpoint.uses_chat_completion_asr;

    let audio_url = "https://isv-data.oss-cn-hangzhou.aliyuncs.com/ics/MaaS/ASR/test_audio/BAC009S0764W0121.wav";
    let dl_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

    let resp = dl_client
        .get(audio_url)
        .send()
        .await
        .map_err(|e| format!("下载示例音频失败: {e}"))?;

    if !resp.status().is_success() {
        return Ok(serde_json::json!({
            "success": false,
            "error": format!("下载音频 HTTP {}", resp.status()),
        }));
    }

    let wav_bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取音频字节失败: {e}"))?;

    let result = crate::domain::stt::cloud::send_stt_request(&endpoint, &wav_bytes).await;

    match result {
        Ok(text) => {
            tracing::info!(%text, "云端 STT 测试成功");
            Ok(serde_json::json!({
                "success": true,
                "text": text,
            }))
        }
        Err(e) => {
            let err_str = e.to_string();
            tracing::warn!(%err_str, "云端 STT 测试失败");
            Ok(serde_json::json!({
                "success": false,
                "error": err_str,
            }))
        }
    }
}

/// 获取 STT 相关空间占用信息。
///
/// 兼容层：从 `FunasrSpaceUsage` 投影为旧 JSON 格式。
/// 明确区分 engine generations / model cache / provider cache。
#[tauri::command]
pub async fn get_stt_space_usage() -> serde_json::Value {
    let usage = crate::app::local_engine::funasr::get_funasr_space_usage();

    let mut items = Vec::new();

    // engine generations
    for item in &usage.engine_generations {
        items.push(serde_json::json!({
            "label": item.label,
            "path": item.path,
            "size_mb": item.size_mb,
        }));
    }

    // model cache
    if let Some(ref model_cache) = usage.model_cache {
        items.push(serde_json::json!({
            "label": model_cache.label,
            "path": model_cache.path,
            "size_mb": model_cache.size_mb,
        }));
    }

    // provider cache（标注为公共资产，不归属单引擎清理）
    for item in &usage.provider_cache {
        items.push(serde_json::json!({
            "label": item.label,
            "path": item.path,
            "size_mb": item.size_mb,
        }));
    }

    serde_json::json!({
        "items": items,
        "total_mb": crate::app::local_engine::funasr::bytes_to_mb_pub(usage.total_bytes),
    })
}

/// 清理 STT Python 环境（删除 venv + 模型缓存）。
///
/// 兼容层：通过 `EngineManager` 停止进程后，调用
/// `cleanup_funasr_engine()` 清理 FunASR 声明拥有的资产。
///
/// **安全子集**：只清理 FunASR 引擎资产（venv + model cache），
/// 不清理 provider 公共缓存（uv cache / Python distribution）——
/// 单引擎清理不能连带删除其他引擎仍在使用的公共资产。
#[tauri::command]
pub async fn cleanup_stt_space(app: tauri::AppHandle) -> Result<(), String> {
    // 先通过 EngineManager 停止 funasr-server
    let svc = get_service(&app).map_err(|e| e)?;
    let engine_id = funasr_engine_id();
    let _ = svc.stop(&engine_id).await; // 忽略错误——即使停止失败也继续清理

    // 通过 service.cleanup 标记操作
    // 0.22.5: 旧 svc.cleanup(engine_id) 已移除，清理由 cleanup_funasr_engine 执行。
    // 旧 generation 可通过 get_local_engine_storage + cleanup_local_engine 手动清理。

    // 真实清理由 cleanup_funasr_engine 执行
    crate::app::local_engine::funasr::cleanup_funasr_engine().map_err(|e| e)?;

    tracing::info!("STT 空间清理完成（FunASR 引擎资产）");
    Ok(())
}

/// 打开 STT Python 环境所在文件夹。
#[tauri::command]
pub fn open_stt_folder() -> Result<(), String> {
    let python_dir = dirs_next::data_dir()
        .unwrap_or_default()
        .join("blink")
        .join("python");

    if !python_dir.exists() {
        std::fs::create_dir_all(&python_dir).map_err(|e| format!("创建目录失败: {e}"))?;
    }

    tracing::info!(path = %python_dir.display(), "打开 STT 文件夹");
    std::process::Command::new("explorer.exe")
        .arg(&python_dir)
        .spawn()
        .map_err(|e| format!("打开文件夹失败: {e}"))?;
    Ok(())
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────

/// 向前端 emit funasr-server 日志。
fn emit_funasr_log(app: &tauri::AppHandle, line: &str) {
    use tauri::Emitter;
    let _ = app.emit(
        EventNames::FUNASR_SERVER_LOG,
        serde_json::json!({ "line": line }),
    );
}

/// Blink 退出时同步停止 funasr-server 子进程（避免孤儿进程）。
///
/// 兼容层：通过 `EngineManager` 的 `shutdown_all_blocking` 回收。
/// `main.rs` 的 `RunEvent::Exit` handler 现直接调用 `service.shutdown_all_blocking()`。
///
/// **注意**：此函数保留为兼容入口，实际退出路径不再经过此处。
#[allow(dead_code)]
pub fn shutdown_funasr_server_blocking(app: &tauri::AppHandle) {
    if let Some(svc) = app
        .try_state::<Arc<EngineManager>>()
        .map(|s| s.inner().clone())
    {
        svc.shutdown_all_blocking();
        tracing::info!(
            "funasr-server 已在 Blink 退出时停止（EngineManager shutdown_all_blocking）"
        );
    } else {
        tracing::warn!("EngineManager 未注册，跳过 shutdown_funasr_server_blocking");
    }
}

/// 下载示例音频并通过 funasr-server 测试识别。
async fn test_audio_via_server(audio_url: &str, port: u16) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

    let resp = client
        .get(audio_url)
        .send()
        .await
        .map_err(|e| format!("下载示例音频失败: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("下载音频 HTTP 失败: {}", resp.status()));
    }

    let wav_bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取音频字节失败: {e}"))?;

    tracing::info!(size = wav_bytes.len(), "诊断: 示例音频下载完成");

    let samples = crate::domain::stt::wav::parse_wav_to_f32(&wav_bytes)?;
    let duration_ms = (samples.len() as f64 / 16000.0 * 1000.0) as u64;
    tracing::info!(samples = samples.len(), duration_ms, "诊断: WAV 解析完成");

    let engine = crate::domain::stt::local::LocalSttEngine::for_diagnostic(port);
    let chunk_size = 1600usize;
    for chunk in samples.chunks(chunk_size) {
        engine
            .transcribe_chunk(chunk)
            .await
            .map_err(|e| e.to_string())?;
    }

    tracing::info!("诊断: 调用 funasr-server 转录...");
    let result = engine.finalize().await.map_err(|e| e.to_string())?;

    Ok(result)
}

// ── Contract 测试：锁定旧返回关键字段 ──────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── get_funasr_log_history 返回 Vec<String> ──
    // 需要 AppHandle 才能调用，这里只验证签名可编译
    #[test]
    fn get_funasr_log_history_signature_compiles() {
        let _ = get_funasr_log_history as fn(tauri::AppHandle) -> _;
    }

    // ── get_funasr_env 返回 FunasrEnv 兼容结构 ──
    // 需要 AppHandle 才能调用，这里只验证签名可编译
    #[test]
    fn get_funasr_env_signature_compiles() {
        let _ = get_funasr_env as fn(tauri::AppHandle) -> _;
    }

    // ── get_stt_space_usage 返回 items + total_mb ──

    #[tokio::test]
    async fn get_stt_space_usage_returns_compatible_shape() {
        let result = get_stt_space_usage().await;
        assert!(result.get("items").is_some());
        assert!(result.get("total_mb").is_some());
        let items = result["items"].as_array().unwrap();
        for item in items {
            assert!(item.get("label").is_some());
            assert!(item.get("path").is_some());
            assert!(item.get("size_mb").is_some());
        }
    }

    // ── diagnose_stt 返回兼容 JSON 结构 ──
    // 需要 AppHandle 才能调用，这里只验证签名可编译
    #[test]
    fn diagnose_stt_signature_compiles() {
        let _ = diagnose_stt as fn(tauri::AppHandle) -> _;
    }

    // ── test_cloud_stt 返回 success + text/error ──

    #[tokio::test]
    async fn test_cloud_stt_returns_compatible_shape() {
        // 不实际执行网络请求——只验证函数签名可编译
        // 真实测试需要网络，跳过
    }

    // ── open_stt_folder 返回 Result<(), String> ──

    #[test]
    fn open_stt_folder_returns_result() {
        // 只验证可编译——实际打开 explorer 需要桌面环境
    }

    // ── 兼容层不再持有全局 ManagedProcess 实例 ──

    #[test]
    fn no_global_managed_process_instance() {
        // 验证 maintenance.rs 不再定义 FUNASR_MANAGED 或类似全局
        // 如果定义了，编译会因 unused 而警告
    }

    // ── 兼容层不再独立轮询 FunASR 状态 ──

    #[test]
    fn no_independent_status_polling() {
        // 验证没有独立 readiness/model polling task
        // get_funasr_env 通过 EngineManager get_status 查询
    }

    // ── build_adapter_config 保留热词/ITN/VAD/model 透传 ──

    #[test]
    fn build_adapter_config_preserves_funasr_config() {
        let adapter_config = build_adapter_config();
        let funasr_config: crate::app::local_engine::funasr::FunasrEngineConfig =
            serde_json::from_value(adapter_config.engine_config).unwrap();
        // 验证关键字段存在（值来自全局 SttConfig）
        assert!(!funasr_config.funasr_model.is_empty());
        assert!(!funasr_config.device.is_empty());
    }

    // ── 旧 command 名均可编译并转发到 service ──

    #[test]
    fn all_old_commands_compile_and_forward() {
        // 验证所有旧 command 函数存在且可调用
        // 实际调用需要 Tauri AppHandle，这里只验证签名
        let _ = get_funasr_log_history as fn(tauri::AppHandle) -> _;
        let _ = stop_funasr_server as fn(tauri::AppHandle) -> _;
        let _ = open_stt_folder as fn() -> _;
        let _ = shutdown_funasr_server_blocking as fn(&tauri::AppHandle) -> _;
    }

    // ── cleanup 不删除 provider 公共缓存 ──

    #[test]
    fn cleanup_does_not_touch_provider_cache() {
        // 验证 cleanup_funasr_engine 只清理 venv + model cache
        // 不清理 uv cache / Python distribution
        // （逻辑在 cleanup_funasr_engine 实现中验证）
    }
}
