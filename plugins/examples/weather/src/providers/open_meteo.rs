//! Open-Meteo 请求与数值归一化，缺失值不伪造为晴天/零值。
use crate::model::{CurrentWeather, DailyWeather, ResolvedLocation, WeatherSnapshot};
use crate::protocol::PluginError;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime};
use serde::Deserialize;

pub fn geocoding_url(city: &str) -> String {
    format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=zh&format=json",
        urlencoding::encode(city)
    )
}

pub fn weather_url(location: &ResolvedLocation) -> String {
    format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,weather_code,wind_speed_10m&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,wind_speed_10m_max&temperature_unit=celsius&wind_speed_unit=kmh&timezone=auto&forecast_days=7",
        location.latitude, location.longitude
    )
}

#[derive(Debug, Deserialize)]
pub struct GeoResponse {
    #[serde(default)]
    pub results: Vec<GeoLocation>,
}

#[derive(Debug, Deserialize)]
pub struct GeoLocation {
    #[serde(default)]
    pub id: Option<u64>,
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub admin1: Option<String>,
    #[serde(default)]
    pub admin2: Option<String>,
    #[serde(default)]
    pub admin3: Option<String>,
    #[serde(default)]
    pub admin4: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WeatherResponse {
    #[serde(default)]
    current: Option<CurrentDto>,
    #[serde(default)]
    daily: Option<DailyDto>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    utc_offset_seconds: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct CurrentDto {
    #[serde(default)]
    temperature_2m: Option<f64>,
    #[serde(default)]
    weather_code: Option<u32>,
    #[serde(default)]
    wind_speed_10m: Option<f64>,
    #[serde(default)]
    time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DailyDto {
    #[serde(default)]
    time: Vec<String>,
    #[serde(default)]
    weather_code: Vec<Option<u32>>,
    #[serde(default)]
    temperature_2m_max: Vec<Option<f64>>,
    #[serde(default)]
    temperature_2m_min: Vec<Option<f64>>,
    #[serde(default)]
    precipitation_probability_max: Vec<Option<f64>>,
    #[serde(default)]
    wind_speed_10m_max: Vec<Option<f64>>,
}

fn invalid_response() -> PluginError {
    PluginError {
        code: "invalid_response".into(),
        message: "天气服务返回了无法解析的数据，请稍后重试".into(),
    }
}

pub fn parse_location(body: &str) -> Result<ResolvedLocation, PluginError> {
    let geo: GeoResponse = serde_json::from_str(body).map_err(|_| invalid_response())?;
    let loc = geo.results.into_iter().next().ok_or_else(|| PluginError {
        code: "location_not_found".into(),
        message: "未找到城市，请检查名称或配置默认城市".into(),
    })?;
    if !loc.latitude.is_finite()
        || !loc.longitude.is_finite()
        || !(-90.0..=90.0).contains(&loc.latitude)
        || !(-180.0..=180.0).contains(&loc.longitude)
        || loc.name.trim().is_empty()
    {
        return Err(invalid_response());
    }
    let administrative_area = loc.admin1.clone().unwrap_or_default();
    let administrative_areas = [loc.admin1, loc.admin2, loc.admin3, loc.admin4]
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>();
    let country = loc.country.unwrap_or_default();
    let display_name = [&loc.name, &administrative_area, &country]
        .into_iter()
        .filter(|v| !v.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ");
    Ok(ResolvedLocation {
        name: loc.name,
        display_name,
        latitude: loc.latitude,
        longitude: loc.longitude,
        coordinate_system: "wgs84",
        administrative_area,
        country,
        country_code: loc.country_code,
        administrative_areas,
        timezone: loc.timezone,
        kind: "city_center",
        source: "open_meteo",
        source_id: loc.id.map(|id| format!("open_meteo:{id}")),
        resolved_at: chrono::Utc::now().to_rfc3339(),
    })
}

fn number(values: &[Option<f64>], index: usize) -> Option<f64> {
    values
        .get(index)
        .copied()
        .flatten()
        .filter(|v| v.is_finite())
}

fn valid_time(time: Option<&str>, offset: Option<i32>) -> Option<String> {
    let time = time?;
    if let Ok(value) = DateTime::parse_from_rfc3339(time) {
        return Some(value.to_rfc3339());
    }
    let value = NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    let offset = FixedOffset::east_opt(offset?)?;
    Some(value.and_local_timezone(offset).single()?.to_rfc3339())
}

pub fn parse_weather(
    body: &str,
    mut location: ResolvedLocation,
) -> Result<WeatherSnapshot, PluginError> {
    let response: WeatherResponse = serde_json::from_str(body).map_err(|_| invalid_response())?;
    if response.timezone.is_some() {
        location.timezone = response.timezone;
    }
    let mut warnings = Vec::new();
    let current = response
        .current
        .map(|c| CurrentWeather {
            temperature_c: c.temperature_2m.filter(|v| v.is_finite()),
            condition_code: c.weather_code,
            wind_speed_kmh: c.wind_speed_10m.filter(|v| v.is_finite() && *v >= 0.0),
            valid_at: valid_time(c.time.as_deref(), response.utc_offset_seconds),
        })
        .filter(|c| {
            c.temperature_c.is_some() || c.condition_code.is_some() || c.wind_speed_kmh.is_some()
        });
    if current.is_none() {
        warnings.push("当前天气不可用，仅提供预报".into());
    } else if current.as_ref().is_some_and(|c| {
        c.temperature_c.is_none()
            || c.condition_code.is_none()
            || c.wind_speed_kmh.is_none()
            || c.valid_at.is_none()
    }) {
        warnings.push("当前天气部分字段不可用".into());
    }
    let mut daily = Vec::new();
    if let Some(d) = response.daily {
        let mut seen = std::collections::HashSet::new();
        for (i, date) in d.time.iter().enumerate() {
            if daily.len() >= 7 {
                break;
            }
            if NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() || !seen.insert(date) {
                warnings.push("已忽略无效或重复的预报日期".into());
                continue;
            }
            let mut temperature_max_c = number(&d.temperature_2m_max, i);
            let mut temperature_min_c = number(&d.temperature_2m_min, i);
            if temperature_max_c
                .zip(temperature_min_c)
                .is_some_and(|(max, min)| max < min)
            {
                warnings.push("已忽略无效的预报温度范围，保留其他可用字段".into());
                temperature_max_c = None;
                temperature_min_c = None;
            }
            let condition_code = d.weather_code.get(i).copied().flatten();
            let precipitation_probability_percent =
                number(&d.precipitation_probability_max, i).filter(|v| (0.0..=100.0).contains(v));
            let wind_max_kmh = number(&d.wind_speed_10m_max, i).filter(|v| *v >= 0.0);
            if temperature_max_c.is_none()
                && temperature_min_c.is_none()
                && condition_code.is_none()
                && precipitation_probability_percent.is_none()
                && wind_max_kmh.is_none()
            {
                warnings.push("部分日期没有可用预报字段".into());
                continue;
            }
            if temperature_max_c.is_none()
                || temperature_min_c.is_none()
                || condition_code.is_none()
                || precipitation_probability_percent.is_none()
                || wind_max_kmh.is_none()
            {
                warnings.push("逐日预报部分字段不可用".into());
            }
            daily.push(DailyWeather {
                date: date.clone(),
                condition_code,
                temperature_max_c,
                temperature_min_c,
                precipitation_probability_percent,
                wind_max_kmh,
            });
        }
    }
    daily.sort_by(|a, b| a.date.cmp(&b.date));
    if daily.len() < 7 {
        warnings.push(format!("未来预报可用 {} 天（请求 7 天）", daily.len()));
    }
    if current.is_none() && daily.is_empty() {
        return Err(invalid_response());
    }
    let mut unique = std::collections::HashSet::new();
    warnings.retain(|w| unique.insert(w.clone()));
    Ok(WeatherSnapshot {
        location,
        provider: "open_meteo",
        fetched_at: chrono::Utc::now().to_rfc3339(),
        current,
        daily,
        warnings,
        attributions: vec!["https://open-meteo.com/"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn location() -> ResolvedLocation {
        parse_location(r#"{"results":[{"name":"广州","latitude":23.13,"longitude":113.26,"timezone":"Asia/Shanghai"}]}"#).unwrap()
    }

    #[test]
    fn no_geocoding_results_is_not_a_parse_error() {
        assert_eq!(parse_location("{}").unwrap_err().code, "location_not_found");
        assert_eq!(
            parse_location(r#"{"results":[{"name":"bad","latitude":999,"longitude":0}]}"#)
                .unwrap_err()
                .code,
            "invalid_response"
        );
    }
    #[test]
    fn missing_daily_codes_and_short_arrays_never_become_clear_weather() {
        let result=parse_weather(r#"{"daily":{"time":["2026-10-16","2026-10-15","bad","2026-10-15"],"temperature_2m_max":[25,26,27,28],"temperature_2m_min":[20]}}"#,location()).unwrap();
        assert_eq!(result.status(), "partial");
        assert!(result.current.is_none());
        assert_eq!(result.daily.len(), 2);
        assert_eq!(result.daily[0].date, "2026-10-15");
        assert!(result.daily.iter().all(|d| d.condition_code.is_none()));
        assert!(result.daily[0].temperature_min_c.is_none());
    }
    #[test]
    fn current_time_uses_requested_location_offset_not_computer_time() {
        let result=parse_weather(r#"{"utc_offset_seconds":32400,"timezone":"Asia/Tokyo","current":{"temperature_2m":20,"time":"2026-10-16T00:15","weather_code":0,"wind_speed_10m":2}}"#,location()).unwrap();
        assert_eq!(
            result.current.unwrap().valid_at.as_deref(),
            Some("2026-10-16T00:15:00+09:00")
        );
        assert_eq!(result.location.timezone.as_deref(), Some("Asia/Tokyo"));
    }
    #[test]
    fn no_usable_current_or_daily_is_an_error() {
        assert_eq!(
            parse_weather("{}", location()).unwrap_err().code,
            "invalid_response"
        );
    }

    #[test]
    fn temperature_missing_preserves_other_current_and_daily_fields() {
        let result = parse_weather(r#"{"utc_offset_seconds":28800,"current":{"weather_code":61,"wind_speed_10m":8,"time":"2026-10-15T12:00"},"daily":{"time":["2026-10-15"],"weather_code":[95],"precipitation_probability_max":[80],"wind_speed_10m_max":[30]}}"#, location()).unwrap();
        let current = result.current.as_ref().unwrap();
        assert!(current.temperature_c.is_none());
        assert_eq!(current.condition_code, Some(61));
        assert_eq!(current.wind_speed_kmh, Some(8.0));
        assert_eq!(result.daily.len(), 1);
        assert_eq!(
            result.daily[0].precipitation_probability_percent,
            Some(80.0)
        );
        let (_, data) =
            crate::presentation::render(&result, crate::settings::TemperatureUnit::Celsius);
        assert_eq!(data[0]["temp"], "未知");
        assert!(
            data[0]["copy_text"]
                .as_str()
                .unwrap()
                .contains("降水概率 80% · 最大风速 30km/h")
        );
        assert!(
            data[0]["copy_text"]
                .as_str()
                .unwrap()
                .contains("未知～未知")
        );
        assert_eq!(data[0]["status"], "partial");
    }

    #[test]
    fn invalid_temperature_range_keeps_the_days_other_weather_fields() {
        let result = parse_weather(r#"{"daily":{"time":["2026-10-15"],"weather_code":[95],"temperature_2m_max":[5],"temperature_2m_min":[20],"precipitation_probability_max":[80]}}"#, location()).unwrap();
        assert_eq!(result.daily.len(), 1);
        assert!(result.daily[0].temperature_max_c.is_none());
        assert!(result.daily[0].temperature_min_c.is_none());
        assert_eq!(result.daily[0].condition_code, Some(95));
        assert_eq!(
            result.daily[0].precipitation_probability_percent,
            Some(80.0)
        );
        assert_eq!(result.status(), "partial");
    }
}
