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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenMeteo,
    QWeather,
    Caiyun,
}

/// 地点解析来源；Auto = 已配置和风时优先和风，否则 Open-Meteo。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolver {
    Auto,
    OpenMeteo,
    QWeather,
}

/// 本次调用可用的和风凭据（host 已通过校验规范化；key 仅用于请求头，不进日志）。
#[derive(Clone)]
pub struct QWeatherCredentials {
    pub host: String,
    pub key: String,
}

impl QWeatherCredentials {
    pub fn headers(&self) -> Vec<(String, String)> {
        vec![("X-QW-Api-Key".to_owned(), self.key.clone())]
    }
}

/// 本次调用可用的彩云凭据；App Key 只进入已验证的路径段，App Secret 只进签名，
/// 两者都不进日志。URL 固定为官方服务，不提供可编辑 Host。
#[derive(Clone)]
pub struct CaiyunCredentials {
    pub app_key: String,
    pub app_secret: String,
}

pub struct RequestParameters {
    #[cfg(test)]
    pub city: String,
    pub unit: TemperatureUnit,
    pub location: Option<crate::model::ResolvedLocation>,
    pub location_query: Option<crate::location::LocationQuery>,
    pub provider: Provider,
    pub qweather: Option<QWeatherCredentials>,
    pub caiyun: Option<CaiyunCredentials>,
}

/// 从 settings 提取已通过校验的彩云凭据；任一缺失即未配置（None）。
/// 非法 App Key（不满足安全路径段字符集）在执行边界明确报错，不静默跳过。
pub fn caiyun_credentials(
    settings: Option<&serde_json::Map<String, Value>>,
) -> Result<Option<CaiyunCredentials>, crate::protocol::PluginError> {
    let Some(settings) = settings else {
        return Ok(None);
    };
    let app_key = optional_string(Some(settings), "caiyun_app_key", "caiyun_app_key")?
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let app_secret = optional_string(Some(settings), "caiyun_app_secret", "caiyun_app_secret")?
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (app_key, app_secret) {
        (None, _) | (_, None) => Ok(None),
        (Some(app_key), Some(app_secret)) => {
            if !valid_caiyun_app_key(app_key) {
                return Err(invalid_arguments(
                    "彩云 App Key 只能包含字母、数字与 . _ ~ -（作为安全路径段使用）",
                ));
            }
            if app_secret.chars().count() > 200 {
                return Err(invalid_arguments("彩云 App Secret 过长"));
            }
            Ok(Some(CaiyunCredentials {
                app_key: app_key.to_owned(),
                app_secret: app_secret.to_owned(),
            }))
        }
    }
}

/// App Key 直接拼进 URL 路径段：只允许无需转义且无路径/查询语义的字符，
/// 从结构上排除 `/`、`?`、`#`、`%` 等注入，长度不超过 128。
pub fn valid_caiyun_app_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'-'))
}

/// 从 settings 提取已通过校验的和风凭据；host/key 任一缺失即未配置（None）。
/// 非法 host 值返回 Err——执行边界不能静默跳过无效配置。
pub fn qweather_credentials(
    settings: Option<&serde_json::Map<String, Value>>,
) -> Result<Option<QWeatherCredentials>, crate::protocol::PluginError> {
    let Some(settings) = settings else {
        return Ok(None);
    };
    let host = optional_string(Some(settings), "qweather_api_host", "qweather_api_host")?
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let key = optional_string(Some(settings), "qweather_api_key", "qweather_api_key")?
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (host, key) {
        (None, _) | (_, None) => Ok(None),
        (Some(host), Some(key)) => {
            if !valid_qweather_host(host) {
                return Err(invalid_arguments(
                    "和风 API Host 请填写域名或 HTTPS 根地址（如 abc123.re.qweatherapi.com）",
                ));
            }
            Ok(Some(QWeatherCredentials {
                host: format!(
                    "https://{}",
                    host.trim()
                        .strip_prefix("https://")
                        .unwrap_or(host.trim())
                        .trim_end_matches('/')
                ),
                key: key.to_owned(),
            }))
        }
    }
}

