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
        /// 插件配置 settings(0.5.1 透传)。本插件消费 use_ipv6 / geo_provider。
        #[serde(default)]
        settings: Option<serde_json::Value>,
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
            PluginRequest::Query { id, settings, .. } => {
                let resp = handle_query(id, &settings);
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

fn handle_query(id: String, settings: &Option<serde_json::Value>) -> PluginResponse {
    let use_ipv6 = settings
        .as_ref()
        .and_then(|s| s.get("use_ipv6"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let geo_provider = settings
        .as_ref()
        .and_then(|s| s.get("geo_provider"))
        .and_then(|v| v.as_str())
        .unwrap_or("ip-api.com");

    let mut items = Vec::new();

    // IPv6 本机出口 IP(仅 use_ipv6=true 时查;失败静默,多数环境无 v6)
    if use_ipv6 {
        if let Some(ip) = get_local_ip_v6() {
            items.push(PluginItem {
                title: format!("本地 IPv6: {ip}"),
                subtitle: Some("按 Enter 复制".to_string()),
                score: 0.8,
                action: PluginAction::Copy { text: ip },
            });
        }
    }

    // 本机出口 IP(IPv4)
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
        Some((ip, mut loc)) => {
            // geo_provider 非 ip-api.com 时,本插件未实现其他定位服务,不显示定位。
            if geo_provider != "ip-api.com" {
                loc.clear();
            }
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

/// 获取本机默认路由 IP（IPv4）:UDP connect 到公网地址,local_addr 即出口 IP。
fn get_local_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

/// 获取本机默认路由 IP（IPv6）:UDP connect 到 Google 公网 DNS v6 地址。
/// 无 IPv6 环境时返回 None（静默）。
fn get_local_ip_v6() -> Option<String> {
    let socket = UdpSocket::bind("[::]:0").ok()?;
    socket.connect("[2001:4860:4860::8888]:80").ok()?;
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
