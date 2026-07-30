//! MCP server 编排（0.13.4）——Blink 作为 MCP server 暴露能力给外部 client。
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
//! ## 传输
//!
//! stdio 模式——外部 client 拉起 Blink 子进程（`blink mcp-server`），走 stdin/stdout JSON-RPC。
//! 与 0.13.0 MCP client 对称（client 拉 server 子进程，server 暴露 tool）。
//!
//! ## 与 CLI 化的关系
//!
//! `blink mcp-server` 命令（0.13.5）等价于调用 `run_stdio_server()`——以独立进程运行，
//! 不启动 GUI。设置页只控制暴露配置，实际 server 由 CLI 启动。

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServiceExt;
use serde_json::Value;

use crate::domain::capability::{CapabilityRegistry, CapabilityResult, InvokeContext};
use crate::domain::event::DomainEnv;
use crate::domain::mcp::projection::capability_schemas_to_mcp_tools;
use crate::domain::mcp::server_config::{McpServerModeConfig, McpServerModeConfigStore};
use crate::infra::data::ai_audit;

/// Blink MCP server——实现 rmcp ServerHandler，暴露 Capability 给外部 client。
///
/// 持有 CapabilityRegistry + DomainEnv + AI DB pool + 配置，
/// 在 `list_tools` / `call_tool` 时按配置过滤和执行。
///
/// **0.14.6 §2.2**：`app_handle: tauri::AppHandle` 替换为 `env: Arc<dyn DomainEnv>`，
/// domain 层不再直接依赖 tauri。
pub struct BlinkMcpServer {
    /// 能力注册表——list_tools 和 call_tool 的数据源。
    cap_registry: Arc<CapabilityRegistry>,
    /// 领域环境——构造 InvokeContext 用（能力通过它访问 managed state）。
    env: Arc<dyn DomainEnv>,
    /// AI 库连接池——审计日志写入。
    ai_pool: sqlx::SqlitePool,
    /// server 配置——控制哪些 capability 暴露。
    config: McpServerModeConfig,
    /// 缓存的 tool 列表（配置不变时复用，避免每次 list_tools 都投影）。
    cached_tools: Vec<Tool>,
}

impl BlinkMcpServer {
    /// 构造 MCP server。
    ///
    /// 构造时立即按配置过滤 + 投影 Capability schema → rmcp Tool，缓存结果。
    pub fn new(
        cap_registry: Arc<CapabilityRegistry>,
        env: Arc<dyn DomainEnv>,
        ai_pool: sqlx::SqlitePool,
        config: McpServerModeConfig,
    ) -> Self {
        // 按配置过滤 Capability，只暴露用户勾选的
        let exposed_schemas: Vec<_> = cap_registry
            .list()
            .into_iter()
            .filter(|s| config.exposed_capabilities.contains(&s.name))
            .collect();

        let cached_tools = capability_schemas_to_mcp_tools(&exposed_schemas);

        tracing::info!(
            total_capabilities = cap_registry.len(),
            exposed = cached_tools.len(),
            "BlinkMcpServer: 初始化完成"
        );

        Self {
            cap_registry,
            env,
            ai_pool,
            config,
            cached_tools,
        }
    }

    /// 按 name 查找 tool 定义（供 `get_tool` 用）。
    fn find_tool(&self, name: &str) -> Option<Tool> {
        self.cached_tools
            .iter()
            .find(|t| t.name == name)
            .cloned()
    }

