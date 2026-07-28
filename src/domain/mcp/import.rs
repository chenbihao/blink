//! MCP 配置导入（0.13.6）——从其他 agent 导入 MCP server 配置。
//!
//! 支持 Claude Desktop / Claude Code / Cursor / Windsurf / VS Code 五种来源，
//! 以及通用 JSON 粘贴导入。各 agent 格式大同小异，核心差异在配置文件路径和字段名。
//!
//! ## 架构
//!
//! `parse_external_mcp_config()` 是纯函数（输入 JSON 文本 + 来源类型 → 输出 `Vec<McpServerConfig>`），
//! 便于单测。`detect_config_file_path()` 探测指定 agent 的配置文件路径。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::domain::mcp::config::{McpServerConfig, McpTransport};

/// 可导入的 MCP 配置来源。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum McpImportSource {
    ClaudeDesktop,
    ClaudeCode,
    Cursor,
    Windsurf,
    Vscode,
    /// OpenCode (sst/opencode)。
    OpenCode,
    /// OpenAI Codex CLI。
    Codex,
    /// 通用 JSON（用户粘贴或选择文件）。
    Json,
}

impl McpImportSource {
    /// 显示名称。
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::ClaudeDesktop => "Claude Desktop",
            Self::ClaudeCode => "Claude Code",
            Self::Cursor => "Cursor",
            Self::Windsurf => "Windsurf",
            Self::Vscode => "VS Code",
            Self::OpenCode => "OpenCode",
            Self::Codex => "Codex",
            Self::Json => "通用 JSON",
        }
    }
}

/// 批量导入结果。
#[derive(Debug, Clone, Serialize)]
pub struct ImportResult {
    /// 成功导入的数量。
    pub imported: usize,
    /// 跳过的数量（同名且 overwrite=false）。
    pub skipped: usize,
    /// 被覆盖的数量（同名且 overwrite=true）。
    pub overwritten: usize,
    /// 导入的 server 名称列表。
    pub names: Vec<String>,
}

/// 探测指定 agent 的 MCP 配置文件路径。
///
/// 返回 `None` 表示文件不存在（该 agent 可能未安装）。
pub fn detect_config_file_path(source: McpImportSource) -> Option<String> {
    let path = config_file_path(source)?;
    if std::path::Path::new(&path).exists() {
        Some(path)
    } else {
        None
    }
}

/// 获取指定 agent 的配置文件路径（不检查是否存在）。
fn config_file_path(source: McpImportSource) -> Option<String> {
    match source {
        McpImportSource::ClaudeDesktop => {
            let appdata = std::env::var("APPDATA").ok()?;
            Some(format!(
                "{}\\Claude\\claude_desktop_config.json",
                appdata
            ))
        }
        McpImportSource::ClaudeCode => {
            let userprofile = std::env::var("USERPROFILE").ok()?;
            Some(format!("{}\\.claude.json", userprofile))
        }
        McpImportSource::Cursor => {
            let userprofile = std::env::var("USERPROFILE").ok()?;
            Some(format!("{}\\.cursor\\mcp.json", userprofile))
        }
        McpImportSource::Windsurf => {
            let userprofile = std::env::var("USERPROFILE").ok()?;
            Some(format!(
                "{}\\.codeium\\windsurf\\mcp_config.json",
                userprofile
            ))
        }
        McpImportSource::Vscode => {
            let appdata = std::env::var("APPDATA").ok()?;
            Some(format!("{}\\Code\\User\\settings.json", appdata))
        }
        McpImportSource::OpenCode => {
            let userprofile = std::env::var("USERPROFILE").ok()?;
            Some(format!("{}\\.config\\opencode\\config.json", userprofile))
        }
        McpImportSource::Codex => {
            let userprofile = std::env::var("USERPROFILE").ok()?;
            Some(format!("{}\\.codex\\config.json", userprofile))
        }
        McpImportSource::Json => None, // 通用 JSON 无固定路径
    }
}

