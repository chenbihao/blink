//! stt 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use super::diagnostic::{bytes_to_mb, dir_size_bytes};
use crate::domain::event_names::EventNames;
use crate::domain::stt::SttEngine;
use tauri::{Emitter, Manager};

/// 启动 chat 窗口语音录音（0.12.2 §4.3）。
///
/// 解耦热键驱动——由 chat composer 麦克风按钮 IPC 调用，不走 `HotkeyEvent::Hold`。
/// 与 G1/G2 三方互斥（`VoiceService` 内部 `session.recording` 保证同一时刻只有一个 target）。
/// 识别结果通过 `blink://voice-partial(target="chat")` 定向 emit 到 chat 窗口。
#[tauri::command]
pub async fn start_chat_stt(app: tauri::AppHandle) -> Result<(), String> {
    let voice = app
        .state::<std::sync::Arc<crate::app::voice::VoiceService>>()
        .inner();
    voice.start_chat_recording().await;
    Ok(())
}

/// 停止 chat 窗口语音录音（0.12.2 §4.3）。
///
/// 停止录音 → STT 最终识别 → 通过 `voice-partial(target="chat")` emit 最终文本。
#[tauri::command]
pub async fn stop_chat_stt(app: tauri::AppHandle) -> Result<(), String> {
    let voice = app
        .state::<std::sync::Arc<crate::app::voice::VoiceService>>()
        .inner();
    voice.stop_chat_recording().await;
    Ok(())
}

/// 调整 voice-overlay 窗口高度（G2 语音 mini overlay 自动撑高）。
///
/// 前端在文本更新后调用，传入期望的逻辑高度。宽度固定 300。
/// 若窗口底部超出显示器工作区，自动上移使其完整可见。
#[tauri::command]
pub async fn resize_voice_overlay(app: tauri::AppHandle, height: f64) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("voice-overlay") {
        let size = tauri::LogicalSize::new(260.0, height);
        win.set_size(size).map_err(|e| e.to_string())?;
        crate::infra::platform::window::clamp_to_work_area(&win);
    }
    Ok(())
}

/// 保存 STT API Key 到 Windows Credential Manager。
///
/// **参数**:
/// - `secret`:明文密钥——只在本 command 函数内活着,写完 CM 立即 SecretString drop
#[tauri::command]
pub async fn save_stt_secret(secret: String) -> Result<(), String> {
    let secret_wrapped = crate::infra::platform::secret::SecretString::new(secret);
    crate::infra::platform::secret::save_secret("stt:cloud", "key", &secret_wrapped)
        .map_err(|e| e.to_string())?;
    tracing::info!("STT 密钥已保存到 Credential Manager");
    Ok(())
}

