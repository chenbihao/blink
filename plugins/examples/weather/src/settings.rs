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
    pub city: String,
    pub unit: TemperatureUnit,
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

    let primary_city = query.or(argument_city);
    let city = primary_city
        .filter(|city| !city.trim().is_empty())
        .or(default_city)
        .map(str::trim)
        .filter(|city| !city.is_empty())
        .ok_or_else(|| crate::protocol::PluginError {
            code: "location_not_configured".to_owned(),
            message: "请在查询中输入城市，或配置默认城市。".to_owned(),
        })?;

    if city.chars().count() > 200 {
        return Err(invalid_arguments("城市名称不能超过 200 个字符。"));
    }

    Ok(RequestParameters {
        city: city.to_owned(),
        unit: argument_unit
            .or(configured_unit)
            .unwrap_or(TemperatureUnit::Celsius),
    })
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
    use super::{TemperatureUnit, parse};
    use crate::protocol::PluginError;
    use serde_json::{Value, json};

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
                assert!(error.message.contains("默认城市"));
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
