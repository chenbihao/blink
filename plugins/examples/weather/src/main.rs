//! Blink builtin 插件:天气查询 —— open-meteo(免费,无需 key)。
//!
//! 输入 "天气 北京" / "tq 北京"(首拼) → RuleRouter 前缀命中,arg="北京" 传入。
//! 流程:geocoding(城市→坐标) → weather(坐标→当前天气)。
//! settings:default_city(无参时默认城市)、temperature_unit(°C/°F)。

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
            PluginRequest::Query { id, query, settings } => {
                let resp = handle_query(id, &query, &settings);
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

fn handle_query(id: String, query: &str, settings: &Option<serde_json::Value>) -> PluginResponse {
    eprintln!("[weather] 收到查询: id={id}, query={query:?}");

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
        eprintln!("[weather] 无城市参数且未设置默认城市");
        return PluginResponse {
            id,
            items: vec![],
            error: Some(PluginError {
                code: "no_city".into(),
                message: "请输入城市名，如「天气 北京」\n或在设置中配置默认城市".into(),
            }),
        };
    }

    eprintln!("[weather] 开始查询: city={city}, fahrenheit={use_fahrenheit}");
    let mut items = Vec::new();
    let mut error = None;
    match fetch_weather(city, use_fahrenheit) {
        Some((title, subtitle)) => {
            eprintln!("[weather] 查询成功: {title}");
            items.push(PluginItem {
                title: title.clone(),
                subtitle: Some(subtitle),
                score: 1.0,
                action: PluginAction::Copy { text: title },
            })
        }
        None => {
            eprintln!("[weather] 查询失败(城市 '{city}' 未找到或无网络)");
            error = Some(PluginError {
                code: "fetch_failed".into(),
                message: format!("查询「{city}」失败，请检查城市名或网络"),
            });
        }
    }
    PluginResponse { id, items, error }
}

/// 查询天气,返回 (title, subtitle)。失败返回 None(静默降级)。
fn fetch_weather(city: &str, fahrenheit: bool) -> Option<(String, String)> {
    // 1. geocoding:城市名 → 坐标
    let encoded = urlencoding::encode(city);
    let geo_url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={encoded}&count=1&language=zh&format=json"
    );
    eprintln!("[weather] geocoding 请求: {geo_url}");
    let geo_body = ureq::get(&geo_url)
        .call()
        .map_err(|e| eprintln!("[weather] geocoding 请求失败: {e}"))
        .ok()?
        .into_body()
        .read_to_string()
        .map_err(|e| eprintln!("[weather] geocoding 读取失败: {e}"))
        .ok()?;
    eprintln!("[weather] geocoding 响应: {} bytes", geo_body.len());
    let geo: GeoResponse = serde_json::from_str(&geo_body)
        .map_err(|e| eprintln!("[weather] geocoding 解析失败: {e}"))
        .ok()?;
    let loc = geo.results.into_iter().next()?;
    eprintln!("[weather] 解析到城市: {} ({}, {})", loc.name, loc.latitude, loc.longitude);

    // 2. weather:坐标 → 当前天气
    let weather_url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,weather_code,wind_speed_10m&timezone=auto",
        loc.latitude, loc.longitude
    );
    eprintln!("[weather] weather 请求: {weather_url}");
    let w_body = ureq::get(&weather_url)
        .call()
        .map_err(|e| eprintln!("[weather] weather 请求失败: {e}"))
        .ok()?
        .into_body()
        .read_to_string()
        .map_err(|e| eprintln!("[weather] weather 读取失败: {e}"))
        .ok()?;
    eprintln!("[weather] weather 响应: {} bytes", w_body.len());
    let w: WeatherResponse = serde_json::from_str(&w_body)
        .map_err(|e| eprintln!("[weather] weather 解析失败: {e}"))
        .ok()?;
    let cur = w.current;
    eprintln!("[weather] 当前天气: temp={}, code={}, wind={}", cur.temperature_2m, cur.weather_code, cur.wind_speed_10m);

    let temp_str = if fahrenheit {
        format!("{:.0}°F", cur.temperature_2m * 9.0 / 5.0 + 32.0)
    } else {
        format!("{:.0}°C", cur.temperature_2m)
    };
    let desc = wmo_description(cur.weather_code);
    let region = if loc.admin1.is_empty() {
        loc.country.clone()
    } else {
        loc.admin1.clone()
    };

    let title = format!("{} {} {}", loc.name, temp_str, desc);
    let subtitle = format!("{} · 风速 {:.0}km/h | 按 Enter 复制", region, cur.wind_speed_10m);
    Some((title, subtitle))
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
    admin1: String,
    #[serde(default)]
    country: String,
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
