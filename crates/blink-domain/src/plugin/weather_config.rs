//! 配置写入的天气信任边界；查询与地理解析仍由天气插件负责。
use serde_json::Value;

pub(super) fn validate_settings(settings: &Value) -> Result<(), String> {
    let invalid = || "invalid_arguments: 天气地点或设置格式无效".to_string();
    if let Some(provider) = settings.get("default_provider") {
        if provider.as_str() != Some("open_meteo") {
            return Err(invalid());
        }
    }
    if let Some(unit) = settings.get("temperature_unit") {
        if !matches!(unit.as_str(), Some("celsius" | "fahrenheit")) {
            return Err(invalid());
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