/// 从 Credential Manager 删除 STT API Key。
#[tauri::command]
pub async fn delete_stt_secret() -> Result<(), String> {
    match crate::infra::platform::secret::delete_secret("stt:cloud", "key") {
        Ok(()) => {
            tracing::info!("STT 密钥已从 CM 删除");
            Ok(())
        }
        Err(crate::infra::platform::secret::SecretError::NotFound(_)) => {
            tracing::debug!("STT 密钥不在 CM 中,跳过删除");
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// 检查 STT 是否已配 API Key(不返回明文,只返 true/false)。
#[tauri::command]
pub async fn has_stt_secret() -> Result<bool, String> {
    match crate::infra::platform::secret::load_secret("stt:cloud", "key") {
        Ok(_) => Ok(true),
        Err(crate::infra::platform::secret::SecretError::NotFound(_)) => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

/// 获取 STT 密钥的首尾掩码(如 `"sk-a••••cdef"`),供设置页占位展示。
#[tauri::command]
pub async fn get_stt_secret_hint() -> Result<Option<String>, String> {
    match crate::infra::platform::secret::load_secret("stt:cloud", "key") {
        Ok(secret) => Ok(Some(crate::infra::platform::secret::format_hint(
            secret.expose(),
        ))),
        Err(crate::infra::platform::secret::SecretError::NotFound(_)) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// 读取 STT 配置。
#[tauri::command]
pub async fn get_stt_config(
    app: tauri::AppHandle,
) -> Result<crate::app::stt_config::SttConfig, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let config =
        crate::app::config::ConfigStore::get::<crate::app::stt_config::SttConfig>(&pool).await;
    Ok(config)
}

/// 保存 STT 配置。
///
/// `scope` 用于按区段打印日志，避免改本地配置时把云端字段也全部打印出来：
/// - `"global"`: 总开关 / 模式 / 流式 / 音频设备
/// - `"cloud"`: 云端供应商
/// - `"local"`: 本地引擎（模型 / 设备 / 热词 / ITN / VAD）
/// - `None`: 兼容旧调用，打印全量字段
#[tauri::command]
pub async fn set_stt_config(
    app: tauri::AppHandle,
    config: crate::app::stt_config::SttConfig,
    scope: Option<String>,
) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::app::config::ConfigStore::set(&pool, &config)
        .await
        .map_err(|e| format!("保存 STT 配置失败: {e}"))?;
    // 更新内存缓存（供 STT 引擎同步读取）
    crate::app::stt_config::update_cache(&config);
    // 广播配置变更
    let _ = app.emit(
        EventNames::CONFIG_CHANGED,
        serde_json::json!({ "key": "stt:config" }),
    );

    match scope.as_deref() {
        Some("global") => {
            tracing::info!(
                scope = "global",
                enabled = config.enabled,
                mode = ?config.mode,
                streaming_mode = ?config.streaming_mode,
                audio_device_id = ?config.audio_device_id,
                "STT 配置已更新"
            );
        }
        Some("cloud") => {
            tracing::info!(
                scope = "cloud",
                cloud_provider = ?config.cloud_provider.as_ref().map(|p| (&p.kind, &p.model_id, &p.base_url)),
                "STT 配置已更新"
            );
        }
        Some("local") => {
            tracing::info!(
                scope = "local",
                local_model_id = ?config.local_model_id,
                funasr_model = %config.local_engine.funasr_model,
                device = %config.local_engine.device,
                auto_start_server = config.local_engine.auto_start_server,
                use_itn = config.local_engine.use_itn,
                hotwords_len = config.local_engine.hotwords.as_ref().map(|h| h.len()).unwrap_or(0),
                vad_silence_threshold = config.local_engine.vad.silence_threshold,
                vad_min_silence_ms = config.local_engine.vad.min_silence_ms,
                vad_min_sentence_ms = config.local_engine.vad.min_sentence_ms,
                "STT 配置已更新"
            );
        }
        _ => {
            // 兼容旧调用（无 scope）：打印全量
            tracing::info!(
                enabled = config.enabled,
                mode = ?config.mode,
                streaming_mode = ?config.streaming_mode,
                cloud_provider = ?config.cloud_provider.as_ref().map(|p| (&p.kind, &p.model_id, &p.base_url)),
                local_model_id = ?config.local_model_id,
                audio_device_id = ?config.audio_device_id,
                funasr_model = %config.local_engine.funasr_model,
                device = %config.local_engine.device,
                auto_start_server = config.local_engine.auto_start_server,
                use_itn = config.local_engine.use_itn,
                hotwords_len = config.local_engine.hotwords.as_ref().map(|h| h.len()).unwrap_or(0),
                vad_silence_threshold = config.local_engine.vad.silence_threshold,
                vad_min_silence_ms = config.local_engine.vad.min_silence_ms,
                vad_min_sentence_ms = config.local_engine.vad.min_sentence_ms,
                "STT 配置已更新"
            );
        }
    }
    Ok(())
}

/// 列出可用 STT 模型。
///
/// 新方案中模型由 FunASR 自动管理（首次使用时自动从 ModelScope 下载）。
/// 此接口返回模型元数据，供前端展示和选择。
#[tauri::command]
pub async fn list_stt_models() -> Result<Vec<serde_json::Value>, String> {
    let models = crate::domain::stt::model_registry();
    let config = crate::app::stt_config::get_stt_config();

    let result: Vec<serde_json::Value> = models
        .iter()
        .map(|m| {
            let is_selected = config.local_model_id.as_deref() == Some(m.id);
            serde_json::json!({
            "id": m.id,
            "display_name": m.display_name,
            "engine": m.engine,
            "params": m.params,
            "size_mb": m.size_mb,
            "languages": m.languages,
            "device": m.device,
                "description": m.description,
                "funasr_model_id": m.funasr_model_id,
                "is_selected": is_selected,
                // 兼容前端: 新方案中模型由 FunASR 自动管理,"已就绪"状态取决于服务是否运行
                "status": "managed_by_funasr",
            })
        })
        .collect();
    Ok(result)
}

/// 选择本地 STT 模型。
///
/// 新方案中模型由 FunASR 自动管理（首次启动 funasr-server 时自动下载）。
/// 此命令设置配置中的 `local_model_id` 和 `funasr_model` 并持久化到数据库，
/// 实际模型下载在 funasr-server 首次启动时由 FunASR 自动完成。
#[tauri::command]
pub async fn download_stt_model(app: tauri::AppHandle, model_id: String) -> Result<(), String> {
    let model =
        crate::domain::stt::find_model(&model_id).ok_or_else(|| format!("未知模型: {model_id}"))?;

    tracing::info!(
        model = %model_id,
        funasr_model = model.funasr_model_id,
        "选择 STT 模型（FunASR 自动管理下载）",
    );

    // 更新配置：设置选中的模型 + funasr_model 标识
    let mut config = crate::app::stt_config::get_stt_config();
    config.local_model_id = Some(model_id);
    config.local_engine.funasr_model = model.funasr_model_id.to_string();

    // 持久化到数据库（否则重启后丢失模型选择）
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::app::config::ConfigStore::set(&pool, &config)
        .await
        .map_err(|e| format!("保存 STT 配置失败: {e}"))?;

    // 更新内存缓存
    crate::app::stt_config::update_cache(&config);

    Ok(())
}

/// 取消选择 STT 模型。
///
/// 新方案中模型由 FunASR 管理，此命令仅清除配置中的选中状态。
#[tauri::command]
pub async fn delete_stt_model(model_id: String) -> Result<(), String> {
    tracing::info!(model = %model_id, "取消选择 STT 模型");
    let mut config = crate::app::stt_config::get_stt_config();
    if config.local_model_id.as_deref() == Some(model_id.as_str()) {
        config.local_model_id = None;
        crate::app::stt_config::update_cache(&config);
    }
    Ok(())
}

/// 取消语音录音(ESC 中断)。
#[tauri::command]
pub fn cancel_voice_recording(app: tauri::AppHandle) {
    if let Some(vs) = app.try_state::<std::sync::Arc<crate::app::voice::VoiceService>>() {
        vs.cancel_recording();
    }
}

/// 查询当前是否正在语音录音。
#[tauri::command]
pub fn is_voice_recording(app: tauri::AppHandle) -> bool {
    if let Some(vs) = app.try_state::<std::sync::Arc<crate::app::voice::VoiceService>>() {
        vs.is_recording()
    } else {
        false
    }
}

/// 列出可用的音频输入设备。
#[tauri::command]
pub fn list_audio_devices() -> Vec<crate::infra::platform::audio::AudioDevice> {
    crate::infra::platform::audio::list_input_devices()
}

/// 测试音频设备:开始采集并发送音量级别事件。
/// 前端通过 `blink://audio-test-level` 事件接收音量级别 (0.0~1.0)。
#[tauri::command]
pub async fn start_audio_test(
    app: tauri::AppHandle,
    device_id: Option<String>,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;

    tracing::info!(?device_id, "音频测试: 开始");

    // 停止之前的测试（如果有）
    AUDIO_TEST_ACTIVE.store(false, Ordering::SeqCst);

    let mut capture = if let Some(id) = device_id {
        crate::infra::platform::audio::create_capture_with_device(id)
    } else {
        crate::infra::platform::audio::create_capture()
    };

    let format = crate::infra::platform::audio::AudioFormat::default();
    let mut rx = capture.start(format).map_err(|e| {
        tracing::error!(%e, "音频测试: 采集启动失败");
        format!("音频采集启动失败: {e}")
    })?;

    AUDIO_TEST_ACTIVE.store(true, Ordering::SeqCst);

    tracing::info!("音频测试: 采集已启动, 等待数据...");

    let app_clone = app.clone();
    tokio::spawn(async move {
        let mut chunk_count = 0u32;
        let mut max_level = 0.0f64;
        while let Some(chunk) = rx.recv().await {
            if !AUDIO_TEST_ACTIVE.load(Ordering::SeqCst) {
                break;
            }
            chunk_count += 1;
            // 计算 RMS 音量
            let level = if chunk.samples.is_empty() {
                0.0
            } else {
                let sum_sq: f64 = chunk
                    .samples
                    .iter()
                    .map(|s| (*s as f64) * (*s as f64))
                    .sum();
                let rms = (sum_sq / chunk.samples.len() as f64).sqrt();
                (rms * 3.0).min(1.0)
            };
            if level > max_level {
                max_level = level;
            }
            // 首个 chunk + 每 10 个 chunk 打一次日志，让用户知道数据在流动
            if chunk_count == 1 {
                tracing::info!(samples = chunk.samples.len(), "音频测试: 收到首个 chunk");
            } else if chunk_count % 10 == 0 {
                tracing::trace!(
                    chunk_count,
                    level = format!("{:.3}", level),
                    max_level = format!("{:.3}", max_level),
                    "音频测试: 数据流动中"
                );
            }
            let _ = app_clone.emit(
                EventNames::AUDIO_TEST_LEVEL,
                serde_json::json!({ "level": level }),
            );
        }
        // capture 的 Drop 会设置 capturing=false，capture 线程随即退出
        drop(capture);
        tracing::info!(
            chunk_count,
            max_level = format!("{:.3}", max_level),
            "音频测试: 已停止"
        );
    });

    Ok(())
}

/// 停止音频测试。
#[tauri::command]
pub fn stop_audio_test() {
    use std::sync::atomic::Ordering;
    AUDIO_TEST_ACTIVE.store(false, Ordering::SeqCst);
    tracing::info!("音频测试: 用户停止");
}

/// 递归复制目录。
pub(crate) fn copy_dir_recursive(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("创建目录失败: {e}"))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("读取源目录失败: {e}"))? {
        let entry = entry.map_err(|e| format!("读取目录条目失败: {e}"))?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).map_err(|e| format!("复制文件失败: {e}"))?;
        }
    }
    Ok(())
}

static AUDIO_TEST_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

mod maintenance;
pub use maintenance::*;
