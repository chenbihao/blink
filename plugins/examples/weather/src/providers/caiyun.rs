//! 彩云天气 v2.6 适配：HMAC 签名、综合 weather 请求与响应解析。
//!
//! 依据 2026-10 官方文档：
//! - 认证：`{method}:{path}:{query}:{app_key}:{nonce}:{timestamp}` 以 App Secret 做
//!   HMAC-SHA256，URL-safe Base64（保留 `=` padding）；query 按键排序后参与签名与
//!   实际 URL（同一份编码，只构造一次）；App Key 在路径中。
//! - 综合 weather：`/v2.6/{app_key}/{经度},{纬度}/weather`，经度在前；响应
//!   `location` 数组纬度在前，仅作诊断，不回写地点。大陆请求 GCJ-02、其余 WGS84
//!   （坐标派生见 `location::coordinates`）。
//! - 单位：显式请求 `unit=metric:v2`（温度 ℃、风速 km/h、概率 0~1），规避默认
//!   metric 下实况风速文档口径不一致的风险。
//! - skycon 是彩云命名空间字符串代码，经 `conditions::skycon_description` 映射，
//!   不转 WMO/和风代码；逐日 skycon 是全天主导现象，不代表全天持续。
//! - 顶层 status=ok 不保证 realtime/daily 完整，两块各自检查；server_time 是
//!   服务时间，不冒充实况观测时间（valid_at 保持缺失）。

use crate::conditions::skycon_description;
use crate::model::{CurrentWeather, DailyWeather, ProviderCondition, ResolvedLocation, WeatherSnapshot};
use crate::protocol::PluginError;
use crate::settings::CaiyunCredentials;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use crate::location::coordinates::RequestCoordinates;
use serde::Deserialize;
use sha2::Sha256;

pub const API_BASE: &str = "https://api.caiyunapp.com";
const DAILY_STEPS: u8 = 7;
const UNIT: &str = "metric:v2";

type HmacSha256 = Hmac<Sha256>;

// ── 签名（纯函数，nonce/时间戳由调用方注入） ─────────────────────────────────

/// QueryEscape 语义编码：保留 `[A-Za-z0-9-_.~]`，空格 → `+`，其余 `%XX`（大写）。
/// 与 Go `url.QueryEscape` / Python `quote_plus` 一致；签名与 URL 共用本实现。
fn query_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                escaped.push(byte as char)
            }
            b' ' => escaped.push('+'),
            _ => escaped.push_str(&format!("%{byte:02X}")),
        }
    }
    escaped
}

