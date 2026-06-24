//! Blink builtin 插件:IP 查询 —— 本机 IP + 公网 IP + 定位。
//!
//! 数据来源:
//! - 本机 IP: UDP connect 到公网地址,local_addr 即出口 IP。
//! - 公网 IP: ipify.org (免费,无需 key)。
//! - 定位: ip-api.com (免费,非商业,45 req/min)。

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::{self, BufRead, Write};
use std::net::UdpSocket;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum PluginRequest {
    Query {
        id: String,
        #[allow(dead_code)]
        query: String,
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
            PluginRequest::Query { id, .. } => {
                let resp = handle_query(id);
                let json = serde_json::to_string(&resp).unwrap();
                if writeln!(stdout, "{json}").is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
            PluginRequest::Cancel { .. } => {}
        }
    }
}

fn handle_query(id: String) -> PluginResponse {
    let mut items = Vec::new();

    // 本机出口 IP
    if let Some(ip) = get_local_ip() {
        items.push(PluginItem {
            title: format!("本地 IP: {ip}"),
            subtitle: Some("按 Enter 复制".to_string()),
            score: 1.0,
            action: PluginAction::Copy { text: ip },
        });
    }

    // 公网 IP + 定位(网络查询,失败静默)
    match fetch_public_ip_info() {
        Some((ip, loc)) => {
            let subtitle = if !loc.is_empty() {
                Some(format!("{} | 按 Enter 复制", loc))
            } else {
                Some("按 Enter 复制".to_string())
            };
            items.push(PluginItem {
                title: format!("公网 IP: {ip}"),
                subtitle,
                score: 0.9,
                action: PluginAction::Copy { text: ip },
            });
        }
        None => {
            eprintln!("ip: 公网 IP 查询失败(可能无网络)");
        }
    }

    PluginResponse { id, items }
}

/// 获取本机默认路由 IP:UDP connect 到公网地址,local_addr 即出口 IP。
fn get_local_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

/// 查询公网 IP 与定位。失败返回 None(静默降级,不阻塞)。
fn fetch_public_ip_info() -> Option<(String, String)> {
    // ip-api.com 免费版:无需 key,返回 JSON。限制 45 req/min(自用足够)。
    let resp = ureq::get("http://ip-api.com/json/")
        .call()
        .ok()?;
    let body = resp.into_body().read_to_string().ok()?;
    let info: IpApiResponse = serde_json::from_str(&body).ok()?;

    if info.status != "success" {
        return None;
    }

    let ip = info.query;
    let loc = format!("{}, {}", info.city, info.country);
    Some((ip, loc))
}

#[derive(Debug, Deserialize)]
struct IpApiResponse {
    status: String,
    query: String,
    city: String,
    country: String,
}
