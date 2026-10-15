//! 和风天气 v1 请求构造与数值归一化：温度 Celsius、风速 km/h、概率百分比。
//!
//! 响应依据 2026-10 官方文档：数值字段为 `{value, unit}` 对象；condition 是
//! `{text, code}`（文本+字符串代码，不映射 WMO 表）；逐日按当地日期关联
//! （`localTime=true` 请求），daytime/nighttime 分段现象不合并成"全天最严重"。
use crate::model::{CurrentWeather, DailyWeather, ProviderCondition, ResolvedLocation};
use crate::protocol::PluginError;
use crate::settings::QWeatherCredentials;
use chrono::NaiveDate;
use serde::Deserialize;

/// 坐标最多两位小数（和风路径参数约束）；负零归一为 0，保留默认地点原精度。
fn coordinate(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    let normalized = if rounded == 0.0 { 0.0 } else { rounded };
    format!("{normalized:.2}")
}

pub fn current_url(credentials: &QWeatherCredentials, location: &ResolvedLocation) -> String {
    format!(
        "{}/weather/v1/current/{}/{}/?lang=zh",
        credentials.host,
        coordinate(location.latitude),
        coordinate(location.longitude),
    )
}

pub fn daily_url(credentials: &QWeatherCredentials, location: &ResolvedLocation) -> String {
    format!(
        "{}/weather/v1/daily/{}/{}/?days=7&localTime=true&lang=zh",
        credentials.host,
        coordinate(location.latitude),
        coordinate(location.longitude),
    )
}

/// 和风实况/逐日共用的 problem+json 错误分类；detail 不透出，防止回显凭据/URL。
pub fn error_from_response(status: u16, body: &str) -> PluginError {
    let error_type = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("type"))
                .and_then(|kind| kind.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let kind = error_type
        .rsplit('#')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match kind.as_str() {
        "unauthorized" => error(
            "authentication_failed",
            "和风鉴权失败，请检查 API Host 与 API Key",
        ),
        "no-credit" | "overdue" => error("quota_exceeded", "和风账号额度不足或已欠费"),
        "over-monthly-limit" => error("quota_exceeded", "已超出和风每月订阅量，请下月再试或升级"),
        "invalid-host" => error(
            "provider_not_configured",
            "API Host 不正确，请使用和风控制台分配的 Host",
        ),
        "security-restriction" | "account-suspension" | "forbidden" => error(
            "authentication_failed",
            "和风账号或凭据受限，无权访问该数据",
        ),
        "too-many-requests" => error("rate_limited", "和风请求过于频繁，请稍后重试"),
        "no-such-location" => error("location_not_found", "和风不支持该地点"),
        "data-not-available" => error("invalid_response", "该地点暂不支持请求的天气数据"),
        "invalid-parameter" | "missing-parameter" => {
            error("invalid_arguments", "和风无法处理该查询，请检查地点参数")
        }
        "deprecated" => error("invalid_response", "和风接口版本已废弃"),
        _ => match status {
            401 => error(
                "authentication_failed",
                "和风鉴权失败，请检查 API Host 与 API Key",
            ),
            403 => error(
                "quota_exceeded",
                "和风拒绝了该请求（额度、权限或 Host 受限）",
            ),
            429 => error("rate_limited", "和风请求过于频繁，请稍后重试"),
            404 => error("invalid_response", "和风服务路径不可用，请检查 API Host"),
            _ => error("network_failed", "和风服务连接失败，请稍后重试"),
        },
    }
}

