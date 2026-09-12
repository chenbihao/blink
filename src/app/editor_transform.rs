//! EditorTransformService（0.23.4）——编辑器 AI 整理的应用层编排。
//!
//! **语义**（phase 文档 §3.7 / §3.10）：
//! - 服从全进程 AI 单活跃：请求经 `ChatService::register_editor_transform`
//!   注册进同一 `RequestTracker`，主窗口/Chat/Editor 任意方向互斥；
//! - 无记忆、无工具、关闭 thinking（复用主窗口模型池实例——thinking 默认
//!   关闭补丁在 provider 构造期已合并）；模型复用主窗口当前选中
//!   （`resolve_current_entries(Ephemeral)`）；
//! - system instruction 与用户正文分消息（正文不进指令模板）；
//! - 只产候选：终稿经 `evaluate_transform_output` 判定后发事件，本服务
//!   **永不**触碰编辑器正文；确认替换在前端 Adapter 内发生；
//! - 取消路径唯一：新请求/视图切换/会话结束由前端显式调
//!   `cancel_editor_transform`，chat 侧 abort 不会误杀本服务任务；
//! - 日志只记 session/generation/request/revision/长度/耗时，不记正文与
//!   AI 输入输出（§3.8 隐私）。
//!
//! **并发纪律**：`std::sync::Mutex` 只保护活跃槽，guard 不跨 `.await`；
//! 任务收尾按 `request_id` 复查才清槽，迟到结果不污染新请求。

use std::sync::Mutex;

use tauri::{Emitter, Manager};
use tokio::sync::Notify;

use crate::app::editor::CONTENT_EDITOR_LABEL;
use crate::app::editor::EditorSessionService;
use crate::domain::ai::chat_service::{ChatService, ConversationKind};
use crate::domain::ai::message::{ChatMessage, CompletionRequest, Role};
use crate::domain::ai::registry::AIProviderRegistry;
use crate::domain::editor::{
    EditorError, EditorTransformError, TRANSFORM_SYSTEM_INSTRUCTION, TransformOutput,
    TransformScope, evaluate_transform_output, plan_transform,
};
use crate::infra::event_names::EventNames;

/// 整理请求硬超时（毫秒）。整理是用户显式发起的成段任务，输入门 24K token，
/// 主窗口搜索的 20s SLO 不适用；仍有限界防挂死。
const TRANSFORM_TIMEOUT_MS: u32 = 120_000;

/// 取消后等待任务收尾的上限（正常路径 notify 后任务立即退出）。
const CANCEL_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 一个运行中整理请求的槽位状态。
struct ActiveTransform {
    /// 全局协调器（RequestTracker）分配的请求 id。
    request_id: u64,
    session_ref: String,
    generation: u64,
    cancel: std::sync::Arc<Notify>,
}

pub struct EditorTransformService {
    app: tauri::AppHandle,
    chat: std::sync::Arc<ChatService>,
    /// 活跃请求槽（全进程同时最多一个运行中整理，与全局单活跃一致）。
    active: Mutex<Option<ActiveTransform>>,
}

impl EditorTransformService {
    pub fn new(app: tauri::AppHandle, chat: std::sync::Arc<ChatService>) -> Self {
        Self {
            app,
            chat,
            active: Mutex::new(None),
        }
    }

