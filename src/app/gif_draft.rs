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

use blink_domain::gif::{SessionDraftFile, TIMELINE_DRAFT_VERSION, Timeline, TimelineDraft};
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
    saved_revision: Option<u64>,
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
            tauri::async_runtime::spawn_blocking(move || {
                if let Err(e) = save_now(&app, &session) {
                    tracing::warn!(session_id = %session, error = %e, "编辑草稿保存失败（下次编辑/关闭时重试）");
                }
            });
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

/// 退出时也保存已切走会话中尚在防抖的编辑。
pub(crate) fn flush_all_drafts(app: &AppHandle) {
    let ids: Vec<_> = app
        .state::<Arc<GifService>>()
        .drafts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    for id in ids {
        flush_draft(app, &id);
    }
}

fn save_now(app: &AppHandle, session_id: &str) -> Result<(), String> {
    let service = app.state::<Arc<GifService>>();
    save_to(&service, &GifService::root(), session_id)
}

fn save_to(service: &Arc<GifService>, root: &Path, session_id: &str) -> Result<(), String> {
    let dir = root.join("sessions").join(session_id);
    // 会话已被删除（显式丢弃/TTL 清理与防抖任务竞态）：不写盘——
    // atomic_write 的 create_dir_all 会复活只含草稿的空目录。
    if !dir.join("session.json").is_file() {
        forget_session(&service, session_id);
        return Err("会话已删除，草稿随目录清理".into());
    }
    // 写盘与状态变更串行，禁止旧快照后落盘或清除新一轮 dirty 标记。
    let mut drafts = service.drafts.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = drafts.get_mut(session_id).filter(|s| s.dirty) else {
        return Ok(());
    };
    let (draft, identity, revision) = {
        let identities = service
            .session_identity
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let play = state.play_pos_ms;
        let Some(&(created_at, material_frames)) = identities.get(session_id) else {
            return Err("缺少素材身份记录，跳过草稿写入".into());
        };
        let timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
        let Some(tl) = timelines.get(session_id) else {
            return Err("时间轴不在内存，跳过草稿写入".into());
        };
        (
            (state.saved_revision != Some(tl.revision())).then(|| tl.to_draft(play)),
            (created_at, material_frames),
            tl.revision(),
        )
    };
    if let Some(draft) = draft {
        let file = SessionDraftFile {
            version: TIMELINE_DRAFT_VERSION,
            session_created_at: identity.0,
            material_frames: identity.1,
            draft,
        };
        let body = serde_json::to_string(&file).map_err(|e| e.to_string())?;
        infra_gif::spool::write_draft(&dir, &body).map_err(|e| e.to_string())?;
    }
    let position = serde_json::json!({"revision":revision,"createdAt":identity.0,"frames":identity.1,"posMs":state.play_pos_ms});
    infra_gif::spool::write_position(&dir, &position.to_string()).map_err(|e| e.to_string())?;
    state.saved_revision = Some(revision);
    // 落地完成：清脏标记（代次保留——后续 schedule 会再递增）
    state.dirty = false;
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

/// 初始化及草稿恢复整体完成后才允许其他命令使用时间轴。
pub(crate) fn ensure_timeline(
    service: &Arc<GifService>,
    reader: &SpoolReader,
    session_id: &str,
) -> Result<DraftRestore, String> {
    ensure_timeline_at(service, reader, session_id, &GifService::root())
}

fn ensure_timeline_at(
    service: &Arc<GifService>,
    reader: &SpoolReader,
    session_id: &str,
    root: &Path,
) -> Result<DraftRestore, String> {
    let _initializing = service
        .timeline_init
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fresh = {
        let mut timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
        if timelines.contains_key(session_id) {
            false
        } else {
            let timings = reader.timings().map_err(|e| e.to_string())?;
            timelines.insert(session_id.to_string(), Timeline::from_timings(timings));
            true
        }
    };
    if fresh {
        Ok(try_restore_draft(service, session_id, reader, root))
    } else {
        Ok(DraftRestore::Missing)
    }
}

/// 首建时间轴后尝试恢复草稿（ensure_timeline 调用；时间轴已在 map 中）。
fn try_restore_draft(
    service: &Arc<GifService>,
    session_id: &str,
    reader: &SpoolReader,
    root: &Path,
) -> DraftRestore {
    let identity = (reader.meta.created_at, reader.frame_count());
    service
        .session_identity
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.to_string(), identity);

    let Some(raw) = infra_gif::spool::read_draft(root, session_id) else {
        return DraftRestore::Missing;
    };
    let restore = (|| -> Option<(TimelineDraft, u64)> {
        let file: SessionDraftFile = serde_json::from_str(&raw).ok()?;
        if !file.identity_matches(identity.0, identity.1) {
            return None;
        }
        let play = infra_gif::spool::read_position(&root.join("sessions").join(session_id))
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .filter(|v| {
                v["revision"].as_u64() == Some(file.draft.revision)
                    && v["createdAt"].as_u64() == Some(identity.0)
                    && v["frames"].as_u64() == Some(identity.1 as u64)
            })
            .and_then(|v| v["posMs"].as_u64())
            .unwrap_or(file.draft.play_pos_ms);
        Some((file.draft, play))
    })();
    let Some((draft, play_pos)) = restore else {
        tracing::info!(session_id, "编辑草稿不兼容/素材身份不匹配，已回退原始素材");
        return DraftRestore::Discarded;
    };
    // 锁序约定：timelines 单独持有并在写 drafts 前释放（save_now 为
    // drafts → identity → timelines，保持无环）。
    let saved_revision = draft.revision;
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
                saved_revision: Some(saved_revision),
            },
        );
    DraftRestore::Restored(play_pos)
}

