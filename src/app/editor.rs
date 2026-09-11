//! EditorSessionService（0.23.1）——单活动编辑会话的应用层编排。
//!
//! **所有权**（phase 文档 §3.2）：后端拥有 session 身份、来源、来源 revision、
//! 窗口绑定和 generation；前端拥有工作文本、selection、undo/redo 和单调
//! `content_revision`。保存时前端提交完整 UTF-8 文本，本服务校验后执行目标副作用。
//!
//! **并发纪律**：`std::sync::Mutex` 只保护会话槽，guard 绝不跨 `.await`
//! （对齐 VoiceService 惯例）。异步副作用（便签写库、剪贴板写入）在锁外执行，
//! 完成后重新加锁并校验 `session_ref + generation` 未变才回写新基线——
//! 旧会话的迟到结果不能污染新会话。
//!
//! **调用窗口校验**（§3.10）：`get/commit/end` 只接受 `content-editor` 窗口调用；
//! `open` 任何窗口都可发起（主窗/便签/管理器/CLI 路径），会话固定绑定到
//! `content-editor`。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tauri::{Emitter, Manager};

use crate::domain::editor::{
    CommitEditorRequest, CommitOutcome, EditorError, EditorSessionSnapshot, EndEditorRequest,
    OpenDecision, OpenEditorRequest, SourceDescriptor, decide_open, validate_source_body,
};
use crate::domain::event::CapabilityEnv;
use crate::domain::sticky::StickyChangeSource;
use crate::domain::sticky::StickyError;

/// 编辑器窗口唯一 label（窗口、命令校验、事件 source 共用）。
pub const CONTENT_EDITOR_LABEL: &str = "content-editor";

/// 一个活动会话的后端状态。
struct LiveSession {
    session_ref: String,
    generation: u64,
    title: Option<String>,
    /// 会话绑定时后端记录的基线正文（便签来源为 DB 内容）。
    body: String,
    source: SourceDescriptor,
    /// 便签来源的冲突基线（打开时的 `updated_at`），随每次成功提交前移。
    source_revision: Option<i64>,
}

impl LiveSession {
    fn snapshot(&self) -> EditorSessionSnapshot {
        EditorSessionSnapshot {
            session_ref: self.session_ref.clone(),
            generation: self.generation,
            title: self.title.clone(),
            body: self.body.clone(),
            source: self.source.clone(),
            source_revision: self.source_revision,
            markdown_policy: crate::domain::editor::markdown_view_policy(&self.source),
        }
    }
}

/// 单活动 EditorSession 服务。由 main.rs manage，command 层经 State 取用。
pub struct EditorSessionService {
    app: tauri::AppHandle,
    active: Mutex<Option<LiveSession>>,
    generation_counter: AtomicU64,
}