/// 与宿主 `weather_config.rs` 同一规则：HTTPS 域名根地址，无路径/查询/碎片/
/// 用户信息/端口，至少两段且末段为字母（排除 IP 字面量）。
pub fn valid_qweather_host(value: &str) -> bool {
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
    let last = labels[labels.len() - 1];
    last.len() >= 2 && last.chars().all(|c| c.is_ascii_alphabetic())
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
    let provider = match optional_string(settings, "default_provider", "settings.default_provider")?
    {
        None | Some("open_meteo") => Provider::OpenMeteo,
        Some("qweather") => Provider::QWeather,
        Some("caiyun") => Provider::Caiyun,
        Some(_) => {
            return Err(crate::protocol::PluginError {
                code: "invalid_arguments".into(),
                message: "天气来源必须是 open_meteo、qweather 或 caiyun".into(),
            });
        }
    };
    let mut qweather = if provider == Provider::QWeather {
        qweather_credentials(settings)?
    } else {
        None
    };
    if provider == Provider::QWeather && qweather.is_none() {
        return Err(crate::protocol::PluginError {
            code: "provider_not_configured".into(),
            message: "和风天气需要先在设置中配置 API Host 与 API Key".into(),
        });
    }
    let caiyun = if provider == Provider::Caiyun {
        caiyun_credentials(settings)?
    } else {
        None
    };
    if provider == Provider::Caiyun && caiyun.is_none() {
        return Err(crate::protocol::PluginError {
            code: "provider_not_configured".into(),
            message: "彩云天气需要先在设置中配置 App Key 与 App Secret".into(),
        });
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
        // 名称查询的 auto 解析仍优先已配置和风；明确坐标不使用解析器，
        // 不让闲置来源的凭据错误阻断当前天气来源。
        if provider != Provider::QWeather {
            qweather = qweather_credentials(settings)?;
        }
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
        provider,
        qweather,
        caiyun,
    })
}

/// 地点解析来源策略；显式和风缺配置在服务层报错（不静默换源），auto 由调用方决定。
pub fn parse_resolver(arguments: Option<&Value>) -> Result<Resolver, crate::protocol::PluginError> {
    match optional_string(
        as_object(arguments, "arguments")?,
        "resolver",
        "arguments.resolver",
    )? {
        None | Some("auto") => Ok(Resolver::Auto),
        Some("open_meteo") => Ok(Resolver::OpenMeteo),
        Some("qweather") => Ok(Resolver::QWeather),
        Some(_) => Err(invalid_arguments(
            "resolver 必须是 auto、open_meteo 或 qweather",
        )),
    }
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
    // 已确认地点保留解析来源（含和风命名空间）；工具传入的来源是用户元信息，不冒充。
    let source = match obj.get("source").and_then(Value::as_str) {
        Some("open_meteo") if trusted => "open_meteo",
        Some("qweather") if trusted => "qweather",
        _ => "user",
    };
    // 区域声明：保存地点保留解析器证据（Open-Meteo 国家码派生）；显式 location
    // 携带的是用户明确声明，同样有效（彩云据此决定 GCJ-02/WGS84），缺失即待确认。
    let in_mainland_china = match obj.get("in_mainland_china") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => return Err(invalid_arguments("in_mainland_china 必须是布尔值")),
    }
    .or_else(|| {
        if trusted && source == "qweather" {
            crate::location::coordinates::mainland_from_qweather_admin(
                &country,
                &administrative_area,
            )
        } else {
            None
        }
    });
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
        in_mainland_china,
    })
}

pub struct SearchParameters {
    pub query: crate::location::LocationQuery,
    pub resolver: Resolver,
}

