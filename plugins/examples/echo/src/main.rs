//! Blink 示例插件:echo —— 回显输入 + 显示上下文快照,验证 Context 链路。
//!
//! 任何能读 stdin / 写 stdout 的程序都是插件。本插件用 Rust 写、阻塞式 stdio:
//! 每行读一个 JSON 请求,处理后写一行 JSON 响应。stderr 用于日志(core 汇入 tracing)。
//!
//! 协议 struct 目前各端各持一份副本(core 在 src/plugin/protocol.rs);
//! 后续抽 blink-plugin-sdk crate 共享。
//!
//! windows_subsystem=windows:插件由 core 以管道方式拉起,无需控制台;不加会闪黑窗。
//! stdio 管道在 windows subsystem 下仍正常工作(管道由父进程创建,不依赖控制台)。
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

/// 查询上下文快照（从 core 传来，字段为 Option，值缺失则为 None）
#[derive(Debug, Deserialize, Default)]
struct PluginQueryContext {
    #[allow(dead_code)] // context 协议字段,echo 不消费(其余字段已在用),保留以体现完整快照
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub foreground_app: Option<String>,
    #[serde(default)]
    pub window_title: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum PluginRequest {
    Query {
        id: String,
        query: String,
        #[serde(default)]
        context: PluginQueryContext,
    },
    Cancel {
        #[allow(dead_code)]
        id: String,
    },
}

#[derive(Debug, Serialize)]
struct PluginResponse {
    id: String,
    items: Vec<PluginItem>,
}

#[derive(Debug, Serialize)]
struct PluginItem {
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    score: f32,
    action: PluginAction,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum PluginAction {
    Copy {
        text: String,
    },
    #[allow(dead_code)]
    Open {
        path: String,
    },
}

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let req: PluginRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("invalid request: {e}");
                continue;
            }
        };
        match req {
            PluginRequest::Query { id, query, context } => {
                let resp = handle_query(id, query, context);
                let json = serde_json::to_string(&resp).unwrap();
                if writeln!(stdout, "{json}").is_err() {
                    break; // stdout 关闭(core 退出),结束
                }
                let _ = stdout.flush();
            }
            PluginRequest::Cancel { .. } => {
                // 本切片不实现 cancel
            }
        }
    }
}

/// 回显 query + 显示 Context 快照（验证链路通了）。
fn handle_query(id: String, query: String, ctx: PluginQueryContext) -> PluginResponse {
    eprintln!(
        "echo: query={query:?}, foreground_app={:?}",
        ctx.foreground_app
    );

    let mut items = Vec::new();

    // 1. 主结果：回显
    items.push(PluginItem {
        title: format!("echo: {query}"),
        subtitle: Some("Blink Rust 示例插件".to_string()),
        score: 1.0,
        action: PluginAction::Copy {
            text: format!("echo: {query}"),
        },
    });

    // 2. 前台应用信息
    if let Some(app) = &ctx.foreground_app {
        items.push(PluginItem {
            title: format!("前台应用: {app}"),
            subtitle: ctx.window_title.clone(),
            score: 0.9,
            action: PluginAction::Copy { text: app.clone() },
        });
    }

    // 3. 剪贴板内容（0.8.6 §8.2.2：clipboard_text 已从协议移除，插件须走 Suggestion 域）

    PluginResponse { id, items }
}
