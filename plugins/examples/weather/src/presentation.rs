//! 搜索与工具出口共用的格式化；兼容旧数据字段。
use crate::conditions::wmo_description;
use crate::model::WeatherSnapshot;
use crate::protocol::{PluginAction, PluginItem};
use crate::settings::TemperatureUnit;
use serde_json::{Value, json};

fn condition(code: Option<u32>) -> &'static str {
    code.map(wmo_description).unwrap_or("未知")
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

pub fn render(snapshot: &WeatherSnapshot, unit: TemperatureUnit) -> (Vec<PluginItem>, Value) {
    let current = snapshot.current.as_ref();
    let temp = current
        .map(|c| temperature(unit, c.temperature_c))
        .unwrap_or_default();
    let desc = current
        .map(|c| condition(c.condition_code))
        .unwrap_or("仅预报");
    let wind_speed = current.map(|c| wind(c.wind_speed_kmh)).unwrap_or_default();
    let summary = format!("{} {} {}", snapshot.location.name, temp, desc)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let daily: Vec<Value> = snapshot
        .daily
        .iter()
        .map(|d| {
            json!({
                "date":d.date,
                "condition":condition(d.condition_code),
                "condition_code":d.condition_code,
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
        "{summary}\n{} · 来源：Open-Meteo",
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
    copy.push_str("\n逐日天气现象表示当天最严重天气，不代表全天持续。");
    let data = json!([{
        "status":snapshot.status(), "summary":summary,
        "city":snapshot.location.name, "temp":temp, "condition":desc,
        "wind_speed":wind_speed, "region":if snapshot.location.administrative_area.is_empty() { &snapshot.location.country } else { &snapshot.location.administrative_area },
        "daily":daily, "location":snapshot.location, "provider":snapshot.provider,
        "fetched_at":snapshot.fetched_at, "current":snapshot.current,
        "warnings":snapshot.warnings, "attributions":snapshot.attributions,
        "availability":{"current":current.is_some(),"daily_days":snapshot.daily.len()},
        "temperature_unit":unit, "daily_condition_semantics":"most_severe_of_day",
        "copy_text":copy,
    }]);
    let mut subtitle = format!("{} · Open-Meteo", snapshot.location.display_name);
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
    let items = vec![PluginItem {
        title: summary,
        subtitle: Some(subtitle),
        score: 1.0,
        action: PluginAction::Copy { text: copy },
        payload: Some(data[0].clone()),
    }];
    (items, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CurrentWeather, DailyWeather, ResolvedLocation};

    #[test]
    fn fahrenheit_applies_to_current_daily_and_copy_without_losing_raw_codes() {
        let snapshot = WeatherSnapshot {
            location: ResolvedLocation {
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
            },
            provider: "open_meteo",
            fetched_at: "2026-10-15T00:00:00Z".into(),
            current: Some(CurrentWeather {
                temperature_c: Some(0.0),
                condition_code: Some(96),
                wind_speed_kmh: None,
                valid_at: None,
            }),
            daily: vec![DailyWeather {
                date: "2026-10-15".into(),
                condition_code: None,
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
        }
    }
}
