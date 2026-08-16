//! FunASR、诊断与 STT 存储维护 commands。

use super::*;

/// 获取 funasr-server 历史日志（带时间戳）。
///
/// 设置页打开时调用此命令回补自启动期间产生的日志，
/// 避免用户打开设置页后看不到服务启动过程。
#[tauri::command]
pub fn get_funasr_log_history() -> Vec<String> {
    FUNASR_LOG_BUFFER.lock().unwrap().iter().cloned().collect()
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
/// 进度通过 `blink://python-env-progress` 事件通知前端：
/// - `{"stage": "uv", "status": "starting"}` — 检查/下载 uv
/// - `{"stage": "uv", "status": "done"}` — uv 就绪
/// - `{"stage": "venv", "status": "starting"}` — 创建 venv
/// - `{"stage": "venv", "status": "done"}` — venv 就绪
/// - `{"stage": "funasr", "status": "installing"}` — 安装 funasr
/// - `{"stage": "funasr", "status": "done"}` — funasr 安装完成
/// - `{"stage": "complete", "status": "ready"}` — 全部完成
/// - `{"stage": "error", "error": "..."}` — 出错
#[tauri::command]
pub async fn setup_python_env(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Emitter;

    // 进度回调：转发到前端 blink://python-env-progress
    let app_progress = app.clone();
    let on_progress: crate::infra::platform::python::ProgressCallback =
        std::sync::Arc::new(move |stage, status| {
            let _ = app_progress.emit(
                EventNames::PYTHON_ENV_PROGRESS,
                serde_json::json!({ "stage": stage, "status": status }),
            );
        });

    // 日志回调：转发到前端 blink://funasr-server-log（含 uv 逐行安装进度）
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
    // setup_with_progress 内部会检测已安装 PyTorch 是否含 CUDA 支持，
    // 若 device==cuda 但 PyTorch 为 CPU 版，会自动重装 CUDA 版。
    let py_status = crate::infra::platform::python::check_status_async().await;
    if !py_status.env_ready || (device == "cuda" && !py_status.torch_cuda_available) {
        let need_cuda_reinstall =
            device == "cuda" && py_status.torch_installed && !py_status.torch_cuda_available;
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
                // 安装后重新检查 CUDA 支持
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

    // ── 孤儿进程检测：FUNASR_SERVER_CHILD 为空但端口被占 ──
    // Blink 崩溃/异常退出后，上次的 funasr-server 子进程可能变成孤儿进程继续运行，
    // 占用监听端口。此时 child handle 丢失，无法通过正常途径管理。
    // 在启动新服务前先清理孤儿进程，避免端口冲突 + 日志无法捕获。
    //
    // MutexGuard 非 Send，必须在独立块中释放，不能跨 await 持有。
    let has_live_child = {
        let mut guard = FUNASR_SERVER_CHILD.lock().unwrap();
        guard
            .as_mut()
            .map(|c| c.try_wait().ok().flatten().is_none())
            .unwrap_or(false)
    };

    if !has_live_child && crate::domain::stt::funasr::is_server_ready(port) {
        // 端口被占但没有 Blink 管理的子进程 → 孤儿进程
        if let Some(pid) = crate::infra::platform::process::kill_process_by_port(port) {
            emit_funasr_log(
                &app,
                &format!("[Blink] ⚠️ 检测到孤儿进程 PID {pid} 占用端口 {port}，已自动清理"),
            );
            tracing::warn!(pid, port, "检测到孤儿 funasr-server 进程，已清理");
        } else {
            emit_funasr_log(
                &app,
                &format!("[Blink] ⚠️ 端口 {port} 被占用但无法定位进程，请手动检查任务管理器"),
            );
        }
        // 等端口释放
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // ── 防止重复启动：如果已有子进程在运行，直接返回 ──
    {
        let mut guard = FUNASR_SERVER_CHILD.lock().unwrap();
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(None) => {
                    // 子进程仍在运行，不重复启动
                    drop(guard);
                    let _ = app.emit(
                        EventNames::FUNASR_SERVER_STATUS,
                        serde_json::json!({ "stage": "already_running", "port": port, "model": &model }),
                    );
                    tracing::info!("funasr-server 子进程已在运行，跳过重复启动");
                    return Ok(());
                }
                Ok(Some(_)) => {
                    // 子进程已退出，清理后继续
                    *guard = None;
                    tracing::info!("检测到旧的 funasr-server 子进程已退出，清理后重新启动");
                }
                Err(_) => {}
            }
        }
    }

    match crate::domain::stt::funasr::start_server(&params).await {
        Ok(Some((child, mut log_rx))) => {
            // 存储子进程句柄
            {
                let mut guard = FUNASR_SERVER_CHILD.lock().unwrap();
                *guard = Some(child);
            }

            // ── 转发 funasr-server 日志到前端 ──
            // 日志来自 start_server 内部的 stdout/stderr 读取 task，
            // 通过 unbounded channel 发送，这里转发为 Tauri 事件。
            // 同时写入全局缓冲区，供设置页打开时回补历史日志。
            let app_log = app.clone();
            tokio::spawn(async move {
                while let Some(line) = log_rx.recv().await {
                    emit_funasr_log(&app_log, &line);
                }
            });

            // ── 异步等待服务就绪（带子进程退出检测）──
            // 两阶段检查：先等 FastAPI HTTP 起来，再等模型加载完成。
            // 模型首次需从 ModelScope 下载（~234MB），可能需要数分钟。
            let app_clone = app.clone();
            let model_clone = model.clone();
            tokio::spawn(async move {
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(
                        crate::domain::stt::funasr::SERVER_STARTUP_TIMEOUT_SECS,
                    );

                // 是否已通知前端「模型加载中」（避免每轮轮询都发事件）
                let mut loading_notified = false;

                loop {
                    if std::time::Instant::now() > deadline {
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
                        // 清理子进程 + 标记停止（避免 SERVER_RUNNING 残留为 true）
                        let mut guard = FUNASR_SERVER_CHILD.lock().unwrap();
                        if let Some(child) = guard.as_mut() {
                            let _ = child.start_kill();
                        }
                        *guard = None;
                        drop(guard);
                        crate::domain::stt::funasr::mark_server_stopped();
                        return;
                    }

                    // 检查子进程是否已退出（崩溃 / 异常终止）
                    {
                        let mut guard = FUNASR_SERVER_CHILD.lock().unwrap();
                        if let Some(child) = guard.as_mut() {
                            match child.try_wait() {
                                Ok(Some(status)) => {
                                    // 子进程已退出
                                    *guard = None;
                                    drop(guard);
                                    crate::domain::stt::funasr::mark_server_stopped();
                                    let _ = app_clone.emit(
                                        EventNames::FUNASR_SERVER_STATUS,
                                        serde_json::json!({
                                            "stage": "error",
                                            "error": format!("funasr-server 进程已退出: {status}")
                                        }),
                                    );
                                    tracing::error!(%status, port, "funasr-server 进程异常退出");
                                    return;
                                }
                                Ok(None) => {} // 仍在运行
                                Err(e) => {
                                    tracing::warn!(%e, "try_wait 失败");
                                }
                            }
                        } else {
                            // 子进程已被停止（用户点击停止按钮）
                            return;
                        }
                    }

                    // 检查模型加载状态（/health 端点的 model_status 字段）
                    let model_status = crate::domain::stt::funasr::check_model_loaded(port).await;
                    match model_status {
                        crate::domain::stt::funasr::ModelLoadStatus::Ready => {
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
                            // FastAPI 尚未启动，继续等待
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                    }
                }
            });

            Ok(())
        }
        Ok(None) => {
            // 端口已被占用但 FUNASR_SERVER_CHILD 为空——通常是孤儿进程
            // （start_funasr_server 开头已尝试清理，但可能清理失败或进程刚启动）
            // 此时无法捕获子进程 stdout/stderr，日志窗口不会有实时日志。
            emit_funasr_log(
                &app,
                &format!(
                    "[Blink] ⚠️ 端口 {port} 已被占用（可能是之前遗留的进程），无法捕获实时日志。建议先停止服务再重新启动。"
                ),
            );
            let app_clone = app.clone();
            let model_clone = model.clone();
            tokio::spawn(async move {
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(
                        crate::domain::stt::funasr::SERVER_STARTUP_TIMEOUT_SECS,
                    );
                let mut loading_notified = false;
                loop {
                    if std::time::Instant::now() > deadline {
                        let _ = app_clone.emit(
                            EventNames::FUNASR_SERVER_STATUS,
                            serde_json::json!({
                                "stage": "error",
                                "error": format!(
                                    "funasr-server 模型在 {}s 内未加载完成（端口 {}）",
                                    crate::domain::stt::funasr::SERVER_STARTUP_TIMEOUT_SECS,
                                    port
                                )
                            }),
                        );
                        // 标记停止（Ok(None) 分支没有 child handle，只需标记状态）
                        crate::domain::stt::funasr::mark_server_stopped();
                        return;
                    }
                    let model_status = crate::domain::stt::funasr::check_model_loaded(port).await;
                    match model_status {
                        crate::domain::stt::funasr::ModelLoadStatus::Ready => {
                            let _ = app_clone.emit(
                                EventNames::FUNASR_SERVER_STATUS,
                                serde_json::json!({ "stage": "ready", "port": port, "model": &model_clone }),
                            );
                            return;
                        }
                        crate::domain::stt::funasr::ModelLoadStatus::Error => {
                            let _ = app_clone.emit(
                                EventNames::FUNASR_SERVER_STATUS,
                                serde_json::json!({
                                    "stage": "error",
                                    "error": "模型加载失败，请检查网络连接后重试"
                                }),
                            );
                            return;
                        }
                        crate::domain::stt::funasr::ModelLoadStatus::Loading
                        | crate::domain::stt::funasr::ModelLoadStatus::Idle => {
                            if !loading_notified {
                                let _ = app_clone.emit(
                                    EventNames::FUNASR_SERVER_STATUS,
                                    serde_json::json!({ "stage": "loading_model", "port": port, "model": &model_clone }),
                                );
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
        Err(e) => Err(e),
    }
}

/// 停止 funasr-server 子进程。
///
/// 先 kill Blink 管理的子进程（通过 child handle），再检查端口是否仍被占。
/// 如果端口仍被占，说明存在孤儿进程（Blink 崩溃后遗留），通过 PID 清理。
#[tauri::command]
pub async fn stop_funasr_server() -> Result<(), String> {
    // 1. 先从 Mutex 中取出 child，避免跨 await 持有 MutexGuard（非 Send）
    let mut child_opt = FUNASR_SERVER_CHILD.lock().unwrap().take();
    if let Some(child) = child_opt.as_mut() {
        let _ = child.kill().await;
    }
    drop(child_opt);

    // 2. 检查端口是否仍被占（可能是孤儿进程）
    let port = crate::app::stt_config::get_stt_config()
        .local_engine
        .server_port;
    if crate::domain::stt::funasr::is_server_ready(port) {
        if let Some(pid) = crate::infra::platform::process::kill_process_by_port(port) {
            tracing::warn!(pid, port, "停止服务时检测到孤儿进程，已清理");
        }
        // 等端口释放
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    crate::domain::stt::funasr::mark_server_stopped();
    tracing::info!("funasr-server 已停止");
    Ok(())
}

/// STT 诊断：检查 FunASR 环境 + 服务状态 + 配置。
///
/// 返回详细诊断报告，帮助定位 "STT 不工作" 的具体原因：
/// 1. Python 是否安装及版本
/// 2. funasr 包是否安装及版本
/// 3. funasr-server 是否在运行（健康检查）
/// 4. 当前配置（模式、模型、端口）
/// 5. 如果服务就绪，下载示例音频调一次 HTTP API 验证识别效果
///
/// 所有诊断步骤同步输出到 tracing 日志，便于从日志文件排查问题。
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

    // ── FunASR 环境状态（异步，不阻塞 UI）──
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

    // 同步诊断信息到 tracing 日志
    tracing::info!(
        available = env.uv_available,
        version = ?env.uv_version,
        "诊断: uv"
    );
    tracing::info!(
        exists = env.venv_exists,
        version = ?env.venv_python_version,
        "诊断: venv"
    );
    tracing::info!(
        installed = env.torch_installed,
        version = ?env.torch_version,
        "诊断: torch"
    );
    tracing::info!(
        installed = env.funasr_installed,
        version = ?env.funasr_version,
        "诊断: funasr"
    );
    tracing::info!(
        running = env.server_running,
        ready = server_ready,
        model_status = %model_status_str,
        port,
        "诊断: server"
    );

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

    // ── 配置状态 ──
    tracing::info!(
        mode = ?config.mode,
        model = %config.local_engine.funasr_model,
        device = %config.local_engine.device,
        streaming = config.streaming,
        "诊断: config"
    );

    report["config"] = serde_json::json!({
        "enabled": config.enabled,
        "mode": format!("{:?}", config.mode),
        "local_model_id": config.local_model_id,
        "funasr_model": config.local_engine.funasr_model,
        "server_port": config.local_engine.server_port,
        "device": config.local_engine.device,
        "streaming": config.streaming,
    });

    // ── 模型列表 ──
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

    // ── API 测试：如果服务就绪，下载示例音频测试识别 ──
    if server_ready {
        tracing::info!("诊断: 开始 API 测试（下载示例音频）");
        // FunASR 官方中文示例音频（BAC009 数据集）
        let audio_url = "https://isv-data.oss-cn-hangzhou.aliyuncs.com/ics/MaaS/ASR/test_audio/BAC009S0764W0121.wav";

        match test_audio_via_server(audio_url, port).await {
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
        tracing::info!("诊断: API 测试跳过（服务未就绪）");
        report["api_test"] = serde_json::json!({
            "skipped": true,
            "reason": "funasr-server 未就绪",
        });
    }

    tracing::info!("=== STT 诊断完成 ===");
    tracing::info!(report = %report, "STT 诊断报告");
    Ok(report)
}

/// 云端 STT 连接测试：下载示例音频 → 发送到云端供应商 API → 返回识别文本。
///
/// 与 `diagnose_stt` 中的 `test_audio_via_server` 对称，
/// 区别是此命令发送到云端供应商而非本地 funasr-server。
#[tauri::command]
pub async fn test_cloud_stt() -> Result<serde_json::Value, String> {
    let config = crate::app::stt_config::get_stt_config();

    // 独立模式：直接从 SttCloudProvider 解析，不依赖 AIConfig
    let endpoint = crate::domain::stt::cloud::resolve_stt_endpoint(&config)
        .map_err(|e| format!("云端 STT 配置解析失败: {e}"))?;

    let is_chat_asr = endpoint.uses_chat_completion_asr;
    let url = format!(
        "{}/{}",
        endpoint.base_url,
        if is_chat_asr {
            "chat/completions"
        } else {
            "audio/transcriptions"
        }
    );

    tracing::info!(
        url = %url,
        model = %endpoint.model_id,
        protocol = if is_chat_asr { "chat-completion" } else { "whisper" },
        "云端 STT 测试"
    );

    // 下载示例音频（复用与本地诊断相同的音频）
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

    tracing::info!(size = wav_bytes.len(), "云端 STT 测试: 示例音频下载完成");

    // 发送到云端 API（复用 send_stt_request，与 finalize 同路径）
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

            // 根据错误内容给出更友好的提示
            let friendly = if err_str.contains("404") {
                format!(
                    "供应商未提供该接口（404）。\
                     请确认 {url} 存在。\
                     若使用 Mimo，请确认模型 ID 为 mimo-v2.5-asr；\
                     若使用其他供应商，请确认其支持音频转写端点。"
                )
            } else if err_str.contains("401") || err_str.contains("403") {
                "认证失败（401/403）。请检查 API Key 是否正确，以及是否有相应权限。".to_string()
            } else if err_str.contains("400") {
                format!(
                    "请求参数错误（400）。请检查模型 ID「{}」是否正确。原始错误: {err_str}",
                    endpoint.model_id
                )
            } else {
                err_str
            };

            Ok(serde_json::json!({
                "success": false,
                "error": friendly,
            }))
        }
    }
}

/// 获取 STT 相关空间占用信息。
///
/// 返回 uv 二进制、Python venv、ModelScope 模型缓存的大小。
#[tauri::command]
pub async fn get_stt_space_usage() -> serde_json::Value {
    let python_dir = dirs_next::data_dir()
        .unwrap_or_default()
        .join("blink")
        .join("python");

    let uv_dir = python_dir.join("uv");
    let venv_dir = python_dir.join("venv");

    // ModelScope 模型缓存：Blink 将其重定向到 python/models 目录（通过 MODELSCOPE_CACHE 环境变量）。
    // 旧版本可能仍在 ~/.cache/modelscope，也检查并显示。
    let models_dir = python_dir.join("models");
    let legacy_modelscope_cache =
        dirs_next::home_dir().map(|h| h.join(".cache").join("modelscope"));

    let mut items = Vec::new();
    let mut total_bytes: u64 = 0;

    // uv 二进制
    if uv_dir.exists() {
        let size = dir_size_bytes(&uv_dir);
        total_bytes += size;
        items.push(serde_json::json!({
            "label": "uv 二进制",
            "path": uv_dir.display().to_string(),
            "size_mb": bytes_to_mb(size),
        }));
    }

    // Python venv（含 torch + funasr）
    if venv_dir.exists() {
        let size = dir_size_bytes(&venv_dir);
        total_bytes += size;
        items.push(serde_json::json!({
            "label": "Python 虚拟环境 (venv + torch + funasr)",
            "path": venv_dir.display().to_string(),
            "size_mb": bytes_to_mb(size),
        }));
    }

    // ModelScope 模型缓存（Blink 自管理目录）
    if models_dir.exists() {
        let size = dir_size_bytes(&models_dir);
        total_bytes += size;
        items.push(serde_json::json!({
            "label": "FunASR 模型缓存",
            "path": models_dir.display().to_string(),
            "size_mb": bytes_to_mb(size),
        }));
    }

    // 旧版残留：~/.cache/modelscope（可能存在历史下载）
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
/// 会先停止 funasr-server（如果在运行），然后删除整个 python 目录。
/// 清理后需重新安装环境才能使用本地 STT。
#[tauri::command]
pub async fn cleanup_stt_space() -> Result<(), String> {
    // 先停止 funasr-server
    let mut child_opt = FUNASR_SERVER_CHILD.lock().unwrap().take();
    if let Some(child) = child_opt.as_mut() {
        let _ = child.kill().await;
        crate::domain::stt::funasr::mark_server_stopped();
    }
    drop(child_opt);

    let python_dir = dirs_next::data_dir()
        .unwrap_or_default()
        .join("blink")
        .join("python");

    let mut errors = Vec::new();

    // 删除 venv
    let venv_dir = python_dir.join("venv");
    if venv_dir.exists() {
        tracing::info!(path = %venv_dir.display(), "清理 venv");
        if let Err(e) = std::fs::remove_dir_all(&venv_dir) {
            errors.push(format!("删除 venv 失败: {e}"));
        }
    }

    // 删除 uv
    let uv_dir = python_dir.join("uv");
    if uv_dir.exists() {
        tracing::info!(path = %uv_dir.display(), "清理 uv");
        if let Err(e) = std::fs::remove_dir_all(&uv_dir) {
            errors.push(format!("删除 uv 失败: {e}"));
        }
    }

    // 删除模型缓存（Blink 自管理目录）
    let models_dir = python_dir.join("models");
    if models_dir.exists() {
        tracing::info!(path = %models_dir.display(), "清理模型缓存");
        if let Err(e) = std::fs::remove_dir_all(&models_dir) {
            errors.push(format!("删除模型缓存失败: {e}"));
        }
    }

    // 清理旧版残留：~/.cache/modelscope
    if let Some(legacy_dir) = dirs_next::home_dir().map(|h| h.join(".cache").join("modelscope"))
        && legacy_dir.exists()
    {
        tracing::info!(path = %legacy_dir.display(), "清理旧版模型缓存残留");
        if let Err(e) = std::fs::remove_dir_all(&legacy_dir) {
            // 旧版残留清理失败不阻断（可能被其他程序占用）
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

/// 打开 STT Python 环境所在文件夹（`%APPDATA%\blink\python\`）。
///
/// 方便用户查看 venv、uv、模型缓存等文件。目录不存在时自动创建。
#[tauri::command]
pub fn open_stt_folder() -> Result<(), String> {
    let python_dir = dirs_next::data_dir()
        .unwrap_or_default()
        .join("blink")
        .join("python");

    // 目录不存在时先创建，避免 explorer 打开"文档"等默认位置
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

// ── 辅助函数与类型（从 commands.rs 迁移）──

/// 全局 funasr-server 子进程句柄。
static FUNASR_SERVER_CHILD: std::sync::Mutex<Option<tokio::process::Child>> =
    std::sync::Mutex::new(None);

/// funasr-server 日志环形缓冲区（最近 200 条）。
///
/// 服务可能在设置页打开前就自启动（auto_start_server），此时前端
/// `listen("blink://funasr-server-log")` 尚未注册，日志会丢失。
/// 缓冲区让设置页打开时通过 `get_funasr_log_history` 命令回补历史日志。
const FUNASR_LOG_BUFFER_CAP: usize = 200;

static FUNASR_LOG_BUFFER: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

/// 向日志缓冲区追加一行（带时间戳），同时 emit 到前端。
fn emit_funasr_log(app: &tauri::AppHandle, line: &str) {
    let ts = chrono::Local::now().format("%H:%M:%S").to_string();
    let entry = format!("[{}] {}", ts, line);
    {
        let mut buf = FUNASR_LOG_BUFFER.lock().unwrap();
        if buf.len() >= FUNASR_LOG_BUFFER_CAP {
            buf.pop_front();
        }
        buf.push_back(entry.clone());
    }
    let _ = app.emit(
        EventNames::FUNASR_SERVER_LOG,
        serde_json::json!({ "line": line }),
    );
}

/// Blink 退出时同步停止 funasr-server 子进程（避免孤儿进程）。
///
/// 使用 `start_kill()`（非 async）——发送 kill 信号后不等待进程退出，
/// 避免阻塞 app 退出。由 `main.rs` 的 `RunEvent::Exit` handler 调用。
pub fn shutdown_funasr_server_blocking() {
    let mut child_opt = FUNASR_SERVER_CHILD.lock().unwrap().take();
    if let Some(child) = child_opt.as_mut() {
        let _ = child.start_kill();
        crate::domain::stt::funasr::mark_server_stopped();
        tracing::info!("funasr-server 已在 Blink 退出时停止");
    }
}

/// 下载示例音频并通过 funasr-server 测试识别。
///
/// 流程：
/// 1. HTTP 下载 WAV 音频
/// 2. 解析 WAV → f32 PCM 样本
/// 3. 分块喂入 LocalSttEngine（模拟 transcribe_chunk）
/// 4. 调用 finalize → POST 到 funasr-server
/// 5. 返回识别文本
async fn test_audio_via_server(audio_url: &str, port: u16) -> Result<String, String> {
    // 1. 下载音频
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

    // 2. 解析 WAV → f32 PCM 样本
    let samples = crate::domain::stt::wav::parse_wav_to_f32(&wav_bytes)?;
    let duration_ms = (samples.len() as f64 / 16000.0 * 1000.0) as u64;
    tracing::info!(samples = samples.len(), duration_ms, "诊断: WAV 解析完成");

    // 3. 创建引擎并分块喂入音频
    let engine = crate::domain::stt::local::LocalSttEngine::for_diagnostic(port);
    let chunk_size = 1600usize; // 100ms chunks
    for chunk in samples.chunks(chunk_size) {
        engine
            .transcribe_chunk(chunk)
            .await
            .map_err(|e| e.to_string())?;
    }

    // 4. 调用 finalize → POST 到 funasr-server
    tracing::info!("诊断: 调用 funasr-server 转录...");
    let result = engine.finalize().await.map_err(|e| e.to_string())?;

    Ok(result)
}
