//! FunASR、诊断与 STT 存储维护 commands（0.22.1 迁移到 ManagedProcess）。
//!
//! 全局 funasr-server 子进程句柄从裸 `tokio::process::Child` 迁移为
//! `Arc<ManagedProcess>`。通用进程生命周期逻辑（spawn/pump/wait/stop/
//! 进程树回收）进入 infra/local_engine/ManagedProcess。
//! FunASR 特有的日志过滤、模型等待和诊断保留在 app 层。

use super::*;

use std::sync::Arc;

use crate::infra::local_engine::{ManagedProcess, ProcessStatus};
use tokio::sync::broadcast;

/// 全局 ManagedProcess 实例（替代旧 FUNASR_SERVER_CHILD）。
///
/// 完整 LocalEngineService 留到 0.22.3，当前 app 层持有此实例。
static FUNASR_MANAGED: once_cell::sync::Lazy<Arc<ManagedProcess>> =
    once_cell::sync::Lazy::new(|| ManagedProcess::with_defaults());

/// funasr-server 日志环形缓冲区（最近 500 条，与 LogPipeConfig 对齐）。
///
/// 服务可能在设置页打开前就自启动（auto_start_server），此时前端
/// `listen("blink://funasr-server-log")` 尚未注册，日志会丢失。
/// 缓冲区让设置页打开时通过 `get_funasr_log_history` 命令回补历史日志。
///
/// 0.22.1：此缓冲区现在从 ManagedProcess 的 LogPipe 历史中读取，
/// 不再单独维护 std::sync::Mutex<VecDeque>。
const FUNASR_LOG_BUFFER_CAP: usize = 500;

/// 获取 funasr-server 历史日志（带原始事件时间戳）。
///
/// 设置页打开时调用此命令回补自启动期间产生的日志。
/// 时间戳来自 LogEntry 在 append 时记录的事件时间，不重新生成。
#[tauri::command]
pub async fn get_funasr_log_history() -> Vec<String> {
    let history = FUNASR_MANAGED.log_history().await;
    history
        .into_iter()
        .filter_map(|entry| {
            // 应用 FunASR 特有日志噪声过滤
            let text = entry.text.as_str();
            if crate::domain::stt::funasr::is_funasr_noise_pub(text) {
                return None;
            }
            // 使用原始事件时间戳（不重新生成）
            let ts = format_timestamp_ms(entry.timestamp_ms);
            Some(format!("[{}] {}", ts, text))
        })
        .collect()
}

/// 查询 Python 环境 + funasr-server 状态。
///
/// 返回 uv/venv/funasr 安装状态 + server 运行状态，供前端展示和诊断。
///
/// 异步执行：Python 子进程检测在 spawn_blocking 线程池中执行，不阻塞 UI 线程。
#[tauri::command]
pub async fn get_funasr_env() -> crate::domain::stt::funasr::FunasrEnv {
    let config = crate::app::stt_config::get_stt_config();
    crate::domain::stt::funasr::get_env_status_async(
        config.local_engine.server_port,
        config.local_engine.funasr_model.clone(),
    )
    .await
}

/// 一键安装 Python 环境（uv + venv + funasr）。
///
/// Blink 通过 uv 自动创建独立的 Python 3.12 虚拟环境并安装 funasr。
/// 用户无需手动安装 Python 或 pip 包。
///
/// 进度通过 `blink://python-env-progress` 事件通知前端。
#[tauri::command]
pub async fn setup_python_env(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Emitter;

    let app_progress = app.clone();
    let on_progress: crate::infra::platform::python::ProgressCallback =
        std::sync::Arc::new(move |stage, status| {
            let _ = app_progress.emit(
                EventNames::PYTHON_ENV_PROGRESS,
                serde_json::json!({ "stage": stage, "status": status }),
            );
        });

    let app_log = app.clone();
    let on_log: std::sync::Arc<dyn Fn(&str) + Send + Sync> = std::sync::Arc::new(move |line| {
        emit_funasr_log(&app_log, line);
    });

    let device = crate::app::stt_config::get_stt_config().local_engine.device;
    crate::infra::platform::python::setup_with_progress(&device, on_progress, on_log).await
}