pub fn parse_search(arguments: &Value) -> Result<SearchParameters, crate::protocol::PluginError> {
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
    Ok(SearchParameters {
        query: crate::location::parse_query(query, country, admin, limit)?,
        resolver: parse_resolver(Some(arguments))?,
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
    use super::TemperatureUnit;
    use super::{
        Provider, Resolver, caiyun_credentials, parse, parse_location, parse_resolver,
        parse_search, valid_caiyun_app_key, valid_qweather_host,
    };
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
    fn saved_qweather_location_keeps_resolver_metadata_on_reload() {
        // 和风解析器确认后保存的地点重启回读时保留来源/ID/类型，不退化为手工坐标。
        let saved = json!({"latitude":23.13,"longitude":113.26,"coordinate_system":"wgs84","name":"广州","source":"qweather","source_id":"qweather:101280101","kind":"city_center"});
        let defaults = parse(
            None,
            Some(&json!({})),
            Some(&json!({"default_location":saved})),
        )
        .unwrap();
        let location = defaults.location.unwrap();
        assert_eq!(location.source, "qweather");
        assert_eq!(location.source_id.as_deref(), Some("qweather:101280101"));
        assert_eq!(location.kind, "city_center");
        // 工具传入的同对象仍视为用户元信息，不冒充解析器确认。
        let explicit = parse(
            None,
            Some(&json!({"location":{"latitude":23.13,"longitude":113.26,"coordinate_system":"wgs84","source":"qweather","source_id":"qweather:101280101","kind":"city_center"}})),
            None,
        )
        .unwrap()
        .location
        .unwrap();
        assert_eq!(explicit.source, "user");
        assert_eq!(explicit.kind, "manual");
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

    #[test]
    fn provider_whitelist_and_qweather_readiness() {
        let host = json!("https://abc123.re.qweatherapi.com");
        // 显式选择和风但未配置 → provider_not_configured
        assert_eq!(
            error_code(parse(
                None,
                Some(&json!({})),
                Some(&json!({"default_provider": "qweather"}))
            )),
            "provider_not_configured"
        );
        // 有 host 无 key 同样未配置
        assert_eq!(
            error_code(parse(
                None,
                Some(&json!({})),
                Some(&json!({"default_provider": "qweather", "qweather_api_host": host}))
            )),
            "provider_not_configured"
        );
        // 完整配置：默认坐标直接查询
        let configured = parse(
            None,
            Some(&json!({})),
            Some(&json!({
                "default_provider": "qweather",
                "qweather_api_host": host,
                "qweather_api_key": "secret-key",
                "default_location": {"latitude": 23.13, "longitude": 113.26, "coordinate_system": "wgs84", "name": "广州"}
            })),
        )
        .unwrap();
        assert_eq!(configured.provider, Provider::QWeather);
        assert!(configured.qweather.is_some());
        // 未知来源值拒绝；open_meteo 是缺省，和风配置同时存在不改变缺省来源
        assert_eq!(
            error_code(parse(
                None,
                Some(&json!({})),
                Some(&json!({"default_provider": "visualcrossing"}))
            )),
            "invalid_arguments"
        );
        let open_meteo = parse(
            None,
            Some(&json!({"city": "北京"})),
            Some(&json!({"qweather_api_host": host, "qweather_api_key": "secret-key"})),
        )
        .unwrap();
        assert_eq!(open_meteo.provider, Provider::OpenMeteo);
        assert!(open_meteo.qweather.is_some());
    }

    #[test]
    fn caiyun_provider_requires_both_credentials_and_validates_app_key() {
        // 显式彩云未配置 / 只有一半 → provider_not_configured，不混用一新一旧快照
        assert_eq!(
            error_code(parse(
                None,
                Some(&json!({})),
                Some(&json!({"default_provider": "caiyun"}))
            )),
            "provider_not_configured"
        );
        assert_eq!(
            error_code(parse(
                None,
                Some(&json!({})),
                Some(&json!({"default_provider": "caiyun", "caiyun_app_key": "key-only"}))
            )),
            "provider_not_configured"
        );
        // 完整凭据 + 明确区域声明的默认地点可直接执行
        let configured = parse(
            None,
            Some(&json!({})),
            Some(&json!({
                "default_provider": "caiyun",
                "caiyun_app_key": "T1FpNxFaceFhxX-0C",
                "caiyun_app_secret": "secret",
                "default_location": {"latitude": 39.9, "longitude": 116.4, "coordinate_system": "wgs84", "name": "北京", "in_mainland_china": true}
            })),
        )
        .unwrap();
        assert_eq!(configured.provider, Provider::Caiyun);
        let caiyun = configured.caiyun.as_ref().unwrap();
        assert_eq!(caiyun.app_key, "T1FpNxFaceFhxX-0C");
        assert_eq!(configured.location.unwrap().in_mainland_china, Some(true));
        // App Key 是路径段：斜杠/查询/编码字符/非 ASCII 拒绝
        for key in [
            "key/with/slash",
            "key?query",
            "key#fragment",
            "key%2F",
            "key with space",
            "密钥",
        ] {
            assert!(!valid_caiyun_app_key(key), "应拒绝 App Key: {key}");
            assert_eq!(
                error_code(parse(
                    None,
                    Some(&json!({})),
                    Some(
                        &json!({"default_provider":"caiyun", "caiyun_app_key": key, "caiyun_app_secret": "s"})
                    )
                )),
                "invalid_arguments",
                "key={key}"
            );
        }
        // 未选择彩云时，无效 Key 也不影响其他来源执行（凭据只在需要时提取）
        let open_meteo = parse(
            None,
            Some(&json!({"city": "北京"})),
            Some(&json!({"caiyun_app_key": "bad key", "caiyun_app_secret": "s"})),
        );
        assert!(open_meteo.is_ok());
        // 凭据提取独立可用：缺失/空白=未配置（None），不报错
        assert!(caiyun_credentials(None).unwrap().is_none());
        let blank = json!({"caiyun_app_key": "  ", "caiyun_app_secret": ""});
        assert!(caiyun_credentials(blank.as_object()).unwrap().is_none());
    }

    #[test]
    fn coordinate_weather_ignores_unused_credentials_but_validates_the_selected_provider() {
        let location = json!({"latitude":23.13,"longitude":113.26,"coordinate_system":"wgs84"});
        let mut settings = json!({
            "default_location":location,
            "qweather_api_host":"not-a-host","qweather_api_key":"fixture-key",
            "caiyun_app_key":"bad/key","caiyun_app_secret":"fixture-secret"
        });
        let request = parse(None, None, Some(&settings)).unwrap();
        assert!(request.qweather.is_none() && request.caiyun.is_none());

        settings["default_provider"] = json!("qweather");
        assert_eq!(
            error_code(parse(None, None, Some(&settings))),
            "invalid_arguments"
        );
        settings["qweather_api_host"] = json!("https://fixture.qweatherapi.com");
        let request = parse(None, None, Some(&settings)).unwrap();
        assert!(request.qweather.is_some() && request.caiyun.is_none());

        settings["default_provider"] = json!("caiyun");
        assert_eq!(
            error_code(parse(None, None, Some(&settings))),
            "invalid_arguments"
        );
        settings["caiyun_app_key"] = json!("fixture-key");
        settings["qweather_api_host"] = json!("not-a-host");
        let request = parse(None, None, Some(&settings)).unwrap();
        assert!(request.qweather.is_none() && request.caiyun.is_some());

        // 名称查询实际使用 auto 解析器，错误和风配置仍需反馈，不能静默换源。
        assert_eq!(
            error_code(parse(Some("广州"), None, Some(&settings))),
            "invalid_arguments"
        );
    }

    #[test]
    fn location_region_declaration_round_trips_for_saved_and_explicit_locations() {
        // 保存地点：解析器证据随地点保留
        let saved = json!({"latitude":39.9,"longitude":116.4,"coordinate_system":"wgs84","name":"北京","in_mainland_china":true});
        let defaults = parse(
            None,
            Some(&json!({})),
            Some(&json!({"default_location":saved})),
        )
        .unwrap();
        assert_eq!(defaults.location.unwrap().in_mainland_china, Some(true));
        // 显式 location：用户声明同样保留（香港=非大陆）
        let explicit = json!({"latitude":22.32,"longitude":114.17,"coordinate_system":"wgs84","in_mainland_china":false});
        let manual = parse(None, Some(&json!({"location":explicit})), None).unwrap();
        assert_eq!(manual.location.unwrap().in_mainland_china, Some(false));
        // 缺失 = 待确认；非法形状拒绝
        let unknown = json!({"latitude":0,"longitude":0,"coordinate_system":"wgs84"});
        let manual = parse(None, Some(&json!({"location":unknown})), None).unwrap();
        assert_eq!(manual.location.unwrap().in_mainland_china, None);
        assert_eq!(
            error_code(parse(
                None,
                Some(
                    &json!({"location":{"latitude":0,"longitude":0,"coordinate_system":"wgs84","in_mainland_china":"yes"}})
                ),
                None
            )),
            "invalid_arguments"
        );
    }

    #[test]
    fn saved_qweather_province_recovers_missing_region_but_explicit_location_requires_a_declaration()
     {
        let saved = json!({"latitude":23.13,"longitude":113.26,"coordinate_system":"wgs84",
            "name":"海珠","country":"中国","source":"qweather","administrative_area":"广东省",
            "administrative_areas":["广东省","广州"],"in_mainland_china":null});
        let resolved = parse_location(&saved, true).unwrap();
        assert_eq!(resolved.in_mainland_china, Some(true));
        assert_eq!(
            parse_location(&saved, false).unwrap().in_mainland_china,
            None
        );
        let mut manual_override = saved.clone();
        manual_override["in_mainland_china"] = json!(false);
        assert_eq!(
            parse_location(&manual_override, true)
                .unwrap()
                .in_mainland_china,
            Some(false)
        );
    }

    #[test]
    fn invalid_qweather_host_fails_at_execution_boundary() {
        for host in [
            "http://abc.re.qweatherapi.com",
            "https://abc.re.qweatherapi.com/path",
            "https://abc.re.qweatherapi.com:8443",
            "https://127.0.0.1",
        ] {
            assert_eq!(
                error_code(parse(
                    None,
                    Some(&json!({"city": "北京"})),
                    Some(&json!({
                        "qweather_api_host": host,
                        "qweather_api_key": "secret-key"
                    }))
                )),
                "invalid_arguments",
                "host={host}"
            );
        }
        assert!(valid_qweather_host("https://abc123.re.qweatherapi.com"));
        assert!(valid_qweather_host("abc123.re.qweatherapi.com"));
        assert!(valid_qweather_host("https://abc123.re.qweatherapi.com/"));
    }

    #[test]
    fn resolver_argument_accepts_known_values_only() {
        assert_eq!(parse_resolver(None).unwrap(), Resolver::Auto);
        assert_eq!(
            parse_resolver(Some(&json!({"resolver": "qweather"}))).unwrap(),
            Resolver::QWeather
        );
        assert_eq!(
            parse_resolver(Some(&json!({"resolver": "open_meteo"}))).unwrap(),
            Resolver::OpenMeteo
        );
        assert_eq!(
            parse_resolver(Some(&json!({"resolver": "caiyun"})))
                .unwrap_err()
                .code,
            "invalid_arguments"
        );
        let search = parse_search(&json!({"query": "广州", "resolver": "qweather"})).unwrap();
        assert_eq!(search.resolver, Resolver::QWeather);
        assert_eq!(search.query.query, "广州");
    }
}
