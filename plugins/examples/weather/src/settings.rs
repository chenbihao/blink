use serde::Serialize;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TemperatureUnit {
    Celsius,
    Fahrenheit,
}

impl TemperatureUnit {
    /// Format a Celsius input in this unit, rounded to the nearest whole degree.
    pub fn format_temperature(self, value: f64) -> String {
        let (value, suffix) = match self {
            Self::Celsius => (value, "°C"),
            Self::Fahrenheit => (value * 9.0 / 5.0 + 32.0, "°F"),
        };
        let rounded = value.round();
        let rounded = if rounded == 0.0 { 0.0 } else { rounded };
        format!("{rounded:.0}{suffix}")
    }
}

pub struct RequestParameters {
    #[cfg(test)]
    pub city: String,
    pub unit: TemperatureUnit,
    pub location: Option<crate::model::ResolvedLocation>,
    pub location_query: Option<crate::location::LocationQuery>,
}

pub fn parse(
    query: Option<&str>,
    arguments: Option<&Value>,
    settings: Option<&Value>,
) -> Result<RequestParameters, crate::protocol::PluginError> {
    let arguments = as_object(arguments, "arguments")?;
    let settings = as_object(settings, "settings")?;

    let argument_city = optional_string(arguments, "city", "arguments.city")?;
    let default_city = optional_string(settings, "default_city", "settings.default_city")?;
    let argument_unit = parse_unit(
        optional_string(arguments, "unit", "arguments.unit")?,
        "arguments.unit",
    )?;
    let configured_unit = parse_unit(
        optional_string(settings, "temperature_unit", "settings.temperature_unit")?,
        "settings.temperature_unit",
    )?;

    if let Some(provider) =
        optional_string(settings, "default_provider", "settings.default_provider")?
    {
        if provider != "open_meteo" {
            return Err(crate::protocol::PluginError {
                code: "provider_not_configured".into(),
                message: "当前版本仅支持 Open-Meteo 天气来源".into(),
            });
        }
    }
    let country = optional_string(arguments, "country_code", "arguments.country_code")?;
    let admin = optional_string(arguments, "admin", "arguments.admin")?;
    let explicit_location = arguments.and_then(|v| v.get("location"));
    if explicit_location.is_some() && arguments.is_some_and(|v| v.contains_key("city")) {
        return Err(invalid_arguments("city 与 location 不能同时提供"));
    }
    let explicit_city = query
        .or(argument_city)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let configured_location = settings
        .and_then(|v| v.get("default_location"))
        .filter(|v| !v.is_null());
    let (city, location) = if let Some(value) = explicit_location {
        (String::new(), Some(parse_location(value, false)?))
    } else if let Some(city) = explicit_city {
        (city.to_owned(), None)
    } else if let Some(value) = configured_location {
        (String::new(), Some(parse_location(value, true)?))
    } else {
        let city = default_city
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| crate::protocol::PluginError {
                code: "location_not_configured".into(),
                message: "请在设置中确认默认地点，或输入城市/提供坐标".into(),
            })?;
        (city.to_owned(), None)
    };
    if location.is_some() && (country.is_some() || admin.is_some()) {
        return Err(invalid_arguments(
            "country_code/admin 只用于城市名称搜索，不能与坐标地点同传",
        ));
    }
    let location_query = if location.is_none() {
        Some(crate::location::parse_query(&city, country, admin, 20)?)
    } else {
        None
    };
    Ok(RequestParameters {
        #[cfg(test)]
        city,
        location,
        location_query,
        unit: argument_unit
            .or(configured_unit)
            .unwrap_or(TemperatureUnit::Celsius),
    })
}

