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
    CommitEditorRequest, CommitOutcome, CommitPlan, CommitState, CommitTarget, CommittedRevision,
    EditorError, EditorSessionSnapshot, EndEditorRequest, FileIdentity, OpenDecision,
    OpenEditorRequest, RevisionVerdict, SourceDescriptor, body_digest, check_commit_revision,
    decide_open, default_commit_target, plan_commit, validate_source_body,
};
use crate::domain::event::CapabilityEnv;
use crate::domain::sticky::StickyChangeSource;
use crate::domain::sticky::StickyError;

/// 编辑器窗口唯一 label（窗口、命令校验、事件 source 共用）。
pub const CONTENT_EDITOR_LABEL: &str = "content-editor";

/// 同会话变更串行门（0.23.6 §5.7 ④）：`commit` 的锁外副作用与 `end` 互斥，
/// 防止"end 清槽后 commit 副作用迟到写入"——用户放弃修改后不会再有该会话
/// 的便签、文件或剪贴板写入。tokio Mutex 的 guard 可跨 `.await` 持有
///（这正是本门的用途）；锁序恒为 gate → active 槽，无反向获取路径。
#[derive(Default)]
struct MutationGate(tokio::sync::Mutex<()>);

impl MutationGate {
    async fn acquire(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.0.lock().await
    }
}

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
    /// 主保存目标（§3.5）：绑定时按来源推导，"保存到…"成功后切换。
    commit_target: CommitTarget,
    /// 会话剪贴板结果项 id（首次保存创建后存在，后续保存更新同一项）。
    result_item_id: Option<String>,
    /// 已确认文件身份（写入成功后记录；原位保存前据此判外部修改）。
    file_identity: Option<FileIdentity>,
    /// 已接受的提交水位（0.23.6 §5.7 ⑤：revision + 正文摘要；None = 尚未提交）。
    committed: Option<CommittedRevision>,
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
            commit_target: self.commit_target.clone(),
        }
    }
}

/// 编辑器退出确认超时（§3.5）：前端无响应时放弃本次退出，可重试。
const EXIT_CONFIRM_TIMEOUT_SECS: u64 = 10;

/// 单活动 EditorSession 服务。由 main.rs manage，command 层经 State 取用。
pub struct EditorSessionService {
    app: tauri::AppHandle,
    active: Mutex<Option<LiveSession>>,
    generation_counter: AtomicU64,
    /// 待决退出确认请求 id（一次只挂一个；超时或应答后清除）。
    pending_exit: Mutex<Option<String>>,
    /// commit/end 同会话串行门（0.23.6 §5.7 ④）。
    mutation_gate: MutationGate,
}

