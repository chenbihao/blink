//! 录制编辑进度草稿（0.28.23 §5.7.4，bin 侧）。
//!
//! 职责：
//! - **保存**：`gif_edit` 成功 / 播放位置更新后防抖写 `sessions/<id>/draft.json`
//!   （编辑态 + 素材身份），切换会话与关闭编辑器前同步 flush；原子写复用
//!   infra `atomic_write_bytes`，旧异步写入按 generation 失效，不覆盖新状态。
//! - **恢复**：`ensure_timeline` 首建时间轴时读取草稿——版本/素材身份
//!   （创建时间 + 帧数）匹配才应用；损坏/不匹配如实返回 `Discarded`，
//!   不宣称恢复全部编辑。
//! - undo/redo 栈不持久化（domain 语义）；播放位置随草稿保存/恢复。
//!
//! 草稿文件在会话目录内，随 TTL 清理与显式删除一起消失，无独立生命周期。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Manager};

use blink_domain::gif::{SessionDraftFile, TimelineDraft, TIMELINE_DRAFT_VERSION};
use blink_infra::gif::{self as infra_gif, SpoolReader};

use crate::app::gif_service::GifService;

/// 编辑后防抖保存间隔（短防抖；切换/关闭走同步 flush 不等这里）。
const DRAFT_SAVE_DEBOUNCE: Duration = Duration::from_millis(800);

/// 每会话草稿状态（播放位置 + 防抖代次）。
#[derive(Debug, Default)]
pub(crate) struct DraftState {
    pub play_pos_ms: u64,
    /// 防抖代次：schedule 递增，落地任务校验后才写盘（旧任务不覆盖新状态）。
    generation: u64,
    /// 是否有待写盘的变更。
    dirty: bool,
}

/// 播放位置更新（前端定位提交；与编辑保存共用防抖链）。
pub(crate) fn set_play_position(app: &AppHandle, session_id: &str, pos_ms: u64) {
    let service = app.state::<Arc<GifService>>();
    {
        let mut drafts = service.drafts.lock().unwrap_or_else(|e| e.into_inner());
        let state = drafts.entry(session_id.to_string()).or_default();
        state.play_pos_ms = pos_ms;
        state.dirty = true;
    }
    schedule(app, session_id);
}

/// 编辑命令成功后调度防抖保存。
pub(crate) fn schedule_draft_save(app: &AppHandle, session_id: &str) {
    {
        let service = app.state::<Arc<GifService>>();
        let mut drafts = service.drafts.lock().unwrap_or_else(|e| e.into_inner());
        let state = drafts.entry(session_id.to_string()).or_default();
        state.dirty = true;
    }
    schedule(app, session_id);
}

fn schedule(app: &AppHandle, session_id: &str) {
    let generation = {
        let service = app.state::<Arc<GifService>>();
        let mut drafts = service.drafts.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = drafts.get_mut(session_id) else {
            return;
        };
        state.generation += 1;
        state.generation
    };
    let app = app.clone();
    let session = session_id.to_string();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(DRAFT_SAVE_DEBOUNCE).await;
        let service = app.state::<Arc<GifService>>();
        let still_current = {
            let drafts = service.drafts.lock().unwrap_or_else(|e| e.into_inner());
            drafts
                .get(&session)
                .map(|s| s.generation == generation && s.dirty)
                .unwrap_or(false)
        };
        if still_current {
            if let Err(e) = save_now(&app, &session) {
                tracing::warn!(session_id = %session, error = %e, "编辑草稿保存失败（下次编辑/关闭时重试）");
            }
        }
    });
}

/// 立即保存（切换会话/关闭编辑器前 flush）。
pub(crate) fn flush_draft(app: &AppHandle, session_id: &str) {
    let service = app.state::<Arc<GifService>>();
    // 无任何编辑且无位置记录时跳过（不产生无意义草稿）
    let has_state = {
        let drafts = service.drafts.lock().unwrap_or_else(|e| e.into_inner());
        drafts
            .get(session_id)
            .map(|s| s.dirty || s.play_pos_ms > 0)
            .unwrap_or(false)
    };
    if !has_state {
        return;
    }
    if let Err(e) = save_now(app, session_id) {
        tracing::warn!(session_id, error = %e, "编辑草稿 flush 失败（编辑进度可能未保存）");
    }
}

