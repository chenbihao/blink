//! Blink builtin 插件:IP 查询 —— 本机 IP + 公网 IP + 定位。
//!
//! 使用插件 HTTP 代理协议：插件不直接联网，通过 core 代理发起 HTTP 请求。
//!
//! 数据流：
//! 1. core → 插件：Query 请求
//! 2. 插件 → core：HttpRequest（请求 ip-api.com）
//! 3. core → 插件：HttpResponse（返回公网 IP+定位）
//! 4. 插件 → core：Response（整理结果）

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// core → 插件的所有消息（与主程序 protocol.rs 保持一致）。
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CoreToPlugin {
    /// 查询请求
    #[serde(rename = "query")]
    Query {
        id: String,
        #[allow(dead_code)]
        query: String,
        #[serde(default)]
        settings: Option<serde_json::Value>,
    },
    /// HTTP 响应（core 代理请求的结果）
    #[serde(rename = "http_response")]
    HttpResponse {
        id: String,
        status: u16,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        error: Option<String>,
    },
    /// 取消请求（可忽略）
    #[serde(rename = "cancel")]
    Cancel {
        #[allow(dead_code)]
        id: String,
    },
    /// tool-call 请求（0.9.3）
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        tool_name: String,
        #[serde(default)]
        arguments: serde_json::Value,
        #[serde(default)]
        settings: Option<serde_json::Value>,
    },
}

/// 插件 → core 的上行消息
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum PluginToCore {
    /// 查询结果响应
    #[serde(rename = "response")]
    Response(PluginResponse),
    /// HTTP 请求（请求 core 代理）
    #[serde(rename = "http_request")]
    HttpRequest(HttpRequest),
    /// tool-call 结果（0.9.3，轨道 B 旧协议）
    #[serde(rename = "tool_result")]
    ToolResult(ToolResultPayload),
    /// 轨道 A 纯数据 tool 结果（0.14.3）——manifest 配了 projection 的 tool 走此路径。
    #[serde(rename = "raw_result")]
    RawResult(RawToolResult),
}

/// tool-call 结果（与 PluginResponse 统一格式，轨道 B 旧协议）
#[derive(Debug, Serialize)]
struct ToolResultPayload {
    id: String,
    items: Vec<PluginItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<PluginError>,
}

/// 轨道 A 纯数据 tool 结果（0.14.3）——插件只吐纯 data，投影规则在 manifest。
#[derive(Debug, Serialize)]
struct RawToolResult {
    id: String,
    data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<PluginError>,
}

#[derive(Debug, Serialize)]
struct PluginError {
    code: String,
    message: String,
}

/// HTTP 请求消息
#[derive(Debug, Serialize)]
struct HttpRequest {
    id: String,
    method: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    10000
}

/// 插件响应
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

