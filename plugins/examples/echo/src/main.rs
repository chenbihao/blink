//! Blink 示例插件:echo —— 回显输入,验证 stdio JSONL 协议(见 §3.2)。
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

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum PluginRequest {
    Query {
        id: String,
        query: String,
        #[serde(default)]
        #[allow(dead_code)]
        context: serde_json::Value,
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
    Copy { text: String },
    #[allow(dead_code)]
    Open { path: String },
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
            PluginRequest::Query { id, query, .. } => {
                let resp = handle_query(id, query);
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

/// 回显:把 query 原样作为一条 Copy 结果(Enter 复制 "echo: <query>")。
fn handle_query(id: String, query: String) -> PluginResponse {
    eprintln!("echo: handling query {query:?}");
    PluginResponse {
        id,
        items: vec![PluginItem {
            title: format!("echo: {query}"),
            subtitle: Some("Blink Rust 示例插件".to_string()),
            score: 0.8,
            action: PluginAction::Copy { text: format!("echo: {query}") },
        }],
    }
}