impl EditorSessionService {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self {
            app,
            active: Mutex::new(None),
            generation_counter: AtomicU64::new(0),
        }
    }

    // ── open ────────────────────────────────────────────────────────────────

    /// 打开（绑定/激活/Busy）。返回快照给调用方窗口；Busy 时同时激活现有
    /// 编辑器窗口让用户先完成当前任务。
    pub async fn open(
        &self,
        request: OpenEditorRequest,
    ) -> Result<EditorSessionSnapshot, EditorError> {
        validate_source_body(&request.body)?;

        // 便签来源：正文与 revision 以 DB 为真源，不信任调用方传入的 body。
        let (body, source_revision) = self.load_sticky_authoritative(&request.source).await?;
        self.decide_and_apply(request.title, request.source, body, source_revision)
    }

    /// 同步打开（非便签来源）。GUI starter/Capability 路径使用——来源不含
    /// 便签（无需读 DB），决策与状态写入可同步完成。
    pub fn open_sync(
        &self,
        request: OpenEditorRequest,
    ) -> Result<EditorSessionSnapshot, EditorError> {
        validate_source_body(&request.body)?;
        if matches!(request.source, SourceDescriptor::Sticky { .. }) {
            return Err(EditorError::Unsupported {
                detail: "便签来源必须经 open() 从 DB 读取真源".into(),
            });
        }
        self.decide_and_apply(request.title, request.source, request.body, None)
    }

    /// 锁内决策 + 状态写入，锁外执行事件与窗口副作用。
    fn decide_and_apply(
        &self,
        title: Option<String>,
        source: SourceDescriptor,
        body: String,
        source_revision: Option<i64>,
    ) -> Result<EditorSessionSnapshot, EditorError> {
        enum OpenOutcome {
            Created(LiveSession),
            Activate(EditorSessionSnapshot),
            Busy(Option<String>),
        }

        // 锁内完成决策与状态写入（无 await）；副作用在锁外执行。
        let outcome = {
            let guard = self.lock()?;
            let active_key = guard
                .as_ref()
                .and_then(|s| s.source.persistence_key())
                .map(|k| k.to_string());
            match decide_open(active_key.as_deref(), &source) {
                OpenDecision::Create => {
                    let generation = self.generation_counter.fetch_add(1, Ordering::Relaxed) + 1;
                    OpenOutcome::Created(LiveSession {
                        session_ref: generate_session_ref(),
                        generation,
                        title,
                        body,
                        source,
                        source_revision,
                    })
                }
                OpenDecision::ActivateExisting => {
                    let existing = guard.as_ref().ok_or(EditorError::StaleSession)?;
                    OpenOutcome::Activate(existing.snapshot())
                }
                OpenDecision::Busy => {
                    OpenOutcome::Busy(guard.as_ref().and_then(|s| s.title.clone()))
                }
            }
        };

        match outcome {
            OpenOutcome::Created(session) => {
                let snapshot = session.snapshot();
                let sticky_id = session.source.sticky_id().map(str::to_string);
                *self.lock()? = Some(session);
                self.emit_session_changed(
                    "bound",
                    Some(&snapshot.session_ref),
                    Some(snapshot.generation),
                    sticky_id.as_deref(),
                );
                self.show_window()?;
                tracing::info!(
                    session_ref = %snapshot.session_ref,
                    generation = snapshot.generation,
                    body_chars = snapshot.body.chars().count(),
                    "editor session: 已绑定"
                );
                Ok(snapshot)
            }
            OpenOutcome::Activate(snapshot) => {
                self.show_window()?;
                tracing::debug!(
                    session_ref = %snapshot.session_ref,
                    "editor session: 同源请求，激活现有窗口"
                );
                Ok(snapshot)
            }
            OpenOutcome::Busy(active_title) => {
                self.show_window()?;
                tracing::info!(
                    active_title = ?active_title,
                    "editor session: 异源请求返回 EditorBusy，已激活现有任务"
                );
                Err(EditorError::EditorBusy { active_title })
            }
        }
    }

    /// 前端绑定时拉取只读快照；无活动会话返回 None（预热窗口 init 时）。
    pub fn current_snapshot(
        &self,
        caller_label: &str,
    ) -> Result<Option<EditorSessionSnapshot>, EditorError> {
        self.ensure_editor_window(caller_label)?;
        Ok(self.lock()?.as_ref().map(LiveSession::snapshot))
    }

    // ── commit ──────────────────────────────────────────────────────────────

    /// 按来源推导的保存目标提交正文（0.23.1 默认映射：便签→原位更新，
    /// 其余→剪贴板结果）。
    pub async fn commit(
        &self,
        caller_label: &str,
        request: CommitEditorRequest,
    ) -> Result<CommitOutcome, EditorError> {
        self.ensure_editor_window(caller_label)?;
        validate_source_body(&request.body)?;

        // 锁内校验会话身份并取出来源快照。
        let (session_ref, generation, source, source_revision) = {
            let guard = self.lock()?;
            let live = guard.as_ref().ok_or(EditorError::StaleSession)?;
            if live.session_ref != request.session_ref || live.generation != request.generation {
                tracing::warn!(
                    session_ref = %request.session_ref,
                    generation = request.generation,
                    "editor session commit: 会话身份不匹配，拒绝提交"
                );
                return Err(EditorError::StaleSession);
            }
            (
                live.session_ref.clone(),
                live.generation,
                live.source.clone(),
                live.source_revision,
            )
        };

        match &source {
            SourceDescriptor::Sticky { sticky_id } => {
                let env = self.domain_env()?;
                let new_revision = env
                    .update_sticky_content_and_notify(
                        sticky_id,
                        &request.body,
                        source_revision,
                        StickyChangeSource::ContentEditor,
                    )
                    .await
                    .map_err(|e| map_sticky_workflow_error(e, sticky_id))?;

                // 副作用在锁外完成后回写基线；会话已被替换/结束时视为迟到结果。
                let mut guard = self.lock()?;
                match guard.as_mut() {
                    Some(live)
                        if live.session_ref == session_ref && live.generation == generation =>
                    {
                        live.source_revision = Some(new_revision);
                        live.body = request.body.clone();
                        drop(guard);
                        tracing::info!(
                            session_ref = %session_ref,
                            sticky_id = %sticky_id,
                            revision = new_revision,
                            "editor session commit: 便签原位更新完成"
                        );
                        Ok(CommitOutcome {
                            source_revision: Some(new_revision),
                        })
                    }
                    _ => {
                        tracing::warn!(
                            session_ref = %session_ref,
                            "editor session commit: 保存成功但会话已结束，丢弃新基线"
                        );
                        Err(EditorError::StaleSession)
                    }
                }
            }
            _ => {
                // 剪贴板结果：继承命中数新建历史项 + 写回系统剪贴板
                //（0.23.2 起改为“首次创建、后续更新同一结果项”）。
                let item_ref = match &source {
                    SourceDescriptor::ClipboardItem { item_ref } => item_ref.clone(),
                    _ => None,
                };
                let new_id = self
                    .commit_clipboard_result(&request.body, item_ref.as_deref())
                    .await?;
                // 基线前移（失败不回滚：文本已是事实）。
                let mut guard = self.lock()?;
                if let Some(live) = guard.as_mut() {
                    if live.session_ref == session_ref && live.generation == generation {
                        live.body = request.body.clone();
                    }
                }
                drop(guard);
                tracing::info!(
                    session_ref = %session_ref,
                    new_id = %new_id,
                    "editor session commit: 剪贴板结果完成"
                );
                Ok(CommitOutcome {
                    source_revision: None,
                })
            }
        }
    }

    // ── end ─────────────────────────────────────────────────────────────────

    /// 结束会话并释放便签租约；窗口由前端负责隐藏与完整 reset。
    pub fn end(&self, caller_label: &str, request: EndEditorRequest) -> Result<(), EditorError> {
        self.ensure_editor_window(caller_label)?;
        let ended = {
            let mut guard = self.lock()?;
            match guard.as_ref() {
                Some(live)
                    if live.session_ref == request.session_ref
                        && live.generation == request.generation =>
                {
                    guard.take()
                }
                _ => return Err(EditorError::StaleSession),
            }
        };
        let ended = ended.expect("上方 match 已确认 Some");
        let sticky_id = ended.source.sticky_id().map(str::to_string);
        // 携带 session_ref：前端据此判别迟到事件，不误清新会话
        self.emit_session_changed(
            "ended",
            Some(&ended.session_ref),
            Some(ended.generation),
            sticky_id.as_deref(),
        );
        tracing::info!(
            session_ref = %ended.session_ref,
            reason = ?request.reason,
            sticky_released = sticky_id.is_some(),
            "editor session: 已结束"
        );
        Ok(())
    }

    // ── 内部工具 ────────────────────────────────────────────────────────────

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<LiveSession>>, EditorError> {
        self.active.lock().map_err(|e| {
            tracing::error!(error = %e, "editor session: 会话槽锁失败");
            EditorError::Io {
                detail: format!("会话状态锁失败: {e}"),
            }
        })
    }

    fn ensure_editor_window(&self, caller_label: &str) -> Result<(), EditorError> {
        if caller_label == CONTENT_EDITOR_LABEL {
            Ok(())
        } else {
            tracing::warn!(
                caller = %caller_label,
                "editor session: 非 content-editor 窗口调用被拒绝"
            );
            Err(EditorError::Unsupported {
                detail: format!("该命令只能由 {CONTENT_EDITOR_LABEL} 窗口调用"),
            })
        }
    }

    fn domain_env(
        &self,
    ) -> Result<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>, EditorError> {
        Ok(self
            .app
            .try_state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
            .map(|s| s.inner().clone())
            .ok_or_else(|| EditorError::Io {
                detail: "DomainEnv 不可用".into(),
            })?)
    }

    fn history_pool(&self) -> Result<sqlx::SqlitePool, EditorError> {
        Ok(self
            .app
            .try_state::<crate::infra::data::DbPools>()
            .map(|pools| pools.history.clone())
            .ok_or_else(|| EditorError::Io {
                detail: "history 连接池不可用".into(),
            })?)
    }

    /// 便签来源打开时读取 DB 真源（正文 + revision）。
    async fn load_sticky_authoritative(
        &self,
        source: &SourceDescriptor,
    ) -> Result<(String, Option<i64>), EditorError> {
        let Some(sticky_id) = source.sticky_id().map(str::to_string) else {
            return Ok((String::new(), None));
        };
        let pool = self.history_pool()?;
        let note = crate::infra::data::sticky::get_result(&pool, &sticky_id)
            .await
            .map_err(|e| EditorError::Io { detail: e })?
            .ok_or_else(|| EditorError::TargetUnavailable {
                detail: format!("便签不存在或已删除: {sticky_id}"),
            })?;
        if note.trashed {
            return Err(EditorError::TargetUnavailable {
                detail: format!("便签已在回收站: {sticky_id}"),
            });
        }
        Ok((note.content, Some(note.updated_at)))
    }

    /// 剪贴板结果提交：新建历史项（继承 hit_count）+ 写回系统剪贴板。
    async fn commit_clipboard_result(
        &self,
        body: &str,
        item_ref: Option<&str>,
    ) -> Result<String, EditorError> {
        let pool = self.history_pool()?;

        let hit_count = match item_ref {
            Some(id) => match crate::infra::data::clipboard::query_by_id(&pool, id).await {
                Some(item) => item.hit_count,
                None => {
                    tracing::warn!(origin_ref = %id, "原剪贴板记录不存在，hit_count 从 0 开始");
                    0
                }
            },
            None => 0,
        };

        let new_item = crate::infra::data::clipboard::ClipboardItem {
            id: crate::infra::data::clipboard::generate_id(),
            text: body.to_string(),
            preview: crate::infra::data::clipboard::make_preview(body),
            created_at: chrono::Utc::now().timestamp(),
            source_app: None,
            hit_count,
        };

        crate::infra::data::clipboard::save_item(&pool, &new_item)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "editor session commit: 保存剪贴板记录失败");
                EditorError::Io {
                    detail: format!("保存失败: {e}"),
                }
            })?;

        crate::app::commands::copy_to_clipboard(body.to_string())
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "editor session commit: 写回系统剪贴板失败");
                EditorError::TargetUnavailable {
                    detail: format!("写回剪贴板失败: {e}"),
                }
            })?;

        Ok(new_item.id)
    }

    fn show_window(&self) -> Result<(), EditorError> {
        crate::infra::platform::window::show_content_editor_window(&self.app).map_err(|e| {
            EditorError::Io {
                detail: format!("显示编辑器窗口失败: {e}"),
            }
        })
    }

    fn emit_session_changed(
        &self,
        kind: &str,
        session_ref: Option<&str>,
        generation: Option<u64>,
        sticky_id: Option<&str>,
    ) {
        let payload = serde_json::json!({
            "kind": kind,
            "sessionRef": session_ref,
            "generation": generation,
            "stickyId": sticky_id,
        });
        if let Err(error) = self.app.emit(
            crate::infra::event_names::EventNames::EDITOR_SESSION_CHANGED,
            payload,
        ) {
            tracing::warn!(kind, %error, "editor session: 会话变更事件发送失败");
        }
    }
}