    /// 把 CapabilityResult 投影为 MCP CallToolResult。
    ///
    /// 0.14.1: 改调 canonical 投影（`to_rig_tool_result()` + `rig_tool_result_to_text()`），
    /// 消除内联 match + Blob 摘要重复 + Items score 漂移。
    ///
    /// - `Text` → MCP TextContent
    /// - `Items` → 序列化 data JSON（不含 desc/actions/score）
    /// - `Blob` → 文本摘要（不传原始字节，与 rig 投影策略一致）
    /// - `Done` → summary 文本
    fn result_to_call_tool_result(result: CapabilityResult) -> CallToolResult {
        let text = crate::domain::capability::rig_tool_result_to_text(
            &result.to_rig_tool_result(),
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
        caps.tools = Some(Default::default());
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
        let tools = self.cached_tools.clone();
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.find_tool(name)
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

        // Clone 所有需要的值到 async block 中（避免借用 self）
        let cap_registry = self.cap_registry.clone();
        let ai_pool = self.ai_pool.clone();
        let env = self.env.clone();
        let exposed = self.config.exposed_capabilities.clone();
        let available = self.config.exposed_capabilities.join(", ");
        let tool_name_for_audit = tool_name.clone();
        let args_for_audit = args.clone();

        async move {
            // 检查 tool 是否在暴露列表中
            if !exposed.contains(&tool_name) {
                tracing::warn!(
                    tool = %tool_name,
                    "MCP server: 外部 client 调用了未暴露的 tool"
                );
                let result = BlinkMcpServer::error_to_call_tool_result(&format!(
                    "Tool '{tool_name}' is not exposed. Available tools: {available}"
                ));
                return Ok(result);
            }

            let start = std::time::Instant::now();

            // 构造 InvokeContext（env 在 async block 内 owned，取引用）
            let ctx = InvokeContext {
                env: env.capability_env(),
                deadline: None,
            };

            // 调用 Capability
            let result = cap_registry.invoke(&tool_name, args, &ctx).await;
            let elapsed_ms = start.elapsed().as_millis();

            let call_tool_result = match result {
                Ok(cap_result) => {
                    // 审计日志（caller = mcp_external）——0.14.1 改调 canonical 投影
                    let summary = crate::domain::capability::rig_tool_result_to_text(
                        &cap_result.to_rig_tool_result(),
                    );
                    ai_audit::save_audit_log(
                        &ai_pool,
                        &tool_name_for_audit,
                        &args_for_audit,
                        &summary,
                        "", // provider_kind — MCP 外部调用无 provider
                        "", // model_id — MCP 外部调用无模型
                        1,  // turn
                        "mcp_external", // caller — 外部 MCP client 调用
                    )
                    .await;

                    BlinkMcpServer::result_to_call_tool_result(cap_result)
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
                        "mcp_external", // caller — 外部 MCP client 调用
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
}

/// 以 stdio 模式运行 MCP server。
///
/// 读取 stdin / 写入 stdout，走 JSON-RPC 协议。
/// 外部 MCP client 拉起 `blink mcp-server` 子进程后，通过 stdio 通信。
///
/// **不启动 GUI**——纯 stdio 进程，适合 CLI / 脚本场景。
/// 设置页的暴露配置从配置库读取，决定哪些 Capability 暴露。
///
/// # 参数
/// - `cap_registry`：能力注册表
/// - `env`：领域环境（能力通过它访问 managed state）
/// - `ai_pool`：AI 库连接池（审计日志）
/// - `config_pool`：配置库连接池（读取 MCP server 配置）
pub async fn run_stdio_server(
    cap_registry: Arc<CapabilityRegistry>,
    env: Arc<dyn DomainEnv>,
    ai_pool: sqlx::SqlitePool,
    config_pool: sqlx::SqlitePool,
) -> Result<(), String> {
    // 读取配置
    let config = McpServerModeConfigStore::load(&config_pool)
        .await
        .map_err(|e| format!("读取 MCP server 配置失败: {e}"))?;

    if !config.enabled {
        return Err("MCP server 未启用。请在设置页「MCP Server」中开启。".into());
    }

    if config.exposed_capabilities.is_empty() {
        return Err(
            "MCP server 已启用但未暴露任何能力。请在设置页勾选要暴露的 Capability。".into(),
        );
    }

    let server = BlinkMcpServer::new(cap_registry, env, ai_pool, config);

    tracing::info!("MCP server: 启动 stdio 模式");

    // 用 rmcp 的 stdio transport 启动 server
    let transport = rmcp::transport::io::stdio();
    let service = server
        .serve(transport)
        .await
        .map_err(|e| format!("MCP server 启动失败: {e}"))?;

    // 等待服务结束（stdin 关闭或 client 断开）
    service
        .waiting()
        .await
        .map_err(|e| format!("MCP server 运行错误: {e}"))?;

    tracing::info!("MCP server: stdio 服务已结束");
    Ok(())
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
        let projected = BlinkMcpServer::result_to_call_tool_result(result);
        assert_eq!(projected.content.len(), 1);
        // CallToolResult::success() sets is_error = Some(false)
        assert_eq!(projected.is_error, Some(false));
    }

    #[test]
    fn done_result_projects_to_summary() {
        let result = CapabilityResult::Done {
            summary: "已写入剪贴板".into(),
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result);
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
        let projected = BlinkMcpServer::result_to_call_tool_result(result);
        assert_eq!(projected.content.len(), 1);
        // 文本摘要应包含 mime 类型和大小
        let text = projected.content[0].as_text().map(|t| t.text.as_str()).unwrap_or("");
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
        let projected = BlinkMcpServer::result_to_call_tool_result(result);
        assert_eq!(projected.content.len(), 1);
        let text = projected.content[0].as_text().map(|t| t.text.as_str()).unwrap_or("");
        assert!(text.contains("file.txt"));
    }

    /// P3: 验证 CapabilityResult → CallToolResult 全变体投影闭环。
    ///
    /// 模拟 Blink 作为 MCP server 被 client 调用 call_tool 后返回的格式。
    #[test]
    fn mcp_result_projection_all_variants_roundtrip() {
        use crate::domain::capability::{CapabilityResult, ItemResult};

        // Text → Content::text
        let result = CapabilityResult::Text { content: "hello".into(), desc: None };
        let projected = BlinkMcpServer::result_to_call_tool_result(result);
        assert_eq!(projected.is_error, Some(false));
        assert_eq!(projected.content.len(), 1);
        let text = projected.content[0].as_text().unwrap().text.clone();
        assert_eq!(text, "hello");

        // Done → Content::text(summary)
        let result = CapabilityResult::Done { summary: "已完成".into() };
        let projected = BlinkMcpServer::result_to_call_tool_result(result);
        assert_eq!(projected.content.len(), 1);
        let text = projected.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("已完成"));

        // Items → JSON（0.14：只序列化 data，不含 score/desc/actions）
        let result = CapabilityResult::Items {
            items: vec![ItemResult {
                data: serde_json::json!({"name": "test.txt", "path": "C:\\test.txt"}),
                desc: None,
                actions: vec![],
            }],
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result);
        let text = projected.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("test.txt"));

        // Blob → 文本摘要
        let result = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0u8; 4096],
            desc: None,
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result);
        let text = projected.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("image/png"));
        assert!(text.contains("KB"));