/// 启动 blink_stt_server 子进程。
///
/// 在后台异步启动 STT server，前端通过 `blink://funasr-server-status` 事件
/// 监听启动进度。模型首次下载可能需要较长时间。
///
/// 0.22.1：通用进程生命周期委托给 ManagedProcess，不再直接操作裸 Child。
#[tauri::command]
pub async fn start_funasr_server(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Emitter;

    // 从配置构建启动参数（含脚本释放 + 热词文件写入）
    let params = match crate::domain::stt::funasr::ServerStartParams::from_config() {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    let model = params.model.clone();
    let port = params.port;
    let device = params.device.clone();

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

    // 检查 Python 环境是否就绪，未就绪则自动安装
    let py_status = crate::infra::platform::python::check_status_async().await;
    let (torch_installed, funasr_installed, torch_cuda_available) =
        tokio::task::spawn_blocking(|| {
            let (torch, _) = crate::infra::platform::python::check_torch();
            let (funasr, _) = crate::infra::platform::python::check_funasr();
            let cuda = torch && crate::infra::platform::python::check_torch_cuda();
            (torch, funasr, cuda)
        })
        .await
        .unwrap_or((false, false, false));
    let funasr_env_ready = py_status.env_ready && torch_installed && funasr_installed;
    if !funasr_env_ready || (device == "cuda" && !torch_cuda_available) {
        let need_cuda_reinstall = device == "cuda" && torch_installed && !torch_cuda_available;
        let _ = app.emit(
            EventNames::FUNASR_SERVER_STATUS,
            serde_json::json!({ "stage": "setup_env", "message": "正在安装 Python 环境..." }),
        );
        if need_cuda_reinstall {
            emit_funasr_log(
                &app,
                "[Blink] ⚠️ 当前 PyTorch 为 CPU 版，正在重装 CUDA 版 PyTorch（可能需要数分钟）...",
            );
        }
        match crate::infra::platform::python::setup(&device).await {
            Ok(()) => {
                tracing::info!("Python 环境安装完成");
                if device == "cuda" {
                    let cuda_ok = crate::infra::platform::python::check_torch_cuda();
                    if cuda_ok {
                        emit_funasr_log(&app, "[Blink] ✅ PyTorch CUDA 支持已就绪，GPU 加速可用");
                    } else {
                        emit_funasr_log(
                            &app,
                            "[Blink] ⚠️ PyTorch CUDA 支持不可用，将使用 CPU 推理",
                        );
                    }
                }
            }
            Err(e) => {
                let _ = app.emit(
                    EventNames::FUNASR_SERVER_STATUS,
                    serde_json::json!({ "stage": "error", "error": format!("Python 环境安装失败: {e}") }),
                );
                return Err(format!(
                    "Python 环境安装失败: {e}
请在设置页手动点击「安装环境」按钮。"
                ));
            }
        }
    }

    let _ = app.emit(
        EventNames::FUNASR_SERVER_STATUS,
        serde_json::json!({ "stage": "starting", "model": model, "port": port, "device": device }),
    );

    // ── 0.22.1：端口冲突检测（不杀未知进程）──
    // 如果 preferred port 上已有健康服务但不能证明是当前 Blink 实例：
    // 返回可行动的端口冲突/未知服务错误，不自动 kill 未知 PID。
    let managed_state = FUNASR_MANAGED.snapshot().await;
    let is_our_process_running = matches!(
        managed_state.status,
        ProcessStatus::Running { .. } | ProcessStatus::Starting
    );

    if !is_our_process_running && crate::domain::stt::funasr::is_server_ready(port) {
        // 端口被占但不是我们的 ManagedProcess → 未知进程
        tracing::warn!(port, "端口被未知进程占用，不自动终止");
        emit_funasr_log(
            &app,
            &format!(
                "[Blink] ⚠️ 端口 {port} 已被其他程序占用，无法启动 funasr-server。请在设置页更换端口或关闭占用端口的程序。"
            ),
        );
        let _ = app.emit(
            EventNames::FUNASR_SERVER_STATUS,
            serde_json::json!({
                "stage": "error",
                "error": format!("端口 {port} 被未知进程占用，Blink 不会自动终止未知进程。请更换端口或手动关闭占用端口的程序。")
            }),
        );
        return Err(format!(
            "端口 {port} 被未知进程占用，Blink 不会自动终止未知进程。请在设置页更换端口。"
        ));
    }

    // ── 通过 ManagedProcess 启动 ──
    match crate::domain::stt::funasr::start_server(&params, &FUNASR_MANAGED).await {
        Ok(true) => {
            // 取得本次启动的 generation + instance_id（用于绑定日志转发和 readiness task）
            let start_token = FUNASR_MANAGED.current_token().await;

            // ── 启动日志转发 task：绑定本次 generation ──
            // 旧 generation 的日志转发 task 在发现 generation 不匹配时退出，
            // 不继续投影新一代或重复投影。
            let app_log = app.clone();
            let managed_for_log = Arc::clone(&FUNASR_MANAGED);
            let log_token = start_token.clone();
            let mut sub = FUNASR_MANAGED.subscribe_logs();
            tokio::spawn(async move {
                loop {
                    match sub.recv().await {
                        Ok(entry) => {
                            // 验证当前 generation 仍匹配
                            let current = managed_for_log.current_token().await;
                            if current != log_token {
                                tracing::debug!(
                                    gen = log_token.generation,
                                    "日志转发 task: generation 不匹配，退出"
                                );
                                return;
                            }
                            // 应用 FunASR 特有日志噪声过滤
                            if crate::domain::stt::funasr::is_funasr_noise_pub(&entry.text) {
                                continue;
                            }
                            emit_funasr_log(&app_log, &entry.text);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Lagged 后不永久退出，继续接收
                            tracing::warn!(
                                lag = n,
                                gen = log_token.generation,
                                "日志广播 Lagged，跳过 {n} 条，继续接收"
                            );
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::debug!(
                                gen = log_token.generation,
                                "日志广播已关闭，转发 task 退出"
                            );
                            return;
                        }
                    }
                }
            });

            // ── 异步等待服务就绪（绑定本次 generation）──
            // 每个 await 返回后必须重新验证 token，防止旧 generation 的迟到结果
            // 修改新实例的状态或发出错误的事件。
            let app_clone = app.clone();
            let model_clone = model.clone();
            let managed = Arc::clone(&FUNASR_MANAGED);
            let readiness_token = start_token.clone();
            tokio::spawn(async move {
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(
                        crate::domain::stt::funasr::SERVER_STARTUP_TIMEOUT_SECS,
                    );

                let mut loading_notified = false;

                loop {
                    // 循环开头验证 generation
                    if !managed.is_current_token(&readiness_token).await {
                        tracing::debug!(
                            gen = readiness_token.generation,
                            "readiness task: generation 不匹配，退出（不停止新实例）"
                        );
                        return;
                    }

                    if std::time::Instant::now() > deadline {
                        // 超时前再验证 token
                        if !managed.is_current_token(&readiness_token).await {
                            return;
                        }
                        let _ = app_clone.emit(
                            EventNames::FUNASR_SERVER_STATUS,
                            serde_json::json!({
                                "stage": "error",
                                "error": format!(
                                    "funasr-server 在 {}s 内未就绪（端口 {}）",
                                    crate::domain::stt::funasr::SERVER_STARTUP_TIMEOUT_SECS,
                                    port
                                )
                            }),
                        );
                        tracing::error!(port, "funasr-server 启动超时");
                        // stop_if_current 内部会验证 token
                        let _ = managed.stop_if_current(&readiness_token).await;
                        // stop 后再验证 token 仍匹配才标记全局状态
                        if managed.is_current_token(&readiness_token).await {
                            crate::domain::stt::funasr::mark_server_stopped();
                        }
                        return;
                    }

                    // 检查子进程是否已退出
                    let state = managed.snapshot().await;
                    if state.token != readiness_token {
                        tracing::debug!(
                            gen = readiness_token.generation,
                            "readiness task: generation 不匹配，退出"
                        );
                        return;
                    }
                    if let ProcessStatus::Exited { ref reason } = state.status {
                        let _ = app_clone.emit(
                            EventNames::FUNASR_SERVER_STATUS,
                            serde_json::json!({
                                "stage": "error",
                                "error": format!("funasr-server 进程已退出: {reason:?}")
                            }),
                        );
                        tracing::error!(?reason, port, "funasr-server 进程异常退出");
                        // token 已验证匹配
                        crate::domain::stt::funasr::mark_server_stopped();
                        return;
                    }

                    // 检查模型加载状态——这个 await 返回后必须重新验证 token
                    let model_status = crate::domain::stt::funasr::check_model_loaded(port).await;

                    // ★ 关键：await 返回后重新验证 token
                    if !managed.is_current_token(&readiness_token).await {
                        tracing::debug!(
                            gen = readiness_token.generation,
                            "readiness task: health await 后 generation 不匹配，丢弃结果退出"
                        );
                        return;
                    }

                    match model_status {
                        crate::domain::stt::funasr::ModelLoadStatus::Ready => {
                            // emit 前已验证 token
                            let _ = app_clone.emit(
                                EventNames::FUNASR_SERVER_STATUS,
                                serde_json::json!({ "stage": "ready", "port": port, "model": &model_clone }),
                            );
                            tracing::info!(port, "funasr-server 就绪（模型已加载）");
                            return;
                        }
                        crate::domain::stt::funasr::ModelLoadStatus::Error => {
                            let _ = app_clone.emit(
                                EventNames::FUNASR_SERVER_STATUS,
                                serde_json::json!({
                                    "stage": "error",
                                    "error": "模型加载失败，请检查网络连接后重试，或查看日志排查原因"
                                }),
                            );
                            tracing::error!(port, "funasr-server 模型加载失败");
                            return;
                        }
                        crate::domain::stt::funasr::ModelLoadStatus::Loading
                        | crate::domain::stt::funasr::ModelLoadStatus::Idle => {
                            if !loading_notified {
                                let _ = app_clone.emit(
                                    EventNames::FUNASR_SERVER_STATUS,
                                    serde_json::json!({ "stage": "loading_model", "port": port, "model": &model_clone }),
                                );
                                tracing::info!(port, "funasr-server HTTP 已就绪，模型加载中...");
                                loading_notified = true;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                        crate::domain::stt::funasr::ModelLoadStatus::Unreachable => {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                    }
                }
            });

            Ok(())
        }
        Ok(false) => {
            // 端口已被占用但 ManagedProcess 无 child——通常已有服务在运行
            let _ = app.emit(
                EventNames::FUNASR_SERVER_STATUS,
                serde_json::json!({ "stage": "already_running", "port": port, "model": &model }),
            );
            tracing::info!("funasr-server 子进程已在运行，跳过重复启动");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// 停止 funasr-server 子进程。
///
/// 0.22.1：通过 ManagedProcess 的幂等 stop 回收进程树。
/// 不再通过端口查找 PID 并 kill 未知进程。
#[tauri::command]
pub async fn stop_funasr_server() -> Result<(), String> {
    // 通过 ManagedProcess 停止（graceful → 超时 → 强制 kill）
    FUNASR_MANAGED
        .stop()
        .await
        .map_err(|e| format!("停止 funasr-server 失败: {e}"))?;

    crate::domain::stt::funasr::mark_server_stopped();
    tracing::info!("funasr-server 已停止");
    Ok(())
}

/// STT 诊断：检查 FunASR 环境 + 服务状态 + 配置。
#[tauri::command]
pub async fn diagnose_stt() -> Result<serde_json::Value, String> {
    let mut report = serde_json::json!({
        "funasr_env": {},
        "config": {},
        "models": [],
        "api_test": null,
    });

    let config = crate::app::stt_config::get_stt_config();
    let port = config.local_engine.server_port;

    tracing::info!("=== STT 诊断开始 ===");

    let env = crate::domain::stt::funasr::get_env_status_async(
        port,
        config.local_engine.funasr_model.clone(),
    )
    .await;

    let server_ready_tcp = crate::domain::stt::funasr::is_server_ready(port);
    let model_status = if server_ready_tcp {
        crate::domain::stt::funasr::check_model_loaded(port).await
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
        match test_audio_via_server(
            "https://isv-data.oss-cn-hangzhou.aliyuncs.com/ics/MaaS/ASR/test_audio/BAC009S0764W0121.wav",
            port,
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
#[tauri::command]
pub async fn get_stt_space_usage() -> serde_json::Value {
    let python_dir = dirs_next::data_dir()
        .unwrap_or_default()
        .join("blink")
        .join("python");

    let uv_dir = python_dir.join("uv");
    let venv_dir = python_dir.join("venv");
    let models_dir = python_dir.join("models");
    let legacy_modelscope_cache =
        dirs_next::home_dir().map(|h| h.join(".cache").join("modelscope"));

    let mut items = Vec::new();
    let mut total_bytes: u64 = 0;

    if uv_dir.exists() {
        let size = dir_size_bytes(&uv_dir);
        total_bytes += size;
        items.push(serde_json::json!({
            "label": "uv 二进制",
            "path": uv_dir.display().to_string(),
            "size_mb": bytes_to_mb(size),
        }));
    }

    if venv_dir.exists() {
        let size = dir_size_bytes(&venv_dir);
        total_bytes += size;
        items.push(serde_json::json!({
            "label": "Python 虚拟环境 (venv + torch + funasr)",
            "path": venv_dir.display().to_string(),
            "size_mb": bytes_to_mb(size),
        }));
    }

    if models_dir.exists() {
        let size = dir_size_bytes(&models_dir);
        total_bytes += size;
        items.push(serde_json::json!({
            "label": "FunASR 模型缓存",
            "path": models_dir.display().to_string(),
            "size_mb": bytes_to_mb(size),
        }));
    }

    if let Some(legacy_dir) = &legacy_modelscope_cache
        && legacy_dir.exists()
    {
        let size = dir_size_bytes(legacy_dir);
        if size > 0 {
            total_bytes += size;
            items.push(serde_json::json!({
                "label": "旧版模型缓存残留 (ModelScope 默认路径)",
                "path": legacy_dir.display().to_string(),
                "size_mb": bytes_to_mb(size),
            }));
        }
    }

    serde_json::json!({
        "items": items,
        "total_mb": bytes_to_mb(total_bytes),
    })
}

/// 清理 STT Python 环境（删除 venv + uv）。
///
/// 0.22.1：通过 ManagedProcess 停止进程，不再直接操作裸 Child。
#[tauri::command]
pub async fn cleanup_stt_space() -> Result<(), String> {
    // 先通过 ManagedProcess 停止 funasr-server
    let _ = FUNASR_MANAGED.stop().await;
    crate::domain::stt::funasr::mark_server_stopped();

    let python_dir = dirs_next::data_dir()
        .unwrap_or_default()
        .join("blink")
        .join("python");

    let mut errors = Vec::new();

    let venv_dir = python_dir.join("venv");
    if venv_dir.exists() {
        tracing::info!(path = %venv_dir.display(), "清理 venv");
        if let Err(e) = std::fs::remove_dir_all(&venv_dir) {
            errors.push(format!("删除 venv 失败: {e}"));
        }
    }

    let uv_dir = python_dir.join("uv");
    if uv_dir.exists() {
        tracing::info!(path = %uv_dir.display(), "清理 uv");
        if let Err(e) = std::fs::remove_dir_all(&uv_dir) {
            errors.push(format!("删除 uv 失败: {e}"));
        }
    }

    let models_dir = python_dir.join("models");
    if models_dir.exists() {
        tracing::info!(path = %models_dir.display(), "清理模型缓存");
        if let Err(e) = std::fs::remove_dir_all(&models_dir) {
            errors.push(format!("删除模型缓存失败: {e}"));
        }
    }

    if let Some(legacy_dir) = dirs_next::home_dir().map(|h| h.join(".cache").join("modelscope"))
        && legacy_dir.exists()
    {
        tracing::info!(path = %legacy_dir.display(), "清理旧版模型缓存残留");
        if let Err(e) = std::fs::remove_dir_all(&legacy_dir) {
            tracing::warn!(%e, "清理旧版模型缓存残留失败（不阻断）");
        }
    }

    if errors.is_empty() {
        tracing::info!("STT 空间清理完成");
        Ok(())
    } else {
        Err(errors.join("; "))
    }
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

/// 将 Unix 毫秒时间戳格式化为 HH:MM:SS 字符串（本地时区）。
fn format_timestamp_ms(ts_ms: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ts_ms as i64)
        .single()
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string())
}

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
/// 0.22.1：通过 ManagedProcess 的 shutdown_blocking 安全回收。
/// Windows Job Object 的 KILL_ON_JOB_CLOSE 确保进程树被回收。
/// 由 `main.rs` 的 `RunEvent::Exit` handler 调用。
pub fn shutdown_funasr_server_blocking() {
    FUNASR_MANAGED.shutdown_blocking();
    crate::domain::stt::funasr::mark_server_stopped();
    tracing::info!("funasr-server 已在 Blink 退出时停止（ManagedProcess shutdown_blocking）");
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
