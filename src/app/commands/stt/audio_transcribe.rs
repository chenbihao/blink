//! 本地 UI 音频转写命令（0.22.16 Handoff 06；0.23.12 迁移到统一 ResourceStore）。
//!
//! 实现最小可信闭环：
//! - `pick_audio_file` — 由可信 native picker 选择本地 .wav 文件，
//!   后端经 ResourceStore 签发短期 audio_ref 返回前端（不返回绝对路径）
//! - `transcribe_audio_file` — 前端用 audio_ref 调用此命令，
//!   后端走 `CapabilityRegistry::invoke("transcribe_audio", ...)` 统一原子执行语义
//!
//! **信任边界**：
//! - 前端业务层只拿 audio_ref 和安全元数据，不长期持有绝对路径
//! - .wav 过滤只用于体验，真实格式仍由 magic/decoder 判断
//! - 调用仍走同一个 transcribe_audio Capability
//! - 不复制 STT service，不接 VoiceService，不发送录音 UI 事件
//! - 不新增大规模设置页或通用附件系统
//!
//! **0.23.12 revoke_group 真实调用方**：每次成功 pick 分配新撤销组并撤销
//! 上一次 pick 组的未消费 ref——「一次选择 = 一个调试会话」，防止累积占用
//! 配额（旧机制只能等 TTL 过期）。

use std::sync::Mutex;

use tauri::{Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

use crate::domain::capability::{
    CapabilityRegistry, InvocationOrigin, InvokeContext, RuntimeCapabilities,
};
use crate::domain::event_names::EventNames;
use crate::domain::resource::{
    DefaultResourceStore, ResourceGrantSpec, ResourceRef, ResourceUse, ResourceUseSet, ReusePolicy,
};

/// 设置页音频 pick 的会话状态——记录上一次 pick 的撤销组，
/// 新 pick 成功后撤销旧组（revoke_group 的非测试调用方，0.23.12）。
#[derive(Default)]
pub struct AudioPickSession {
    pub last_group: Mutex<Option<u64>>,
}

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
    Ok(
        pick_audio_file_ref(&app, ResourceUseSet::single(ResourceUse::TranscribeAudio))
            .await?
            .map(|(audio_ref, _)| audio_ref),
    )
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
    let uses = ResourceUseSet::single(ResourceUse::TranscribeAudio)
        .union(ResourceUseSet::single(ResourceUse::PreviewAudio));
    Ok(pick_audio_file_ref(&app, uses)
        .await?
        .map(|(audio_ref, display_name)| VadDebugPickedFile {
            audio_ref,
            display_name,
        }))
}

