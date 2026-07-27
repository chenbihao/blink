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
        McpImportSource::Json => None, // 通用 JSON 无固定路径
    }
}

/// 解析外部 agent 的 MCP 配置。
///
/// 各 agent 格式大同小异，核心差异在：
/// - VS Code 用 `mcp.servers` 而非 `mcpServers`
/// - Claude Code 支持 `type: "sse"` / `type: "http"` 字段
///
/// 返回待导入的 server 列表（不含去重——调用方决定覆盖/跳过策略）。
pub fn parse_external_mcp_config(
    source: McpImportSource,
    json: &str,
) -> Result<Vec<McpServerConfig>, String> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("JSON 解析失败: {e}"))?;

    let servers = match source {
        McpImportSource::Vscode => v["mcp"]["servers"].as_object(),
        _ => v["mcpServers"].as_object(),
    };

    let Some(servers) = servers else {
        return Err("配置中未找到 mcpServers 字段".into());
    };

    let mut configs = Vec::new();
    for (name, cfg) in servers {
        let config = parse_single_server(name, cfg)?;
        configs.push(config);
    }
    Ok(configs)
}

/// 解析单个 server 配置。
///
/// Claude Code 格式支持 `type` 字段：`"stdio"` | `"sse"` | `"http"`。
fn parse_single_server(name: &str, cfg: &serde_json::Value) -> Result<McpServerConfig, String> {
    // Claude Code 格式：type: "stdio" | "sse" | "http"
    let transport_type = cfg
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("stdio");

    let transport = match transport_type {
        "sse" | "http" => {
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
            McpTransport::Stdio => panic!("expected Http transport"),
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
        assert!(result.unwrap_err().contains("mcpServers"));
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
        assert_eq!(McpImportSource::Json.display_name(), "通用 JSON");
    }
}
