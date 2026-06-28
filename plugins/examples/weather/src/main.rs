//! Blink builtin 插件:天气查询 —— open-meteo(免费,无需 key)。
//!
//! 使用插件 HTTP 代理协议：插件不直接联网，通过 core 代理发起 HTTP 请求。
//!
//! 数据流：
//! 1. core → 插件：Query 请求（带 city）
//! 2. 插件 → core：HttpRequest（geocoding 查城市坐标）
//! 3. core → 插件：HttpResponse（返回坐标）
//! 4. 插件 → core：HttpRequest（weather 查天气）
//! 5. core → 插件：HttpResponse（返回天气）
//! 6. 插件 → core：Response（整理结果）

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
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
}

/// HTTP 请求消息
#[derive(Debug, Serialize)]
struct HttpRequest {
    id: String,
    method: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    timeout_ms: u64,
}

/// 插件响应
#[derive(Debug, Serialize)]
struct PluginResponse {
    id: String,
    items: Vec<PluginItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<PluginError>,
}

#[derive(Debug, Serialize)]
struct PluginError {
    code: String,
    message: String,
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

/// HTTP 请求阶段
#[derive(Debug)]
enum WeatherStage {
    /// 等待 geocoding 结果（城市→坐标）
    Geocoding {
        query_id: String,
        city: String,
        use_fahrenheit: bool,
    },
    /// 等待 weather 结果（坐标→天气）
    Weather {
        query_id: String,
        city_name: String,
        admin1: String,
        country: String,
        use_fahrenheit: bool,
    },
}

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    // http_request_id -> WeatherStage
    let pending: Arc<Mutex<HashMap<String, WeatherStage>>> = Arc::new(Mutex::new(HashMap::new()));
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
            CoreToPlugin::Query { id, query, settings } => {
                let default_city = settings
                    .as_ref()
                    .and_then(|s| s.get("default_city"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let use_fahrenheit = settings
                    .as_ref()
                    .and_then(|s| s.get("temperature_unit"))
                    .and_then(|v| v.as_str())
                    .map(|s| s == "fahrenheit")
                    .unwrap_or(false);

                // 城市:优先 query 参数,其次默认城市
                let city = query.trim();
                let city = if city.is_empty() { default_city.trim() } else { city };

                if city.is_empty() {
                    let resp = PluginToCore::Response(PluginResponse {
                        id,
                        items: vec![],
                        error: Some(PluginError {
                            code: "no_city".into(),
                            message: "请输入城市名，如「天气 北京」\n或在设置中配置默认城市".into(),
                        }),
                    });
                    send_message(&mut stdout, &resp);
                    continue;
                }

                // 第一步：geocoding（城市→坐标）
                let encoded = urlencoding::encode(city);
                let url = format!(
                    "https://geocoding-api.open-meteo.com/v1/search?name={encoded}&count=1&language=zh&format=json"
                );
                let http_id = format!("geo_{}", chrono::Local::now().timestamp_millis());
                pending.lock().unwrap().insert(
                    http_id.clone(),
                    WeatherStage::Geocoding {
                        query_id: id,
                        city: city.into(),
                        use_fahrenheit,
                    },
                );

                let http_req = PluginToCore::HttpRequest(HttpRequest {
                    id: http_id,
                    method: "GET".into(),
                    url,
                    body: None,
                    timeout_ms: 15000,
                });
                send_message(&mut stdout, &http_req);
            }
            CoreToPlugin::HttpResponse { id, status, body, error } => {
                let mut pending_guard = pending_clone.lock().unwrap();
                let Some(stage) = pending_guard.remove(&id) else {
                    eprintln!("http response for unknown request: {id}");
                    continue;
                };

                match stage {
                    WeatherStage::Geocoding { query_id, city, use_fahrenheit } => {
                        // geocoding 响应
                        if error.is_some() || status != 200 {
                            let resp = PluginToCore::Response(PluginResponse {
                                id: query_id,
                                items: vec![],
                                error: Some(PluginError {
                                    code: "fetch_failed".into(),
                                    message: format!("查询「{city}」失败，请检查网络"),
                                }),
                            });
                            send_message(&mut stdout, &resp);
                            continue;
                        }

                        let Some(body) = body else {
                            let resp = PluginToCore::Response(PluginResponse {
                                id: query_id,
                                items: vec![],
                                error: Some(PluginError {
                                    code: "fetch_failed".into(),
                                    message: format!("查询「{city}」失败：无响应"),
                                }),
                            });
                            send_message(&mut stdout, &resp);
                            continue;
                        };

                        let geo: GeoResponse = match serde_json::from_str(&body) {
                            Ok(g) => g,
                            Err(e) => {
                                let resp = PluginToCore::Response(PluginResponse {
                                    id: query_id,
                                    items: vec![],
                                    error: Some(PluginError {
                                        code: "fetch_failed".into(),
                                        message: format!("解析「{city}」失败：{e}"),
                                    }),
                                });
                                send_message(&mut stdout, &resp);
                                continue;
                            }
                        };

                        let Some(loc) = geo.results.into_iter().next() else {
                            let resp = PluginToCore::Response(PluginResponse {
                                id: query_id,
                                items: vec![],
                                error: Some(PluginError {
                                    code: "fetch_failed".into(),
                                    message: format!("未找到城市「{city}」"),
                                }),
                            });
                            send_message(&mut stdout, &resp);
                            continue;
                        };

                        // 第二步：weather API（坐标→天气）
                        let weather_url = format!(
                            "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,weather_code,wind_speed_10m&timezone=auto",
                            loc.latitude, loc.longitude
                        );
                        let http_id = format!("w_{}", chrono::Local::now().timestamp_millis());
                        pending_guard.insert(
                            http_id.clone(),
                            WeatherStage::Weather {
                                query_id,
                                city_name: loc.name,
                                admin1: loc.admin1.unwrap_or_default(),
                                country: loc.country.unwrap_or_default(),
                                use_fahrenheit,
                            },
                        );

                        let http_req = PluginToCore::HttpRequest(HttpRequest {
                            id: http_id,
                            method: "GET".into(),
                            url: weather_url,
                            body: None,
                            timeout_ms: 15000,
                        });
                        send_message(&mut stdout, &http_req);
                    }
                    WeatherStage::Weather { query_id, city_name, admin1, country, use_fahrenheit } => {
                        // weather 响应
                        if error.is_some() || status != 200 {
                            let resp = PluginToCore::Response(PluginResponse {
                                id: query_id,
                                items: vec![],
                                error: Some(PluginError {
                                    code: "fetch_failed".into(),
                                    message: format!("查询「{city_name}」天气失败，请检查网络"),
                                }),
                            });
                            send_message(&mut stdout, &resp);
                            continue;
                        }

                        let Some(body) = body else {
                            let resp = PluginToCore::Response(PluginResponse {
                                id: query_id,
                                items: vec![],
                                error: Some(PluginError {
                                    code: "fetch_failed".into(),
                                    message: format!("查询「{city_name}」天气失败：无响应"),
                                }),
                            });
                            send_message(&mut stdout, &resp);
                            continue;
                        };

                        let weather: WeatherResponse = match serde_json::from_str(&body) {
                            Ok(w) => w,
                            Err(e) => {
                                let resp = PluginToCore::Response(PluginResponse {
                                    id: query_id,
                                    items: vec![],
                                    error: Some(PluginError {
                                        code: "fetch_failed".into(),
                                        message: format!("解析天气失败：{e}"),
                                    }),
                                });
                                send_message(&mut stdout, &resp);
                                continue;
                            }
                        };

                        let cur = weather.current;
                        let temp_str = if use_fahrenheit {
                            format!("{:.0}°F", cur.temperature_2m * 9.0 / 5.0 + 32.0)
                        } else {
                            format!("{:.0}°C", cur.temperature_2m)
                        };
                        let desc = wmo_description(cur.weather_code);
                        let region = if admin1.is_empty() { country } else { admin1 };

                        let title = format!("{city_name} {temp_str} {desc}");
                        let subtitle = format!("{region} · 风速 {:.0}km/h | 按 Enter 复制", cur.wind_speed_10m);

                        let resp = PluginToCore::Response(PluginResponse {
                            id: query_id,
                            items: vec![PluginItem {
                                title,
                                subtitle: Some(subtitle),
                                score: 1.0,
                                action: PluginAction::Copy { text: city_name },
                            }],
                            error: None,
                        });
                        send_message(&mut stdout, &resp);
                    }
                }
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

/// WMO weather_code → 中文描述(简化版)。
fn wmo_description(code: u32) -> &'static str {
    match code {
        0 => "晴",
        1 | 2 | 3 => "多云",
        45 | 48 => "雾",
        51 | 53 | 55 => "毛毛雨",
        56 | 57 | 66 | 67 => "冻雨",
        61 | 63 | 65 => "雨",
        71 | 73 | 75 | 77 => "雪",
        80 | 81 | 82 => "阵雨",
        85 | 86 => "阵雪",
        95 => "雷暴",
        96 | 99 => "雷暴冰雹",
        _ => "未知",
    }
}

// ── API 响应结构 ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GeoResponse {
    results: Vec<GeoLocation>,
}

#[derive(Debug, Deserialize)]
struct GeoLocation {
    name: String,
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    admin1: Option<String>,
    #[serde(default)]
    country: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WeatherResponse {
    current: CurrentWeather,
}

#[derive(Debug, Deserialize)]
struct CurrentWeather {
    temperature_2m: f64,
    weather_code: u32,
    wind_speed_10m: f64,
}