fn save_now(app: &AppHandle, session_id: &str) -> Result<(), String> {
    let service = app.state::<Arc<GifService>>();
    let dir = GifService::root().join("sessions").join(session_id);
    // 会话已被删除（显式丢弃/TTL 清理与防抖任务竞态）：不写盘——
    // atomic_write 的 create_dir_all 会复活只含草稿的空目录。
    if !dir.join("session.json").is_file() {
        forget_session(&service, session_id);
        return Err("会话已删除，草稿随目录清理".into());
    }
    let (draft, identity) = {
        let drafts = service.drafts.lock().unwrap_or_else(|e| e.into_inner());
        let identities = service.session_identity.lock().unwrap_or_else(|e| e.into_inner());
        let play = drafts.get(session_id).map(|s| s.play_pos_ms).unwrap_or(0);
        let Some(&(created_at, material_frames)) = identities.get(session_id) else {
            return Err("缺少素材身份记录，跳过草稿写入".into());
        };
        let timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
        let Some(tl) = timelines.get(session_id) else {
            return Err("时间轴不在内存，跳过草稿写入".into());
        };
        (tl.to_draft(play), (created_at, material_frames))
    };
    let file = SessionDraftFile {
        version: TIMELINE_DRAFT_VERSION,
        session_created_at: identity.0,
        material_frames: identity.1,
        draft,
    };
    let body = serde_json::to_string(&file).map_err(|e| e.to_string())?;
    infra_gif::spool::write_draft(&dir, &body).map_err(|e| e.to_string())?;
    // 落地完成：清脏标记（代次保留——后续 schedule 会再递增）
    let mut drafts = service.drafts.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = drafts.get_mut(session_id) {
        state.dirty = false;
    }
    Ok(())
}

/// 草稿恢复结果（gif_get_session 透传给前端）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftRestore {
    /// 恢复成功，携带保存时的播放位置（ms）。
    Restored(u64),
    /// 无草稿（首次打开）。
    Missing,
    /// 草稿存在但版本/身份不匹配或损坏——已回退原始素材，需如实提示。
    Discarded,
}

/// 首建时间轴后尝试恢复草稿（ensure_timeline 调用；时间轴已在 map 中）。
pub(crate) fn try_restore_draft(
    service: &Arc<GifService>,
    session_id: &str,
    reader: &SpoolReader,
) -> DraftRestore {
    let identity = (reader.meta.created_at, reader.frame_count());
    service
        .session_identity
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.to_string(), identity);

    let Some(raw) = infra_gif::spool::read_draft(&GifService::root(), session_id) else {
        return DraftRestore::Missing;
    };
    let restore = (|| -> Option<(TimelineDraft, u64)> {
        let file: SessionDraftFile = serde_json::from_str(&raw).ok()?;
        if !file.identity_matches(identity.0, identity.1) {
            return None;
        }
        let play = file.draft.play_pos_ms;
        Some((file.draft, play))
    })();
    let Some((draft, play_pos)) = restore else {
        tracing::info!(session_id, "编辑草稿不兼容/素材身份不匹配，已回退原始素材");
        return DraftRestore::Discarded;
    };
    // 锁序约定：timelines 单独持有并在写 drafts 前释放（save_now 为
    // drafts → identity → timelines，保持无环）。
    let restored = {
        let mut timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
        match timelines.get_mut(session_id) {
            Some(tl) => tl.restore_draft(draft).is_ok(),
            None => false,
        }
    };
    if !restored {
        tracing::info!(session_id, "编辑草稿恢复校验失败，已回退原始素材");
        return DraftRestore::Discarded;
    }
    service
        .drafts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            session_id.to_string(),
            DraftState {
                play_pos_ms: play_pos,
                generation: 0,
                dirty: false,
            },
        );
    DraftRestore::Restored(play_pos)
}

/// 删除会话时清理内存草稿/身份数据（目录删除已带走草稿文件）。
pub(crate) fn forget_session(service: &Arc<GifService>, session_id: &str) {
    service
        .drafts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session_id);
    service
        .session_identity
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session_id);
}

/// 会话目录是否存在草稿（列表摘要用；文件探测，不解析）。
#[allow(dead_code)]
pub(crate) fn has_draft(session_dir: &Path) -> bool {
    infra_gif::spool::draft_path(session_dir).is_file()
}
