//! MCP server 编排（0.13.4 / 0.19.13）——Blink 作为 MCP server 暴露能力给外部 client。
//!
//! ## 架构
//!
//! `BlinkMcpServer` 实现 rmcp 的 `ServerHandler` trait，处理外部 MCP client 的：
//! - `list_tools`——返回暴露的 Capability 列表（正向投影为 rmcp::model::Tool）
//! - `call_tool`——调用对应 Capability，结果投影为 MCP CallToolResult
//!
//! ## 授权
//!
//! - sensitive capability：不直接暴露给外部（安全优先），需用户在设置页显式启用
//! - 所有对外调用走审计日志（`ai_tool_audit`，`caller = mcp_external`）
//!
//! ## 传输（0.19.13）
//!
//! 主进程通过 Streamable HTTP transport 暴露，endpoint `http://127.0.0.1:{port}/mcp`。
//! 旧 stdio CLI 路径已收口为迁移错误。所有 HTTP session 共享同一份 `ExposureSnapshot`，
//! 修改暴露清单时立即重建快照并通知所有活跃 session 的 `tools/listChanged`。

use std::collections::HashSet;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use serde_json::Value;
use tokio::sync::{RwLock, watch};

use crate::domain::capability::{CapabilityRegistry, CapabilityResult, InvokeContext};
use crate::domain::event::{CapabilityEnv, EventPort};
use crate::domain::mcp::projection::capability_schemas_to_mcp_tools;
use crate::domain::mcp::server_config::McpServerModeConfig;
use crate::infra::data::ai_audit;

// ── ExposureSnapshot ─────────────────────────────────────────────────────────

/// 所有 HTTP session 共享的暴露快照（0.19.13）。
///
/// - `generation`：单调递增版本号，每次重建 +1，用于驱动 `tools/listChanged` 通知。
/// - `allowed`：当前允许暴露的 Capability id 集合（call-time 二次门禁用）。
/// - `tools`：按 capability name 排序的 rmcp Tool 列表（list_tools 用）。
///
/// 修改暴露清单时重建整个快照并递增 generation；
/// `list_tools` / `get_tool` / `call_tool` 都读取当前快照，
/// 已撤销暴露的工具即使旧 session 曾经获取过也会被 call-time 门禁拒绝。
///
/// **0.21.5**：build 时同时过滤 policy——Dangerous 和 `mcp_default == Forbidden`
/// 的 Capability 即使被用户加入 `exposed_capabilities` 也不进入 MCP 暴露清单。
/// call-time 再检查 policy + exposed list + runtime，撤销后旧 session 调用失败。
pub struct ExposureSnapshot {
    pub generation: u64,
    pub allowed: HashSet<String>,
    pub tools: Vec<Tool>,
}

impl ExposureSnapshot {
    /// 空快照（初始状态，不暴露任何能力）。
    fn empty() -> Self {
        Self {
            generation: 0,
            allowed: HashSet::new(),
            tools: Vec::new(),
        }
    }

