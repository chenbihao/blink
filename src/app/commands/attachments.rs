//! 0.27.9 聊天附件命令层——可信签发入口 + @ 面板数据源 + 草稿事务采纳。
//!
//! 铁则（phase 0.27 §3.3/§8.2）：
//! - **前端任意 path 不是授权凭据**：ref 只由本文件的受控入口签发——
//!   picker 对话框、选中项确认（awareness 快照核验）、剪贴板历史读取链。
//! - owner 语义：会话内附件 `chat_attach:{cid}`；跨窗口草稿事务 `chat_prefill`
//!  （临时 owner，chat 前端采纳草稿时经 `adopt_chat_attachments` 转移为会话
//!   owner——「确认消费后再转移资源归属」§3.3）。
//! - 文本附件按 §3.3 预检：≤256 KiB、UTF-8（probe）、非目录、逐项反馈原因。

use std::path::Path;
use std::sync::Arc;

use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use crate::app::command_error::CommandError;

use crate::domain::ai::attachment::{
    TEXT_FILE_MAX_BYTES, build_text_injection, classify_text_probe,
};
use crate::domain::resource::{
    DefaultResourceStore, ResourceGrantSpec, ResourceRef, ResourceUse, ResourceUseSet, ReusePolicy,
};

/// 附件 grant TTL：对齐音频附件先例（一轮对话 + 重试窗口，30 分钟）。
const ATTACHMENT_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// 跨窗口草稿事务的临时 owner（open_chat 预填 → chat 前端 adopt 转移）。
const PREFILL_OWNER: &str = "chat_prefill";

/// @ 面板最近复制文件条目数上限。
const RECENT_FILE_ENTRIES: i64 = 5;

fn session_owner(conversation_id: &str) -> String {
    format!("chat_attach:{conversation_id}")
}

fn resource_store(app: &tauri::AppHandle) -> Result<Arc<DefaultResourceStore>, CommandError> {
    app.try_state::<Arc<DefaultResourceStore>>()
        .map(|state| state.inner().clone())
        .ok_or_else(|| CommandError::new("no_resource_store", "运行时未启用资源库", false))
}

// ── wire 载荷 ────────────────────────────────────────────────────────────────

/// 已签发附件（与 ChatAttachmentInput 同形）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SignedAttachment {
    pub kind: String,
    pub r#ref: String,
    pub display: String,
}

/// 文本附件附加元数据（chip 展示注入规模/截断状态）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextFileMeta {
    pub size_bytes: u64,
    /// utf8 | empty（其余状态在签发阶段已拒绝）。
    pub text_status: String,
    pub token_est: usize,
    /// 是否会在注入时被截断（原文 > 4000 tokens）。
    pub truncated: bool,
}

/// 单文件签发结果：成功带 ref + 元数据，失败带原因（逐项反馈，不静默丢项）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachFileOutcome {
    pub display: String,
    pub ok: Option<SignedOk>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedOk {
    #[serde(flatten)]
    pub attachment: SignedAttachment,
    pub meta: TextFileMeta,
}

/// @ 面板：当前会话有效引用。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRefItem {
    pub kind: String,
    pub r#ref: String,
    pub size_bytes: u64,
    pub expires_in_seconds: u64,
    pub label: Option<String>,
}

/// @ 面板：最近复制文件条目内单文件预检。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentFileItem {
    pub name: String,
    pub path: String,
    pub attachable: bool,
    /// 不可附加原因（attachable=false 时给前端展示）。
    pub reason: Option<String>,
    pub size_bytes: u64,
}

/// @ 面板：最近复制的文件条目（一次复制的一组文件）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentFileEntry {
    pub id: String,
    pub created_at: i64,
    pub files: Vec<RecentFileItem>,
}

/// @ 面板：最近剪贴板图片条目（0.27.10）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentImageItem {
    pub id: String,
    pub width: i64,
    pub height: i64,
    pub created_at: i64,
}

/// `attachment_list_candidates` 返回载荷。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentCandidates {
    pub session_refs: Vec<SessionRefItem>,
    pub recent_files: Vec<RecentFileEntry>,
    /// 0.27.10：最近剪贴板图片（缩略图走 blink-clipimg 协议）。
    pub recent_images: Vec<RecentImageItem>,
}