impl EditorSessionService {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self {
            app,
            active: Mutex::new(None),
            generation_counter: AtomicU64::new(0),
            pending_exit: Mutex::new(None),
            mutation_gate: MutationGate::default(),
        }
    }

    // ── open ────────────────────────────────────────────────────────────────

    /// 打开（绑定/激活/Busy）。返回快照给调用方窗口；Busy 时同时激活现有
    /// 编辑器窗口让用户先完成当前任务。
    pub async fn open(
        &self,
        request: OpenEditorRequest,
    ) -> Result<EditorSessionSnapshot, EditorError> {
        // 便签来源：正文与 revision 以 DB 为真源，不信任调用方传入的 body；
        // 其他来源必须保留调用方正文。最终只校验实际将绑定的权威正文。
        let (body, source_revision) = if request.source.sticky_id().is_some() {
            self.load_sticky_authoritative(&request.source).await?
        } else {
            (request.body, None)
        };
        validate_source_body(&body)?;
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
            Created(EditorSessionSnapshot, Option<String>),
            Activate(EditorSessionSnapshot),
            Busy(Option<String>),
        }

        // 锁内完成决策与状态写入（无 await）；副作用在锁外执行。
        let outcome = {
            let mut guard = self.lock()?;
            let active_key = guard
                .as_ref()
                .and_then(|s| s.source.persistence_key())
                .map(|k| k.to_string());
            let commit_target = default_commit_target(&source);
            match decide_open(active_key.as_deref(), &source) {
                OpenDecision::Create => {
                    let generation = self.generation_counter.fetch_add(1, Ordering::Relaxed) + 1;
                    let session = LiveSession {
                        session_ref: generate_session_ref(),
                        generation,
                        title,
                        body,
                        source,
                        source_revision,
                        commit_target,
                        result_item_id: None,
                        file_identity: None,
                        committed: None,
                    };
                    let snapshot = session.snapshot();
                    let sticky_id = session.source.sticky_id().map(str::to_string);
                    // 决策与写槽在同一个 guard 内完成：并发 open 只有一个创建者，
                    // 其余请求必然观察到活动槽并走 Activate/Busy。
                    *guard = Some(session);
                    OpenOutcome::Created(snapshot, sticky_id)
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
            OpenOutcome::Created(snapshot, sticky_id) => {
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

    /// 校验会话身份是否仍然活动且匹配（0.23.3：start_editor_voice 冻结身份用）。
    ///
    /// 只读、不产生副作用；锁 poisoned 时保守返回 false（拒绝启动听写）。
    pub fn verify_active_session(&self, session_ref: &str, generation: u64) -> bool {
        match self.lock() {
            Ok(guard) => guard.as_ref().is_some_and(|live| {
                live.session_ref == session_ref && live.generation == generation
            }),
            Err(_) => false,
        }
    }

    // ── commit ──────────────────────────────────────────────────────────────

    /// 按保存目标提交正文（§3.5）。
    ///
    /// 锁内取会话状态 → `plan_commit` 纯决策 → 锁外执行副作用 → 锁内按
    /// `session_ref + generation` 复查后落新基线。保存失败/冲突/取消不改变
    /// 正文基线、结果项、文件身份或主目标。
    pub async fn commit(
        &self,
        caller_label: &str,
        request: CommitEditorRequest,
    ) -> Result<CommitOutcome, EditorError> {
        self.ensure_editor_window(caller_label)?;
        validate_source_body(&request.body)?;

        // ④ 同会话 mutation gate（§5.7）：gate 持有覆盖"取状态 → 锁外副作用
        // → 复查落槽"全程，end 在此期间排队等待。
        let _gate = self.mutation_gate.acquire().await;

        // 锁内校验会话身份并取出状态快照（拷贝出锁，借用不跨块）。
        let (
            session_ref,
            generation,
            state_target,
            state_result_item,
            state_file_identity,
            state_sticky_revision,
            state_source_item_ref,
            state_committed,
        ) = {
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
            let source_item_ref = match &live.source {
                SourceDescriptor::ClipboardItem { item_ref } => item_ref.clone(),
                _ => None,
            };
            (
                live.session_ref.clone(),
                live.generation,
                live.commit_target.clone(),
                live.result_item_id.clone(),
                live.file_identity,
                live.source_revision,
                source_item_ref,
                live.committed.clone(),
            )
        };

        // ⑤ 提交协议校验（§5.7）：副作用前完成——旧 revision 与"同 revision、
        // 不同正文"的重放返回 StaleRevision；同 revision 同正文仅在"同一
        // mutation 的精确重放"（mutation id 一致）时幂等跳过副作用，新意图
        // （切换目标/保存副本/覆盖/重新复制）必须真实执行（二次 Review）。
        let body_hash = body_digest(&request.body);
        match check_commit_revision(
            state_committed.clone(),
            request.revision,
            &request.body,
            request.mutation_id.as_deref(),
        ) {
            RevisionVerdict::Accept => {}
            RevisionVerdict::Idempotent => {
                tracing::info!(
                    session_ref = %session_ref,
                    revision = request.revision,
                    "editor session commit: 幂等提交（同一 mutation 精确重放），跳过副作用"
                );
                return Ok(CommitOutcome {
                    source_revision: state_sticky_revision,
                    commit_target: state_target,
                    file_identity: state_file_identity.map(Into::into),
                });
            }
            RevisionVerdict::StaleRevision => {
                tracing::warn!(
                    session_ref = %session_ref,
                    accepted = ?state_committed.map(|c| c.revision),
                    incoming = request.revision,
                    "editor session commit: revision 过期或重放，拒绝提交"
                );
                return Err(EditorError::StaleRevision);
            }
        }

        let plan_state = CommitState {
            target: &state_target,
            result_item_id: state_result_item.as_deref(),
            file_identity: state_file_identity,
            sticky_revision: state_sticky_revision,
            source_item_ref: state_source_item_ref.as_deref(),
        };
        let plan = plan_commit(plan_state, request.target.as_ref());
        tracing::debug!(
            session_ref = %session_ref,
            plan = ?plan,
            "editor session commit: 执行计划"
        );

        // 锁外执行副作用；产出待落槽的新状态。
        let executed = self.execute_plan(&plan, &request.body).await?;

        // 副作用完成后复查会话身份，落新基线；旧会话迟到结果丢弃。
        let mut guard = self.lock()?;
        let live = match guard.as_mut() {
            Some(live) if live.session_ref == session_ref && live.generation == generation => live,
            _ => {
                tracing::warn!(
                    session_ref = %session_ref,
                    "editor session commit: 保存成功但会话已结束，丢弃新基线"
                );
                return Err(EditorError::StaleSession);
            }
        };
        if let Some(revision) = executed.new_sticky_revision {
            live.source_revision = Some(revision);
        }
        if let Some(item_id) = executed.result_item_id {
            live.result_item_id = Some(item_id);
        }
        if let Some(identity) = executed.file_identity {
            live.file_identity = Some(identity);
        }
        if let Some(target) = executed.switch_target.as_ref() {
            live.commit_target = target.clone();
        }
        live.body = request.body.clone();
        // ⑤ 提交水位与新 baseline 同一 guard 内原子前移（§5.7）。
        live.committed = Some(CommittedRevision {
            revision: request.revision,
            body_hash,
            mutation_id: request.mutation_id.clone(),
        });
        drop(guard);

        let target = executed.switch_target.unwrap_or(state_target);
        tracing::info!(
            session_ref = %session_ref,
            target = ?target,
            "editor session commit: 完成"
        );
        Ok(CommitOutcome {
            source_revision: executed.new_sticky_revision,
            commit_target: target,
            file_identity: executed.file_identity.map(Into::into),
        })
    }

    /// 执行单条提交计划（锁外副作用）。文件写入统一经
    /// [`Self::write_text_file_target`] 原语——0.23.2 起唯一文本文件输出。
    async fn execute_plan(
        &self,
        plan: &CommitPlan,
        body: &str,
    ) -> Result<ExecutedCommit, EditorError> {
        match plan {
            CommitPlan::UpdateSticky {
                sticky_id,
                expected_revision,
            } => {
                let env = self.domain_env()?;
                let new_revision = env
                    .update_sticky_content_and_notify(
                        sticky_id,
                        body,
                        *expected_revision,
                        StickyChangeSource::ContentEditor,
                    )
                    .await
                    .map_err(|e| map_sticky_workflow_error(e, sticky_id))?;
                tracing::info!(
                    sticky_id = %sticky_id,
                    revision = new_revision,
                    "editor session commit: 便签原位更新完成"
                );
                Ok(ExecutedCommit {
                    new_sticky_revision: Some(new_revision),
                    ..ExecutedCommit::default()
                })
            }
            CommitPlan::CreateClipboardResult { origin_ref } => {
                let item_id = self
                    .create_clipboard_result(body, origin_ref.as_deref())
                    .await?;
                Ok(ExecutedCommit {
                    result_item_id: Some(item_id),
                    ..ExecutedCommit::default()
                })
            }
            CommitPlan::UpdateClipboardResult { result_item_id } => {
                self.update_clipboard_result(result_item_id, body).await?;
                Ok(ExecutedCommit {
                    result_item_id: Some(result_item_id.clone()),
                    ..ExecutedCommit::default()
                })
            }
            CommitPlan::WriteConfirmedFile { path, identity } => {
                let new_identity = self
                    .write_text_file_target(path, body, Some(*identity))
                    .await?;
                Ok(ExecutedCommit {
                    file_identity: Some(new_identity),
                    ..ExecutedCommit::default()
                })
            }
            CommitPlan::OverwriteConfirmedFile { path } => {
                let new_identity = self.write_text_file_target(path, body, None).await?;
                Ok(ExecutedCommit {
                    file_identity: Some(new_identity),
                    ..ExecutedCommit::default()
                })
            }
            CommitPlan::SaveToFile { path } => {
                let new_identity = self.write_text_file_target(path, body, None).await?;
                Ok(ExecutedCommit {
                    file_identity: Some(new_identity),
                    switch_target: Some(CommitTarget::ConfirmedFile { path: path.clone() }),
                    ..ExecutedCommit::default()
                })
            }
            CommitPlan::SaveCopyToFile { path } => {
                // 副本写入不记录 identity、不切换主目标（§3.5）。
                self.write_text_file_target(path, body, None).await?;
                Ok(ExecutedCommit::default())
            }
            CommitPlan::ReturnToCaller => Err(EditorError::TargetUnavailable {
                detail: "调用方回写端点不可用（0.23 无可靠 endpoint 调用方）".into(),
            }),
        }
    }

    /// 剪贴板结果首存：新建历史项（继承原条目命中数）+ 写回系统剪贴板。
    ///
    /// 写入带 `EditorResult` 自写标记——监听器跳过持久化，结果项由本服务
    /// 显式入库，同一会话不堆积历史（§3.10）。
    ///
    /// 顺序：先剪贴板后 DB——剪贴板失败则整体未发生（无残留）；DB 失败时
    /// 剪贴板已有内容（用户可粘贴），重试保存自然重建结果项。
    async fn create_clipboard_result(
        &self,
        body: &str,
        item_ref: Option<&str>,
    ) -> Result<String, EditorError> {
        let pool = self.history_pool()?;

        write_clipboard_suppressed(body).await?;

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

        tracing::info!(result_item_id = %new_item.id, "editor session commit: 剪贴板结果项已创建");
        Ok(new_item.id)
    }

    /// 剪贴板结果后续保存：按原 id 原位更新同一项 + 写回系统剪贴板。
    async fn update_clipboard_result(
        &self,
        result_item_id: &str,
        body: &str,
    ) -> Result<(), EditorError> {
        let pool = self.history_pool()?;
        let updated = crate::infra::data::clipboard::update_item_text(&pool, result_item_id, body)
            .await
            .map_err(|e| EditorError::Io {
                detail: format!("更新结果项失败: {e}"),
            })?;
        if !updated {
            // 结果项被用户从历史中删除：以原 id 重建（hit_count 从 0），
            // 保证"同一会话始终对应同一结果项"的契约。
            tracing::info!(result_item_id = %result_item_id, "结果项已被删除，按原 id 重建");
            let item = crate::infra::data::clipboard::ClipboardItem {
                id: result_item_id.to_string(),
                text: body.to_string(),
                preview: crate::infra::data::clipboard::make_preview(body),
                created_at: chrono::Utc::now().timestamp(),
                source_app: None,
                hit_count: 0,
            };
            crate::infra::data::clipboard::save_item(&pool, &item)
                .await
                .map_err(|e| EditorError::Io {
                    detail: format!("重建结果项失败: {e}"),
                })?;
        }

        write_clipboard_suppressed(body).await?;
        Ok(())
    }

    /// 文件保存目标原语（§3.5：UTF-8、临时文件 + 原子替换、identity 冲突保护）。
    ///
    /// 统一走 `write_text_file` Capability（registry 唯一实现，§A3.5）；
    /// identity 检查在 Capability 内部的写入同闭包内完成，无 TOCTOU 窗口。
    /// 成功后读取新身份作为后续冲突基线；失败不产生任何落槽副作用。
    async fn write_text_file_target(
        &self,
        path: &str,
        body: &str,
        expected: Option<FileIdentity>,
    ) -> Result<FileIdentity, EditorError> {
        let env_arc = self.domain_env()?;
        let cap_reg = self
            .app
            .state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
        let ctx = crate::domain::capability::InvokeContext {
            env: env_arc.as_ref(),
            origin: crate::domain::capability::InvocationOrigin::LocalSurface,
            runtime: crate::domain::capability::RuntimeCapabilities {
                surface: Some(env_arc.as_ref()),
                main_process: true,
                desktop_session: true,
            },
            deadline: None,
        };
        let mut args = serde_json::json!({ "path": path, "content": body });
        if let Some(exp) = expected {
            args["expected_size"] = serde_json::json!(exp.size);
            args["expected_mtime_ms"] = serde_json::json!(exp.mtime_ms);
        }

        match cap_reg.invoke("write_text_file", args, &ctx).await {
            Ok(_) => {}
            Err(crate::domain::capability::CapabilityError::Conflict { .. }) => {
                // 冲突：读取磁盘现状构造结构化 SourceConflict（前端按 code 分类）。
                let actual = file_mtime_ms(path);
                let expected_updated_at = expected.map(|e| e.mtime_ms).unwrap_or(0);
                tracing::warn!(
                    path = %path,
                    expected = expected_updated_at,
                    actual = actual,
                    "editor session commit: 文件已被外部修改"
                );
                return Err(EditorError::SourceConflict {
                    expected_updated_at,
                    actual_updated_at: actual,
                });
            }
            Err(e) => return Err(map_capability_error(e)),
        }

        let path_buf = std::path::PathBuf::from(path);
        let (size, mtime_ms) =
            crate::infra::utils::fs::file_identity(&path_buf).ok_or_else(|| EditorError::Io {
                detail: format!("写入后读取文件身份失败: {path}"),
            })?;
        tracing::info!(path = %path, size, "editor session commit: 文件写入完成");
        Ok(FileIdentity { size, mtime_ms })
    }

    // ── end ─────────────────────────────────────────────────────────────────

    /// 结束会话并释放便签租约；窗口由前端负责隐藏与完整 reset。
    ///
    /// ④（§5.7）：先取得 mutation gate——进行中的 commit 副作用（含落槽）
    /// 完成后才清槽结束，保证"放弃修改"之后不再有该会话的迟到外部写入。
    pub async fn end(
        &self,
        caller_label: &str,
        request: EndEditorRequest,
    ) -> Result<(), EditorError> {
        self.ensure_editor_window(caller_label)?;
        let _gate = self.mutation_gate.acquire().await;
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

    // ── 用户退出确认（§3.5）───────────────────────────────────────────────

    /// 用户主动退出的统一入口（托盘退出 / `exit_blink` Capability）。
    ///
    /// 无活动会话立即退出；有活动会话时向 content-editor 发出一次汇总确认
    /// 请求（显示并聚焦编辑器窗口），由前端展示三态对话框后经
    /// [`Self::resolve_editor_exit`] 应答。超时视为放弃本次退出——
    /// 不静默丢稿。系统关机/强制退出走 `RunEvent::Exit`，不经此路径。
    pub fn request_user_exit(self: &std::sync::Arc<Self>) {
        let has_session = match self.lock() {
            Ok(guard) => guard.is_some(),
            Err(_) => true, // 状态不可读时保守视为有会话，走确认路径
        };
        if !has_session {
            tracing::info!("user exit: 无活动编辑会话，直接退出");
            self.app.exit(0);
            return;
        }

        let request_id = generate_exit_request_id();
        self.set_pending_exit(Some(request_id.clone()));

        // 确认对话框在编辑器窗口展示——先带到底前
        if let Err(e) = self.show_window() {
            tracing::warn!(error = %e, "user exit: 唤起编辑器窗口失败");
        }
        if let Some(win) = self.app.get_webview_window(CONTENT_EDITOR_LABEL)
            && let Err(e) = win.set_focus()
        {
            tracing::warn!(error = %e, "user exit: 聚焦编辑器窗口失败");
        }
        if let Err(e) = self.app.emit_to(
            CONTENT_EDITOR_LABEL,
            crate::infra::event_names::EventNames::EDITOR_EXIT_REQUEST,
            serde_json::json!({ "requestId": request_id }),
        ) {
            tracing::warn!(error = %e, "user exit: 退出确认事件发送失败");
        }
        tracing::info!(request_id = %request_id, "user exit: 已请求编辑器确认（存在活动会话）");

        // 超时守护：webview 无响应（异常/挂起）时放弃本次退出，不阻塞
        let service = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(EXIT_CONFIRM_TIMEOUT_SECS)).await;
            if service.take_pending_exit_if(&request_id) {
                tracing::warn!(
                    request_id = %request_id,
                    timeout_secs = EXIT_CONFIRM_TIMEOUT_SECS,
                    "user exit: 确认超时，放弃本次退出"
                );
            }
        });
    }

    /// 前端应答退出确认。只有与待决 id 匹配的应答生效；`confirmed=true`
    /// 时执行退出（`RunEvent::Exit` 收尾 flush）。返回应答是否被接受。
    pub fn resolve_editor_exit(&self, request_id: &str, confirmed: bool) -> bool {
        if !self.take_pending_exit_if(request_id) {
            tracing::debug!(request_id = %request_id, "user exit: 迟到或不匹配的应答，忽略");
            return false;
        }
        if confirmed {
            tracing::info!("user exit: 用户确认退出");
            self.app.exit(0);
        } else {
            tracing::info!("user exit: 用户取消退出");
        }
        true
    }

    fn set_pending_exit(&self, value: Option<String>) {
        match self.pending_exit.lock() {
            Ok(mut guard) => *guard = value,
            Err(poisoned) => *poisoned.into_inner() = value,
        }
    }

    /// 取出待决请求（仅当 id 匹配）；返回是否命中。
    fn take_pending_exit_if(&self, request_id: &str) -> bool {
        match self.pending_exit.lock() {
            Ok(mut guard) => {
                guard.as_deref() == Some(request_id) && {
                    *guard = None;
                    true
                }
            }
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                let hit = guard.as_deref() == Some(request_id);
                if hit {
                    *guard = None;
                }
                hit
            }
        }
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

/// 一次提交计划执行后的状态增量（只在副作用全部成功后由调用方落槽）。
#[derive(Debug, Default)]
struct ExecutedCommit {
    /// 便签写入后的新 revision。
    new_sticky_revision: Option<i64>,
    /// 剪贴板结果项 id（首存后存在；更新时回传确认）。
    result_item_id: Option<String>,
    /// 文件写入后的新身份。
    file_identity: Option<FileIdentity>,
    /// 主目标切换（仅"保存到…"成功后发生）。
    switch_target: Option<CommitTarget>,
}

/// 编辑器结果回写剪贴板（带 `EditorResult` 自写标记，监听器不重复采集）。
async fn write_clipboard_suppressed(body: &str) -> Result<(), EditorError> {
    crate::domain::clipboard::write_text(
        body.to_string(),
        crate::domain::clipboard::ClipboardWriteSource::EditorResult,
    )
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "editor session commit: 写回系统剪贴板失败");
        EditorError::TargetUnavailable {
            detail: format!("写回剪贴板失败: {e}"),
        }
    })
}

