//! 本地 UI 音频转写命令（0.22.16 Handoff 06）。
//!
//! 实现最小可信闭环：
//! - `pick_audio_file` — 由可信 native picker 选择本地 .wav 文件，
//!   后端签发短期 audio_ref 返回前端（不返回绝对路径）
//! - `transcribe_audio_file` — 前端用 audio_ref 调用此命令，
//!   后端走 `CapabilityRegistry::invoke("transcribe_audio", ...)` 统一原子执行语义
//!
//! **信任边界**：
//! - 前端业务层只拿 audio_ref 和安全元数据，不长期持有绝对路径
//! - .wav 过滤只用于体验，真实格式仍由 magic/decoder 判断
//! - 调用仍走同一个 transcribe_audio Capability
//! - 不复制 STT service，不接 VoiceService，不发送录音 UI 事件
//! - 不新增大规模设置页或通用附件系统

use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use crate::domain::capability::{
    CapabilityRegistry, InvocationOrigin, InvokeContext, RuntimeCapabilities,
};

/// `pick_audio_file` — 由可信 native picker 选择本地 .wav 文件。
///
/// 返回 opaque `audio_ref`（短期有效），前端不持有绝对路径。
/// 用户取消选择时返回 `null`。
///
/// .wav 过滤只用于体验，真实格式仍由 decoder 判断。
#[tauri::command]
pub async fn pick_audio_file(
    app: tauri::AppHandle,
) -> Result<Option<String>, crate::app::command_error::CommandError> {
    let audio_registry = app
        .state::<std::sync::Arc<crate::app::audio_resource::AudioResourceRegistry>>()
        .inner()
        .clone();

    // 由后端/可信 native picker 选择文件
    let mut dialog = app.dialog().file();
    dialog = dialog.set_title("选择音频文件");
    dialog = dialog.add_filter("WAV 音频", &["wav"]);

    let (sender, receiver) = tokio::sync::oneshot::channel();
    dialog.pick_file(move |picked| {
        let _ = sender.send(picked);
    });
    let picked = receiver.await.map_err(|_| {
        crate::app::command_error::CommandError::new(
            "dialog_closed",
            "文件选择器意外关闭，请重试",
            true,
        )
    })?;

    let path = match picked {
        Some(tauri_plugin_dialog::FilePath::Path(path)) => path,
        Some(tauri_plugin_dialog::FilePath::Url(_)) => {
            // 拒绝 URL
            return Err(crate::app::command_error::CommandError::new(
                "remote_path_forbidden",
                "不接受 URL 音频资源",
                false,
            ));
        }
        None => return Ok(None), // 用户取消
    };

    // 签发短期 audio_ref（scope = "stt_transcribe"）
    // AudioResourceRegistry 内部验证 regular file、大小、identity
    match audio_registry.issue(&path, "stt_transcribe") {
        Ok(audio_ref) => {
            tracing::debug!("pick_audio_file: audio_ref issued");
            Ok(Some(audio_ref))
        }
        Err(e) => {
            tracing::warn!(error = %e, "pick_audio_file: 无法签发 audio_ref");
            Err(crate::app::command_error::CommandError::new(
                e.kind.as_str(),
                "无法使用所选音频文件",
                false,
            ))
        }
    }
}

/// `transcribe_audio_file` — 用 audio_ref 调用 transcribe_audio Capability。
///
/// 走 `CapabilityRegistry::invoke("transcribe_audio", ...)` 统一原子执行语义，
/// 与 CLI 和 AI/MCP 出口调用同一个 Capability。
///
/// 返回 `CapabilityResult` 的 JSON 表示，前端从中提取 text 和 identity。
#[tauri::command]
pub async fn transcribe_audio_file(
    app: tauri::AppHandle,
    audio_ref: String,
) -> Result<serde_json::Value, crate::app::command_error::CommandError> {
    let cap_registry = app
        .state::<std::sync::Arc<CapabilityRegistry>>()
        .inner()
        .clone();

    let env_arc = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();

    let ctx = InvokeContext {
        env: env_arc.as_ref(),
        origin: InvocationOrigin::LocalSurface,
        runtime: RuntimeCapabilities {
            surface: None,
            main_process: true,
            desktop_session: true,
        },
        deadline: None,
    };

    let args = serde_json::json!({ "audio_ref": audio_ref });

    let result = cap_registry
        .invoke("transcribe_audio", args, &ctx)
        .await
        .map_err(crate::app::command_error::CommandError::from)?;

    // 返回完整 CapabilityResult 的 JSON
    serde_json::to_value(&result).map_err(|_| {
        crate::app::command_error::CommandError::new("internal_error", "转写结果序列化失败", false)
    })
}