    /// 发起整理：校验会话 → 解析主窗口模型 → 输入门/预算 → 注册全局单活跃
    /// → 后台调用 provider → 完成事件（只产候选，不改正文）。
    ///
    /// 结构化错误经 command 层投影：`ai_already_active`（携带 activeWindow）、
    /// `unsupported`（AI 未配置/输入超限/上下文过小）、`stale_session`、
    /// `cancelled`。
    pub async fn start(
        &self,
        editor: &EditorSessionService,
        session_ref: String,
        generation: u64,
        scope: TransformScope,
        text: String,
        revision: u64,
        range_handle: String,
    ) -> Result<u64, EditorError> {
        if !editor.verify_active_session(&session_ref, generation) {
            return Err(EditorError::StaleSession);
        }
        if text.trim().is_empty() {
            return Err(EditorError::Unsupported {
                detail: "整理范围为空".into(),
            });
        }

        // 主窗口当前选中的模型（policy light/main/显式自选 + 失效回落链）。
        let entries = self
            .chat
            .resolve_current_entries(ConversationKind::Ephemeral)
            .map_err(|_| EditorError::Unsupported {
                detail: "AI 未配置，请先在设置中配置模型".into(),
            })?;
        let registry = self
            .app
            .try_state::<std::sync::Arc<AIProviderRegistry>>()
            .map(|r| r.inner().clone())
            .ok_or_else(|| EditorError::Unsupported {
                detail: "AI 未配置，请先在设置中配置模型".into(),
            })?;
        let provider = registry
            .resolve_explicit(&entries.provider.id, &entries.model.id)
            .map_err(|_| EditorError::Unsupported {
                detail: "AI 未配置，请先在设置中配置模型".into(),
            })?;

        // 输入门 + max_tokens 推导（§3.10 冻结公式；超限不截断发送）。
        let plan = plan_transform(text, entries.model.context_window).map_err(map_plan_error)?;

        // 活跃槽互斥（同服务不并发整理）；guard 不跨 await——预检 → 全局注册
        // → 复查落槽（竞争时回滚全局槽位）。
        let cancel = std::sync::Arc::new(Notify::new());
        {
            let guard = self.lock()?;
            if guard.is_some() {
                // 前端契约是"先取消旧请求再发新请求"；残留槽位视为竞争，拒绝。
                return Err(EditorError::Cancelled);
            }
        }
        let request_id = self
            .chat
            .register_editor_transform()
            .await
            .map_err(|active| EditorError::AiAlreadyActive {
                active_window: active.target_window,
            })?;
        {
            let mut guard = self.lock()?;
            if guard.is_some() {
                // 预检与注册之间被并发 start 抢先：回滚全局槽位，拒绝本次。
                self.chat.release_editor_transform(request_id);
                return Err(EditorError::Cancelled);
            }
            *guard = Some(ActiveTransform {
                request_id,
                session_ref: session_ref.clone(),
                generation,
                cancel: cancel.clone(),
            });
        }

        tracing::info!(
            session_ref = %session_ref,
            generation,
            request_id,
            scope = scope.as_str(),
            revision,
            input_chars = plan.input_text.chars().count(),
            input_tokens = plan.input_tokens,
            max_tokens = plan.max_tokens,
            model = %entries.model.id,
            "editor transform: 请求已启动"
        );

        let app = self.app.clone();
        tokio::spawn(async move {
            let req = CompletionRequest {
                messages: vec![
                    ChatMessage {
                        role: Role::System,
                        content: TRANSFORM_SYSTEM_INSTRUCTION.to_string(),
                        tool_call_id: None,
                        tool_name: None,
                    },
                    ChatMessage {
                        role: Role::User,
                        content: plan.input_text,
                        tool_call_id: None,
                        tool_name: None,
                    },
                ],
                tools: Vec::new(),
                max_tokens: Some(plan.max_tokens),
                temperature: Some(plan.temperature),
                timeout_ms: Some(TRANSFORM_TIMEOUT_MS),
            };

            let started = std::time::Instant::now();
            // 取消路径唯一：select 在 provider await 点响应 notify。
            let result = tokio::select! {
                res = provider.complete(req) => res,
                _ = cancel.notified() => Err(crate::domain::ai::AIError::Cancelled),
            };
            let elapsed_ms = started.elapsed().as_millis() as u32;

            // (event, payload)：completed 无 code 字段，failed 带 code。
            let (event, payload) = match result {
                Ok(resp) => match evaluate_transform_output(resp.finish_reason, resp.text) {
                    TransformOutput::Applicable { text } => {
                        tracing::info!(
                            request_id,
                            elapsed_ms,
                            output_chars = text.chars().count(),
                            output_tokens = resp.usage.output_tokens,
                            finish = ?resp.finish_reason,
                            "editor transform: 完成（候选待确认）"
                        );
                        (
                            EventNames::EDITOR_TRANSFORM_COMPLETED,
                            serde_json::json!({
                                "sessionRef": session_ref,
                                "generation": generation,
                                "requestId": request_id,
                                "scope": scope.as_str(),
                                "revisedText": text,
                                "revision": revision,
                                "rangeHandle": range_handle,
                            }),
                        )
                    }
                    TransformOutput::Truncated { reason } => {
                        // §3.10：明确截断不生成可应用候选
                        tracing::warn!(
                            request_id,
                            reason,
                            elapsed_ms,
                            "editor transform: 输出被截断"
                        );
                        (
                            EventNames::EDITOR_TRANSFORM_FAILED,
                            failed_payload(&session_ref, generation, request_id, reason),
                        )
                    }
                    TransformOutput::Empty => {
                        tracing::warn!(request_id, elapsed_ms, "editor transform: 输出为空");
                        (
                            EventNames::EDITOR_TRANSFORM_FAILED,
                            failed_payload(&session_ref, generation, request_id, "empty"),
                        )
                    }
                },
                Err(crate::domain::ai::AIError::Cancelled) => {
                    tracing::info!(request_id, elapsed_ms, "editor transform: 已取消");
                    (
                        EventNames::EDITOR_TRANSFORM_FAILED,
                        failed_payload(&session_ref, generation, request_id, "cancelled"),
                    )
                }
                Err(crate::domain::ai::AIError::Timeout) => {
                    tracing::warn!(request_id, elapsed_ms, "editor transform: 超时");
                    (
                        EventNames::EDITOR_TRANSFORM_FAILED,
                        failed_payload(&session_ref, generation, request_id, "timeout"),
                    )
                }
                Err(crate::domain::ai::AIError::NotConfigured) => {
                    tracing::warn!(request_id, "editor transform: AI 未配置");
                    (
                        EventNames::EDITOR_TRANSFORM_FAILED,
                        failed_payload(&session_ref, generation, request_id, "not_configured"),
                    )
                }
                Err(e) => {
                    // provider/network/serialization——只记脱敏摘要，不记正文与输出
                    let code = match e {
                        crate::domain::ai::AIError::Network(_) => "network",
                        _ => "provider",
                    };
                    tracing::warn!(request_id, error = %e, "editor transform: 供应商调用失败");
                    (
                        EventNames::EDITOR_TRANSFORM_FAILED,
                        failed_payload(&session_ref, generation, request_id, code),
                    )
                }
            };

            // 事件只发编辑器窗口；迟到事件由前端按 requestId 过滤。
            if let Err(error) = app.emit_to(CONTENT_EDITOR_LABEL, event, payload) {
                tracing::warn!(request_id, %error, "editor transform: 事件发送失败");
            }

            // 全局槽位释放：先清 tracker（单活跃），再清本服务槽（按 id 复查）。
            if let Some(svc) = app.try_state::<std::sync::Arc<EditorTransformService>>() {
                svc.chat.release_editor_transform(request_id);
                if let Ok(mut guard) = svc.active.lock() {
                    if guard.as_ref().is_some_and(|a| a.request_id == request_id) {
                        *guard = None;
                    }
                }
            }
        });

        Ok(request_id)
    }