/// 读取文件 mtime（ms）；不可读时返回 0（仅用于冲突详情展示）。
fn file_mtime_ms(path: &str) -> i64 {
    crate::infra::utils::fs::file_identity(std::path::Path::new(path))
        .map(|(_, m)| m)
        .unwrap_or(0)
}

/// CapabilityError → EditorError（编辑器侧语义投影）。
fn map_capability_error(error: crate::domain::capability::CapabilityError) -> EditorError {
    use crate::domain::capability::CapabilityError as CapErr;
    match error {
        // 冲突的正常路径已在 write_text_file_target 特判（带结构化时间戳）；
        // 此处为兜底投影。
        CapErr::Conflict { .. } => EditorError::SourceConflict {
            expected_updated_at: 0,
            actual_updated_at: 0,
        },
        CapErr::InvalidArgs { detail } | CapErr::InvalidState { detail } => {
            EditorError::Unsupported { detail }
        }
        CapErr::Permission { detail } => EditorError::TargetUnavailable { detail },
        other => EditorError::Io {
            detail: other.to_string(),
        },
    }
}

/// 生成不可猜测的退出确认请求 id（同 session_ref 生成策略）。
fn generate_exit_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default() as u64;
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ex_{nanos:016x}{seq:04x}")
}

/// 用户主动退出统一入口（自由函数包装：托盘退出与 `exit_app` SurfacePort
/// 都从 `AppHandle` 进入；服务未初始化时直接退出兜底）。
pub fn request_user_exit(app: &tauri::AppHandle) {
    match app.try_state::<std::sync::Arc<EditorSessionService>>() {
        Some(service) => service.request_user_exit(),
        None => {
            tracing::warn!("user exit: 编辑器会话服务不可用，跳过确认直接退出");
            app.exit(0);
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

    #[tokio::test]
    async fn mutation_gate_serializes_concurrent_access() {
        // ④（§5.7）：第二个获取者必须等第一个释放后才进入——end 与 commit
        // 副作用互斥的结构保证。
        let gate = std::sync::Arc::new(MutationGate::default());
        let first = gate.acquire().await;

        let g = std::sync::Arc::clone(&gate);
        let second = tokio::spawn(async move {
            let _guard = g.acquire().await;
            42u8
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!second.is_finished(), "gate 被持有时后来者必须等待");

        drop(first);
        assert_eq!(tokio::join!(second).0.unwrap(), 42);
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
