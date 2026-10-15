//! 搜索与工具出口共用的格式化；兼容旧数据字段。
//!
//! 条件描述多轨：供应商原始文本（和风/彩云）优先，否则 WMO 数值代码映射
//! （Open-Meteo）；两轨都缺失即"未知"，不用缺省代码伪造晴天。逐日语义三分：
//! 和风白天/夜间分段、彩云全天主导现象、Open-Meteo 当天最严重现象。
use crate::conditions::wmo_description;
use crate::model::{CurrentWeather, DailyWeather, ProviderCondition, WeatherSnapshot};
use crate::protocol::{PluginAction, PluginItem};
use crate::settings::TemperatureUnit;
use serde_json::{Value, json};

/// 归因链接的显示来源名。
pub fn provider_label(provider: &str) -> &'static str {
    match provider {
        "qweather" => "QWeather",
        "caiyun" => "彩云天气",
        _ => "Open-Meteo",
    }
}

/// 归因链接出口白名单：HTTPS 且 host 属于已知数据源域（保留声明页路径），
/// 防止响应里的任意 URL 经结果项直达 open_url。
fn attribution_url(attributions: &[String]) -> Option<String> {
    attributions.iter().find_map(|value| {
        let rest = value.strip_prefix("https://")?;
        let host = rest.split(['/', '?', '#']).next()?;
        let allowed = host == "open-meteo.com"
            || host.ends_with(".open-meteo.com")
            || host == "qweather.com"
            || host.ends_with(".qweather.com")
            || host == "caiyunapp.com"
            || host.ends_with(".caiyunapp.com");
        allowed.then(|| value.clone())
    })
}

fn condition_label(current: &CurrentWeather) -> String {
    match &current.condition {
        Some(ProviderCondition { text, .. }) => text.clone(),
        None => wmo_description(current.condition_code.unwrap_or(u32::MAX)).to_owned(),
    }
}

fn daily_condition_label(day: &DailyWeather) -> String {
    if let Some(all_day) = &day.condition {
        // 彩云逐日 skycon：全天主导现象，不标注白天/夜间分段。
        return all_day.text.clone();
    }
    match (&day.day_condition, &day.night_condition) {
        (Some(daytime), Some(nighttime)) => format!("白天{}/夜间{}", daytime.text, nighttime.text),
        (Some(daytime), None) => format!("白天{}", daytime.text),
        (None, Some(nighttime)) => format!("夜间{}", nighttime.text),
        (None, None) => wmo_description(day.condition_code.unwrap_or(u32::MAX)).to_owned(),
    }
}

fn temperature(unit: TemperatureUnit, value: Option<f64>) -> String {
    value
        .map(|v| unit.format_temperature(v))
        .unwrap_or_else(|| "未知".into())
}

fn wind(value: Option<f64>) -> String {
    value
        .map(|v| format!("{v:.0}km/h"))
        .unwrap_or_else(|| "未知".into())
}

pub fn needs_location(
    candidates: &[crate::model::ResolvedLocation],
    resolver: &str,
) -> (Vec<PluginItem>, Value) {
    let summary = "需要确认地点，请补充行政区限定或在设置中选择默认地点";
    let data = json!([{"status":"needs_location","summary":summary,"resolver":resolver,"candidates":candidates}]);
    let items = candidates
        .iter()
        .map(|location| PluginItem {
            title: location.display_name.clone(),
            subtitle: Some(format!("{summary}（例如：天气 辽宁/朝阳）")),
            score: 1.0,
            action: PluginAction::None,
            payload: Some(data[0].clone()),
        })
        .collect();
    (items, data)
}

/// 地点搜索结果包（search_locations 工具出口）；resolver 是实际使用的解析来源。
pub fn search_envelope(
    resolver: &str,
    candidates: &[crate::model::ResolvedLocation],
    truncated: bool,
    attributions: &[String],
) -> Value {
    json!([{
        "resolver": resolver,
        "summary": format!("找到 {} 个地点候选", candidates.len()),
        "count": candidates.len(),
        "truncated": truncated || candidates.len() >= 20,
        "attributions": attributions,
        "candidates": candidates,
    }])
}

