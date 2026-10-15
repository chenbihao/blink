//! 配置写入的天气信任边界；查询与地理解析仍由天气插件负责。
//!
//! 和风 Host 校验与插件 `settings.rs` 的执行边界校验保持同一规则（允许的
//! API Host 形式见 `valid_qweather_host`）；两侧各自实现，改动须同步。
use serde_json::Value;

pub(super) fn validate_settings(settings: &Value) -> Result<(), String> {
    let invalid = || "invalid_arguments: 天气地点或设置格式无效".to_string();
    if let Some(provider) = settings.get("default_provider") {
        if !matches!(
            provider.as_str(),
            Some("open_meteo" | "qweather" | "caiyun")
        ) {
            return Err(invalid());
        }
    }
    if let Some(unit) = settings.get("temperature_unit") {
        if !matches!(unit.as_str(), Some("celsius" | "fahrenheit")) {
            return Err(invalid());
        }
    }
    if settings
        .get("allow_fallback")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(invalid());
    }
    if let Some(order) = settings.get("fallback_order") {
        if !order.as_array().is_some_and(|values| {
            values.len() <= 3
                && values.iter().all(|value| {
                    matches!(value.as_str(), Some("open_meteo" | "qweather" | "caiyun"))
                })
        }) {
            return Err(invalid());
        }
    }
    if let Some(host) = settings.get("qweather_api_host") {
        match host.as_str() {
            Some("") | None => {}
            Some(value) if valid_qweather_host(value) => {}
            _ => {
                return Err(
                    "invalid_arguments: 和风 API Host 请填写域名或 HTTPS 根地址（如 abc.re.qweatherapi.com）"
                        .to_string(),
                )
            }
        }
    }
    if let Some(key) = settings.get("qweather_api_key").filter(|v| !v.is_null()) {
        // 密钥明文不经过此处落库（CM 还原前已是空串占位）；只拒绝非文本形状。
        if !key.as_str().is_some_and(|v| v.chars().count() <= 200) {
            return Err(invalid());
        }
    }
    // App Secret 使用 CM；App Key 为普通可见设置，这里校验文本形状；
    // App Key 会拼进 URL 路径段，字符集由插件执行边界再校验一次。
    for field in ["caiyun_app_key", "caiyun_app_secret"] {
        if let Some(value) = settings.get(field).filter(|v| !v.is_null()) {
            if !value.as_str().is_some_and(|v| v.chars().count() <= 200) {
                return Err(invalid());
            }
        }
    }
    if let Some(version) = settings.get("settings_version") {
        if version.as_u64() != Some(1) {
            return Err(invalid());
        }
    }
    if let Some(location) = settings.get("default_location").filter(|v| !v.is_null()) {
        let obj = location.as_object().ok_or_else(invalid)?;
        let latitude = obj
            .get("latitude")
            .and_then(Value::as_f64)
            .ok_or_else(invalid)?;
        let longitude = obj
            .get("longitude")
            .and_then(Value::as_f64)
            .ok_or_else(invalid)?;
        if !latitude.is_finite()
            || !longitude.is_finite()
            || !(-90.0..=90.0).contains(&latitude)
            || !(-180.0..=180.0).contains(&longitude)
            || obj.get("coordinate_system").and_then(Value::as_str) != Some("wgs84")
        {
            return Err(invalid());
        }
        for key in [
            "name",
            "display_name",
            "administrative_area",
            "country",
            "country_code",
            "timezone",
            "source_id",
            "resolved_at",
        ] {
            if let Some(v) = obj.get(key).filter(|v| !v.is_null()) {
                if !v.as_str().is_some_and(|s| s.chars().count() <= 200) {
                    return Err(invalid());
                }
            }
        }
        if let Some(code) = obj
            .get("country_code")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if code.len() != 2 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
                return Err(invalid());
            }
        }
        // 区域声明（彩云坐标系规则用）：可选布尔；缺失 = 待用户确认
        if let Some(region) = obj.get("in_mainland_china").filter(|v| !v.is_null()) {
            if !region.is_boolean() {
                return Err(invalid());
            }
        }
        if let Some(areas) = obj.get("administrative_areas").filter(|v| !v.is_null()) {
            if !areas.as_array().is_some_and(|values| {
                values.len() <= 4
                    && values
                        .iter()
                        .all(|v| v.as_str().is_some_and(|s| s.chars().count() <= 200))
            }) {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

/// 和风 API Host 只接受 HTTPS 域名服务根地址：无路径/查询/碎片/用户信息/端口，
/// 域名至少两段且末段为字母（排除 IP 字面量），总长不超过 253。
///
/// 语法层无法区分"用户自有的自定义域名"与"形似官方域的伪后缀域名"（和风允许
/// 自定义域名）；凭据安全由宿主 HTTP 执行边界保证：Key 只发往此处配置的 host，
/// 跨目的地重定向被拒绝（见 `process.rs::execute_http_request`）。
pub(super) fn valid_qweather_host(value: &str) -> bool {
    let value = value.trim();
    let rest = value.strip_prefix("https://").unwrap_or(value);
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest.len() > 253 || rest.contains(['/', '?', '#', '@', ':', '\\']) {
        return false;
    }
    let labels: Vec<&str> = rest.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let valid_label = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    };
    if !labels.iter().all(|label| valid_label(label)) {
        return false;
    }
    // 末段须为纯字母 TLD（≥2 字符）：排除 IP 字面量与伪后缀数字结尾。
    let last = labels[labels.len() - 1];
    last.len() >= 2 && last.chars().all(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_dedicated_hosts_and_rejects_disguised_or_partial_urls() {
        assert!(valid_qweather_host("https://abc123.re.qweatherapi.com"));
        assert!(valid_qweather_host("https://weather.example.cn"));
        assert!(valid_qweather_host("abc.re.qweatherapi.com"));
        assert!(valid_qweather_host("https://abc.re.qweatherapi.com/"));
        // 路径 / 查询 / 非法 scheme / 端口 / IP / 单段域 / 前后点。语法合法的
        // 伪后缀域名（qweatherapi.com.evil.net）无法在语法层拒绝——和风允许自定义
        // 域名，凭据保护依赖重定向边界，见 valid_qweather_host 文档。
        for host in [
            "http://abc.re.qweatherapi.com",
            "https://abc.re.qweatherapi.com/geo/v2",
            "https://abc.re.qweatherapi.com?key=secret",
            "https://user@abc.re.qweatherapi.com",
            "https://abc.re.qweatherapi.com:8443",
            "https://127.0.0.1",
            "https://localhost",
            "https://abc..qweatherapi.com",
            "https://-abc.re.qweatherapi.com",
        ] {
            assert!(!valid_qweather_host(host), "应拒绝: {host}");
        }
    }

    #[test]
    fn settings_validation_accepts_qweather_provider_and_host_shapes() {
        validate_settings(&json!({
            "default_provider": "qweather",
            "qweather_api_host": "https://abc123.re.qweatherapi.com",
            "qweather_api_key": "",
            "temperature_unit": "celsius",
            "settings_version": 1
        }))
        .unwrap();
        assert!(validate_settings(&json!({"default_provider": "visualcrossing"})).is_err());
        assert!(validate_settings(&json!({"qweather_api_host": "abc"})).is_err());
        assert!(validate_settings(&json!({"qweather_api_key": 17})).is_err());
        // 空串/缺失 host 表示未配置，合法
        validate_settings(&json!({"qweather_api_host": ""})).unwrap();
        validate_settings(&json!({})).unwrap();
    }

    #[test]
    fn settings_validation_accepts_caiyun_credentials_and_region_declaration() {
        validate_settings(&json!({
            "default_provider": "caiyun",
            "caiyun_app_key": "T1FpNxFaceFhxX-0C",
            "caiyun_app_secret": "",
            "default_location": {
                "latitude": 31.1774, "longitude": 121.5272,
                "coordinate_system": "wgs84", "name": "上海",
                "in_mainland_china": true
            }
        }))
        .unwrap();
        // 区域声明只接受布尔；凭据非文本形状拒绝
        assert!(validate_settings(&json!({
            "default_location": {"latitude": 0, "longitude": 0, "coordinate_system": "wgs84", "in_mainland_china": "yes"}
        }))
        .is_err());
        assert!(validate_settings(&json!({"caiyun_app_key": 17})).is_err());
        assert!(validate_settings(&json!({"caiyun_app_secret": false})).is_err());
        validate_settings(&json!({"caiyun_app_key": ""})).unwrap();
    }
}