/// 解析外部 agent 的 MCP 配置。
///
/// 各 agent 格式大同小异，核心差异在：
/// - VS Code 用 `mcp.servers` 而非 `mcpServers`
/// - Claude Code 支持 `type: "sse"` / `type: "http"` / `type: "streamable-http"` 字段
///
/// 除了标准的 `{ "mcpServers": { ... } }` 格式外，还支持：
/// - **裸配置**：单个 server 配置不含 name 和 mcpServers 包裹，
///   如 `{ "type": "sse", "url": "http://...", "headers": {} }`
/// - **JSON 数组**：多个裸配置的数组，如 `[{ "type": "stdio", ... }, { ... }]`
///
/// 裸配置会自动从 URL 主机名或 command 基名生成名称。
///
/// 返回待导入的 server 列表（不含去重——调用方决定覆盖/跳过策略）。
pub fn parse_external_mcp_config(
    source: McpImportSource,
    json: &str,
) -> Result<Vec<McpServerConfig>, String> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("JSON 解析失败: {e}"))?;

    // 1. 尝试标准 mcpServers 格式
    // VS Code 用 `mcp.servers`，其他 agent 用 `mcpServers`
    // VS Code settings.json 是大 JSON，如果 mcp 字段不存在会返回 Null → as_object() = None
    let servers = match source {
        McpImportSource::Vscode => {
            // VS Code 优先尝试 mcp.servers，fallback 到 mcpServers
            v["mcp"]["servers"].as_object()
                .or_else(|| v["mcpServers"].as_object())
        }
        _ => v["mcpServers"].as_object(),
    };

    if let Some(servers) = servers {
        let mut configs = Vec::new();
        for (name, cfg) in servers {
            let config = parse_single_server(name, cfg)?;
            configs.push(config);
        }
        return Ok(configs);
    }

    // 2. 尝试裸配置（单个 server 不含 name/mcpServers 包裹）
    if is_bare_server_config(&v) {
        let name = auto_generate_name(&v, 0);
        let config = parse_single_server(&name, &v)?;
        return Ok(vec![config]);
    }

    // 3. 尝试 JSON 数组（多个裸配置）
    if let Some(arr) = v.as_array() {
        let mut configs = Vec::new();
        for (i, item) in arr.iter().enumerate() {
            if is_bare_server_config(item) {
                let name = auto_generate_name(item, i);
                let config = parse_single_server(&name, item)?;
                configs.push(config);
            }
        }
        if !configs.is_empty() {
            return Ok(configs);
        }
    }

    // VS Code 特殊提示：settings.json 中未配置 MCP server
    let hint = match source {
        McpImportSource::Vscode => "（VS Code settings.json 中未找到 mcp.servers 字段，可能未配置 MCP server，或使用 .vscode/mcp.json 工作区级配置）",
        _ => "",
    };
    Err(format!("配置格式不匹配：未找到 mcpServers 字段，也不是有效的裸配置或数组格式{hint}"))
}

/// 解析单个 server 配置。
///
/// Claude Code 格式支持 `type` 字段：`"stdio"` | `"sse"` | `"http"` | `"streamable-http"`。
/// `"sse"` / `"streamable-http"` 统一映射为 `McpTransport::Http`（Blink 内部不区分 HTTP 子类型）。
fn parse_single_server(name: &str, cfg: &serde_json::Value) -> Result<McpServerConfig, String> {
    // Claude Code 格式：type: "stdio" | "sse" | "http" | "streamable-http"
    let transport_type = cfg
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("stdio");

    let transport = match transport_type {
        "sse" => {
            // SSE → 旧版 SSE transport（自建 SseClientTransport）
            let url = cfg
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or("SSE 模式缺少 url 字段")?
                .to_string();
            let headers = cfg
                .get("headers")
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| {
                            v.as_str().map(|s| (k.clone(), s.to_string()))
                        })
                        .collect::<HashMap<String, String>>()
                })
                .unwrap_or_default();
            McpTransport::Sse { url, headers }
        }
        "http" | "streamable-http" => {
            // HTTP / Streamable HTTP → StreamableHttpClientTransport
            let url = cfg
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or("HTTP/SSE 模式缺少 url 字段")?
                .to_string();
            let headers = cfg
                .get("headers")
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| {
                            v.as_str().map(|s| (k.clone(), s.to_string()))
                        })
                        .collect::<HashMap<String, String>>()
                })
                .unwrap_or_default();
            McpTransport::Http { url, headers }
        }
        _ => McpTransport::Stdio,
    };

    let command = cfg
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let args = cfg
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let env = cfg
        .get("env")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect::<HashMap<String, String>>()
        })
        .unwrap_or_default();

    Ok(McpServerConfig {
        name: name.to_string(),
        transport,
        command,
        args,
        env,
        enabled: true, // 导入后默认启用
        disabled_tools: Vec::new(),
    })
}

/// 判断 JSON 是否为裸 server 配置（不含 mcpServers 包裹，直接是单个 server 的字段）。
///
/// 裸配置的特征：顶层有 `type`、`command`、`url` 或 `transport` 字段。
fn is_bare_server_config(v: &serde_json::Value) -> bool {
    v.is_object()
        && v.as_object().map(|obj| {
            obj.contains_key("type")
                || obj.contains_key("command")
                || obj.contains_key("url")
                || obj.contains_key("transport")
        }).unwrap_or(false)
}