pub fn render(snapshot: &WeatherSnapshot, unit: TemperatureUnit) -> (Vec<PluginItem>, Value) {
    let current = snapshot.current.as_ref();
    let temp = current
        .map(|c| temperature(unit, c.temperature_c))
        .unwrap_or_default();
    let desc = current
        .map(condition_label)
        .unwrap_or_else(|| "仅预报".into());
    let wind_speed = current.map(|c| wind(c.wind_speed_kmh)).unwrap_or_default();
    let label = provider_label(snapshot.provider);
    let summary = format!("{} {} {}", snapshot.location.name, temp, desc)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let day_night_split = snapshot
        .daily
        .iter()
        .any(|d| d.day_condition.is_some() || d.night_condition.is_some());
    let all_day_dominant = snapshot.daily.iter().any(|d| d.condition.is_some());
    let daily: Vec<Value> = snapshot
        .daily
        .iter()
        .map(|d| {
            json!({
                "date":d.date,
                "condition":daily_condition_label(d),
                "condition_code":d.condition_code,
                "condition_all_day":d.condition.as_ref().map(|c| c.text.clone()),
                "condition_all_day_code":d.condition.as_ref().map(|c| c.code.clone()),
                "condition_day":d.day_condition.as_ref().map(|c| c.text.clone()),
                "condition_night":d.night_condition.as_ref().map(|c| c.text.clone()),
                "condition_day_code":d.day_condition.as_ref().map(|c| c.code.clone()),
                "condition_night_code":d.night_condition.as_ref().map(|c| c.code.clone()),
                "temp_max":temperature(unit,d.temperature_max_c),
                "temp_min":temperature(unit,d.temperature_min_c),
                "precip_prob":d.precipitation_probability_percent.map(|v|format!("{v:.0}%")),
                "wind_max":wind(d.wind_max_kmh),
                "temperature_max_c":d.temperature_max_c,
                "temperature_min_c":d.temperature_min_c,
            })
        })
        .collect();
    let mut copy = format!(
        "{summary}\n{} · 来源：{label}",
        snapshot.location.display_name
    );
    if !wind_speed.is_empty() {
        copy.push_str(&format!(" · 风速 {wind_speed}"));
    }
    if let Some(time) = current.and_then(|c| c.valid_at.as_deref()) {
        copy.push_str(&format!("\n当前天气有效时间：{time}"));
    }
    for day in &daily {
        copy.push_str(&format!(
            "\n{} {} {}～{} · 降水概率 {} · 最大风速 {}",
            day["date"].as_str().unwrap_or(""),
            day["condition"].as_str().unwrap_or("未知"),
            day["temp_min"].as_str().unwrap_or(""),
            day["temp_max"].as_str().unwrap_or("未知"),
            day["precip_prob"].as_str().unwrap_or("未知"),
            day["wind_max"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("未知")
        ));
    }
    if !snapshot.warnings.is_empty() {
        copy.push_str(&format!("\n{}", snapshot.warnings.join("；")));
    }
    if day_night_split {
        copy.push_str("\n逐日现象按白天/夜间分段描述。");
    } else if all_day_dominant {
        copy.push_str("\n逐日天气现象为当天主导现象，不代表全天持续。");
    } else {
        copy.push_str("\n逐日天气现象表示当天最严重天气，不代表全天持续。");
    }
    let data = json!([{
        "status":snapshot.status(), "summary":summary,
        "city":snapshot.location.name, "temp":temp, "condition":desc,
        "wind_speed":wind_speed, "region":if snapshot.location.administrative_area.is_empty() { &snapshot.location.country } else { &snapshot.location.administrative_area },
        "daily":daily, "location":snapshot.location, "provider":snapshot.provider,
        "fetched_at":snapshot.fetched_at, "current":snapshot.current,
        "warnings":snapshot.warnings, "attributions":snapshot.attributions,
        "availability":{"current":current.is_some(),"daily_days":snapshot.daily.len()},
        "temperature_unit":unit,
        "daily_condition_semantics":if day_night_split { "day_night_split" } else if all_day_dominant { "all_day_dominant" } else { "most_severe_of_day" },
        "copy_text":copy,
    }]);
    let mut subtitle = format!("{} · {label}", snapshot.location.display_name);
    if !wind_speed.is_empty() {
        subtitle.push_str(&format!(" · 风速 {wind_speed}"));
    }
    if let Some(time) = current.and_then(|c| c.valid_at.as_deref()) {
        subtitle.push_str(&format!(" · {time}"));
    }
    if !snapshot.warnings.is_empty() {
        subtitle.push_str(&format!(" · {}", snapshot.warnings.join("；")));
    }
    subtitle.push_str(" | 按 Enter 复制天气摘要");
    let mut items = vec![PluginItem {
        title: summary,
        subtitle: Some(subtitle),
        score: 1.0,
        action: PluginAction::Copy { text: copy },
        payload: Some(data[0].clone()),
    }];
    // 数据来源声明项：链接经 HTTPS + 来源域白名单校验，Enter 在外部浏览器打开。
    if let Some(url) = attribution_url(&snapshot.attributions) {
        items.push(PluginItem {
            title: format!("数据来源：{label}"),
            subtitle: Some(format!("按 Enter 打开数据来源与使用声明 {url}")),
            score: 0.9,
            action: PluginAction::Open { path: url },
            payload: Some(data[0].clone()),
        });
    }
    (items, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CurrentWeather, DailyWeather, ProviderCondition, ResolvedLocation};

    fn location() -> ResolvedLocation {
        ResolvedLocation {
            name: "广州".into(),
            display_name: "广州 · 广东 · 中国".into(),
            latitude: 23.13,
            longitude: 113.26,
            coordinate_system: "wgs84",
            administrative_area: "广东".into(),
            country: "中国".into(),
            country_code: Some("CN".into()),
            administrative_areas: vec!["广东".into()],
            timezone: Some("Asia/Shanghai".into()),
            kind: "city_center",
            source: "open_meteo",
            source_id: None,
            resolved_at: "2026-10-15T00:00:00Z".into(),
            in_mainland_china: Some(true),
        }
    }

    #[test]
    fn fahrenheit_applies_to_current_daily_and_copy_without_losing_raw_codes() {
        let snapshot = WeatherSnapshot {
            location: location(),
            provider: "open_meteo",
            fetched_at: "2026-10-15T00:00:00Z".into(),
            current: Some(CurrentWeather {
                temperature_c: Some(0.0),
                condition_code: Some(96),
                condition: None,
                wind_speed_kmh: None,
                valid_at: None,
            }),
            daily: vec![DailyWeather {
                date: "2026-10-15".into(),
                condition_code: None,
                condition: None,
                day_condition: None,
                night_condition: None,
                temperature_max_c: Some(10.0),
                temperature_min_c: Some(0.0),
                precipitation_probability_percent: None,
                wind_max_kmh: None,
            }],
            warnings: vec![],
            attributions: vec![],
        };
        let (items, data) = render(&snapshot, TemperatureUnit::Fahrenheit);
        assert_eq!(data[0]["temp"], "32°F");
        assert_eq!(data[0]["daily"][0]["temp_max"], "50°F");
        assert_eq!(data[0]["daily"][0]["temp_min"], "32°F");
        assert_eq!(data[0]["current"]["condition_code"], 96);
        assert_eq!(data[0]["daily"][0]["condition"], "未知");
        assert!(data[0]["daily"][0]["precip_prob"].is_null());
        match &items[0].action {
            PluginAction::Copy { text } => {
                assert!(text.contains("雷暴伴小冰雹"));
                assert!(text.contains("50°F"));
                assert_ne!(text, "广州");
            }
            _ => panic!("weather item must copy summary"),
        }
    }

    #[test]
    fn qweather_day_night_split_and_attribution_item() {
        let snapshot = WeatherSnapshot {
            location: location(),
            provider: "qweather",
            fetched_at: "2026-10-15T00:00:00Z".into(),
            current: Some(CurrentWeather {
                temperature_c: Some(25.0),
                condition_code: None,
                condition: Some(ProviderCondition {
                    text: "小雨".into(),
                    code: "305".into(),
                }),
                wind_speed_kmh: Some(17.06),
                valid_at: None,
            }),
            daily: vec![DailyWeather {
                date: "2026-10-15".into(),
                condition_code: None,
                condition: None,
                day_condition: Some(ProviderCondition {
                    text: "小雨".into(),
                    code: "305".into(),
                }),
                night_condition: Some(ProviderCondition {
                    text: "多云".into(),
                    code: "101".into(),
                }),
                temperature_max_c: Some(29.94),
                temperature_min_c: Some(20.93),
                precipitation_probability_percent: Some(64.0),
                wind_max_kmh: Some(8.0),
            }],
            warnings: vec![],
            attributions: vec!["https://developer.qweather.com/attribution.html".into()],
        };
        let (items, data) = render(&snapshot, TemperatureUnit::Celsius);
        assert_eq!(data[0]["provider"], "qweather");
        assert_eq!(data[0]["condition"], "小雨");
        // WMO 数值代码缺失不为 0，未知代码不落进晴
        assert_eq!(data[0]["daily"][0]["condition"], "白天小雨/夜间多云");
        assert_eq!(data[0]["daily"][0]["condition_day"], "小雨");
        assert_eq!(data[0]["daily"][0]["condition_night_code"], "101");
        assert_eq!(data[0]["daily_condition_semantics"], "day_night_split");
        assert!(items[0].subtitle.as_deref().unwrap().contains("QWeather"));
        // 第二项是可打开的来源声明
        assert_eq!(items.len(), 2);
        match &items[1].action {
            PluginAction::Open { path } => {
                assert_eq!(path, "https://developer.qweather.com/attribution.html");
            }
            _ => panic!("attribution item must open the source page"),
        }
    }

    #[test]
    fn attribution_link_whitelist_rejects_unknown_hosts() {
        let snapshot = WeatherSnapshot {
            location: location(),
            provider: "open_meteo",
            fetched_at: "2026-10-15T00:00:00Z".into(),
            current: None,
            daily: vec![],
            warnings: vec!["仅预报".into()],
            attributions: vec![
                "http://qweather.com/insecure".into(),
                "https://evil.example.com/qweather".into(),
                "https://open-meteo.com/".into(),
            ],
        };
        let (items, data) = render(&snapshot, TemperatureUnit::Celsius);
        assert_eq!(items.len(), 2);
        match &items[1].action {
            PluginAction::Open { path } => assert_eq!(path, "https://open-meteo.com/"),
            _ => panic!("expected whitelisted attribution"),
        }
        assert_eq!(data[0]["status"], "partial");
    }
}
