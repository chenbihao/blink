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

use tauri::{Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

use crate::domain::capability::{
    CapabilityRegistry, InvocationOrigin, InvokeContext, RuntimeCapabilities,
};
use crate::domain::event_names::EventNames;

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
    Ok(pick_audio_file_ref(&app)
        .await?
        .map(|(audio_ref, _)| audio_ref))
}

/// 调试入口额外返回文件名供界面标识；不向前端暴露目录或绝对路径。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VadDebugPickedFile {
    audio_ref: String,
    display_name: String,
}

#[tauri::command]
pub async fn pick_audio_file_for_vad_debug(
    app: tauri::AppHandle,
) -> Result<Option<VadDebugPickedFile>, crate::app::command_error::CommandError> {
    Ok(pick_audio_file_ref(&app)
        .await?
        .map(|(audio_ref, display_name)| VadDebugPickedFile {
            audio_ref,
            display_name,
        }))
}

async fn pick_audio_file_ref(
    app: &tauri::AppHandle,
) -> Result<Option<(String, String)>, crate::app::command_error::CommandError> {
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
            let display_name = safe_audio_display_name(&path);
            Ok(Some((audio_ref, display_name)))
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

fn safe_audio_display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| {
            name.to_string_lossy()
                .chars()
                .filter(|character| !character.is_control())
                .take(160)
                .collect()
        })
        .filter(|name: &String| !name.is_empty())
        .unwrap_or_else(|| "WAV 文件".into())
}

#[cfg(test)]
mod vad_debug_picker_tests {
    use super::safe_audio_display_name;

    #[test]
    fn display_name_contains_only_safe_basename() {
        let path = std::path::Path::new("private/audio/测试录音.wav");
        assert_eq!(safe_audio_display_name(path), "测试录音.wav");
        let noisy = std::path::Path::new("private/audio/name\n.wav");
        assert_eq!(safe_audio_display_name(noisy), "name.wav");
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

/// 使用用户主动选择的一次性 audio_ref，按实时速率回放伪流式 VAD 与当前本地模型。
#[tauri::command]
pub async fn debug_vad_audio_file(
    app: tauri::AppHandle,
    window: tauri::Window,
    audio_ref: String,
    run_id: String,
) -> Result<
    crate::app::audio_transcription_service::VadDebugResult,
    crate::app::command_error::CommandError,
> {
    let service = app
        .state::<std::sync::Arc<crate::app::audio_transcription_service::AudioTranscriptionService>>()
        .inner()
        .clone();
    let target = window.label().to_string();
    let progress_app = app.clone();
    service
        .debug_vad(&audio_ref, move |phase, fed_ms, duration_ms| {
            let _ = progress_app.emit_to(
                target.as_str(),
                EventNames::STT_VAD_DEBUG_PROGRESS,
                serde_json::json!({
                    "runId": run_id,
                    "phase": phase,
                    "fedMs": fed_ms,
                    "durationMs": duration_ms,
                }),
            );
        })
        .await
        .map_err(|error| {
            crate::app::command_error::CommandError::new(error.category(), error.to_string(), false)
        })
}

/// 为 VAD 调试回放克隆一条新 audio_ref（不消费原 ref）。
///
/// `debug_vad_audio_file` 的分析会一次性消费原 ref，前端取回放字节
/// （`read_audio_for_playback`）也需一次性授权，两条用途各持一条 ref。
#[tauri::command]
pub async fn clone_audio_ref_for_vad_debug(
    app: tauri::AppHandle,
    audio_ref: String,
) -> Result<String, crate::app::command_error::CommandError> {
    let registry = app
        .state::<std::sync::Arc<crate::app::audio_resource::AudioResourceRegistry>>()
        .inner()
        .clone();
    registry
        .clone_audio_ref(&audio_ref, "stt_transcribe")
        .map_err(|error| {
            tracing::warn!(error = %error, "clone_audio_ref_for_vad_debug: 无法克隆 audio_ref");
            crate::app::command_error::CommandError::new(
                error.kind.as_str(),
                "无法复用所选音频文件",
                false,
            )
        })
}

/// 读取 audio_ref 指向的音频字节供前端本地播放（VAD 调试回放）。
///
/// 走 registry 完整校验并一次性消费该 ref；字节经原始 IPC 返回，
/// 避免 JSON 数字数组序列化。前端转 blob URL 后交给 `<audio>` 播放。
#[tauri::command]
pub async fn read_audio_for_playback(
    app: tauri::AppHandle,
    audio_ref: String,
) -> Result<tauri::ipc::Response, crate::app::command_error::CommandError> {
    let registry = app
        .state::<std::sync::Arc<crate::app::audio_resource::AudioResourceRegistry>>()
        .inner()
        .clone();
    let opened = registry
        .resolve(&audio_ref, "stt_transcribe")
        .map_err(|error| {
            tracing::warn!(error = %error, "read_audio_for_playback: 无法解析 audio_ref");
            crate::app::command_error::CommandError::new(
                error.kind.as_str(),
                "音频资源不可用",
                false,
            )
        })?;
    let size = opened.size;
    let mut file = opened.file;
    // 从 resolve 返回的已验证 file handle 读取，不走"路径再打开"路径
    let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        let mut buffer = Vec::with_capacity(size as usize);
        file.read_to_end(&mut buffer).map(|_| buffer)
    })
    .await
    .map_err(|error| {
        crate::app::command_error::CommandError::new(
            "internal_error",
            format!("读取音频任务失败: {error}"),
            true,
        )
    })?
    .map_err(|error| {
        crate::app::command_error::CommandError::new(
            "io_error",
            format!("读取音频失败: {error}"),
            false,
        )
    })?;
    Ok(tauri::ipc::Response::new(bytes))
}