    /// 取消运行中整理（新请求/视图切换/会话结束/窗口关闭时由前端调用）。
    ///
    /// 与待决 request_id 不匹配的取消静默忽略；notify 后轮询等待任务清槽，
    /// 保证后续 start 注册全局槽位时不会撞上未释放的旧请求；超时兜底放行。
    pub async fn cancel(&self, request_id: u64) {
        let cancel = {
            let guard = match self.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            match guard.as_ref() {
                Some(active) if active.request_id == request_id => active.cancel.clone(),
                _ => return, // 不匹配的取消（迟到/重复）忽略
            }
        };
        cancel.notify_waiters();

        let deadline = std::time::Instant::now() + CANCEL_JOIN_TIMEOUT;
        while std::time::Instant::now() < deadline {
            {
                let guard = match self.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                if guard.as_ref().is_none_or(|a| a.request_id != request_id) {
                    return; // 任务已收尾清槽
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        tracing::warn!(
            request_id,
            "editor transform: 取消后槽位未及时释放（超时兜底放行）"
        );
    }

    /// 编辑器会话结束时清理悬空请求（防跨会话迟到事件/请求）。
    pub async fn cancel_active_for_session(&self, session_ref: &str, generation: u64) {
        let request_id = {
            let Ok(guard) = self.lock() else {
                return;
            };
            match guard.as_ref() {
                Some(active)
                    if active.session_ref == session_ref && active.generation == generation =>
                {
                    Some(active.request_id)
                }
                _ => None,
            }
        };
        if let Some(request_id) = request_id {
            self.cancel(request_id).await;
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<ActiveTransform>>, EditorError> {
        self.active.lock().map_err(|e| {
            tracing::error!(error = %e, "editor transform: 活跃槽锁失败");
            EditorError::Io {
                detail: format!("整理状态锁失败: {e}"),
            }
        })
    }
}

/// plan_transform 错误 → 结构化 EditorError（IPC 层按 code 分类展示）。
fn map_plan_error(error: EditorTransformError) -> EditorError {
    match error {
        EditorTransformError::InputTooLarge {
            input_tokens,
            max_tokens,
        } => EditorError::Unsupported {
            detail: format!(
                "文本超过整理输入上限（{input_tokens}/{max_tokens} token），请缩小范围"
            ),
        },
        EditorTransformError::ContextTooSmall { .. } => EditorError::Unsupported {
            detail: "模型上下文不足以整理该文本".into(),
        },
    }
}

/// 失败事件 payload（不含正文与 AI 输出，§3.8 隐私）。
fn failed_payload(
    session_ref: &str,
    generation: u64,
    request_id: u64,
    code: &str,
) -> serde_json::Value {
    serde_json::json!({
        "sessionRef": session_ref,
        "generation": generation,
        "requestId": request_id,
        "code": code,
    })
}