/// 保存地点沿用解析器信息；工具传入的名称/来源仅是用户元信息，不冒充解析器确认。
pub fn parse_location(
    value: &Value,
    trusted: bool,
) -> Result<crate::model::ResolvedLocation, crate::protocol::PluginError> {
    let obj = value
        .as_object()
        .ok_or_else(|| invalid_arguments("location 必须是对象"))?;
    let latitude = obj
        .get("latitude")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && (-90.0..=90.0).contains(v))
        .ok_or_else(|| invalid_arguments("纬度必须在 -90 到 90 之间"))?;
    let longitude = obj
        .get("longitude")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && (-180.0..=180.0).contains(v))
        .ok_or_else(|| invalid_arguments("经度必须在 -180 到 180 之间"))?;
    if obj.get("coordinate_system").and_then(Value::as_str) != Some("wgs84") {
        return Err(invalid_arguments("当前仅支持明确标注的 wgs84 坐标"));
    }
    let string = |key: &str| -> Result<Option<String>, crate::protocol::PluginError> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Some(s)
                .map(|s| {
                    if s.chars().count() > 200 {
                        Err(invalid_arguments("地点元信息过长"))
                    } else {
                        Ok(s.trim().to_string())
                    }
                })
                .transpose(),
            _ => Err(invalid_arguments("地点元信息须为文本")),
        }
    };
    let name = string("name")?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "指定坐标".into());
    let administrative_area = string("administrative_area")?.unwrap_or_default();
    let country = string("country")?.unwrap_or_default();
    let country_code = string("country_code")?.filter(|s| !s.is_empty());
    if country_code
        .as_ref()
        .is_some_and(|s| s.len() != 2 || !s.bytes().all(|b| b.is_ascii_alphabetic()))
    {
        return Err(invalid_arguments("国家代码须为两位字母"));
    }
    let administrative_areas = match obj.get("administrative_areas") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(values)) if values.len() <= 4 => values
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|s| s.chars().count() <= 200)
                    .map(str::to_string)
                    .ok_or_else(|| invalid_arguments("行政区须为文本列表"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(invalid_arguments("行政区须为至多四层文本列表")),
    };
    let display_name = if trusted {
        string("display_name")?.filter(|s| !s.is_empty())
    } else {
        None
    }
    .unwrap_or_else(|| {
        [
            name.as_str(),
            administrative_area.as_str(),
            country.as_str(),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
    });
    let kind = if trusted {
        match obj.get("kind").and_then(Value::as_str) {
            Some("city_center") => "city_center",
            Some("district_center") => "district_center",
            _ => "manual",
        }
    } else {
        "manual"
    };
    let source = if trusted && obj.get("source").and_then(Value::as_str) == Some("open_meteo") {
        "open_meteo"
    } else {
        "user"
    };
    Ok(crate::model::ResolvedLocation {
        name,
        display_name,
        latitude,
        longitude,
        coordinate_system: "wgs84",
        administrative_area,
        administrative_areas,
        country,
        country_code,
        timezone: string("timezone")?,
        kind,
        source,
        source_id: if trusted { string("source_id")? } else { None },
        resolved_at: if trusted {
            string("resolved_at")?
        } else {
            None
        }
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
    })
}

pub fn parse_search(
    arguments: &Value,
) -> Result<crate::location::LocationQuery, crate::protocol::PluginError> {
    let args = arguments
        .as_object()
        .ok_or_else(|| invalid_arguments("搜索参数必须是对象"))?;
    let query = optional_string(Some(args), "query", "query")?
        .ok_or_else(|| invalid_arguments("query 必填"))?;
    let country = optional_string(Some(args), "country_code", "country_code")?;
    let admin = optional_string(Some(args), "admin", "admin")?;
    let limit = match args.get("limit") {
        None => 10,
        Some(v) => {
            v.as_u64()
                .filter(|v| (1..=20).contains(v))
                .ok_or_else(|| invalid_arguments("limit 必须是1到20的整数"))? as usize
        }
    };
    crate::location::parse_query(query, country, admin, limit)
}
fn as_object<'a>(
    value: Option<&'a Value>,
    field: &str,
) -> Result<Option<&'a Map<String, Value>>, crate::protocol::PluginError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(object)) => Ok(Some(object)),
        Some(_) => Err(invalid_arguments(format!("{field} 必须是对象或 null。"))),
    }
}