async fn pick_audio_file_ref(
    app: &tauri::AppHandle,
    uses: ResourceUseSet,
) -> Result<Option<(String, String)>, crate::app::command_error::CommandError> {
    let store = app
        .state::<std::sync::Arc<DefaultResourceStore>>()
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

    // 签发短期 audio_ref（0.23.12：ResourceStore grant 取代 scope 字符串）。
    // store 内部验证 regular file、大小、identity。
    // 会话边界：新 pick 成功时撤销上一次 pick 组的未消费 ref。
    let session = app.state::<AudioPickSession>();
    let group = store.fresh_group();
    let spec =
        ResourceGrantSpec::new(uses, ReusePolicy::OneShot, "settings.stt_pick").with_group(group);
    match store.issue_local_file(&path, spec) {
        Ok(audio_ref) => {
            let revoked = {
                let mut last = session.last_group.lock().unwrap_or_else(|e| e.into_inner());
                last.replace(group)
                    .map(|prev| store.revoke_group(prev))
                    .unwrap_or(0)
            };
            if revoked > 0 {
                tracing::debug!(revoked, "pick_audio_file: 已撤销上一次会话未消费 ref");
            }
            tracing::debug!("pick_audio_file: audio_ref issued");
            // 0.23.13：设置页转写测试同模式预热（best-effort，决策 11）
            spawn_stt_engine_prewarm(app);
            let display_name = safe_audio_display_name(&path);
            Ok(Some((audio_ref.as_str().to_string(), display_name)))
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

/// 为 VAD 调试回放派生一条只含 `PreviewAudio` 的新 audio_ref（不消费原 ref）。
///
/// **权限衰减铁则**：派生只能缩小授权。VAD 调试 picker
/// （`pick_audio_file_for_vad_debug`）签发的源 ref 同时授予
/// `TranscribeAudio`（分析）与 `PreviewAudio`（回放），此处只派生回放
/// 实际需要的 `PreviewAudio`；对只允许 `TranscribeAudio` 的 ref（普通
/// picker / chat 附件）派生会在 store 层被 `permission_escalation` 拒绝。
#[tauri::command]
pub async fn clone_audio_ref_for_vad_debug(
    app: tauri::AppHandle,
    audio_ref: String,
) -> Result<String, crate::app::command_error::CommandError> {
    let store = app
        .state::<std::sync::Arc<DefaultResourceStore>>()
        .inner()
        .clone();
    let spec = ResourceGrantSpec::new(
        ResourceUseSet::single(ResourceUse::PreviewAudio),
        ReusePolicy::OneShot,
        "settings.vad_debug",
    );
    store
        .issue_from_ref(&ResourceRef::from_token(audio_ref), spec)
        .map(|derived| derived.as_str().to_string())
        .map_err(|error| {
            tracing::warn!(error = %error, "clone_audio_ref_for_vad_debug: 无法派生 audio_ref");
            crate::app::command_error::CommandError::new(
                error.kind.as_str(),
                "无法复用所选音频文件",
                false,
            )
        })
}

// ── 对话窗口音频附件闭环（0.23.13 §8.3-4）───────────────────────────────

/// chat 附件签发结果——不暴露目录或绝对路径。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAudioAttachment {
    audio_ref: String,
    display_name: String,
}

/// chat 附件 grant TTL：一轮对话常超过 LocalFile 默认 5 分钟，
/// Reusable + 会话结束 revoke 场景放宽到 30 分钟（0.23 §8.3-4 决策）。
const CHAT_ATTACHMENT_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// `pick_chat_audio_attachment` — 对话窗口附加本地 WAV 附件。
///
/// 资源层的第一个 GUI 消费证明（0.23.13）：
/// - 可信 native picker 选择文件 → 签发 `use={TranscribeAudio}`、
///   `owner=chat_attach:<conversation_id>` 的 **Reusable** grant
///   （转写失败重试不烧 ref——ReusePolicy 上移为 grant 属性的价值）
/// - ref 以附件 chip 注入对话上下文，模型将其作为 `transcribe_audio`
///   tool 参数（需用户先在 AI 能力设置开启该 cap，确认流程照旧）
/// - 附件签发时后台预热本地 STT 引擎（对齐 CLI invoke 前 start 的模式，
///   0.23 §8.2 决策 11）；预热失败不阻塞附件，转写时返回结构化错误
#[tauri::command]
pub async fn pick_chat_audio_attachment(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<Option<ChatAudioAttachment>, crate::app::command_error::CommandError> {
    if conversation_id.trim().is_empty() {
        return Err(crate::app::command_error::CommandError::new(
            "invalid_args",
            "会话标识无效",
            false,
        ));
    }

    let store = app
        .state::<std::sync::Arc<DefaultResourceStore>>()
        .inner()
        .clone();

    let mut dialog = app.dialog().file();
    dialog = dialog.set_title("选择音频附件");
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
            return Err(crate::app::command_error::CommandError::new(
                "remote_path_forbidden",
                "不接受 URL 音频资源",
                false,
            ));
        }
        None => return Ok(None), // 用户取消
    };

    let owner = format!("chat_attach:{conversation_id}");
    let spec = ResourceGrantSpec::new(
        ResourceUseSet::single(ResourceUse::TranscribeAudio),
        ReusePolicy::Reusable,
        owner,
    )
    .with_ttl(CHAT_ATTACHMENT_TTL);

    match store.issue_local_file(&path, spec) {
        Ok(audio_ref) => {
            tracing::debug!("pick_chat_audio_attachment: audio_ref issued");
            // 附件签发时后台预热引擎（best-effort，不阻塞附件）
            spawn_stt_engine_prewarm(&app);
            let display_name = safe_audio_display_name(&path);
            Ok(Some(ChatAudioAttachment {
                audio_ref: audio_ref.as_str().to_string(),
                display_name,
            }))
        }
        Err(e) => {
            tracing::warn!(error = %e, "pick_chat_audio_attachment: 无法签发 audio_ref");
            Err(crate::app::command_error::CommandError::new(
                e.kind.as_str(),
                "无法使用所选音频文件",
                false,
            ))
        }
    }
}

/// `remove_chat_audio_attachment` — 移除单个附件 chip 时撤销其 ref。
#[tauri::command]
pub async fn remove_chat_audio_attachment(
    app: tauri::AppHandle,
    audio_ref: String,
) -> Result<bool, crate::app::command_error::CommandError> {
    let store = app
        .state::<std::sync::Arc<DefaultResourceStore>>()
        .inner()
        .clone();
    Ok(store.revoke(&ResourceRef::from_token(audio_ref)))
}

/// `revoke_chat_audio_attachments` — 会话结束/切换时按 owner 批量撤销附件 ref。
///
/// 返回撤销条数（诊断用）。
#[tauri::command]
pub async fn revoke_chat_audio_attachments(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<usize, crate::app::command_error::CommandError> {
    let store = app
        .state::<std::sync::Arc<DefaultResourceStore>>()
        .inner()
        .clone();
    let revoked = store.revoke_by_owner(&format!("chat_attach:{conversation_id}"));
    if revoked > 0 {
        tracing::debug!(revoked, "revoke_chat_audio_attachments: 会话附件已撤销");
    }
    Ok(revoked)
}

/// 后台预热本地 STT 引擎（best-effort，0.23 §8.2 决策 11）。
///
/// ensure 责任在可信入口（picker 签发时），capability 保持纯粹——
/// `transcribe_audio` 服务侧维持「不启动、不安装、不切换」。
/// 未启用/云端模式/未选择模型/未安装 → 静默跳过；启动失败只记 warn，
/// 不阻塞附件（转写时返回结构化 `SttBackendUnavailable`）。
fn spawn_stt_engine_prewarm(app: &tauri::AppHandle) {
    use crate::domain::local_engine::ModelInstallState;

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let config = crate::domain::config::stt_config::get_stt_config();
        if !config.enabled {
            tracing::debug!("stt prewarm: STT 未启用，跳过");
            return;
        }
        if config.mode != crate::domain::config::stt_config::SttMode::Local {
            tracing::debug!("stt prewarm: 非本地模式，跳过（云端文件转写 Unsupported）");
            return;
        }
        let Some(selection) = config.local_stt_selection.clone() else {
            tracing::debug!("stt prewarm: 未选择本地 STT 模型，跳过");
            return;
        };
        let Ok(engine_id) =
            crate::infra::local_engine::runtime::EngineId::new(&selection.engine_id)
        else {
            tracing::warn!("stt prewarm: engine_id 无效，跳过");
            return;
        };

        let manager = app
            .state::<std::sync::Arc<crate::app::local_engine::EngineManager>>()
            .inner()
            .clone();

        // 已在运行 → 无需预热
        match manager.get_connection(&engine_id).await {
            Ok(Some(_)) => {
                tracing::debug!("stt prewarm: 引擎已运行，跳过");
                return;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "stt prewarm: 查询引擎状态失败，跳过预热");
                return;
            }
        }

        let installed = manager
            .list_models(&engine_id)
            .await
            .into_iter()
            .any(|model| {
                model.model_id == selection.model_id
                    && model.install_state == ModelInstallState::Installed
            });
        if !installed {
            tracing::debug!("stt prewarm: 所选模型未安装，跳过（不自动下载）");
            return;
        }

        let mut adapter_config = crate::app::local_engine::config_source::funasr_adapter_config();
        adapter_config.engine_config["funasr_model"] =
            serde_json::Value::String(selection.model_id.clone());
        match manager.start(&engine_id, adapter_config).await {
            Ok(()) => {
                tracing::info!(model_id = %selection.model_id, "stt prewarm: 引擎已预热");
            }
            Err(error) => {
                tracing::warn!(%error, "stt prewarm: 引擎预热失败（转写时将返回结构化错误）");
            }
        }
    });
}