/// 规范 query：按 (key, value) 排序后以 `k=v` 用 `&` 连接；键值均编码。
/// 无重复键场景下与官方"按键排序"一致，同键时按值排序保证确定性。
fn canonical_query(params: &[(&str, &str)]) -> String {
    let mut sorted: Vec<(&str, &str)> = params.to_vec();
    sorted.sort();
    sorted
        .into_iter()
        .map(|(key, value)| format!("{}={}", query_escape(key), query_escape(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// 官方签名：HMAC-SHA256(App Secret, "{method}:{path}:{query}:{app_key}:{nonce}:{timestamp}")
/// → URL-safe Base64（保留 padding）。path/query 必须是实际发送值。
fn signature(
    secret: &str,
    method: &str,
    path: &str,
    query: &str,
    app_key: &str,
    nonce: &str,
    timestamp_seconds: i64,
) -> String {
    let payload = format!("{method}:{path}:{query}:{app_key}:{nonce}:{timestamp_seconds}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    base64::engine::general_purpose::URL_SAFE.encode(mac.finalize().into_bytes())
}

/// 生产随机 nonce：16 字节 OS CSPRNG → 32 个十六进制字符（官方要求 16–40 位，每次唯一）。
fn generate_nonce() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

// ── 请求构造 ─────────────────────────────────────────────────────────────────

pub struct SignedRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// 请求坐标格式化：6 位小数（≈0.1m），负零归一；只含 URL 安全字符（数字/点/负号/逗号）。
fn coordinate(value: f64) -> String {
    let rounded = (value * 1e6).round() / 1e6;
    let normalized = if rounded == 0.0 { 0.0 } else { rounded };
    format!("{normalized:.6}")
}

/// 构造综合 weather 请求。路径（含 App Key 与派生坐标）与 query 只生成一次，
/// 同一份值同时用于签名与 URL——签 WGS84 却发 GCJ-02、或 builder 二次编码，
/// 都会在结构上无法发生。
pub fn build_request(
    credentials: &CaiyunCredentials,
    coordinates: &RequestCoordinates,
    nonce: &str,
    timestamp_seconds: i64,
) -> SignedRequest {
    let path = format!(
        "/v2.6/{}/{},{}/weather",
        credentials.app_key,
        coordinate(coordinates.longitude),
        coordinate(coordinates.latitude),
    );
    let query = canonical_query(&[("dailysteps", DAILY_STEPS.to_string().as_str()), ("unit", UNIT)]);
    let signed = signature(
        &credentials.app_secret,
        "GET",
        &path,
        &query,
        &credentials.app_key,
        nonce,
        timestamp_seconds,
    );
    SignedRequest {
        url: format!("{API_BASE}{path}?{query}"),
        headers: vec![
            ("x-cy-nonce".to_owned(), nonce.to_owned()),
            ("x-cy-timestamp".to_owned(), timestamp_seconds.to_string()),
            ("x-cy-signature".to_owned(), signed),
        ],
    }
}

/// 生产入口：每次生成新的随机 nonce 与秒级时间戳；任一失败按无效响应反馈。
pub fn weather_request(
    credentials: &CaiyunCredentials,
    coordinates: &RequestCoordinates,
) -> Result<SignedRequest, PluginError> {
    let nonce = generate_nonce().ok_or_else(invalid_response)?;
    Ok(build_request(
        credentials,
        coordinates,
        &nonce,
        chrono::Utc::now().timestamp(),
    ))
}

// ── 错误分类 ─────────────────────────────────────────────────────────────────

/// HTTP/顶层错误 → 统一分类。400 可能是凭据/签名/版本问题；429 可能是额度耗尽
/// 或 QPS 限流（响应体 `Rate limit exceeded` 属限流，JSONL 无响应头不可虚构
/// Retry-After）；只按白名单文本细分，其余保守归类，绝不回显可能含凭据的错误体。
pub fn error_from_response(status: u16, body: &str) -> PluginError {
    let error_text = body_error_text(body);
    let mentions_token = error_text.contains("token");
    let mentions_rate_limit = error_text.to_ascii_lowercase().contains("rate limit");
    if (300..400).contains(&status) {
        return error(
            "invalid_response",
            "彩云服务返回重定向，已拒绝跟随以保护签名与凭据",
        );
    }
    match status {
        400 if mentions_token => error(
            "authentication_failed",
            "彩云凭据或签名无效，请检查 App Key 与 App Secret",
        ),
        400 => error("invalid_arguments", "彩云无法处理该查询，请检查地点参数"),
        401 => error("authentication_failed", "彩云 Token 无权访问该数据"),
        403 => error("authentication_failed", "彩云账号或凭据受限（Token 被禁用或 IP 不在白名单）"),
        422 => error("invalid_arguments", "彩云参数错误，请检查请求配置"),
        429 if mentions_rate_limit => error("rate_limited", "彩云请求过于频繁（QPS 限流），请稍后重试"),
        429 => error("quota_exceeded", "彩云额度可能已用完，请到控制台核验额度与有效期"),
        404 => error("invalid_response", "彩云服务路径不可用，请稍后重试"),
        _ => error("network_failed", "彩云服务连接失败，请稍后重试"),
    }
}

/// 错误体 `error` 字段提取（解析失败返回空串——错误体不透出，仅做白名单匹配）。
fn body_error_text(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// HTTP 200 但顶层 status=failed：按错误体白名单文本分类，未知保守归 invalid_response。
fn failed_body_error(error_text: &str) -> PluginError {
    if error_text.contains("token") {
        error(
            "authentication_failed",
            "彩云凭据或签名无效，请检查 App Key 与 App Secret",
        )
    } else if error_text.to_ascii_lowercase().contains("rate limit") {
        error("rate_limited", "彩云请求过于频繁（QPS 限流），请稍后重试")
    } else {
        error("invalid_response", "彩云天气返回了无法解析的数据，请稍后重试")
    }
}

// ── 综合响应解析 ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WeatherResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    result: Option<ResultBlock>,
}

#[derive(Debug, Deserialize)]
struct ResultBlock {
    #[serde(default)]
    realtime: Option<RealtimeBlock>,
    #[serde(default)]
    daily: Option<DailyBlock>,
}

#[derive(Debug, Deserialize)]
struct RealtimeBlock {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    skycon: Option<String>,
    #[serde(default)]
    wind: Option<RealtimeWind>,
}

#[derive(Debug, Deserialize)]
struct RealtimeWind {
    #[serde(default)]
    speed: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DailyBlock {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    temperature: Vec<DatedTemperature>,
    #[serde(default)]
    skycon: Vec<DatedSkycon>,
    #[serde(default)]
    wind: Vec<DatedWindRange>,
    #[serde(default)]
    precipitation: Vec<DatedPrecipitation>,
}

/// 逐日各字段数组按元素自带日期关联，不按下标 zip。
#[derive(Debug, Deserialize)]
struct DatedTemperature {
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    min: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DatedSkycon {
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DatedWindRange {
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    max: Option<WindQuantity>,
}

#[derive(Debug, Deserialize)]
struct WindQuantity {
    #[serde(default)]
    speed: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DatedPrecipitation {
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    probability: Option<f64>,
}

/// 综合响应 → 统一快照。实况/逐日各自检查可用性：一块完整一块缺失构成 partial，
/// 两块都无有效数据返回错误；不足 7 天只说明实际天数，不断言原因。
pub fn parse_weather(body: &str, location: ResolvedLocation) -> Result<WeatherSnapshot, PluginError> {
    let response: WeatherResponse = serde_json::from_str(body).map_err(|_| invalid_response())?;
    if response.status.as_deref() != Some("ok") {
        return Err(failed_body_error(&response.error.unwrap_or_default()));
    }
    let result = response.result.ok_or_else(invalid_response)?;
    let mut warnings = Vec::new();

    let current = result
        .realtime
        .as_ref()
        .filter(|realtime| block_usable(&realtime.status))
        .and_then(|realtime| {
        let temperature_c = realtime.temperature.filter(|value| value.is_finite());
        let wind_speed_kmh = realtime
            .wind
            .as_ref()
            .and_then(|wind| wind.speed)
            .filter(|value| value.is_finite() && *value >= 0.0);
        let condition = realtime
            .skycon
            .as_deref()
            .map(str::trim)
            .filter(|code| !code.is_empty())
            .map(skycon_condition);
        if temperature_c.is_none() && wind_speed_kmh.is_none() && condition.is_none() {
            return None;
        }
            Some(CurrentWeather {
                temperature_c,
                condition_code: None,
                condition,
                wind_speed_kmh,
                // server_time 是服务时间，不是观测时间；保持缺失，不伪造。
                valid_at: None,
            })
        });

    let daily = match result
        .daily
        .as_ref()
        .filter(|daily| block_usable(&daily.status))
    {
        Some(daily) => parse_daily(daily, &mut warnings),
        None => Vec::new(),
    };

    if current.is_none() && daily.is_empty() {
        return Err(invalid_response());
    }
    if current.is_none() {
        warnings.push("彩云实况不可用，仅提供预报".into());
    }
    if daily.is_empty() {
        warnings.push("彩云逐日预报不可用，仅提供实况".into());
    } else if daily.len() < DAILY_STEPS as usize {
        warnings.push(format!(
            "未来预报可用 {} 天（请求 {} 天），可能受账号权限限制",
            daily.len(),
            DAILY_STEPS
        ));
    }
    Ok(WeatherSnapshot {
        location,
        provider: "caiyun",
        fetched_at: chrono::Utc::now().to_rfc3339(),
        current,
        daily,
        warnings,
        attributions: vec!["https://caiyunapp.com/".to_owned()],
    })
}

/// 子块 status：缺失视为可用（顶层 ok 已确认请求成功），明确 failed 才不可用。
fn block_usable(status: &Option<String>) -> bool {
    status.as_deref() != Some("failed")
}

fn skycon_condition(code: &str) -> ProviderCondition {
    ProviderCondition {
        text: skycon_description(code).to_owned(),
        code: code.to_owned(),
    }
}

/// 逐日合并：按日期把各字段数组汇成统一逐日模型，容忍乱序/不等长/缺失。
fn parse_daily(daily: &DailyBlock, warnings: &mut Vec<String>) -> Vec<DailyWeather> {
    #[derive(Default)]
    struct DayAccumulator {
        temperature_max: Option<f64>,
        temperature_min: Option<f64>,
        condition: Option<ProviderCondition>,
        wind_max: Option<f64>,
        probability: Option<f64>,
    }
    let mut days: std::collections::BTreeMap<String, DayAccumulator> =
        std::collections::BTreeMap::new();
    let mut dropped = 0usize;

    let local_date = |value: Option<&str>| -> Option<String> {
        let value = value?.trim();
        let bytes = value.as_bytes();
        if bytes.len() < 10 || !bytes[..10].is_ascii() {
            return None;
        }
        let date = &value[..10];
        chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .ok()
            .map(|_| date.to_owned())
    };

    for entry in &daily.temperature {
        let Some(date) = local_date(entry.date.as_deref()) else {
            dropped += 1;
            continue;
        };
        let day = days.entry(date).or_default();
        let max = entry.max.filter(|value| value.is_finite());
        let min = entry.min.filter(|value| value.is_finite());
        if max.is_some() || min.is_some() {
            day.temperature_max = max.or(day.temperature_max);
            day.temperature_min = min.or(day.temperature_min);
        }
    }
    for entry in &daily.skycon {
        let Some(date) = local_date(entry.date.as_deref()) else {
            dropped += 1;
            continue;
        };
        if let Some(code) = entry.value.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            days.entry(date).or_default().condition = Some(skycon_condition(code));
        }
    }
    for entry in &daily.wind {
        let Some(date) = local_date(entry.date.as_deref()) else {
            dropped += 1;
            continue;
        };
        let speed = entry
            .max
            .as_ref()
            .and_then(|max| max.speed)
            .filter(|value| value.is_finite() && *value >= 0.0);
        if let Some(speed) = speed {
            let day = days.entry(date).or_default();
            day.wind_max = Some(day.wind_max.map_or(speed, |best: f64| best.max(speed)));
        }
    }
    for entry in &daily.precipitation {
        let Some(date) = local_date(entry.date.as_deref()) else {
            dropped += 1;
            continue;
        };
        // 概率是 0~1 比例（所有单位制一致）；越界值不可用，不无声截断。
        if let Some(ratio) = entry.probability.filter(|value| (0.0..=1.0).contains(value)) {
            days.entry(date).or_default().probability = Some(ratio * 100.0);
        }
    }
    if dropped > 0 {
        warnings.push(format!("已忽略 {dropped} 个无法按日期解释的逐日条目"));
    }
    days.into_iter()
        .filter_map(|(date, day)| {
            if day.temperature_max.is_none()
                && day.temperature_min.is_none()
                && day.condition.is_none()
                && day.wind_max.is_none()
                && day.probability.is_none()
            {
                return None;
            }
            Some(DailyWeather {
                date,
                condition_code: None,
                condition: day.condition,
                day_condition: None,
                night_condition: None,
                temperature_max_c: day.temperature_max,
                temperature_min_c: day.temperature_min,
                precipitation_probability_percent: day.probability,
                wind_max_kmh: day.wind_max,
            })
        })
        .take(DAILY_STEPS as usize)
        .collect()
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
        message: "彩云天气返回了无法解析的数据，请稍后重试".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> CaiyunCredentials {
        CaiyunCredentials {
            app_key: "your_app_key".into(),
            app_secret: "your_app_secret".into(),
        }
    }

    fn coordinates() -> RequestCoordinates {
        RequestCoordinates {
            latitude: 39.976,
            longitude: 116.3176,
            coordinate_system: "gcj02",
        }
    }

    fn location() -> ResolvedLocation {
        ResolvedLocation {
            name: "北京".into(),
            display_name: "北京".into(),
            latitude: 39.911954,
            longitude: 116.377817,
            coordinate_system: "wgs84",
            administrative_area: String::new(),
            country: String::new(),
            country_code: Some("CN".into()),
            administrative_areas: vec![],
            timezone: None,
            kind: "city_center",
            source: "open_meteo",
            source_id: None,
            resolved_at: "2026-10-15T00:00:00Z".into(),
            in_mainland_china: Some(true),
        }
    }

    // ── 签名 ──────────────────────────────────────────────────────────────

    /// 官方认证文档固定向量（期望值来自文档示例，非被测函数自产）。
    #[test]
    fn official_signature_vector_is_reproduced() {
        let signed = signature(
            "your_app_secret",
            "GET",
            "/v2.6/your_app_key/116.3176,39.9760/weather",
            "alert=true&dailysteps=1&hourlysteps=24",
            "your_app_key",
            "0195c68a-42e7-7243-bff2-ac97a78b837d",
            1742791910,
        );
        assert_eq!(signed, "KfHsk3z2XfX6Yxox4Uf_VgyM0wHk6bWEyRqZ9QOJUYw=");
    }

    #[test]
    fn canonical_query_sorts_keys_and_escapes_values_like_queryescape() {
        assert_eq!(
            canonical_query(&[("hourlysteps", "24"), ("dailysteps", "1"), ("alert", "true")]),
            "alert=true&dailysteps=1&hourlysteps=24",
        );
        // 中文/空格/加号/百分号/保留字符：期望值按 RFC 3986 + QueryEscape 手工推得
        assert_eq!(query_escape("上海"), "%E4%B8%8A%E6%B5%B7");
        assert_eq!(query_escape("a b"), "a+b");
        assert_eq!(query_escape("a+b"), "a%2Bb");
        assert_eq!(query_escape("100%"), "100%25");
        assert_eq!(query_escape("a&b=c?d#e/f"), "a%26b%3Dc%3Fd%23e%2Ff");
        assert_eq!(query_escape("._~-"), "._~-");
        // 排序先于编码：编码后的字典序不影响键序
        assert_eq!(
            canonical_query(&[("unit", "metric:v2"), ("dailysteps", "7")]),
            "dailysteps=7&unit=metric%3Av2",
        );
    }

    #[test]
    fn signature_uses_url_safe_alphabet_with_padding() {
        let signed = signature("k", "GET", "/p", "a=b", "key", "nonce", 1);
        assert!(!signed.contains('+') && !signed.contains('/'));
        assert_eq!(signed.len() % 4, 0, "URL_SAFE 保留 padding");
    }

    #[test]
    fn build_request_signs_exactly_what_it_sends() {
        let request = build_request(
            &credentials(),
            &coordinates(),
            "test-nonce-0123456789ab",
            1742791910,
        );
        assert_eq!(
            request.url,
            "https://api.caiyunapp.com/v2.6/your_app_key/116.317600,39.976000/weather?dailysteps=7&unit=metric%3Av2"
        );
        // URL 与签名输入同源：用同一路径/query 重算签名必须与请求头一致
        let path = "/v2.6/your_app_key/116.317600,39.976000/weather";
        let query = "dailysteps=7&unit=metric%3Av2";
        let expected = signature(
            "your_app_secret",
            "GET",
            path,
            query,
            "your_app_key",
            "test-nonce-0123456789ab",
            1742791910,
        );
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "x-cy-signature" && value == &expected));
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "x-cy-nonce" && value == "test-nonce-0123456789ab"));
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "x-cy-timestamp" && value == "1742791910"));
        // App Secret 绝不出现在 URL/头里；App Key 只出现在路径
        assert!(!request.url.contains("your_app_secret"));
        assert!(request.headers.iter().all(|(_, value)| !value.contains("your_app_secret")));
        // 负零归一
        let mut zero = coordinates();
        zero.longitude = -0.0000001;
        let request = build_request(&credentials(), &zero, "n", 1);
        assert!(request.url.contains("/0.000000,39.976000/"));
    }

    #[test]
    fn production_nonce_is_unique_hex_within_official_length_bounds() {
        let first = generate_nonce().unwrap();
        let second = generate_nonce().unwrap();
        assert_ne!(first, second, "每次请求的 nonce 必须唯一");
        for nonce in [first, second] {
            assert!((16..=40).contains(&nonce.len()), "nonce={nonce}");
            assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
    }

    // ── 错误分类 ──────────────────────────────────────────────────────────

    #[test]
    fn errors_map_by_status_and_whitelist_text_without_echoing_detail() {
        for (status, body, code) in [
            (400u16, r#"{"status":"failed","error":"token is invalid"}"#, "authentication_failed"),
            (400, r#"{"status":"failed","error":"signature mismatch: secret=xyz"}"#, "invalid_arguments"),
            (401, r#"{"status":"failed","error":"Token 无权限"}"#, "authentication_failed"),
            (403, r#"{"status":"failed","error":"token is disabled"}"#, "authentication_failed"),
            (422, r#"{"status":"failed","error":"参数错误"}"#, "invalid_arguments"),
            (429, r#"{"status":"failed","error":"Rate limit exceeded"}"#, "rate_limited"),
            (429, r#"{"status":"failed","error":"quota used up"}"#, "quota_exceeded"),
            (404, "not-json", "invalid_response"),
            (302, "", "invalid_response"),
            (500, "server error", "network_failed"),
        ] {
            let error = error_from_response(status, body);
            assert_eq!(error.code, code, "status={status} body={body}");
            assert!(!error.message.contains("xyz"), "不得回显错误体");
        }
    }

    // ── 响应解析 ──────────────────────────────────────────────────────────

    #[test]
    fn realtime_normalizes_metric_v2_units_and_keeps_skycon_namespace() {
        let body = r#"{
          "status":"ok","api_version":"v2.6","server_time":1742791910,
          "location":[39.976,116.3176],
          "result":{"forecast_keypoint":"两小时无降雨","realtime":{
            "status":"ok","temperature":21.5,"skycon":"PARTLY_CLOUDY_DAY",
            "wind":{"speed":12.4,"direction":120}}}}"#;
        let snapshot = parse_weather(body, location()).unwrap();
        assert_eq!(snapshot.provider, "caiyun");
        // 原始地点保持 WGS84，不被响应 location（纬度在前）改写
        assert_eq!(snapshot.location.longitude, 116.377817);
        assert_eq!(snapshot.location.coordinate_system, "wgs84");
        assert_eq!(snapshot.status(), "partial", "无逐日数据构成 partial");
        assert!(snapshot.warnings.iter().any(|w| w.contains("逐日预报不可用")));
        let current = snapshot.current.unwrap();
        assert_eq!(current.temperature_c, Some(21.5));
        assert_eq!(current.wind_speed_kmh, Some(12.4));
        let condition = current.condition.unwrap();
        assert_eq!(condition.text, "多云（白天）");
        assert_eq!(condition.code, "PARTLY_CLOUDY_DAY");
        assert!(current.valid_at.is_none());
    }

    #[test]
    fn daily_arrays_join_by_date_across_disorder_and_unequal_lengths() {
        let body = r#"{
          "status":"ok",
          "result":{"realtime":{"status":"ok","temperature":20,"skycon":"CLOUDY"},
          "daily":{
            "status":"ok",
            "temperature":[
              {"date":"2026-10-16T00:00+08:00","max":29.5,"min":21.0},
              {"date":"2026-10-15T00:00+08:00","max":28.0,"min":20.0}],
            "skycon":[{"date":"2026-10-15T00:00+08:00","value":"LIGHT_RAIN"}],
            "wind":[{"date":"2026-10-15T00:00+08:00","max":{"speed":22.1,"direction":150}},
                    {"date":"2026-10-16T00:00+08:00","max":{"speed":15.0}},
                    {"date":"bad-date","max":{"speed":1.0}}],
            "precipitation":[
              {"date":"2026-10-15T00:00+08:00","probability":0.64},
              {"date":"2026-10-16T00:00+08:00","probability":0.0}]}}}"#;
        let snapshot = parse_weather(body, location()).unwrap();
        assert_eq!(snapshot.daily.len(), 2);
        // 按日期排序，不按数组下标
        assert_eq!(snapshot.daily[0].date, "2026-10-15");
        assert_eq!(snapshot.daily[0].condition.as_ref().unwrap().text, "小雨");
        assert_eq!(snapshot.daily[0].temperature_max_c, Some(28.0));
        assert_eq!(snapshot.daily[0].wind_max_kmh, Some(22.1));
        assert_eq!(snapshot.daily[0].precipitation_probability_percent, Some(64.0));
        assert_eq!(snapshot.daily[1].date, "2026-10-16");
        // 10-16 缺 skycon：条件为空，不补晴天；温度/风/概率仍按日期对齐
        assert!(snapshot.daily[1].condition.is_none());
        assert_eq!(snapshot.daily[1].wind_max_kmh, Some(15.0));
        assert!(snapshot
            .warnings
            .iter()
            .any(|warning| warning.contains("已忽略 1 个无法按日期解释")));
    }

    #[test]
    fn full_seven_days_are_ok_and_shorter_days_report_actual_count_only() {
        let day_entry = |offset: usize| {
            format!(
                r#"{{"date":"2026-10-1{}T00:00+08:00","max":30,"min":20}}"#,
                offset
            )
        };
        let full = format!(
            r#"{{"status":"ok","result":{{"realtime":{{"status":"ok","temperature":21,"skycon":"CLEAR_DAY"}},"daily":{{"temperature":[{}],"skycon":[],"wind":[],"precipitation":[]}}}}}}"#,
            (0..7).map(day_entry).collect::<Vec<_>>().join(",")
        );
        let snapshot = parse_weather(&full, location()).unwrap();
        assert_eq!(snapshot.daily.len(), 7);
        assert_eq!(snapshot.status(), "ok");

        let partial = format!(
            r#"{{"status":"ok","result":{{"realtime":{{"status":"ok","temperature":21,"skycon":"CLEAR_DAY"}},"daily":{{"temperature":[{}]}}}}}}"#,
            (0..3).map(day_entry).collect::<Vec<_>>().join(",")
        );
        let snapshot = parse_weather(&partial, location()).unwrap();
        assert_eq!(snapshot.status(), "partial");
        assert!(snapshot
            .warnings
            .iter()
            .any(|warning| warning.contains("可用 3 天") && warning.contains("请求 7 天")));
    }

    #[test]
    fn failed_blocks_dont_fabricate_weather_and_both_missing_is_an_error() {
        // 子块 status=failed：顶层 ok 也不当作可用数据
        let realtime_failed = r#"{"status":"ok","result":{
            "realtime":{"status":"failed"},
            "daily":{"status":"ok","temperature":[{"date":"2026-10-15T00:00+08:00","max":30,"min":20}]}}}"#;
        let snapshot = parse_weather(realtime_failed, location()).unwrap();
        assert!(snapshot.current.is_none());
        assert!(snapshot.warnings.iter().any(|w| w.contains("实况不可用")));
        // 两块均无有效数据 → 错误，不返回空天气
        let empty = r#"{"status":"ok","result":{"realtime":{"status":"failed"},"daily":{"status":"failed"}}}"#;
        assert_eq!(
            parse_weather(empty, location()).unwrap_err().code,
            "invalid_response"
        );
        // 顶层 failed → 按错误体分类（token 白名单 → 鉴权失败）
        let failed = r#"{"status":"failed","error":"token is invalid","api_version":"2.6"}"#;
        assert_eq!(
            parse_weather(failed, location()).unwrap_err().code,
            "authentication_failed"
        );
        // 非 JSON 网关错误 → invalid_response
        assert_eq!(
            parse_weather("<html>gateway error</html>", location())
                .unwrap_err()
                .code,
            "invalid_response"
        );
    }

    #[test]
    fn out_of_range_probability_is_dropped_not_clamped_and_unknown_skycon_keeps_value() {
        let body = r#"{"status":"ok","result":{
            "realtime":{"status":"ok","temperature":20},
            "daily":{
              "skycon":[{"date":"2026-10-15T00:00+08:00","value":"THUNDER_SHOWER"}],
              "precipitation":[{"date":"2026-10-15T00:00+08:00","probability":1.7}]}}}"#;
        let snapshot = parse_weather(body, location()).unwrap();
        assert_eq!(snapshot.daily[0].precipitation_probability_percent, None);
        let condition = snapshot.daily[0].condition.as_ref().unwrap();
        assert_eq!(condition.text, "未知");
        assert_eq!(condition.code, "THUNDER_SHOWER");
    }
}