fn optional_string<'a>(
    object: Option<&'a Map<String, Value>>,
    field: &str,
    label: &str,
) -> Result<Option<&'a str>, crate::protocol::PluginError> {
    match object.and_then(|object| object.get(field)) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(invalid_arguments(format!("{label} 必须是字符串。"))),
    }
}

fn parse_unit(
    value: Option<&str>,
    field: &str,
) -> Result<Option<TemperatureUnit>, crate::protocol::PluginError> {
    match value {
        None => Ok(None),
        Some("celsius") => Ok(Some(TemperatureUnit::Celsius)),
        Some("fahrenheit") => Ok(Some(TemperatureUnit::Fahrenheit)),
        Some(_) => Err(invalid_arguments(format!(
            "{field} 必须是 celsius 或 fahrenheit。"
        ))),
    }
}

fn invalid_arguments(message: impl Into<String>) -> crate::protocol::PluginError {
    crate::protocol::PluginError {
        code: "invalid_arguments".to_owned(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{TemperatureUnit, parse, parse_search};
    use crate::protocol::PluginError;
    use serde_json::{Value, json};

    #[test]
    fn configured_coordinates_and_explicit_location_skip_name_resolution() {
        let saved = json!({"latitude":23.13,"longitude":113.26,"coordinate_system":"wgs84","name":"广州","source":"open_meteo","source_id":"open_meteo:1","kind":"city_center","country_code":null,"timezone":null});
        let settings = json!({"default_location":saved,"default_city":"错误的旧城市","temperature_unit":"fahrenheit"});
        let defaults = parse(None, Some(&json!({})), Some(&settings)).unwrap();
        assert!(defaults.location_query.is_none());
        assert_eq!(defaults.location.unwrap().source, "open_meteo");
        assert_eq!(defaults.unit, TemperatureUnit::Fahrenheit);
        let name = parse(None, Some(&json!({"city":"北京"})), Some(&settings)).unwrap();
        assert!(name.location.is_none());
        assert_eq!(name.location_query.unwrap().query, "北京");
        let explicit = parse(None, Some(&json!({"location":saved})), Some(&settings))
            .unwrap()
            .location
            .unwrap();
        assert_eq!(explicit.source, "user");
        assert_eq!(explicit.kind, "manual");
        assert!(explicit.source_id.is_none());
        assert!(parse(None, Some(&json!({"city":"北京","location":saved})), None).is_err());
    }

    #[test]
    fn cleared_location_does_not_restore_legacy_city_and_coordinates_are_validated() {
        let settings = json!({"default_location":null,"default_city":""});
        assert_eq!(
            error_code(parse(None, Some(&json!({})), Some(&settings))),
            "location_not_configured"
        );
        for location in [
            json!({"latitude":91,"longitude":0,"coordinate_system":"wgs84"}),
            json!({"latitude":0,"longitude":181,"coordinate_system":"wgs84"}),
            json!({"latitude":0,"longitude":0,"coordinate_system":"gcj02"}),
            json!({"latitude":null,"longitude":0,"coordinate_system":"wgs84"}),
        ] {
            assert_eq!(
                error_code(parse(None, Some(&json!({"location":location})), None)),
                "invalid_arguments"
            );
        }
        assert!(parse_search(&json!({"query":"广州","limit":0})).is_err());
        assert!(parse_search(&json!({"query":"广州","limit":1.5})).is_err());
        assert!(parse_search(&json!({"query":"广州","country_code":"CHN"})).is_err());
    }

    fn error_code(result: Result<super::RequestParameters, PluginError>) -> String {
        match result {
            Ok(_) => panic!("expected parsing to fail"),
            Err(error) => error.code,
        }
    }

    #[test]
    fn temperature_formatting_converts_and_rounds_to_whole_degrees() {
        assert_eq!(TemperatureUnit::Celsius.format_temperature(20.4), "20°C");
        assert_eq!(TemperatureUnit::Celsius.format_temperature(20.5), "21°C");
        assert_eq!(TemperatureUnit::Celsius.format_temperature(-0.4), "0°C");
        assert_eq!(TemperatureUnit::Fahrenheit.format_temperature(20.0), "68°F");
        assert_eq!(TemperatureUnit::Fahrenheit.format_temperature(20.3), "69°F");
    }

    #[test]
    fn temperature_units_serialize_as_lowercase_names() {
        assert_eq!(
            serde_json::to_string(&TemperatureUnit::Celsius).unwrap(),
            "\"celsius\""
        );
        assert_eq!(
            serde_json::to_string(&TemperatureUnit::Fahrenheit).unwrap(),
            "\"fahrenheit\""
        );
    }

    #[test]
    fn explicit_argument_unit_overrides_configured_unit() {
        let parameters = parse(
            None,
            Some(&json!({"city": "Paris", "unit": "fahrenheit"})),
            Some(&json!({
                "default_city": "London",
                "temperature_unit": "celsius"
            })),
        )
        .unwrap();

        assert_eq!(parameters.city, "Paris");
        assert_eq!(parameters.unit, TemperatureUnit::Fahrenheit);
    }

    #[test]
    fn query_or_tool_city_overrides_default_and_blank_city_falls_back() {
        let settings = json!({"default_city": "Default City"});
        let cases = [
            (Some("  Kyoto  "), Some(json!({"city": "Osaka"})), "Kyoto"),
            (None, Some(json!({"city": "Osaka"})), "Osaka"),
            (Some("  "), Some(json!({"city": "Osaka"})), "Default City"),
            (None, Some(json!({"city": " \t"})), "Default City"),
            (None, None, "Default City"),
        ];

        for (query, arguments, expected_city) in cases {
            let parameters = parse(query, arguments.as_ref(), Some(&settings)).unwrap();
            assert_eq!(parameters.city, expected_city);
        }
    }

    #[test]
    fn absent_and_null_legacy_arguments_or_settings_are_accepted() {
        let null = Value::Null;
        let null_settings = json!({"default_city": "Shanghai"});

        for (arguments, settings) in [
            (None, Some(&null_settings)),
            (Some(&null), Some(&null_settings)),
            (None, Some(&null)),
            (Some(&null), Some(&null)),
            (None, None),
            (Some(&null), None),
        ] {
            match parse(Some("Beijing"), arguments, settings) {
                Ok(parameters) => assert_eq!(parameters.city, "Beijing"),
                Err(error) => panic!("legacy null inputs should parse: {}", error.code),
            }
        }

        assert_eq!(
            error_code(parse(None, Some(&null), Some(&null))),
            "location_not_configured"
        );
    }

    #[test]
    fn missing_city_returns_a_guidance_error() {
        let result = parse(None, None, Some(&json!({"default_city": "  "})));
        match result {
            Ok(_) => panic!("expected missing location error"),
            Err(error) => {
                assert_eq!(error.code, "location_not_configured");
                assert!(error.message.contains("输入城市"));
                assert!(error.message.contains("默认地点"));
            }
        }
    }

    #[test]
    fn invalid_argument_shapes_and_units_have_a_stable_error_code() {
        let invalid_arguments = [
            Value::String("city".to_owned()),
            json!({"city": 17}),
            json!({"unit": false}),
            json!({"unit": "kelvin"}),
        ];
        for arguments in invalid_arguments {
            assert_eq!(
                error_code(parse(Some("Paris"), Some(&arguments), None)),
                "invalid_arguments"
            );
        }

        let invalid_settings = [
            Value::Bool(true),
            json!({"default_city": 17}),
            json!({"temperature_unit": false}),
            json!({"temperature_unit": "kelvin"}),
        ];
        for settings in invalid_settings {
            assert_eq!(
                error_code(parse(Some("Paris"), None, Some(&settings))),
                "invalid_arguments"
            );
        }
    }

    #[test]
    fn city_limit_counts_unicode_characters_not_utf8_bytes() {
        let two_hundred_chars = "城".repeat(200);
        let accepted = parse(Some(&two_hundred_chars), None, None).unwrap();
        assert_eq!(accepted.city.chars().count(), 200);

        let two_hundred_one_chars = "城".repeat(201);
        assert_eq!(
            error_code(parse(Some(&two_hundred_one_chars), None, None)),
            "invalid_arguments"
        );
    }
}