    /// 从 CapabilityRegistry 和配置构建快照。
    /// tools 按 capability name 排序，保证不同 session list_tools 结果一致。
    ///
    /// **0.21.5**：build 时同时过滤 policy：
    /// - `Dangerous` 的 Capability 永远拒绝（§3.5）
    /// - `mcp_default == Forbidden` 的 Capability 永远拒绝（GUI starter / local-only）
    /// - 用户配置的 `exposed_capabilities` 是授权子集，但不能绕过代码级禁止
    fn build(cap_registry: &CapabilityRegistry, config: &McpServerModeConfig) -> Self {
        use crate::domain::capability::{DangerClass, McpDefault};

        let user_exposed: HashSet<String> = config.exposed_capabilities.iter().cloned().collect();

        // 获取所有 Capability 的 (id, schema, policy)，过滤出允许 MCP 的
        let mut entries: Vec<_> = cap_registry
            .entries()
            .into_iter()
            .filter_map(|(id, cap)| {
                // 只处理用户显式暴露的
                if !user_exposed.contains(&id) {
                    return None;
                }

                let policy = cap.policy();

                // 0.21.5 §3.5: Dangerous 对 MCP 永远拒绝
                if policy.danger == DangerClass::Dangerous {
                    tracing::warn!(
                        capability = %id,
                        "MCP ExposureSnapshot: Dangerous capability 被用户暴露但代码级禁止，已过滤"
                    );
                    return None;
                }

                // 0.21.5 §3.5: mcp_default == Forbidden 的永远拒绝
                if policy.mcp_default == McpDefault::Forbidden {
                    tracing::warn!(
                        capability = %id,
                        "MCP ExposureSnapshot: Forbidden capability 被用户暴露但代码级禁止，已过滤"
                    );
                    return None;
                }

                Some((id, cap.schema()))
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        // allowed 只包含通过 policy 过滤的 id
        let allowed: HashSet<String> = entries.iter().map(|(id, _)| id.clone()).collect();
        let schemas: Vec<_> = entries.into_iter().map(|(_, s)| s).collect();
        let tools = capability_schemas_to_mcp_tools(&schemas);

        Self {
            generation: 0,
            allowed,
            tools,
        }
    }
}

/// 共享暴露状态——所有 HTTP session 通过 `Arc<RwLock<ExposureSnapshot>>` 读取。
///
/// 外层 `Arc<RwLock<>>` 允许 runtime 在修改暴露清单时原子替换快照；
/// 内层 `watch::Sender<u64>` 用于通知 session worker generation 变化。
pub struct SharedExposure {
    snapshot: Arc<RwLock<ExposureSnapshot>>,
    generation_tx: watch::Sender<u64>,
}

impl SharedExposure {
    /// 创建初始空快照。
    pub fn new() -> Self {
        let snapshot = Arc::new(RwLock::new(ExposureSnapshot::empty()));
        let (generation_tx, _) = watch::channel(0u64);
        Self {
            snapshot,
            generation_tx,
        }
    }

    /// 从 CapabilityRegistry 和配置构建并安装新快照，递增 generation。
    /// 返回新 generation 值（调用方据此判断是否需要通知 session）。
    pub async fn rebuild(
        &self,
        cap_registry: &CapabilityRegistry,
        config: &McpServerModeConfig,
    ) -> u64 {
        let mut new_snapshot = ExposureSnapshot::build(cap_registry, config);
        let old_generation = {
            let current = self.snapshot.read().await;
            current.generation
        };
        new_snapshot.generation = old_generation + 1;

        tracing::info!(
            generation = new_snapshot.generation,
            tool_count = new_snapshot.tools.len(),
            "ExposureSnapshot: 已重建"
        );

        *self.snapshot.write().await = new_snapshot;

        let new_gen = old_generation + 1;
        let _ = self.generation_tx.send(new_gen);
        new_gen
    }

    /// 读取当前快照（list_tools / get_tool 用）。
    pub async fn read(&self) -> tokio::sync::RwLockReadGuard<'_, ExposureSnapshot> {
        self.snapshot.read().await
    }

    /// 订阅 generation 变化通知（session on_initialized 用）。
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.generation_tx.subscribe()
    }

    /// 获取当前 generation（只读，不持有锁）。
    #[allow(dead_code)]
    pub async fn current_generation(&self) -> u64 {
        self.snapshot.read().await.generation
    }
}

impl Default for SharedExposure {
    fn default() -> Self {
        Self::new()
    }
}

// ── BlinkMcpServer ───────────────────────────────────────────────────────────

/// Blink MCP server——实现 rmcp ServerHandler，暴露 Capability 给外部 client。
///
/// 持有 CapabilityRegistry + EventPort + CapabilityEnv + AI DB pool + 共享暴露快照。
/// `list_tools` / `call_tool` 读取共享快照，call-time 二次校验 allowed 集合。
///
/// **0.19.13**：config / cached_tools 替换为 `Arc<SharedExposure>`，
/// 所有 HTTP session 共享同一份快照，修改暴露清单不重启 listener。
pub struct BlinkMcpServer {
    /// 能力注册表——call_tool 的数据源。
    cap_registry: Arc<CapabilityRegistry>,
    /// 领域环境——构造 InvokeContext 用（能力通过它访问 managed state）。
    cap_env: Arc<dyn CapabilityEnv>,
    /// 事件发射 port——emit 审计/通知用。
    #[allow(dead_code)]
    env: Arc<dyn EventPort>,
    /// AI 库连接池——审计日志写入。
    ai_pool: sqlx::SqlitePool,
    /// 共享暴露快照——所有 session 共享。
    exposure: Arc<SharedExposure>,
}