/// Release inactive index mappings only after their edits reached disk. Failed
/// saves retain in-memory edits and their lease for a later retry.
pub(crate) fn release_if_saved(service: &GifService, session_id: &str) {
    let mut drafts = service.drafts.lock().unwrap_or_else(|e| e.into_inner());
    if drafts.get(session_id).is_some_and(|s| s.dirty) {
        return;
    }
    service
        .timelines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session_id);
    service
        .session_identity
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session_id);
    drafts.remove(session_id);
    service
        .readers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session_id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use blink_domain::gif::EditCommand;

    fn material(root: &Path) -> SpoolReader {
        let id = infra_gif::generate_session_id();
        let meta = infra_gif::SessionMeta {
            spool_version: infra_gif::SPOOL_VERSION,
            id: id.clone(),
            region: infra_gif::CaptureRegion {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            },
            fps: 10,
            show_cursor: false,
            created_at: 42,
            state: "recording".into(),
            ts_ms: vec![],
            last_dur_ms: 100,
            spool_bytes: 0,
            stop_reason: None,
        };
        let mut writer = infra_gif::SpoolWriter::create(root, meta).unwrap();
        writer.append_frame(0, &[0; 256]).unwrap();
        writer.append_frame(100, &[255; 256]).unwrap();
        writer.finalize("user").unwrap();
        SpoolReader::open(root, &id).unwrap()
    }

    #[test]
    fn new_recording_saves_identity_edits_and_position_then_restores() {
        let root = tempfile::tempdir().unwrap();
        let reader = material(root.path());
        let id = &reader.meta.id;
        let service = Arc::new(GifService::new());
        assert_eq!(
            ensure_timeline_at(&service, &reader, id, root.path()).unwrap(),
            DraftRestore::Missing
        );
        service
            .timelines
            .lock()
            .unwrap()
            .get_mut(id)
            .unwrap()
            .apply(&EditCommand::SetSpeed { factor: 2.0 })
            .unwrap();
        service.drafts.lock().unwrap().insert(
            id.clone(),
            DraftState {
                play_pos_ms: 75,
                dirty: true,
                generation: 1,
                saved_revision: None,
            },
        );
        save_to(&service, root.path(), id).unwrap();
        assert!(!service.drafts.lock().unwrap()[id].dirty);
        let raw = infra_gif::read_draft(root.path(), id).unwrap();
        let file: SessionDraftFile = serde_json::from_str(&raw).unwrap();
        assert!(file.identity_matches(42, 2));
        assert_eq!(file.draft.speed, 2.0);
        assert_eq!(file.draft.play_pos_ms, 75);

        let reopened = Arc::new(GifService::new());
        assert_eq!(
            ensure_timeline_at(&reopened, &reader, id, root.path()).unwrap(),
            DraftRestore::Restored(75)
        );
        assert_eq!(reopened.timelines.lock().unwrap()[id].speed(), 2.0);
        assert_eq!(
            ensure_timeline_at(&reopened, &reader, id, root.path()).unwrap(),
            DraftRestore::Missing
        );
        assert_eq!(reopened.drafts.lock().unwrap()[id].play_pos_ms, 75);
    }

    #[test]
    fn failed_save_keeps_dirty_and_deleted_session_is_not_recreated() {
        let root = tempfile::tempdir().unwrap();
        let reader = material(root.path());
        let id = reader.meta.id.clone();
        let service = Arc::new(GifService::new());
        ensure_timeline_at(&service, &reader, &id, root.path()).unwrap();
        service.drafts.lock().unwrap().insert(
            id.clone(),
            DraftState {
                dirty: true,
                ..Default::default()
            },
        );
        let dir = root.path().join("sessions").join(&id);
        std::fs::create_dir(dir.join("draft.json")).unwrap();
        assert!(save_to(&service, root.path(), &id).is_err());
        assert!(service.drafts.lock().unwrap()[&id].dirty);
        drop(reader);
        assert!(
            infra_gif::spool::discard_session(root.path(), &id).is_err(),
            "时间轴仍持有独立素材租约"
        );
        service.timelines.lock().unwrap().remove(&id);
        infra_gif::spool::discard_session(root.path(), &id).unwrap();
        assert!(save_to(&service, root.path(), &id).is_err());
        assert!(!dir.exists());
        assert!(!service.drafts.lock().unwrap().contains_key(&id));
    }
}