        // Error → is_error = true
        let err = BlinkMcpServer::error_to_call_tool_result("something failed");
        assert_eq!(err.is_error, Some(true));
    }

    // ── 真正的 MCP 协议端到端闭环测试 ──────────────────────────────────────
    //
    // 用 tokio::io::duplex() 创建 in-memory 双向管道，一端跑 server（实现
    // ServerHandler），一端跑 rmcp client。client 发 JSON-RPC：initialize →
    // list_tools → call_tool，验证 MCP 协议完整闭环。
    //
    // 这不是投影测试——是真正的 JSON-RPC 协议在线上的往返。

    /// 测试用 MCP server——模拟 BlinkMcpServer 的行为，但不需要 AppHandle。
    ///
    /// 暴露一个 `echo` tool：收到什么参数就回什么。
    struct TestMcpServer {
        tools: Vec<Tool>,
    }

    impl TestMcpServer {
        fn new() -> Self {
            let tool = Tool::new(
                "echo".to_string(),
                "Echo back the input".to_string(),
                std::sync::Arc::new(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string", "description": "Message to echo" }
                    },
                    "required": ["message"]
                }).as_object().cloned().unwrap_or_default()),
            );
            Self { tools: vec![tool] }
        }
    }

    impl rmcp::handler::server::ServerHandler for TestMcpServer {
        fn get_info(&self) -> ServerInfo {
            let mut caps = ServerCapabilities::default();
            caps.tools = Some(Default::default());
            let mut info = InitializeResult::new(caps);
            info.server_info = Implementation::new("blink-test-server".to_string(), "0.0.0-test".to_string());
            info
        }

        fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> impl Future<Output = Result<ListToolsResult, rmcp::ErrorData>> + Send + '_ {
            let tools = self.tools.clone();
            std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
        }

        fn get_tool(&self, name: &str) -> Option<Tool> {
            self.tools.iter().find(|t| t.name == name).cloned()
        }

        fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> impl Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_ {
            let tool_name = request.name.to_string();
            let args = request
                .arguments
                .map(|m| Value::Object(m))
                .unwrap_or(Value::Null);

            async move {
                if tool_name == "echo" {
                    let msg = args
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no message)");
                    Ok(CallToolResult::success(vec![Content::text(
                        msg.to_string(),
                    )]))
                } else {
                    Ok(BlinkMcpServer::error_to_call_tool_result(&format!(
                        "Unknown tool: {tool_name}"
                    )))
                }
            }
        }
    }

    /// 真正的 MCP 协议端到端闭环：
    /// 1. 用 `tokio::io::duplex()` 创建 in-memory 管道
    /// 2. 一端启动 TestMcpServer（实现 ServerHandler）
    /// 3. 另一端启动 rmcp client（ClientInfo::default()）
    /// 4. client 调 `list_all_tools()` → 验证返回 `echo` tool
    /// 5. client 调 `call_tool("echo", {message: "hello"})` → 验证返回 "hello"
    ///
    /// 这验证了 JSON-RPC 消息在线上的完整往返：
    /// initialize → tools/list → tools/call
    #[tokio::test]
    async fn mcp_protocol_e2e_client_server_loop() {
        use rmcp::ServiceExt;
        use rmcp::model::ClientInfo;

        // 创建双向管道
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        // 分割为 read/write 两半
        let (client_read, client_write) = tokio::io::split(client_stream);
        let (server_read, server_write) = tokio::io::split(server_stream);

        // 启动 server（在后台 task 中）
        let server = TestMcpServer::new();
        let server_handle = tokio::spawn(async move {
            let transport = (server_read, server_write);
            let service = server.serve(transport).await;
            // 等待 client 断开后退出
            if let Ok(svc) = service {
                let _ = svc.waiting().await;
            }
        });

        // 启动 client
        let client_transport = (client_read, client_write);
        let client_info = ClientInfo::default();
        let service = client_info
            .serve(client_transport)
            .await
            .expect("client 握手失败");

        // 1. list_tools → 验证返回 echo tool
        let tools = service
            .peer()
            .list_all_tools()
            .await
            .expect("list_all_tools 失败");

        assert_eq!(tools.len(), 1, "应返回 1 个 tool");
        assert_eq!(tools[0].name, "echo");
        assert_eq!(
            tools[0].description.as_deref(),
            Some("Echo back the input")
        );

        // 2. call_tool("echo", {message: "hello blink"}) → 验证返回 "hello blink"
        // 用 rig McpTool 封装（与生产代码 collect_tools() 同路径）
        use rig_core::tool::ToolDyn;
        let mcp_tool = rig_core::tool::rmcp::McpTool::from_mcp_server(
            tools[0].clone(),
            service.peer().clone(),
        );
        let result = mcp_tool
            .call(serde_json::json!({"message": "hello blink"}).to_string())
            .await
            .expect("McpTool call 失败");
        assert!(result.contains("hello blink"), "result was: {result}");

        // 断开 client → server 应自动退出
        drop(service);
        let _ = server_handle.await;

        tracing::info!("MCP e2e: client→server 协议闭环验证通过");
    }

    /// 验证 client 调用不存在的 tool 时，server 返回 error result。
    #[tokio::test]
    async fn mcp_protocol_e2e_unknown_tool_returns_error() {
        use rmcp::ServiceExt;
        use rmcp::model::ClientInfo;

        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let (client_read, client_write) = tokio::io::split(client_stream);
        let (server_read, server_write) = tokio::io::split(server_stream);

        let server = TestMcpServer::new();
        let server_handle = tokio::spawn(async move {
            let transport = (server_read, server_write);
            let service = server.serve(transport).await;
            if let Ok(svc) = service {
                let _ = svc.waiting().await;
            }
        });

        let client_info = ClientInfo::default();
        let service = client_info
            .serve((client_read, client_write))
            .await
            .expect("client 握手失败");

        // 调用不存在的 tool（用 McpTool 封装一个 fake tool）
        use rig_core::tool::ToolDyn;
        let fake_tool = rmcp::model::Tool::new(
            "nonexistent".to_string(),
            "Does not exist".to_string(),
            std::sync::Arc::new(serde_json::Map::new()),
        );
        let mcp_tool = rig_core::tool::rmcp::McpTool::from_mcp_server(
            fake_tool,
            service.peer().clone(),
        );
        let result = mcp_tool
            .call(serde_json::Value::Null.to_string())
            .await;
        // server 返回错误，McpTool 应转为 ToolError
        assert!(result.is_err(), "调用不存在的 tool 应失败");

        drop(service);
        let _ = server_handle.await;
    }
}