// ── 内部：文本签发/预检 ──────────────────────────────────────────────────────

/// 签发文本附件 ref（可信入口通用路径）：预检 → 签发 → 元数据。
async fn sign_text_file(
    store: &Arc<DefaultResourceStore>,
    path: &Path,
    owner: &str,
) -> Result<SignedOk, String> {
    let display = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());

    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|_| "文件不存在或不可访问".to_string())?;
    if meta.is_dir() {
        return Err("目录不能作为文本附件".into());
    }
    if meta.len() > TEXT_FILE_MAX_BYTES {
        return Err(format!(
            "文件大小 {} KiB 超过 {} KiB 上限",
            meta.len() / 1024,
            TEXT_FILE_MAX_BYTES / 1024
        ));
    }
    // 整读（≤256 KiB）做完整探测与 token 统计；probe 语义与发送侧一致
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("读取文件失败: {e}"))?;
    let status = classify_text_probe(&bytes);
    let (text_status, token_est, truncated) = match status {
        crate::domain::ai::attachment::TextProbeStatus::Utf8 => {
            let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
            let text = std::str::from_utf8(body).map_err(|_| "无效 UTF-8".to_string())?;
            let injection = build_text_injection(text);
            (
                "utf8".to_string(),
                injection.token_est,
                injection.truncated,
            )
        }
        crate::domain::ai::attachment::TextProbeStatus::Empty => ("empty".to_string(), 0, false),
        crate::domain::ai::attachment::TextProbeStatus::Binary => {
            return Err("文件包含二进制内容（NUL 字节），不能按文本附加".into());
        }
        crate::domain::ai::attachment::TextProbeStatus::NonUtf8 => {
            return Err("文件不是 UTF-8 编码，请先转换为 UTF-8 再附加".into());
        }
    };

    let spec = ResourceGrantSpec::new(
        ResourceUseSet::single(ResourceUse::AttachmentText),
        ReusePolicy::Reusable,
        owner.to_string(),
    )
    .with_ttl(ATTACHMENT_TTL)
    .with_label(display.clone());
    let r#ref = store.issue_local_file(path, spec).map_err(|e| {
        tracing::warn!(error = %e, "sign_text_file: 签发失败");
        "无法签发附件引用（文件可能被占用或已变化）".to_string()
    })?;
    tracing::debug!(owner, size = meta.len(), "chat text attachment issued");

    Ok(SignedOk {
        attachment: SignedAttachment {
            kind: "text_file".into(),
            r#ref: r#ref.as_str().to_string(),
            display,
        },
        meta: TextFileMeta {
            size_bytes: meta.len(),
            text_status,
            token_est,
            truncated,
        },
    })
}

/// @ 面板轻量预检（不签发）：目录/超限/非 UTF-8 给出不可附加原因。
async fn precheck_text_path(path: &str) -> RecentFileItem {
    let name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    let p = Path::new(path);
    let fail = |name: String, reason: &str| RecentFileItem {
        name,
        path: path.to_string(),
        attachable: false,
        reason: Some(reason.to_string()),
        size_bytes: 0,
    };
    let meta = match tokio::fs::metadata(p).await {
        Ok(m) => m,
        Err(_) => return fail(name, "文件已不存在"),
    };
    let size = meta.len();
    let item =
        |name: String, reason: Option<String>| RecentFileItem {
            name,
            path: path.to_string(),
            attachable: reason.is_none(),
            reason,
            size_bytes: size,
        };
    if meta.is_dir() {
        return item(name, Some("目录不能附加".into()));
    }
    if size > TEXT_FILE_MAX_BYTES {
        return item(name, Some("超过 256 KiB 上限".into()));
    }
    // 首 8 KiB probe——后续部分在签发/发送侧整读校验
    use tokio::io::AsyncReadExt;
    let mut sample = vec![0u8; 8 * 1024];
    let read = match tokio::fs::File::open(p).await {
        Ok(mut f) => f.read(&mut sample).await.unwrap_or(0),
        Err(_) => return fail(name, "文件不可读"),
    };
    sample.truncate(read);
    match classify_text_probe(&sample) {
        crate::domain::ai::attachment::TextProbeStatus::NonUtf8 => {
            item(name, Some("非 UTF-8 编码".into()))
        }
        crate::domain::ai::attachment::TextProbeStatus::Binary => {
            item(name, Some("二进制文件".into()))
        }
        _ => item(name, None),
    }
}