/// 挂起的查询上下文：等待 HTTP 响应
struct PendingQuery {
    query_id: String,
    use_ipv6: bool,
    local_ip: Option<String>,
    local_ipv6: Option<String>,
    /// 是否来自 tool-call（决定返回 Response 还是 ToolResult）
    is_tool_call: bool,
}

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    // http_request_id -> PendingQuery
    let pending: Arc<Mutex<HashMap<String, PendingQuery>>> = Arc::new(Mutex::new(HashMap::new()));
    let pending_clone = Arc::clone(&pending);

    // 单线程：顺序处理 stdin 行
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }

        let msg: CoreToPlugin = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("invalid message: {e}");
                continue;
            }
        };

        match msg {
            CoreToPlugin::Query { id, settings, .. } => {
                let use_ipv6 = settings
                    .as_ref()
                    .and_then(|s| s.get("use_ipv6"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                // 本地 IP 同步获取（UDP connect，无 IO 等待）
                let local_ip = get_local_ip();
                let local_ipv6 = if use_ipv6 { get_local_ip_v6() } else { None };

                // 公网 IP 通过 core HTTP 代理获取
                let http_id = format!("ip_{}", chrono::Local::now().timestamp_millis());
                pending.lock().unwrap().insert(
                    http_id.clone(),
                    PendingQuery {
                        query_id: id,
                        use_ipv6,
                        local_ip,
                        local_ipv6,
                        is_tool_call: false,
                    },
                );

                // 向 core 发起 HTTP 请求
                let http_req = PluginToCore::HttpRequest(HttpRequest {
                    id: http_id,
                    method: "GET".into(),
                    url: "http://ip-api.com/json/?fields=status,query,city,country".into(),
                    body: None,
                    timeout_ms: 10000,
                });
                send_message(&mut stdout, &http_req);
            }
            CoreToPlugin::HttpResponse {
                id,
                status,
                body,
                error,
            } => {
                let mut pending_guard = pending_clone.lock().unwrap();
                let Some(ctx) = pending_guard.remove(&id) else {
                    eprintln!("http response for unknown request: {id}");
                    continue;
                };

                let mut items = Vec::new();
                let mut raw_ips: Vec<serde_json::Value> = Vec::new();

                // 本地 IPv6
                if let Some(ip) = ctx.local_ipv6 {
                    items.push(PluginItem {
                        title: format!("本地 IPv6: {ip}"),
                        subtitle: Some("按 Enter 复制".to_string()),
                        score: 0.8,
                        action: PluginAction::Copy { text: ip.clone() },
                    });
                    raw_ips.push(serde_json::json!({ "ip": ip, "type": "本地 IPv6" }));
                }

                // 本地 IPv4
                if let Some(ip) = ctx.local_ip {
                    items.push(PluginItem {
                        title: format!("本地 IP: {ip}"),
                        subtitle: Some("按 Enter 复制".to_string()),
                        score: 1.0,
                        action: PluginAction::Copy { text: ip.clone() },
                    });
                    raw_ips.push(serde_json::json!({ "ip": ip, "type": "本地 IP" }));
                }

                // 公网 IP 结果
                if error.is_none() && status == 200 {
                    if let Some(body) = body {
                        if let Ok(info) = serde_json::from_str::<IpApiResponse>(&body) {
                            if info.status == "success" {
                                let location = if !info.city.is_empty() {
                                    format!("{}, {}", info.city, info.country)
                                } else {
                                    info.country
                                };
                                items.push(PluginItem {
                                    title: format!("公网 IP: {}", info.query),
                                    subtitle: Some(format!("{location} | 按 Enter 复制")),
                                    score: 0.9,
                                    action: PluginAction::Copy { text: info.query.clone() },
                                });
                                raw_ips.push(serde_json::json!({
                                    "ip": info.query,
                                    "type": format!("公网 IP · {location}")
                                }));
                            }
                        }
                    }
                }

                // 0.14.3: tool-call 走轨道 A（返回纯 data），query 走旧协议（返回 items）
                if ctx.is_tool_call {
                    let resp = PluginToCore::RawResult(RawToolResult {
                        id: ctx.query_id,
                        data: serde_json::Value::Array(raw_ips),
                        error: None,
                    });
                    send_message(&mut stdout, &resp);
                } else {
                    let resp = PluginToCore::Response(PluginResponse {
                        id: ctx.query_id,
                        items,
                    });
                    send_message(&mut stdout, &resp);
                }
            }
            CoreToPlugin::ToolCall {
                id,
                tool_name: _,
                arguments,
                settings,
            } => {
                // 0.9.3: tool-call 与 query 共用逻辑，只是返回 ToolResult 格式
                let use_ipv6 = settings
                    .as_ref()
                    .and_then(|s| s.get("use_ipv6"))
                    .and_then(|v| v.as_bool())
                    .or_else(|| arguments.get("include_ipv6").and_then(|v| v.as_bool()))
                    .unwrap_or(false);

                let local_ip = get_local_ip();
                let local_ipv6 = if use_ipv6 { get_local_ip_v6() } else { None };

                let http_id = format!("tc_{}", chrono::Local::now().timestamp_millis());
                pending.lock().unwrap().insert(
                    http_id.clone(),
                    PendingQuery {
                        query_id: id,
                        use_ipv6,
                        local_ip,
                        local_ipv6,
                        is_tool_call: true,
                    },
                );

                let http_req = PluginToCore::HttpRequest(HttpRequest {
                    id: http_id,
                    method: "GET".into(),
                    url: "http://ip-api.com/json/?fields=status,query,city,country".into(),
                    body: None,
                    timeout_ms: 10000,
                });
                send_message(&mut stdout, &http_req);
            }
            CoreToPlugin::Cancel { .. } => {
                // 不支持取消，忽略
            }
        }
    }
}

fn send_message<W: Write, S: Serialize>(writer: &mut W, msg: &S) {
    let json = serde_json::to_string(msg).unwrap();
    let _ = writeln!(writer, "{json}");
    let _ = writer.flush();
}

/// 获取本机默认路由 IP（IPv4）:UDP connect 到公网地址,local_addr 即出口 IP。
fn get_local_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

/// 获取本机默认路由 IP（IPv6）:UDP connect 到 Google 公网 DNS v6 地址。
fn get_local_ip_v6() -> Option<String> {
    let socket = UdpSocket::bind("[::]:0").ok()?;
    socket.connect("[2001:4860:4860::8888]:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

#[derive(Debug, Deserialize)]
struct IpApiResponse {
    status: String,
    query: String,
    #[serde(default)]
    city: String,
    #[serde(default)]
    country: String,
}