/// 为裸配置自动生成名称。
///
/// 优先从 URL 提取 `主机:端口`，其次从 command 提取基名（去扩展名），
/// 最后 fallback 到 `server`（多选时追加序号）。
fn auto_generate_name(v: &serde_json::Value, index: usize) -> String {
    let base = if let Some(url) = v.get("url").and_then(|v| v.as_str()) {
        extract_host_port(url)
    } else if let Some(cmd) = v.get("command").and_then(|v| v.as_str()) {
        std::path::Path::new(cmd)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| cmd.to_string())
    } else {
        "server".to_string()
    };
    if index > 0 {
        format!("{base}-{}", index + 1)
    } else {
        base
    }
}

/// 从 URL 字符串提取 `主机:端口`（简单解析，不依赖 url crate）。
fn extract_host_port(url: &str) -> String {
    // 去掉 scheme://
    let rest = url.split("://").nth(1).unwrap_or(url);
    // 取第一个 / 之前的部分
    let host_part = rest.split('/').next().unwrap_or(rest);
    if host_part.is_empty() {
        url.to_string()
    } else {
        host_part.to_string()
    }
}

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claude_desktop_format() {
        let json = r#"{
            "mcpServers": {
                "filesystem": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "C:\\Users"]
                },
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": { "GITHUB_TOKEN": "ghp_xxx" }
                }
            }
        }"#;

        let configs = parse_external_mcp_config(McpImportSource::ClaudeDesktop, json).unwrap();
        assert_eq!(configs.len(), 2);

        let fs = configs.iter().find(|c| c.name == "filesystem").unwrap();
        assert_eq!(fs.command, "npx");
        assert_eq!(fs.args, vec!["-y", "@modelcontextprotocol/server-filesystem", "C:\\Users"]);
        assert_eq!(fs.transport, McpTransport::Stdio);
        assert!(fs.enabled);

        let gh = configs.iter().find(|c| c.name == "github").unwrap();
        assert_eq!(gh.env.get("GITHUB_TOKEN"), Some(&"ghp_xxx".to_string()));
    }

    #[test]
    fn parse_claude_code_with_http_type() {
        let json = r#"{
            "mcpServers": {
                "remote-api": {
                    "type": "http",
                    "url": "https://api.example.com/mcp",
                    "headers": {
                        "Authorization": "Bearer token123"
                    }
                },
                "local-tool": {
                    "type": "stdio",
                    "command": "node",
                    "args": ["server.js"]
                }
            }
        }"#;

        let configs = parse_external_mcp_config(McpImportSource::ClaudeCode, json).unwrap();
        assert_eq!(configs.len(), 2);

        let remote = configs.iter().find(|c| c.name == "remote-api").unwrap();
        match &remote.transport {
            McpTransport::Http { url, headers } => {
                assert_eq!(url, "https://api.example.com/mcp");
                assert_eq!(
                    headers.get("Authorization"),
                    Some(&"Bearer token123".to_string())
                );
            }
            McpTransport::Stdio | McpTransport::Sse { .. } => panic!("expected Http transport"),
        }
        assert!(remote.command.is_empty()); // HTTP 模式 command 为空

        let local = configs.iter().find(|c| c.name == "local-tool").unwrap();
        assert_eq!(local.transport, McpTransport::Stdio);
        assert_eq!(local.command, "node");
    }

    #[test]
    fn parse_cursor_format() {
        let json = r#"{
            "mcpServers": {
                "db": {
                    "command": "uvx",
                    "args": ["mcp-server-postgres"],
                    "env": { "DATABASE_URL": "postgresql://localhost/mydb" }
                }
            }
        }"#;

        let configs = parse_external_mcp_config(McpImportSource::Cursor, json).unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "db");
        assert_eq!(configs[0].command, "uvx");
    }

    #[test]
    fn parse_vscode_format() {
        let json = r#"{
            "mcp": {
                "servers": {
                    "fs": {
                        "command": "npx",
                        "args": ["-y", "@modelcontextprotocol/server-filesystem"]
                    }
                }
            }
        }"#;

        let configs = parse_external_mcp_config(McpImportSource::Vscode, json).unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "fs");
        assert_eq!(configs[0].command, "npx");
    }

    #[test]
    fn parse_generic_json_format() {
        let json = r#"{
            "mcpServers": {
                "echo": {
                    "command": "echo",
                    "args": ["hello"]
                }
            }
        }"#;

        let configs = parse_external_mcp_config(McpImportSource::Json, json).unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "echo");
    }

    #[test]
    fn parse_empty_mcp_servers_returns_empty() {
        let json = r#"{"mcpServers": {}}"#;
        let configs = parse_external_mcp_config(McpImportSource::Json, json).unwrap();
        assert!(configs.is_empty());
    }

    #[test]
    fn parse_missing_mcp_servers_returns_error() {
        let json = r#"{"other": {}}"#;
        let result = parse_external_mcp_config(McpImportSource::Json, json);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("格式不匹配"));
    }

    #[test]
    fn parse_invalid_json_returns_error() {
        let json = "not json at all";
        let result = parse_external_mcp_config(McpImportSource::Json, json);
        assert!(result.is_err());
    }

    #[test]
    fn import_source_display_names() {
        assert_eq!(McpImportSource::ClaudeDesktop.display_name(), "Claude Desktop");
        assert_eq!(McpImportSource::ClaudeCode.display_name(), "Claude Code");
        assert_eq!(McpImportSource::Cursor.display_name(), "Cursor");
        assert_eq!(McpImportSource::Windsurf.display_name(), "Windsurf");
        assert_eq!(McpImportSource::Vscode.display_name(), "VS Code");
        assert_eq!(McpImportSource::OpenCode.display_name(), "OpenCode");
        assert_eq!(McpImportSource::Codex.display_name(), "Codex");
        assert_eq!(McpImportSource::Json.display_name(), "通用 JSON");
    }

    // ── 0.13.8: 裸配置 + streamable-http 支持 ──

    #[test]
    fn parse_bare_sse_config() {
        let json = r#"{ "type": "sse", "url": "http://127.0.0.1:64342/sse", "headers": {} }"#;
        let configs = parse_external_mcp_config(McpImportSource::Json, json).unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "127.0.0.1:64342");
        match &configs[0].transport {
            McpTransport::Sse { url, .. } => {
                assert_eq!(url, "http://127.0.0.1:64342/sse");
            }
            _ => panic!("expected Sse transport for sse"),
        }
    }

    #[test]
    fn parse_bare_streamable_http_config() {
        let json = r#"{ "type": "streamable-http", "url": "http://127.0.0.1:64342/stream", "headers": {} }"#;
        let configs = parse_external_mcp_config(McpImportSource::Json, json).unwrap();
        assert_eq!(configs.len(), 1);
        match &configs[0].transport {
            McpTransport::Http { url, .. } => {
                assert_eq!(url, "http://127.0.0.1:64342/stream");
            }
            McpTransport::Stdio | McpTransport::Sse { .. } => panic!("expected Http transport for streamable-http"),
        }
    }

    #[test]
    fn parse_bare_stdio_config() {
        let json = r#"{ "type": "stdio", "env": { "IJ_MCP_SERVER_PORT": "64342" }, "command": "D:\\DevTools\\JetBrains\\RustRover\\jbr\\bin\\java", "args": [] }"#;
        let configs = parse_external_mcp_config(McpImportSource::Json, json).unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "java");
        assert_eq!(configs[0].transport, McpTransport::Stdio);
        assert_eq!(configs[0].command, "D:\\DevTools\\JetBrains\\RustRover\\jbr\\bin\\java");
        assert_eq!(configs[0].env.get("IJ_MCP_SERVER_PORT"), Some(&"64342".to_string()));
    }

    #[test]
    fn parse_bare_config_array() {
        let json = r#"[
            { "type": "sse", "url": "http://127.0.0.1:64342/sse", "headers": {} },
            { "type": "stdio", "command": "node", "args": ["server.js"] }
        ]"#;
        let configs = parse_external_mcp_config(McpImportSource::Json, json).unwrap();
        assert_eq!(configs.len(), 2);
        // 第一个从 URL 生成名称
        assert_eq!(configs[0].name, "127.0.0.1:64342");
        // SSE transport
        assert!(matches!(&configs[0].transport, McpTransport::Sse { .. }));
        // 第二个从 command 生成名称，带序号
        assert_eq!(configs[1].name, "node-2");
        assert_eq!(configs[1].command, "node");
    }

    #[test]
    fn parse_streamable_http_in_mcp_servers() {
        let json = r#"{
            "mcpServers": {
                "jetbrains": {
                    "type": "streamable-http",
                    "url": "http://127.0.0.1:64342/stream",
                    "headers": {}
                }
            }
        }"#;
        let configs = parse_external_mcp_config(McpImportSource::Json, json).unwrap();
        assert_eq!(configs.len(), 1);
        match &configs[0].transport {
            McpTransport::Http { url, .. } => {
                assert_eq!(url, "http://127.0.0.1:64342/stream");
            }
            McpTransport::Stdio | McpTransport::Sse { .. } => panic!("expected Http transport"),
        }
    }

    #[test]
    fn extract_host_port_from_url() {
        assert_eq!(extract_host_port("http://127.0.0.1:64342/sse"), "127.0.0.1:64342");
        assert_eq!(extract_host_port("https://example.com/mcp"), "example.com");
        assert_eq!(extract_host_port("http://localhost:8080"), "localhost:8080");
    }
}