impl BlinkMcpServer {
    /// 构造 MCP server handler。
    ///
    /// `exposure` 由 `McpServerRuntime` 持有并共享给所有 session。
    pub fn new(
        cap_registry: Arc<CapabilityRegistry>,
        cap_env: Arc<dyn CapabilityEnv>,
        env: Arc<dyn EventPort>,
        ai_pool: sqlx::SqlitePool,
        exposure: Arc<SharedExposure>,
    ) -> Self {
        Self {
            cap_registry,
            cap_env,
            env,
            ai_pool,
            exposure,
        }
    }

    /// 把 CapabilityResult 投影为 MCP CallToolResult。
    ///
    /// 0.14.1: 改调 canonical 投影（`to_rig_tool_result()` + `rig_tool_result_to_text()`），
    /// 消除内联 match + Blob 摘要重复 + Items score 漂移。
    /// 0.19.4: 改用 `to_rig_tool_result_with_stash()`，image Blob 移入 stash 并返回 image_ref。
    fn result_to_call_tool_result(
        result: CapabilityResult,
        stash: Option<&crate::domain::capability::ImageStash>,
    ) -> CallToolResult {
        let text = crate::domain::capability::rig_tool_result_to_text(
            &result.to_rig_tool_result_with_stash(stash),
        );
        CallToolResult::success(vec![Content::text(text)])
    }

    /// 把错误投影为 MCP CallToolResult（is_error = true）。
    fn error_to_call_tool_result(msg: &str) -> CallToolResult {
        CallToolResult::error(vec![Content::text(msg.to_string())])
    }
}

// ── ServerHandler 实现 ───────────────────────────────────────────────────────