/// StickyWorkflowError → EditorError（编辑器侧语义投影）。
fn map_sticky_workflow_error(
    error: crate::domain::sticky::StickyWorkflowError,
    sticky_id: &str,
) -> EditorError {
    match error {
        crate::domain::sticky::StickyWorkflowError::Sticky(sticky_error) => match sticky_error {
            StickyError::Conflict {
                expected_updated_at,
                actual_updated_at,
                ..
            } => EditorError::SourceConflict {
                expected_updated_at,
                actual_updated_at,
            },
            StickyError::NotFound { .. } | StickyError::Trashed { .. } => {
                EditorError::TargetUnavailable {
                    detail: format!("便签不可写: {sticky_id}"),
                }
            }
            StickyError::Db { detail } => EditorError::Io { detail },
        },
        other @ crate::domain::sticky::StickyWorkflowError::SideEffect { .. } => EditorError::Io {
            detail: other.to_string(),
        },
    }
}

/// 生成不可猜测的会话引用：纳秒时间戳 + 进程内单调计数（本地单用户威胁模型）。
fn generate_session_ref() -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default() as u64;
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ed_{nanos:016x}{seq:04x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ref_format_is_opaque_and_unique() {
        let a = generate_session_ref();
        let b = generate_session_ref();
        assert_ne!(a, b);
        assert!(a.starts_with("ed_"));
    }

    #[test]
    fn sticky_workflow_error_maps_to_editor_error() {
        let conflict = map_sticky_workflow_error(
            crate::domain::sticky::StickyWorkflowError::Sticky(StickyError::Conflict {
                id: "s1".into(),
                expected_updated_at: 5,
                actual_updated_at: 6,
            }),
            "s1",
        );
        assert_eq!(
            conflict,
            EditorError::SourceConflict {
                expected_updated_at: 5,
                actual_updated_at: 6,
            }
        );

        let not_found = map_sticky_workflow_error(
            crate::domain::sticky::StickyWorkflowError::Sticky(StickyError::NotFound {
                id: "s1".into(),
            }),
            "s1",
        );
        assert!(matches!(not_found, EditorError::TargetUnavailable { .. }));
    }
}