// ── Commands ────────────────────────────────────────────────────────────────

/// 可信入口 1：文件对话框多选文本附件（签发 + 预检，逐项反馈）。
#[tauri::command]
pub async fn attachment_pick_files(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<Vec<AttachFileOutcome>, CommandError> {
    if conversation_id.trim().is_empty() {
        return Err(CommandError::new("invalid_args", "会话标识无效", false));
    }
    let store = resource_store(&app)?;

    let mut dialog = app.dialog().file();
    dialog = dialog.set_title("选择文本附件");
    dialog = dialog.add_filter(
        "文本文件",
        &[
            "txt", "md", "json", "toml", "yaml", "yml", "csv", "log", "ini", "xml", "rs", "py",
            "js", "ts", "html", "css",
        ],
    );
    dialog = dialog.add_filter("所有文件", &["*"]);

    let (sender, receiver) = tokio::sync::oneshot::channel();
    dialog.pick_files(move |picked| {
        let _ = sender.send(picked);
    });
    let picked = receiver.await.map_err(|_| {
        CommandError::new("dialog_closed", "文件选择器意外关闭，请重试", true)
    })?;

    let paths = match picked {
        Some(list) => list
            .into_iter()
            .filter_map(|p| match p {
                tauri_plugin_dialog::FilePath::Path(path) => Some(path),
                tauri_plugin_dialog::FilePath::Url(_) => None,
            })
            .collect::<Vec<_>>(),
        None => return Ok(Vec::new()), // 用户取消
    };

    let owner = session_owner(&conversation_id);
    let mut outcomes = Vec::with_capacity(paths.len());
    for path in paths {
        let display = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let outcome = match sign_text_file(&store, &path, &owner).await {
            Ok(ok) => AttachFileOutcome {
                display,
                ok: Some(ok),
                reason: None,
            },
            Err(reason) => AttachFileOutcome {
                display,
                ok: None,
                reason: Some(reason),
            },
        };
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// 可信入口 2：awareness 选中项确认——路径必须 ⊆ 当前文件选中快照
/// （跨窗口流转，签临时 owner `chat_prefill`，由 chat 前端 adopt）。
#[tauri::command]
pub async fn attachment_sign_selection_files(
    app: tauri::AppHandle,
    paths: Vec<String>,
) -> Result<Vec<AttachFileOutcome>, CommandError> {
    let store = resource_store(&app)?;
    let search = app
        .try_state::<Arc<crate::domain::search::SearchService>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| CommandError::new("no_search_service", "感知服务不可用", false))?;
    let allowed = search.current_file_selection_paths();

    let mut outcomes = Vec::with_capacity(paths.len());
    for raw in paths {
        let display = Path::new(&raw)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| raw.clone());
        let path = std::path::PathBuf::from(&raw);
        if !allowed.contains(&path) {
            outcomes.push(AttachFileOutcome {
                display,
                ok: None,
                reason: Some("文件不在当前选中集合内，请重新选择".into()),
            });
            continue;
        }
        let outcome = match sign_text_file(&store, &path, PREFILL_OWNER).await {
            Ok(ok) => AttachFileOutcome {
                display,
                ok: Some(ok),
                reason: None,
            },
            Err(reason) => AttachFileOutcome {
                display,
                ok: None,
                reason: Some(reason),
            },
        };
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// 可信入口 3：剪贴板文件历史条目（读取链在后端 DB，路径不出前端签发面）。
#[tauri::command]
pub async fn attachment_attach_clipboard_files(
    app: tauri::AppHandle,
    entry_id: String,
    conversation_id: String,
) -> Result<Vec<AttachFileOutcome>, CommandError> {
    if conversation_id.trim().is_empty() {
        return Err(CommandError::new("invalid_args", "会话标识无效", false));
    }
    let store = resource_store(&app)?;
    let pools = app.state::<crate::infra::data::DbPools>();
    let entry = blink_infra::data::clipboard_files::get_paths_by_id(&pools.history, &entry_id)
        .await
        .ok_or_else(|| CommandError::new("not_found", "剪贴板文件条目不存在", false))?;

    let owner = session_owner(&conversation_id);
    let mut outcomes = Vec::with_capacity(entry.paths.len());
    for path in entry.paths {
        let display = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        let outcome = match sign_text_file(&store, Path::new(&path), &owner).await {
            Ok(ok) => AttachFileOutcome {
                display,
                ok: Some(ok),
                reason: None,
            },
            Err(reason) => AttachFileOutcome {
                display,
                ok: None,
                reason: Some(reason),
            },
        };
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// @ 面板数据源：当前会话有效引用 + 最近复制的文件（含逐项预检）。
#[tauri::command]
pub async fn attachment_list_candidates(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<AttachmentCandidates, CommandError> {
    let store = resource_store(&app)?;

    // 会话内有效引用：按 use 推导种类，过滤过期/已撤销（store 侧完成）
    let session_refs: Vec<SessionRefItem> = store
        .list_by_owner(&session_owner(&conversation_id))
        .into_iter()
        .filter_map(|v| {
            let kind = if v.uses.iter().any(|u| u == "attachment_text") {
                "text_file"
            } else if v.uses.iter().any(|u| u == "vision_image") {
                "image"
            } else if v.uses.iter().any(|u| u == "transcribe_audio") {
                "audio"
            } else {
                return None; // 其他用途资源不进附件面板
            };
            Some(SessionRefItem {
                kind: kind.to_string(),
                r#ref: v.token,
                size_bytes: v.size_bytes,
                expires_in_seconds: v.expires_in_seconds,
                label: v.label,
            })
        })
        .take(16)
        .collect();

    // 最近复制的文件（0.26.9 底座）：逐项预检，非文本/超限/目录只展示原因
    let pools = app.state::<crate::infra::data::DbPools>();
    let metas =
        blink_infra::data::clipboard_files::query_recent_file_list(&pools.history, RECENT_FILE_ENTRIES)
            .await;
    let mut recent_files = Vec::with_capacity(metas.len());
    for meta in metas {
        let Some(full) =
            blink_infra::data::clipboard_files::get_paths_by_id(&pools.history, &meta.id).await
        else {
            continue;
        };
        let mut files = Vec::with_capacity(full.paths.len());
        for path in full.paths {
            files.push(precheck_text_path(&path).await);
        }
        recent_files.push(RecentFileEntry {
            id: meta.id,
            created_at: meta.created_at,
            files,
        });
    }

    // 最近剪贴板图片（0.27.10——签新 ref 不复活截图 SESSION，§2.2）
    let images = blink_infra::data::clipboard_images::query_recent_image_list(&pools.cache, 6)
        .await;
    let recent_images = images
        .into_iter()
        .map(|img| RecentImageItem {
            id: img.id,
            width: img.width,
            height: img.height,
            created_at: img.created_at,
        })
        .collect();

    Ok(AttachmentCandidates {
        session_refs,
        recent_files,
        recent_images,
    })
}

/// 跨窗口草稿采纳：把 `chat_prefill` 临时 owner 的 ref 转移为会话 owner
///（§3.3「确认消费后再转移资源归属」）。返回成功转移数。
#[tauri::command]
pub async fn adopt_chat_attachments(
    app: tauri::AppHandle,
    refs: Vec<String>,
    conversation_id: String,
) -> Result<usize, CommandError> {
    if conversation_id.trim().is_empty() {
        return Err(CommandError::new("invalid_args", "会话标识无效", false));
    }
    let store = resource_store(&app)?;
    let owner = session_owner(&conversation_id);
    let adopted = refs
        .iter()
        .filter(|r| {
            store.transfer_owner(&ResourceRef::from_token(r.as_str()), PREFILL_OWNER, &owner)
        })
        .count();
    if adopted > 0 {
        tracing::debug!(adopted, "chat prefill attachments adopted");
    }
    Ok(adopted)
}

// ── 0.27.10：图片附件签发（Vision 输入） ─────────────────────────────────────

/// 签发内存图片（PNG 字节，VisionImage 用途）。
fn issue_image_ref(
    store: &Arc<DefaultResourceStore>,
    png_bytes: Vec<u8>,
    display: &str,
    owner: &str,
) -> Result<SignedAttachment, CommandError> {
    if png_bytes.is_empty() {
        return Err(CommandError::new("empty_image", "图片数据为空", false));
    }
    // 编码体积上限预检（归一化在发送侧执行——此处只拦明显超限）
    if png_bytes.len() as u64 > crate::domain::ai::vision::MAX_ENCODED_BYTES_PER_IMAGE {
        return Err(CommandError::new(
            "image_too_large",
            format!(
                "图片 {:.1} MiB 超过单图 8 MiB 上限",
                png_bytes.len() as f64 / (1024.0 * 1024.0)
            ),
            false,
        ));
    }
    let spec = ResourceGrantSpec::new(
        ResourceUseSet::single(ResourceUse::VisionImage),
        ReusePolicy::Reusable,
        owner.to_string(),
    )
    .with_ttl(ATTACHMENT_TTL)
    .with_label(display.to_string());
    let r = store.issue_memory(bytes::Bytes::from(png_bytes), "image/png", spec).map_err(|e| {
        tracing::warn!(error = %e, "issue_image_ref: 签发失败");
        CommandError::new("issue_failed", "图片资源签发失败（可能超出内存配额）", false)
    })?;
    Ok(SignedAttachment {
        kind: "image".into(),
        r#ref: r.as_str().to_string(),
        display: display.to_string(),
    })
}

/// paste 入口：读取**当前**剪贴板图片（不经历史表——复制后立即粘贴场景），
/// 签发为会话内 VisionImage 引用。
#[tauri::command]
pub async fn attachment_attach_clipboard_image(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<SignedAttachment, CommandError> {
    if conversation_id.trim().is_empty() {
        return Err(CommandError::new("invalid_args", "会话标识无效", false));
    }
    let store = resource_store(&app)?;
    let png = tokio::task::spawn_blocking(|| {
        crate::infra::platform::clipboard::read_current_image()
    })
    .await
    .map_err(|e| CommandError::new("internal", format!("读取剪贴板失败: {e}"), true))?
    .ok_or_else(|| {
        CommandError::new("no_image", "剪贴板中没有图片", false)
    })?;
    issue_image_ref(&store, png, "剪贴板图片.png", &session_owner(&conversation_id))
}

/// 「问 AI」/ @ 面板共用：剪贴板图片历史条目签发。
///
/// `conversation_id = Some(cid)` → 会话内附件（@ 面板采纳）；`None` →
/// 跨窗口流转临时 owner（问 AI → open_chat 草稿，chat 前端 adopt 转移）。
#[tauri::command]
pub async fn attachment_sign_clipboard_history_image(
    app: tauri::AppHandle,
    image_id: String,
    conversation_id: Option<String>,
) -> Result<SignedAttachment, CommandError> {
    let store = resource_store(&app)?;
    let pools = app.state::<crate::infra::data::DbPools>();
    let png = blink_infra::data::clipboard_images::get_png_by_id(&pools.cache, &image_id)
        .await
        .ok_or_else(|| CommandError::new("not_found", "剪贴板图片条目不存在", false))?;
    let short_id: String = image_id.chars().take(8).collect();
    let display = format!("截图-{short_id}.png");
    let owner = conversation_id
        .as_deref()
        .filter(|cid| !cid.trim().is_empty())
        .map(session_owner)
        .unwrap_or_else(|| PREFILL_OWNER.to_string());
    issue_image_ref(&store, png, &display, &owner)
}

/// 「问 AI」入口 2：截图 overlay 选区 PNG 字节直接签发（跨窗口流转）。
/// 字节数组走 JSON IPC——与 pin_image 先例同通道，选区截图 1-3 MiB 可接受。
#[tauri::command]
pub async fn attachment_sign_png_bytes(
    app: tauri::AppHandle,
    bytes: Vec<u8>,
    display: Option<String>,
) -> Result<SignedAttachment, CommandError> {
    let store = resource_store(&app)?;
    let display = display.unwrap_or_else(|| "截图.png".to_string());
    let display: String = display.chars().filter(|c| !c.is_control()).take(80).collect();
    issue_image_ref(&store, bytes, &display, PREFILL_OWNER)
}

/// drop 入口：拖入文件按内容嗅探分类签发（PNG/JPEG → VisionImage 本地文件
/// ref，发送侧归一化；UTF-8 文本 → AttachmentText）。逐项反馈，不静默丢项。
#[tauri::command]
pub async fn attachment_sign_dropped_files(
    app: tauri::AppHandle,
    paths: Vec<String>,
    conversation_id: String,
) -> Result<Vec<AttachFileOutcome>, CommandError> {
    if conversation_id.trim().is_empty() {
        return Err(CommandError::new("invalid_args", "会话标识无效", false));
    }
    let store = resource_store(&app)?;
    let owner = session_owner(&conversation_id);

    let mut outcomes = Vec::with_capacity(paths.len());
    for raw in paths {
        let display = Path::new(&raw)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| raw.clone());
        let path = std::path::PathBuf::from(&raw);
        let outcome = match sniff_and_sign_file(&store, &path, &display, &owner).await {
            Ok(ok) => AttachFileOutcome {
                display,
                ok: Some(ok),
                reason: None,
            },
            Err(reason) => AttachFileOutcome {
                display,
                ok: None,
                reason: Some(reason),
            },
        };
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// 单文件分类签发：图片 magic（PNG/JPEG）→ VisionImage；否则按文本附件签发。
/// 返回扁平 SignedAttachment（text 时附带 meta 一并投影）。
async fn sniff_and_sign_file(
    store: &Arc<DefaultResourceStore>,
    path: &Path,
    display: &str,
    owner: &str,
) -> Result<SignedOk, String> {
    // 读首 8 字节嗅探（不足则非图片）
    use tokio::io::AsyncReadExt;
    let mut head = [0u8; 8];
    let n = match tokio::fs::File::open(path).await {
        Ok(mut f) => f.read(&mut head).await.unwrap_or(0),
        Err(_) => return Err("文件不可读".into()),
    };
    let is_png = n >= 4 && &head[..4] == &[0x89, b'P', b'N', b'G'];
    let is_jpeg = n >= 3 && &head[..3] == &[0xFF, 0xD8, 0xFF];
    if is_png || is_jpeg {
        // 编码体积预检（与文本路径 256KiB 预检口径一致）：发送时同样会被
        // read_all_bounded(8MiB) 拒绝，签发层先拒可给用户即时反馈，不签出
        // 必然失效的引用
        if let Ok(meta) = tokio::fs::metadata(path).await
            && meta.len() > crate::domain::ai::vision::MAX_ENCODED_BYTES_PER_IMAGE
        {
            return Err(format!(
                "图片体积 {:.1} MiB 超过单图 8 MiB 上限，请压缩后再附加",
                meta.len() as f64 / (1024.0 * 1024.0)
            ));
        }
        let spec = ResourceGrantSpec::new(
            ResourceUseSet::single(ResourceUse::VisionImage),
            ReusePolicy::Reusable,
            owner.to_string(),
        )
        .with_ttl(ATTACHMENT_TTL)
        .with_label(display.to_string());
        let r = store.issue_local_file(path, spec).map_err(|e| {
            tracing::warn!(error = %e, "sniff_and_sign_file: 图片签发失败");
            "无法签发图片附件（文件可能被占用或已变化）".to_string()
        })?;
        return Ok(SignedOk {
            attachment: SignedAttachment {
                kind: "image".into(),
                r#ref: r.as_str().to_string(),
                display: display.to_string(),
            },
            meta: TextFileMeta {
                size_bytes: 0,
                text_status: "image".into(),
                token_est: 0,
                truncated: false,
            },
        });
    }
    sign_text_file(store, path, owner).await
}

/// 移除单个附件 chip 时撤销其 ref（任意 kind 通用）。
#[tauri::command]
pub async fn remove_chat_attachment(
    app: tauri::AppHandle,
    attachment_ref: String,
) -> Result<bool, CommandError> {
    let store = resource_store(&app)?;
    Ok(store.revoke(&ResourceRef::from_token(attachment_ref)))
}