#[allow(unused_variables)]
impl rmcp::handler::server::ServerHandler for BlinkMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut caps = ServerCapabilities::default();
        // 0.19.13: 声明 tools.listChanged = true，让 client 知道我们支持热刷新
        let mut tools_caps = rmcp::model::ToolsCapability::default();
        tools_caps.list_changed = Some(true);
        caps.tools = Some(tools_caps);
        let mut info = InitializeResult::new(caps);
        info.server_info = Implementation::new("blink", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Blink — Windows 全局快捷入口。提供截图、OCR、剪贴板、应用搜索等本地能力。".into(),
        );
        info
    }

    fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, rmcp::ErrorData>> + Send + '_ {
        let exposure = self.exposure.clone();
        async move {
            let snapshot = exposure.read().await;
            Ok(ListToolsResult::with_all_items(snapshot.tools.clone()))
        }
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        // 同步上下文中不能 await，用 try_read 快速尝试
        let snapshot = self.exposure.snapshot.try_read().ok()?;
        snapshot.tools.iter().find(|t| t.name == name).cloned()
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_ {
        let tool_name = request.name.to_string();
        let args = request
            .arguments
            .map(|m| Value::Object(m))
            .unwrap_or(Value::Null);

        let cap_registry = self.cap_registry.clone();
        let ai_pool = self.ai_pool.clone();
        let env = self.env.clone();
        let exposure = self.exposure.clone();
        let tool_name_for_audit = tool_name.clone();
        let args_for_audit = args.clone();

        async move {
            // 0.19.13: call-time 二次门禁——读取最新快照校验 allowed
            let allowed = {
                let snapshot = exposure.read().await;
                snapshot.allowed.clone()
            };

            if !allowed.contains(&tool_name) {
                tracing::warn!(
                    tool = %tool_name,
                    "MCP server: 外部 client 调用了未暴露的 tool（call-time 门禁拒绝）"
                );
                let available = allowed.iter().cloned().collect::<Vec<_>>().join(", ");
                let result = BlinkMcpServer::error_to_call_tool_result(&format!(
                    "Tool '{tool_name}' is not exposed. Available tools: {available}"
                ));
                return Ok(result);
            }

            // 0.21.5: call-time policy 二次检查
            // 即使 allowed 集合中有该 id，也再次验证 policy 是否允许 MCP 调用。
            // 这覆盖了 build 后到 call 之间 Capability 可能被动态替换的极端情况。
            if let Some(cap) = cap_registry.get(&tool_name) {
                let policy = cap.policy();
                use crate::domain::capability::{DangerClass, McpDefault};
                if policy.danger == DangerClass::Dangerous
                    || policy.mcp_default == McpDefault::Forbidden
                {
                    tracing::warn!(
                        tool = %tool_name,
                        danger = ?policy.danger,
                        mcp_default = ?policy.mcp_default,
                        "MCP server: call-time policy 拒绝（Dangerous 或 Forbidden）"
                    );
                    let result = BlinkMcpServer::error_to_call_tool_result(&format!(
                        "Tool '{tool_name}' is not available for MCP (policy denied)"
                    ));
                    return Ok(result);
                }
            }

            let start = std::time::Instant::now();

            // 构造 InvokeContext
            // 0.21.0: 携带 origin=Mcp + runtime（MCP server 运行在主进程中，但不应获得 GUI 权限）
            let ctx = InvokeContext {
                env: self.cap_env.as_ref(),
                origin: crate::domain::capability::InvocationOrigin::Mcp,
                runtime: crate::domain::capability::RuntimeCapabilities {
                    surface: None, // MCP 首版禁止 GUI starter，不注入 surface
                    main_process: true,
                    desktop_session: true,
                },
                deadline: None,
            };

            // 调用 Capability
            let result = cap_registry.invoke(&tool_name, args, &ctx).await;
            let elapsed_ms = start.elapsed().as_millis();

            let call_tool_result = match result {
                Ok(cap_result) => {
                    let stash = self.cap_env.image_stash();
                    // 审计日志（caller = mcp_external）
                    let summary = crate::domain::capability::rig_tool_result_to_text(
                        &cap_result.to_rig_tool_result_with_stash(stash.map(|s| s.as_ref())),
                    );
                    ai_audit::save_audit_log(
                        &ai_pool,
                        &tool_name_for_audit,
                        &args_for_audit,
                        &summary,
                        "",             // provider_kind — MCP 外部调用无 provider
                        "",             // model_id — MCP 外部调用无模型
                        1,              // turn
                        "mcp_external", // caller — 外部 MCP client 调用
                    )
                    .await;

                    BlinkMcpServer::result_to_call_tool_result(
                        cap_result,
                        stash.map(|s| s.as_ref()),
                    )
                }
                Err(e) => {
                    let err_msg = format!("{e}");
                    tracing::warn!(
                        tool = %tool_name_for_audit,
                        error = %err_msg,
                        elapsed_ms,
                        "MCP server: capability 调用失败"
                    );

                    // 审计日志（失败也记录）
                    ai_audit::save_audit_log(
                        &ai_pool,
                        &tool_name_for_audit,
                        &args_for_audit,
                        &format!("ERROR: {err_msg}"),
                        "",
                        "",
                        1,
                        "mcp_external",
                    )
                    .await;

                    BlinkMcpServer::error_to_call_tool_result(&format!(
                        "Capability '{tool_name_for_audit}' failed: {err_msg}"
                    ))
                }
            };

            tracing::info!(
                tool = %tool_name_for_audit,
                elapsed_ms,
                "MCP server: tool 调用完成"
            );

            Ok(call_tool_result)
        }
    }

    /// 0.19.13: client 完成初始化后，订阅 generation 变化通知。
    ///
    /// generation 改变时调用 `peer.notify_tool_list_changed()`，
    /// 让 client 重新拉 `tools/list` 获取最新工具列表。
    /// session 结束后订阅任务自动退出（watch receiver 关闭或 peer 断开）。
    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        let exposure = self.exposure.clone();
        let peer = context.peer;
        async move {
            let mut rx = exposure.subscribe();
            let current_gen = *rx.borrow();
            tracing::debug!(
                generation = current_gen,
                "MCP session initialized, subscribing to tool list changes"
            );

            // 后台任务：监听 generation 变化，通知 client
            tokio::spawn(async move {
                loop {
                    // 等待 generation 变化
                    if rx.changed().await.is_err() {
                        // sender dropped (runtime 关闭)
                        break;
                    }
                    let new_gen = *rx.borrow();
                    tracing::info!(
                        generation = new_gen,
                        "MCP: exposure generation 变化，通知 client tool list changed"
                    );
                    // 通知 client 重新拉 tools/list
                    // 如果 client 不支持 listChanged 或已断开，notify 会失败，忽略即可
                    if let Err(e) = peer.notify_tool_list_changed().await {
                        tracing::debug!(
                            error = %e,
                            "MCP: notify_tool_list_changed 失败（client 可能已断开或不支持）"
                        );
                        break;
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_result_has_is_error_flag() {
        let result = BlinkMcpServer::error_to_call_tool_result("something went wrong");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn text_result_projects_to_content() {
        let result = CapabilityResult::Text {
            content: "hello world".into(),
            desc: None,
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result, None);
        assert_eq!(projected.content.len(), 1);
        assert_eq!(projected.is_error, Some(false));
    }

    #[test]
    fn done_result_projects_to_summary() {
        let result = CapabilityResult::Done {
            summary: "已写入剪贴板".into(),
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result, None);
        assert_eq!(projected.content.len(), 1);
        assert_eq!(projected.is_error, Some(false));
    }

    #[test]
    fn blob_result_projects_to_text_summary() {
        let result = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0u8; 2048],
            desc: None,
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result, None);
        assert_eq!(projected.content.len(), 1);
        let text = projected.content[0]
            .as_text()
            .map(|t| t.text.as_str())
            .unwrap_or("");
        assert!(text.contains("image/png"));
        assert!(text.contains("KB"));
    }

    #[test]
    fn items_result_projects_to_json() {
        use crate::domain::capability::ItemResult;
        let result = CapabilityResult::Items {
            items: vec![ItemResult {
                data: serde_json::json!({"name": "file.txt", "path": "C:\\file.txt"}),
                desc: None,
                actions: vec![],
            }],
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result, None);
        assert_eq!(projected.content.len(), 1);
        let text = projected.content[0]
            .as_text()
            .map(|t| t.text.as_str())
            .unwrap_or("");
        assert!(text.contains("file.txt"));
    }

    /// P3: 验证 CapabilityResult → CallToolResult 全变体投影闭环。
    #[test]
    fn mcp_result_projection_all_variants_roundtrip() {
        use crate::domain::capability::{CapabilityResult, ItemResult};

        // Text → Content::text
        let result = CapabilityResult::Text {
            content: "hello".into(),
            desc: None,
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result, None);
        assert_eq!(projected.is_error, Some(false));
        assert_eq!(projected.content.len(), 1);
        let text = projected.content[0].as_text().unwrap().text.clone();
        assert_eq!(text, "hello");

        // Done → Content::text(summary)
        let result = CapabilityResult::Done {
            summary: "已完成".into(),
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result, None);
        assert_eq!(projected.content.len(), 1);
        let text = projected.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("已完成"));

        // Items → JSON
        let result = CapabilityResult::Items {
            items: vec![ItemResult {
                data: serde_json::json!({"name": "test.txt", "path": "C:\\test.txt"}),
                desc: None,
                actions: vec![],
            }],
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result, None);
        let text = projected.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("test.txt"));

        // Blob → 文本摘要
        let result = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0u8; 4096],
            desc: None,
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result, None);
        let text = projected.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("image/png"));
        assert!(text.contains("KB"));

        // Error → is_error = true
        let err = BlinkMcpServer::error_to_call_tool_result("something failed");
        assert_eq!(err.is_error, Some(true));
    }

    // ── ExposureSnapshot 单测 ──────────────────────────────────────────────

    /// 测试用 mock Capability——避免构造 AppHandle（与 registry.rs 测试同模式）。
    struct MockCap {
        id_val: String,
    }

    #[async_trait::async_trait]
    impl crate::domain::capability::Capability for MockCap {
        fn id(&self) -> &str {
            &self.id_val
        }
        fn schema(&self) -> crate::domain::capability::CapabilitySchema {
            crate::domain::capability::CapabilitySchema::empty(&self.id_val, "mock for test")
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, crate::domain::capability::CapabilityError> {
            Ok(CapabilityResult::Done {
                summary: "mock".into(),
            })
        }
    }

    /// 构建带 N 个 mock capability 的注册表。
    fn mock_registry(names: &[&str]) -> crate::domain::capability::CapabilityRegistry {
        let reg = crate::domain::capability::CapabilityRegistry::default();
        for &name in names {
            reg.register(std::sync::Arc::new(MockCap {
                id_val: name.to_string(),
            })
                as std::sync::Arc<dyn crate::domain::capability::Capability>)
                .unwrap();
        }
        reg
    }

    #[tokio::test]
    async fn exposure_snapshot_starts_empty() {
        let exposure = SharedExposure::new();
        let snapshot = exposure.read().await;
        assert!(snapshot.tools.is_empty());
        assert!(snapshot.allowed.is_empty());
        assert_eq!(snapshot.generation, 0);
    }

    #[tokio::test]
    async fn exposure_generation_increments_on_rebuild() {
        let registry = mock_registry(&["test_cap"]);

        let exposure = SharedExposure::new();
        assert_eq!(exposure.current_generation().await, 0);

        let config = McpServerModeConfig {
            enabled: true,
            port: 32123,
            exposed_capabilities: vec!["test_cap".into()],
            exposure_seeded: false,
        };
        let gen1 = exposure.rebuild(&registry, &config).await;
        assert_eq!(gen1, 1);
        assert_eq!(exposure.current_generation().await, 1);

        let snapshot = exposure.read().await;
        assert_eq!(snapshot.tools.len(), 1);
        assert!(snapshot.allowed.contains("test_cap"));
        drop(snapshot);

        // 再次重建（移除暴露）
        let config2 = McpServerModeConfig {
            enabled: true,
            port: 32123,
            exposed_capabilities: vec![],
            exposure_seeded: false,
        };
        let gen2 = exposure.rebuild(&registry, &config2).await;
        assert_eq!(gen2, 2);
        let snapshot = exposure.read().await;
        assert!(snapshot.tools.is_empty());
    }

    #[tokio::test]
    async fn exposure_snapshot_tools_sorted_by_name() {
        // 故意以非字母序注册
        let registry = mock_registry(&["zebra", "apple", "mango"]);

        let exposure = SharedExposure::new();
        let config = McpServerModeConfig {
            enabled: true,
            port: 32123,
            exposed_capabilities: vec!["zebra".into(), "apple".into(), "mango".into()],
            exposure_seeded: false,
        };
        exposure.rebuild(&registry, &config).await;

        let snapshot = exposure.read().await;
        let names: Vec<&str> = snapshot.tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, vec!["apple", "mango", "zebra"]);
    }

    #[tokio::test]
    async fn exposure_generation_watch_notifies() {
        let registry = mock_registry(&["test_cap"]);

        let exposure = SharedExposure::new();
        let mut rx = exposure.subscribe();
        assert_eq!(*rx.borrow(), 0);

        let config = McpServerModeConfig {
            enabled: true,
            port: 32123,
            exposed_capabilities: vec!["test_cap".into()],
            exposure_seeded: false,
        };
        exposure.rebuild(&registry, &config).await;

        // watch 应该收到 generation = 1
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), 1);
    }

    // ── 0.21.5: ExposureSnapshot policy 过滤单测 ─────────────────────────

    /// 带 policy 的 mock Capability。
    struct PolicyMockCap {
        id_val: String,
        policy: crate::domain::capability::CapabilityPolicy,
    }

    #[async_trait::async_trait]
    impl crate::domain::capability::Capability for PolicyMockCap {
        fn id(&self) -> &str {
            &self.id_val
        }
        fn schema(&self) -> crate::domain::capability::CapabilitySchema {
            crate::domain::capability::CapabilitySchema::empty(&self.id_val, "policy mock")
        }
        fn policy(&self) -> crate::domain::capability::CapabilityPolicy {
            self.policy.clone()
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, crate::domain::capability::CapabilityError> {
            Ok(CapabilityResult::Done {
                summary: "ok".into(),
            })
        }
    }

    use crate::domain::capability::{
        CapabilityPolicy, DangerClass, McpDefault, OriginSet, RuntimeRequirement,
    };

    fn safe_mcp_default_off_policy() -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::NONE,
            danger: DangerClass::Safe,
            sensitive: false,
            ai_default: crate::domain::capability::AiDefault::On,
            mcp_default: McpDefault::DefaultOff,
            confirmation: crate::domain::capability::ConfirmationPolicy::safe(),
        }
    }

    fn dangerous_policy() -> CapabilityPolicy {
        CapabilityPolicy {
            danger: DangerClass::Dangerous,
            mcp_default: McpDefault::Forbidden,
            confirmation: crate::domain::capability::ConfirmationPolicy::dangerous(true),
            ..safe_mcp_default_off_policy()
        }
    }

    fn forbidden_gui_policy() -> CapabilityPolicy {
        // Safe 但 GUI starter —— mcp_default = Forbidden
        CapabilityPolicy {
            danger: DangerClass::Safe,
            mcp_default: McpDefault::Forbidden,
            ..safe_mcp_default_off_policy()
        }
    }

    /// Dangerous Capability 即使被用户暴露也不进入 MCP 清单。
    #[tokio::test]
    async fn exposure_filters_dangerous_even_if_exposed() {
        let reg = crate::domain::capability::CapabilityRegistry::default();
        reg.register(std::sync::Arc::new(PolicyMockCap {
            id_val: "dangerous_cap".into(),
            policy: dangerous_policy(),
        })
            as std::sync::Arc<dyn crate::domain::capability::Capability>)
            .unwrap();

        let exposure = SharedExposure::new();
        let config = McpServerModeConfig {
            enabled: true,
            port: 32123,
            exposed_capabilities: vec!["dangerous_cap".into()],
            exposure_seeded: false,
        };
        exposure.rebuild(&reg, &config).await;

        let snapshot = exposure.read().await;
        assert!(
            snapshot.tools.is_empty(),
            "Dangerous capability 不应进入 MCP 暴露清单"
        );
        assert!(
            !snapshot.allowed.contains("dangerous_cap"),
            "Dangerous capability 不应在 allowed 集合中"
        );
    }

    /// Forbidden（GUI starter）Capability 即使被用户暴露也不进入 MCP 清单。
    #[tokio::test]
    async fn exposure_filters_forbidden_even_if_exposed() {
        let reg = crate::domain::capability::CapabilityRegistry::default();
        reg.register(std::sync::Arc::new(PolicyMockCap {
            id_val: "gui_starter_cap".into(),
            policy: forbidden_gui_policy(),
        })
            as std::sync::Arc<dyn crate::domain::capability::Capability>)
            .unwrap();

        let exposure = SharedExposure::new();
        let config = McpServerModeConfig {
            enabled: true,
            port: 32123,
            exposed_capabilities: vec!["gui_starter_cap".into()],
            exposure_seeded: false,
        };
        exposure.rebuild(&reg, &config).await;

        let snapshot = exposure.read().await;
        assert!(
            snapshot.tools.is_empty(),
            "Forbidden GUI starter capability 不应进入 MCP 暴露清单"
        );
    }

    /// Safe + DefaultOff 的 Capability 被用户暴露后应进入 MCP 清单。
    #[tokio::test]
    async fn exposure_allows_safe_default_off_when_exposed() {
        let reg = crate::domain::capability::CapabilityRegistry::default();
        reg.register(std::sync::Arc::new(PolicyMockCap {
            id_val: "safe_cap".into(),
            policy: safe_mcp_default_off_policy(),
        })
            as std::sync::Arc<dyn crate::domain::capability::Capability>)
            .unwrap();

        let exposure = SharedExposure::new();
        let config = McpServerModeConfig {
            enabled: true,
            port: 32123,
            exposed_capabilities: vec!["safe_cap".into()],
            exposure_seeded: false,
        };
        exposure.rebuild(&reg, &config).await;

        let snapshot = exposure.read().await;
        assert_eq!(snapshot.tools.len(), 1);
        assert!(snapshot.allowed.contains("safe_cap"));
    }

    /// 混合场景：Safe + Dangerous + Forbidden 同时暴露，只 Safe 进入清单。
    #[tokio::test]
    async fn exposure_mixed_only_safe_passes() {
        let reg = crate::domain::capability::CapabilityRegistry::default();
        reg.register(std::sync::Arc::new(PolicyMockCap {
            id_val: "safe_cap".into(),
            policy: safe_mcp_default_off_policy(),
        })
            as std::sync::Arc<dyn crate::domain::capability::Capability>)
            .unwrap();
        reg.register(std::sync::Arc::new(PolicyMockCap {
            id_val: "dangerous_cap".into(),
            policy: dangerous_policy(),
        })
            as std::sync::Arc<dyn crate::domain::capability::Capability>)
            .unwrap();
        reg.register(std::sync::Arc::new(PolicyMockCap {
            id_val: "forbidden_cap".into(),
            policy: forbidden_gui_policy(),
        })
            as std::sync::Arc<dyn crate::domain::capability::Capability>)
            .unwrap();

        let exposure = SharedExposure::new();
        let config = McpServerModeConfig {
            enabled: true,
            port: 32123,
            exposure_seeded: false,
            exposed_capabilities: vec![
                "safe_cap".into(),
                "dangerous_cap".into(),
                "forbidden_cap".into(),
            ],
        };
        exposure.rebuild(&reg, &config).await;

        let snapshot = exposure.read().await;
        // 只有 safe_cap 通过
        assert_eq!(snapshot.tools.len(), 1);
        assert!(snapshot.allowed.contains("safe_cap"));
        assert!(!snapshot.allowed.contains("dangerous_cap"));
        assert!(!snapshot.allowed.contains("forbidden_cap"));
    }
}