#[derive(Debug, Deserialize)]
struct CurrentResponse {
    #[serde(default)]
    condition: Option<ConditionDto>,
    #[serde(default)]
    temperature: Option<QuantityDto>,
    #[serde(default)]
    wind: Option<WindDto>,
    #[serde(default)]
    metadata: Option<MetadataDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DailyResponse {
    #[serde(default)]
    days: Vec<DayDto>,
    #[serde(default)]
    metadata: Option<MetadataDto>,
}

#[derive(Debug, Deserialize)]
struct MetadataDto {
    #[serde(default)]
    attributions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ConditionDto {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    code: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct QuantityDto {
    #[serde(default)]
    value: Option<f64>,
    #[serde(default)]
    unit: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WindDto {
    #[serde(default)]
    speed: Option<QuantityDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DayDto {
    #[serde(default)]
    forecast_start_time: Option<String>,
    #[serde(default)]
    temperature_max: Option<QuantityDto>,
    #[serde(default)]
    temperature_min: Option<QuantityDto>,
    #[serde(default)]
    daytime: Option<PeriodDto>,
    #[serde(default)]
    nighttime: Option<PeriodDto>,
}

#[derive(Debug, Deserialize)]
struct PeriodDto {
    #[serde(default)]
    condition: Option<ConditionDto>,
    #[serde(default)]
    wind: Option<WindDto>,
    #[serde(default)]
    precipitation: Option<PrecipitationDto>,
}

#[derive(Debug, Deserialize)]
struct PrecipitationDto {
    #[serde(default)]
    probability: Option<f64>,
}

/// 和风实况：无上游观测时间字段，valid_at 保持缺失（不用抓取时间伪造）。
pub fn parse_current(body: &str) -> Result<(CurrentWeather, Vec<String>), PluginError> {
    let response: CurrentResponse = serde_json::from_str(body).map_err(|_| invalid_response())?;
    let condition = response.condition.as_ref().and_then(convert_condition);
    let temperature_c = celsius(response.temperature.as_ref());
    let wind_speed_kmh = wind_kmh(response.wind.as_ref().map(|w| w.speed.as_ref()).flatten());
    if condition.is_none() && temperature_c.is_none() && wind_speed_kmh.is_none() {
        return Err(invalid_response());
    }
    let current = CurrentWeather {
        temperature_c,
        condition_code: None,
        condition,
        wind_speed_kmh,
        valid_at: None,
    };
    let attributions = response
        .metadata
        .map(|metadata| metadata.attributions)
        .unwrap_or_default();
    Ok((current, attributions))
}

/// 和风逐日：温度归一 Celsius，风速/概率取白天与夜间较大值，日期按地点当地日期。
pub fn parse_daily(body: &str) -> Result<(Vec<DailyWeather>, Vec<String>), PluginError> {
    let response: DailyResponse = serde_json::from_str(body).map_err(|_| invalid_response())?;
    let mut warnings: Vec<String> = Vec::new();
    let mut daily = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for day in response.days {
        if daily.len() >= 7 {
            break;
        }
        let Some(forecast_start) = day.forecast_start_time.as_deref() else {
            warnings.push("已忽略缺少起始时间的预报日".into());
            continue;
        };
        let date = match local_date(forecast_start) {
            Some(date) => date,
            None => {
                warnings.push("已忽略无法解释为当地日期的预报日".into());
                continue;
            }
        };
        if !seen.insert(date.clone()) {
            warnings.push("已忽略重复的预报日期".into());
            continue;
        }
        let day_condition = day
            .daytime
            .as_ref()
            .and_then(|p| p.condition.as_ref())
            .and_then(convert_condition);
        let night_condition = day
            .nighttime
            .as_ref()
            .and_then(|p| p.condition.as_ref())
            .and_then(convert_condition);
        let mut temperature_max_c = celsius(day.temperature_max.as_ref());
        let mut temperature_min_c = celsius(day.temperature_min.as_ref());
        if temperature_max_c
            .zip(temperature_min_c)
            .is_some_and(|(max, min)| max < min)
        {
            warnings.push("已忽略无效的预报温度范围，保留其他可用字段".into());
            temperature_max_c = None;
            temperature_min_c = None;
        }
        let precipitation_probability_percent = probability_percent(
            day.daytime.as_ref().and_then(|p| p.precipitation.as_ref()),
            day.nighttime
                .as_ref()
                .and_then(|p| p.precipitation.as_ref()),
        );
        let wind_max_kmh = [
            day.daytime
                .as_ref()
                .and_then(|p| p.wind.as_ref().map(|w| w.speed.as_ref()))
                .flatten(),
            day.nighttime
                .as_ref()
                .and_then(|p| p.wind.as_ref().map(|w| w.speed.as_ref()))
                .flatten(),
        ]
        .into_iter()
        .filter_map(wind_kmh)
        .fold(None::<f64>, |best, value| {
            Some(best.map_or(value, |best: f64| best.max(value)))
        });
        if temperature_max_c.is_none()
            && temperature_min_c.is_none()
            && day_condition.is_none()
            && night_condition.is_none()
            && precipitation_probability_percent.is_none()
            && wind_max_kmh.is_none()
        {
            warnings.push("部分日期没有可用预报字段".into());
            continue;
        }
        daily.push(DailyWeather {
            date,
            condition_code: None,
            condition: None,
            day_condition,
            night_condition,
            temperature_max_c,
            temperature_min_c,
            precipitation_probability_percent,
            wind_max_kmh,
        });
    }
    daily.sort_by(|a, b| a.date.cmp(&b.date));
    if daily.is_empty() && warnings.is_empty() {
        return Err(invalid_response());
    }
    let attributions = response
        .metadata
        .map(|metadata| metadata.attributions)
        .unwrap_or_default();
    Ok((daily, attributions))
}

fn convert_condition(dto: &ConditionDto) -> Option<ProviderCondition> {
    let text = dto
        .text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())?;
    let code = match &dto.code {
        Some(serde_json::Value::String(code)) => code.trim().to_owned(),
        Some(serde_json::Value::Number(code)) => code.to_string(),
        _ => String::new(),
    };
    Some(ProviderCondition {
        text: text.to_owned(),
        code,
    })
}

/// 温度只接受明确的摄氏值；其他单位不能当 Celsius 使用。
fn celsius(quantity: Option<&QuantityDto>) -> Option<f64> {
    let quantity = quantity?;
    let value = quantity.value.filter(|v| v.is_finite())?;
    match quantity.unit.as_deref() {
        Some("°C") | Some("C") | Some("celsius") => Some(value),
        _ => None,
    }
}

/// 风速归一 km/h；只接受文档声明单位，未知单位不猜换算。
fn wind_kmh(quantity: Option<&QuantityDto>) -> Option<f64> {
    let quantity = quantity?;
    let value = quantity.value.filter(|v| v.is_finite() && *v >= 0.0)?;
    match quantity.unit.as_deref() {
        Some("m/s") => Some(value * 3.6),
        Some("km/h") | Some("km/hour") => Some(value),
        _ => None,
    }
}

/// 白天/夜间降水概率（[0,1] 比例）取较大者转百分比；越界值不可用。
fn probability_percent(
    daytime: Option<&PrecipitationDto>,
    nighttime: Option<&PrecipitationDto>,
) -> Option<f64> {
    let ratio = [daytime, nighttime]
        .into_iter()
        .flatten()
        .filter_map(|precipitation| precipitation.probability)
        .filter(|value| (0.0..=1.0).contains(value))
        .fold(None::<f64>, |best, value| {
            Some(best.map_or(value, |best: f64| best.max(value)))
        })?;
    Some(ratio * 100.0)
}

/// 当地日期解释：请求带 `localTime=true`，响应时间为当地时间；日期段
/// （`YYYY-MM-DD`）与时间/偏移无关，直接取前 10 个 ASCII 字符解析，
/// 不引入本机时区，也不用电脑日期改写地点日期。无法解释的日期返回 None。
fn local_date(forecast_start: &str) -> Option<String> {
    let bytes = forecast_start.as_bytes();
    if bytes.len() < 10 || !bytes[..10].is_ascii() {
        return None;
    }
    let date = &forecast_start[..10];
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .map(|_| date.to_owned())
}

fn error(code: &str, message: &str) -> PluginError {
    PluginError {
        code: code.into(),
        message: message.into(),
    }
}

fn invalid_response() -> PluginError {
    PluginError {
        code: "invalid_response".into(),
        message: "和风天气返回了无法解析的数据，请稍后重试".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> QWeatherCredentials {
        QWeatherCredentials {
            host: "https://abc123.re.qweatherapi.com".into(),
            key: "test-key".into(),
        }
    }

    fn location() -> ResolvedLocation {
        crate::settings::parse_location(
            &serde_json::json!({"name":"广州","latitude":23.129,"longitude":113.264,"coordinate_system":"wgs84"}),
            true,
        )
        .unwrap()
    }

    #[test]
    fn urls_use_two_decimal_coordinates_lat_first_without_losing_precision() {
        let base = location();
        assert_eq!(
            current_url(&credentials(), &base),
            "https://abc123.re.qweatherapi.com/weather/v1/current/23.13/113.26/?lang=zh"
        );
        assert_eq!(
            daily_url(&credentials(), &base),
            "https://abc123.re.qweatherapi.com/weather/v1/daily/23.13/113.26/?days=7&localTime=true&lang=zh"
        );
        // 零值与负值；默认地点原精度不动
        let mut zero = location();
        zero.latitude = -0.001;
        zero.longitude = 0.0;
        assert!(current_url(&credentials(), &zero).ends_with("/current/0.00/0.00/?lang=zh"));
        assert_eq!(zero.latitude, -0.001);
        let mut negative = location();
        negative.latitude = -41.876;
        assert!(current_url(&credentials(), &negative).contains("/-41.88/"));
    }

    #[test]
    fn current_normalizes_units_and_keeps_provider_text_without_faking_time() {
        let (current, attributions) = parse_current(
            r#"{"metadata":{"attributions":["https://developer.qweather.com/attribution.html"]},
                "condition":{"text":"少云","code":102},
                "temperature":{"value":31.71,"unit":"°C"},
                "wind":{"speed":{"value":4.74,"unit":"m/s"}}}"#,
        )
        .unwrap();
        assert_eq!(current.temperature_c, Some(31.71));
        assert_eq!(current.wind_speed_kmh, Some(4.74 * 3.6));
        assert_eq!(current.condition_code, None);
        let condition = current.condition.unwrap();
        assert_eq!(condition.text, "少云");
        assert_eq!(condition.code, "102");
        assert!(current.valid_at.is_none());
        assert_eq!(
            attributions,
            vec!["https://developer.qweather.com/attribution.html".to_owned()]
        );
    }

    #[test]
    fn unknown_units_are_rejected_instead_of_treated_as_celsius_or_kmh() {
        let (current, _) = parse_current(
            r#"{"condition":{"text":"晴","code":100},
                "temperature":{"value":88,"unit":"°F"},
                "wind":{"speed":{"value":10,"unit":"mph"}}}"#,
        )
        .unwrap();
        assert_eq!(current.temperature_c, None);
        assert_eq!(current.wind_speed_kmh, None);
        assert!(current.condition.is_some());
        assert!(parse_current("{}").is_err());
    }

    fn daily_body() -> String {
        r#"{"metadata":{"attributions":["https://developer.qweather.com/attribution.html"]},
            "days":[
              {"forecastStartTime":"2026-10-15T06:00:00+08:00","forecastEndTime":"2026-10-16T06:00:00+08:00",
               "temperatureMax":{"value":29.94,"unit":"°C"},"temperatureMin":{"value":20.93,"unit":"°C"},
               "daytime":{"condition":{"text":"小雨","code":305},"wind":{"speed":{"value":1.87,"unit":"m/s"}},
                          "precipitation":{"probability":0.64}},
               "nighttime":{"condition":{"text":"多云","code":101},"wind":{"speed":{"value":8.0,"unit":"km/h"}},
                            "precipitation":{"probability":0.1}}},
              {"forecastStartTime":"2026-10-16T06:00","forecastEndTime":"2026-10-17T06:00",
               "temperatureMax":{"value":30,"unit":"°C"},"temperatureMin":{"value":5},
               "daytime":{"condition":{"text":"晴","code":100}},
               "nighttime":{"condition":{"text":"小雨","code":305},"precipitation":{"probability":0.5}}},
              {"forecastStartTime":"duplicate","forecastEndTime":"x"},
              {"forecastStartTime":"duplicate","forecastEndTime":"x"},
              {"temperatureMax":{"value":1,"unit":"°C"},"temperatureMin":{"value":2,"unit":"°C"}}
            ]}"#
            .to_owned()
    }

    #[test]
    fn daily_keeps_day_night_split_and_takes_worst_probability_and_speed() {
        let (daily, attributions) = parse_daily(&daily_body()).unwrap();
        assert_eq!(daily.len(), 2);
        let first = &daily[0];
        assert_eq!(first.date, "2026-10-15");
        assert_eq!(first.day_condition.as_ref().unwrap().text, "小雨");
        assert_eq!(first.night_condition.as_ref().unwrap().text, "多云");
        assert_eq!(first.temperature_max_c, Some(29.94));
        assert_eq!(first.temperature_min_c, Some(20.93));
        assert_eq!(first.precipitation_probability_percent, Some(64.0));
        // 夜间 8km/h > 白天 1.87m/s(≈6.7km/h)
        assert!(first.wind_max_kmh.unwrap() - 8.0 < 1e-9);
        assert_eq!(daily[1].date, "2026-10-16");
        // 缺 unit 的温度不可用；无任何可用字段的日期被忽略
        assert_eq!(daily[1].temperature_min_c, None);
        assert_eq!(
            attributions,
            vec!["https://developer.qweather.com/attribution.html".to_owned()]
        );
    }

    #[test]
    fn daily_rejects_reversed_temperature_range_and_utc_suffix_falls_back() {
        let body = r#"{"days":[
            {"forecastStartTime":"2026-10-15T22:00Z",
             "temperatureMax":{"value":5,"unit":"°C"},"temperatureMin":{"value":20,"unit":"°C"},
             "daytime":{"condition":{"text":"晴","code":100}}}
        ]}"#;
        let (daily, _) = parse_daily(body).unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].date, "2026-10-15");
        assert!(daily[0].temperature_max_c.is_none());
        assert!(daily[0].temperature_min_c.is_none());
        assert!(daily[0].day_condition.is_some());
    }

    #[test]
    fn error_mapping_uses_type_anchor_and_never_leaks_detail() {
        let problem = r#"{"error":{"status":403,"type":"https://api.qweather.com/errors/#invalid-host","title":"Invalid Host","detail":"host=abc secret=xyz"}}"#;
        let error = error_from_response(403, problem);
        assert_eq!(error.code, "provider_not_configured");
        assert!(!error.message.contains("abc"));
        assert!(!error.message.contains("xyz"));

        for (kind, status, code) in [
            ("unauthorized", 401, "authentication_failed"),
            ("no-credit", 403, "quota_exceeded"),
            ("over-monthly-limit", 429, "quota_exceeded"),
            ("too-many-requests", 429, "rate_limited"),
            ("no-such-location", 400, "location_not_found"),
            ("forbidden", 403, "authentication_failed"),
        ] {
            let body = format!(
                r#"{{"error":{{"status":{status},"type":"https://x/#{kind}","title":"t"}}}}"#
            );
            assert_eq!(error_from_response(status, &body).code, code, "{kind}");
        }
        // 无法解析的错误体按状态兜底
        assert_eq!(error_from_response(500, "").code, "network_failed");
        assert_eq!(
            error_from_response(404, "not-json").code,
            "invalid_response"
        );
    }
}