/// 读取 audio_ref 指向的音频字节供前端本地播放（VAD 调试回放）。
///
/// 走 store 完整校验（use = PreviewAudio）并一次性消费该 ref；字节经原始 IPC 返回，
/// 避免 JSON 数字数组序列化。前端转 blob URL 后交给 `<audio>` 播放。
#[tauri::command]
pub async fn read_audio_for_playback(
    app: tauri::AppHandle,
    audio_ref: String,
) -> Result<tauri::ipc::Response, crate::app::command_error::CommandError> {
    let store = app
        .state::<std::sync::Arc<DefaultResourceStore>>()
        .inner()
        .clone();
    let mut opened = store
        .open(
            &ResourceRef::from_token(audio_ref),
            ResourceUse::PreviewAudio,
        )
        .map_err(|error| {
            tracing::warn!(error = %error, "read_audio_for_playback: 无法解析 audio_ref");
            crate::app::command_error::CommandError::new(
                error.kind.as_str(),
                "音频资源不可用",
                false,
            )
        })?;
    // 从 open 返回的已验证 lease 读取，不走"路径再打开"路径
    let bytes = tokio::task::spawn_blocking(move || -> Result<bytes::Bytes, String> {
        opened
            .read_all_bounded(256 * 1024 * 1024)
            .map_err(|e| e.to_string())
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
    Ok(tauri::ipc::Response::new(bytes.to_vec()))
}
