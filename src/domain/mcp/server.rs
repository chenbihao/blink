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
use crate::domain::mcp::projection::capability_schemas_to_mcp_tools;
use crate::domain::mcp::server_config::{McpServerModeConfig, McpServerModeConfigStore};
use crate::infra::data::ai_audit;

/// Blink MCP server——实现 rmcp ServerHandler，暴露 Capability 给外部 client。
///
/// 持有 CapabilityRegistry + AppHandle + AI DB pool + 配置，
/// 在 `list_tools` / `call_tool` 时按配置过滤和执行。
pub struct BlinkMcpServer {
    /// 能力注册表——list_tools 和 call_tool 的数据源。
    cap_registry: Arc<CapabilityRegistry>,
    /// Tauri AppHandle——构造 InvokeContext 用（能力通过它访问 managed state）。
    app_handle: tauri::AppHandle,
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
        app_handle: tauri::AppHandle,
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
            app_handle,
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
    /// - `Text` → MCP TextContent
    /// - `Items` → 序列化 JSON 文本
    /// - `Blob` → 文本摘要（不传原始字节，与 rig 投影策略一致）
    /// - `Done` → summary 文本
    fn result_to_call_tool_result(result: CapabilityResult) -> CallToolResult {
        let content = match result {
            CapabilityResult::Text { content } => {
                vec![Content::text(content)]
            }
            CapabilityResult::Items { items } => {
                let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
                vec![Content::text(json)]
            }
            CapabilityResult::Blob { mime, bytes } => {
                let size_kb = bytes.len() as f64 / 1024.0;
                let size_text = if size_kb >= 1024.0 {
                    format!("{:.1} MB", size_kb / 1024.0)
                } else {
                    format!("{:.1} KB", size_kb)
                };
                vec![Content::text(format!("已获取 {} ({})", mime, size_text))]
            }
            CapabilityResult::Done { summary } => {
                vec![Content::text(summary)]
            }
        };

        CallToolResult::success(content)
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
        let app_handle = self.app_handle.clone();
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

            // 构造 InvokeContext（app_handle 在 async block 内 owned，取引用）
            let ctx = InvokeContext {
                app_handle: &app_handle,
                deadline: None,
            };

            // 调用 Capability
            let result = cap_registry.invoke(&tool_name, args, &ctx).await;
            let elapsed_ms = start.elapsed().as_millis();

            let call_tool_result = match result {
                Ok(cap_result) => {
                    // 审计日志（caller = mcp_external）
                    let summary = match &cap_result {
                        CapabilityResult::Text { content } => content.clone(),
                        CapabilityResult::Done { summary } => summary.clone(),
                        CapabilityResult::Items { items } => {
                            serde_json::to_string(items).unwrap_or_default()
                        }
                        CapabilityResult::Blob { mime, bytes } => {
                            format!("{} ({} bytes)", mime, bytes.len())
                        }
                    };
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
/// - `app_handle`：Tauri AppHandle（能力通过它访问 managed state）
/// - `ai_pool`：AI 库连接池（审计日志）
/// - `config_pool`：配置库连接池（读取 MCP server 配置）
pub async fn run_stdio_server(
    cap_registry: Arc<CapabilityRegistry>,
    app_handle: tauri::AppHandle,
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

    let server = BlinkMcpServer::new(cap_registry, app_handle, ai_pool, config);

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
                title: "file.txt".into(),
                subtitle: None,
                payload: serde_json::json!({"path": "C:\\file.txt"}),
                score: Some(0.9),
            }],
        };
        let projected = BlinkMcpServer::result_to_call_tool_result(result);
        assert_eq!(projected.content.len(), 1);
        let text = projected.content[0].as_text().map(|t| t.text.as_str()).unwrap_or("");
        assert!(text.contains("file.txt"));
    }
}
