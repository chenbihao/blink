use super::pipeline::Stage;
use super::*;

use serde_json::json;

fn qweather_settings() -> serde_json::Value {
    json!({
        "default_provider": "qweather",
        "qweather_api_host": "https://abc123.re.qweatherapi.com",
        "qweather_api_key": "test-key",
        "default_city": "广州"
    })
}

fn caiyun_settings() -> serde_json::Value {
    json!({
        "default_provider": "caiyun",
        "caiyun_app_key": "test-app-key",
        "caiyun_app_secret": "test-app-secret",
        "default_location": {
            "latitude": 31.1774276, "longitude": 121.5272106,
            "coordinate_system": "wgs84", "name": "上海", "in_mainland_china": true
        }
    })
}

fn tool(id: &str, budget_ms: Option<u64>) -> CoreToPlugin {
    CoreToPlugin::ToolCall {
        id: id.into(),
        tool_name: "get_weather".into(),
        arguments: json!({}),
        settings: Some(json!({"default_city":"广州"})),
        budget_ms,
    }
}

fn qweather_location() -> serde_json::Value {
    json!({"location":{"latitude":23.129,"longitude":113.264,"coordinate_system":"wgs84","name":"广州"}})
}

fn http_id(message: &PluginToCore) -> String {
    match message {
        PluginToCore::HttpRequest(request) => request.id.clone(),
        _ => panic!("expected HTTP request"),
    }
}

fn http_ids(messages: &[PluginToCore]) -> Vec<String> {
    messages.iter().map(http_id).collect()
}

fn geo_response(id: String, body: &str) -> CoreToPlugin {
    CoreToPlugin::HttpResponse {
        id,
        status: 200,
        body: Some(body.to_owned()),
        error: None,
    }
}

const GEO_UNIQUE: &str =
    r#"{"results":[{"name":"广州","latitude":23.13,"longitude":113.26,"feature_code":"PPLA"}]}"#;

const QW_DAILY_ONE: &str = r#"{"days":[{"forecastStartTime":"2026-10-15T06:00+08:00","temperatureMax":{"value":30,"unit":"°C"},"temperatureMin":{"value":20,"unit":"°C"},"daytime":{"condition":{"text":"小雨","code":"305"},"wind":{"speed":{"value":1.87,"unit":"m/s"}},"precipitation":{"probability":0.4}},"nighttime":{"condition":{"text":"多云","code":101},"wind":{"speed":{"value":8.0,"unit":"km/h"}},"precipitation":{"probability":0.64}}}]}"#;
const QW_CURRENT: &str = r#"{"condition":{"text":"阴","code":104},"temperature":{"value":21.5,"unit":"°C"},"wind":{"speed":{"value":3.0,"unit":"m/s"}},"metadata":{"attributions":["https://developer.qweather.com/attribution.html"]}}"#;

#[test]
fn caiyun_unconfirmed_region_fails_before_any_http_request() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let result = service.handle(
            CoreToPlugin::ToolCall {
                id: "cy-unknown".into(),
                tool_name: "get_weather".into(),
                arguments: json!({"location":{"latitude":39.9,"longitude":116.4,"coordinate_system":"wgs84"}}),
                settings: Some(caiyun_settings()),
                budget_ms: None,
            },
            now,
        );
    let PluginToCore::RawResult(raw) = &result[0] else {
        panic!("expected error");
    };
    let error = raw.error.as_ref().unwrap();
    assert_eq!(error.code, "invalid_arguments");
    assert!(error.message.contains("中国大陆"));
    assert!(service.pipeline.queries.is_empty() && service.pipeline.http.is_empty());
}

#[test]
fn caiyun_single_request_signs_derived_gcj02_coordinates() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let result = service.handle(
        CoreToPlugin::ToolCall {
            id: "cy".into(),
            tool_name: "get_weather".into(),
            arguments: json!({}),
            settings: Some(caiyun_settings()),
            budget_ms: Some(9000),
        },
        now,
    );
    let PluginToCore::HttpRequest(request) = &result[0] else {
        panic!("expected http request");
    };
    // 经度在前；上海 WGS84→GCJ-02 派生坐标；查询 7 天
    assert_eq!(
        request.url,
        "https://api.caiyunapp.com/v2.6/test-app-key/121.531542,31.175304/weather?dailysteps=7&unit=metric%3Av2"
    );
    let names: Vec<&str> = request
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(names.contains(&"x-cy-signature"));
    assert!(names.contains(&"x-cy-nonce"));
    assert!(names.contains(&"x-cy-timestamp"));
    assert_eq!(request.timeout_ms, 9000);
    // Secret 不出现在 URL 或任何头值；App Key 只出现在路径
    assert!(!request.url.contains("test-app-secret"));
    assert!(
        request
            .headers
            .iter()
            .all(|(_, value)| !value.contains("test-app-secret"))
    );
    // 原始地点仍是 WGS84 原值，未被派生坐标覆盖
    let pending = service.pipeline.queries.values().next().unwrap();
    match &pending.stage {
        Stage::WeatherCaiyun(location) => {
            assert_eq!(location.latitude, 31.1774276);
            assert_eq!(location.coordinate_system, "wgs84");
        }
        _ => panic!("expected caiyun stage"),
    }
}

#[test]
fn caiyun_response_aggregates_partial_and_maps_provider_errors() {
    // 实况可用 + 3 天预报 → partial 并说明实际天数
    let mut service = WeatherService::default();
    let now = Instant::now();
    let result = service.handle(
        CoreToPlugin::ToolCall {
            id: "cy".into(),
            tool_name: "get_weather".into(),
            arguments: json!({}),
            settings: Some(caiyun_settings()),
            budget_ms: None,
        },
        now,
    );
    let id = http_id(&result[0]);
    let body = r#"{"status":"ok","result":{
            "realtime":{"status":"ok","temperature":23.5,"skycon":"CLEAR_DAY","wind":{"speed":8.2}},
            "daily":{
              "skycon":[{"date":"2026-10-15T00:00+08:00","value":"CLOUDY"}],
              "temperature":[
                {"date":"2026-10-15T00:00+08:00","max":27,"min":19},
                {"date":"2026-10-16T00:00+08:00","max":26,"min":18},
                {"date":"2026-10-17T00:00+08:00","max":25,"min":17}]}}}"#;
    let result = service.handle(geo_response(id, body), now);
    let PluginToCore::RawResult(raw) = &result[0] else {
        panic!("expected result");
    };
    assert_eq!(raw.data[0]["provider"], "caiyun");
    assert_eq!(raw.data[0]["status"], "partial");
    assert_eq!(raw.data[0]["temp"], "24°C");
    assert_eq!(raw.data[0]["condition"], "晴（白天）");
    assert_eq!(raw.data[0]["daily_condition_semantics"], "all_day_dominant");
    assert_eq!(raw.data[0]["availability"]["daily_days"], 3);
    assert!(
        raw.data[0]["copy_text"]
            .as_str()
            .unwrap()
            .contains("当天主导现象")
    );

    // 400 + token 错误体 → 鉴权失败；槽位释放
    let mut service = WeatherService::default();
    let result = service.handle(
        CoreToPlugin::ToolCall {
            id: "cy2".into(),
            tool_name: "get_weather".into(),
            arguments: json!({}),
            settings: Some(caiyun_settings()),
            budget_ms: None,
        },
        now,
    );
    let result = service.handle(
        CoreToPlugin::HttpResponse {
            id: http_id(&result[0]),
            status: 400,
            body: Some(r#"{"status":"failed","error":"token is invalid"}"#.into()),
            error: None,
        },
        now,
    );
    let PluginToCore::RawResult(raw) = &result[0] else {
        panic!("expected error");
    };
    assert_eq!(raw.error.as_ref().unwrap().code, "authentication_failed");
    assert!(service.pipeline.queries.is_empty() && service.pipeline.http.is_empty());
}

#[test]
fn caiyun_name_resolution_reuses_resolver_evidence_for_coordinates() {
    // 无和风配置：名称解析走 Open-Meteo；彩云凭据不影响解析来源
    let mut service = WeatherService::default();
    let now = Instant::now();
    let mut settings = caiyun_settings();
    settings["default_location"] = serde_json::Value::Null;
    settings["default_city"] = serde_json::json!("广州");
    let first = service.handle(
        CoreToPlugin::ToolCall {
            id: "cy3".into(),
            tool_name: "get_weather".into(),
            arguments: json!({}),
            settings: Some(settings),
            budget_ms: None,
        },
        now,
    );
    let PluginToCore::HttpRequest(request) = &first[0] else {
        panic!("expected geocoding request");
    };
    assert!(
        request
            .url
            .starts_with("https://geocoding-api.open-meteo.com/")
    );
    // 唯一候选携带国家码证据 → 进入彩云天气阶段无需再次确认区域
    let result = service.handle(
            geo_response(
                http_id(&first[0]),
                r#"{"results":[{"id":1,"name":"广州","latitude":23.13,"longitude":113.26,"feature_code":"PPLA","country_code":"CN","country":"中国"}]}"#,
            ),
            now,
        );
    let PluginToCore::HttpRequest(request) = &result[0] else {
        panic!("expected caiyun weather request");
    };
    assert!(
        request
            .url
            .starts_with("https://api.caiyunapp.com/v2.6/test-app-key/")
    );
    assert!(request.url.contains("/weather?dailysteps=7"));
    assert!(
        request
            .headers
            .iter()
            .any(|(name, _)| name == "x-cy-signature")
    );
}

#[test]
fn default_coordinates_start_weather_without_geocoding() {
    let mut service = WeatherService::default();
    let result = service.handle(CoreToPlugin::ToolCall{id:"coordinates".into(),tool_name:"get_weather".into(),arguments:json!({}),settings:Some(json!({"default_location":{"latitude":23.13,"longitude":113.26,"coordinate_system":"wgs84","name":"广州"},"default_city":"wrong"})),budget_ms:None},Instant::now());
    let PluginToCore::HttpRequest(request) = &result[0] else {
        panic!("weather expected");
    };
    assert!(
        request
            .url
            .starts_with("https://api.open-meteo.com/v1/forecast")
    );
    assert!(request.url.contains("latitude=23.13"));
}

#[test]
fn qweather_weather_issues_two_parallel_requests_with_key_header() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let result = service.handle(
        CoreToPlugin::ToolCall {
            id: "qw".into(),
            tool_name: "get_weather".into(),
            arguments: qweather_location(),
            settings: Some(qweather_settings()),
            budget_ms: Some(8000),
        },
        now,
    );
    assert_eq!(result.len(), 2, "current + daily 并行");
    let (first, second) = match (&result[0], &result[1]) {
        (PluginToCore::HttpRequest(a), PluginToCore::HttpRequest(b)) => (a, b),
        _ => panic!("expected http requests"),
    };
    assert!(first.url.contains("/weather/v1/current/23.13/113.26/"));
    assert!(second.url.contains("/weather/v1/daily/23.13/113.26/"));
    assert!(second.url.contains("days=7"));
    for request in [first, second] {
        assert_eq!(request.timeout_ms, 8000);
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "X-QW-Api-Key" && value == "test-key")
        );
        assert!(!request.url.contains("test-key"), "key 只能出现在 header");
    }
    assert_eq!(
        service.pipeline.queries.len(),
        1,
        "两个子请求只占一条逻辑查询"
    );
    assert_eq!(service.pipeline.http.len(), 2);
}

#[test]
fn qweather_first_response_waits_for_second_then_aggregates() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let requests = service.handle(
        CoreToPlugin::ToolCall {
            id: "qw".into(),
            tool_name: "get_weather".into(),
            arguments: qweather_location(),
            settings: Some(qweather_settings()),
            budget_ms: None,
        },
        now,
    );
    let ids = http_ids(&requests);
    // 第一份（daily）到达：无回复，查询保留等待另一侧
    let pending = service.handle(geo_response(ids[1].clone(), QW_DAILY_ONE), now);
    assert!(pending.is_empty());
    assert!(!service.pipeline.queries.is_empty());
    // 第二份（current）到达：聚合完成
    let result = service.handle(geo_response(ids[0].clone(), QW_CURRENT), now);
    let PluginToCore::RawResult(raw) = &result[0] else {
        panic!("expected aggregated result");
    };
    assert_eq!(raw.data[0]["provider"], "qweather");
    assert_eq!(raw.data[0]["temp"], "22°C");
    assert_eq!(raw.data[0]["condition"], "阴");
    assert_eq!(raw.data[0]["daily"][0]["condition"], "白天小雨/夜间多云");
    assert!(service.pipeline.queries.is_empty());
    assert!(service.pipeline.http.is_empty());
}

#[test]
fn qweather_day_only_daily_condition_has_no_night_segment() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let requests = service.handle(
        CoreToPlugin::ToolCall {
            id: "qw".into(),
            tool_name: "get_weather".into(),
            arguments: qweather_location(),
            settings: Some(qweather_settings()),
            budget_ms: None,
        },
        now,
    );
    let ids = http_ids(&requests);
    service.handle(geo_response(ids[1].clone(), r#"{"days":[{"forecastStartTime":"2026-10-15T06:00","temperatureMax":{"value":30,"unit":"°C"},"daytime":{"condition":{"text":"晴","code":100}}}]}"#), now);
    let result = service.handle(
        geo_response(ids[0].clone(), r#"{"condition":{"text":"晴","code":100}}"#),
        now,
    );
    let PluginToCore::RawResult(raw) = &result[0] else {
        panic!("expected result")
    };
    assert_eq!(raw.data[0]["daily"][0]["condition"], "白天晴");
}

#[test]
fn qweather_authentication_error_on_one_side_fails_the_query_after_both_settle() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let requests = service.handle(
        CoreToPlugin::ToolCall {
            id: "qw".into(),
            tool_name: "get_weather".into(),
            arguments: qweather_location(),
            settings: Some(qweather_settings()),
            budget_ms: None,
        },
        now,
    );
    let ids = http_ids(&requests);
    let unauthorized =
        r#"{"error":{"status":401,"type":"https://x/#unauthorized","title":"Unauthorized"}}"#;
    let result = service.handle(
        CoreToPlugin::HttpResponse {
            id: ids[0].clone(),
            status: 401,
            body: Some(unauthorized.to_owned()),
            error: None,
        },
        now,
    );
    assert!(result.is_empty(), "另一子请求仍在途，暂不回复");
    let result = service.handle(geo_response(ids[1].clone(), r#"{"days":[]}"#), now);
    let PluginToCore::RawResult(raw) = &result[0] else {
        panic!("expected failure")
    };
    assert_eq!(raw.error.as_ref().unwrap().code, "authentication_failed");
    assert!(service.pipeline.queries.is_empty() && service.pipeline.http.is_empty());
}

#[test]
fn qweather_cancel_removes_both_subrequests_and_late_responses_are_dropped() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let requests = service.handle(
        CoreToPlugin::ToolCall {
            id: "qw".into(),
            tool_name: "get_weather".into(),
            arguments: qweather_location(),
            settings: Some(qweather_settings()),
            budget_ms: None,
        },
        now,
    );
    let ids = http_ids(&requests);
    assert!(
        service
            .handle(CoreToPlugin::Cancel { id: "qw".into() }, now)
            .is_empty()
    );
    for id in ids {
        assert!(service.handle(geo_response(id, "{}"), now).is_empty());
    }
    assert!(service.pipeline.queries.is_empty() && service.pipeline.http.is_empty());
}

#[test]
fn qweather_parallel_requests_use_one_of_four_query_slots() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    for i in 0..MAX_PENDING_QUERIES {
        let requests = service.handle(
                CoreToPlugin::ToolCall {
                    id: format!("qw{i}"),
                    tool_name: "get_weather".into(),
                    arguments: json!({"location":{"latitude":23.129 + i as f64 / 100.0,"longitude":113.264,"coordinate_system":"wgs84"}}),
                    settings: Some(qweather_settings()),
                    budget_ms: Some(10_000),
                },
                now,
            );
        assert_eq!(requests.len(), 2);
    }
    assert_eq!(service.pipeline.queries.len(), MAX_PENDING_QUERIES);
    let busy = service.handle(
            CoreToPlugin::ToolCall {
                id: "extra".into(),
                tool_name: "get_weather".into(),
                arguments: json!({"location":{"latitude":24.0,"longitude":113.264,"coordinate_system":"wgs84"}}),
                settings: Some(qweather_settings()),
                budget_ms: None,
            },
            now,
        );
    let PluginToCore::RawResult(raw) = &busy[0] else {
        panic!("expected busy")
    };
    assert_eq!(raw.error.as_ref().unwrap().code, "busy");
}

#[test]
fn name_resolution_prefers_qweather_only_when_configured() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    // 未配置和风：名称解析走 Open-Meteo
    let om = service.handle(tool("om", None), now);
    let PluginToCore::HttpRequest(request) = &om[0] else {
        panic!()
    };
    assert!(
        request
            .url
            .starts_with("https://geocoding-api.open-meteo.com/")
    );
    // 已配置和风：名称解析优先和风 GeoAPI，天气来源仍独立
    let mut qw_tool = tool("qw", None);
    if let CoreToPlugin::ToolCall { settings, .. } = &mut qw_tool {
        *settings = Some(qweather_settings());
    }
    let qw = service.handle(qw_tool, now);
    let PluginToCore::HttpRequest(request) = &qw[0] else {
        panic!()
    };
    assert!(
        request
            .url
            .starts_with("https://abc123.re.qweatherapi.com/geo/v2/city/lookup")
    );
    assert!(request.url.contains("location="));
}

#[test]
fn qweather_geocoding_ambiguity_returns_candidates_with_resolver_label() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let mut qw_tool = tool("qw", None);
    if let CoreToPlugin::ToolCall { settings, .. } = &mut qw_tool {
        *settings = Some(qweather_settings());
    }
    let first = service.handle(qw_tool, now);
    let id = http_id(&first[0]);
    let body = r#"{"code":"200","location":[
            {"name":"朝阳","id":"101070301","lat":"41.57","lon":"120.45","adm1":"辽宁省","country":"中国","type":"city"},
            {"name":"朝阳","id":"101060407","lat":"39.92","lon":"116.44","adm1":"北京市","country":"中国","type":"city"}
        ]}"#;
    let result = service.handle(geo_response(id, body), now);
    let PluginToCore::RawResult(raw) = &result[0] else {
        panic!("expected needs_location")
    };
    assert_eq!(raw.data[0]["status"], "needs_location");
    assert_eq!(raw.data[0]["resolver"], "qweather");
    assert_eq!(raw.data[0]["candidates"].as_array().unwrap().len(), 2);
}

#[test]
fn search_resolver_explicit_qweather_without_credentials_errors() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let result = service.handle(
        CoreToPlugin::ToolCall {
            id: "search".into(),
            tool_name: "search_locations".into(),
            arguments: json!({"query":"广州","resolver":"qweather"}),
            settings: None,
            budget_ms: None,
        },
        now,
    );
    let PluginToCore::RawResult(raw) = &result[0] else {
        panic!("expected error")
    };
    assert_eq!(raw.error.as_ref().unwrap().code, "provider_not_configured");
    assert!(service.pipeline.queries.is_empty());
}

#[test]
fn search_resolver_open_meteo_explicit_or_auto_without_qweather() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    for (index, arguments) in [
        json!({"query":"广州","resolver":"open_meteo"}),
        json!({"query":"广州"}),
    ]
    .into_iter()
    .enumerate()
    {
        let result = service.handle(
            CoreToPlugin::ToolCall {
                id: format!("search{index}"),
                tool_name: "search_locations".into(),
                arguments,
                settings: None,
                budget_ms: None,
            },
            now,
        );
        let PluginToCore::HttpRequest(request) = &result[0] else {
            panic!("expected open-meteo search");
        };
        assert!(
            request
                .url
                .starts_with("https://geocoding-api.open-meteo.com/")
        );
        service.handle(
            CoreToPlugin::Cancel {
                id: format!("search{index}"),
            },
            now,
        );
    }
}

#[test]
fn search_via_qweather_returns_envelope_with_attribution_and_truncation() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let first = service.handle(
        CoreToPlugin::ToolCall {
            id: "search".into(),
            tool_name: "search_locations".into(),
            arguments: json!({"query":"朝阳","limit":2}),
            settings: Some(qweather_settings()),
            budget_ms: None,
        },
        now,
    );
    let PluginToCore::HttpRequest(request) = &first[0] else {
        panic!("expected qweather search")
    };
    assert!(request.url.contains("number=2"));
    let body = r#"{"code":"200","location":[
            {"name":"朝阳","id":"101070301","lat":"41.57","lon":"120.45","adm1":"辽宁省","country":"中国","type":"city"},
            {"name":"朝阳","id":"101060407","lat":"39.92","lon":"116.44","adm1":"北京市","country":"中国","type":"city"}
        ],"refer":{"sources":["https://developer.qweather.com/attribution.html"]}}"#;
    let result = service.handle(geo_response(request.id.clone(), body), now);
    let PluginToCore::RawResult(raw) = &result[0] else {
        panic!("expected envelope")
    };
    assert_eq!(raw.data[0]["resolver"], "qweather");
    assert_eq!(raw.data[0]["count"], 2);
    assert_eq!(raw.data[0]["truncated"], true, "返回数达到 number 上限");
    assert_eq!(
        raw.data[0]["attributions"][0],
        "https://developer.qweather.com/attribution.html"
    );
}

#[test]
fn ambiguous_city_returns_candidates_without_weather_or_config_writes() {
    let now = Instant::now();
    let mut service = WeatherService::default();
    let first = service.handle(tool("ambiguity", None), now);
    let result = service.handle(
            CoreToPlugin::HttpResponse {
                id: http_id(&first[0]),
                status: 200,
                error: None,
                body: Some(r#"{"results":[{"id":1,"name":"广州","latitude":23,"longitude":113,"feature_code":"PPL"},{"id":2,"name":"广州","latitude":24,"longitude":114,"feature_code":"PPL"}]}"#.into()),
            },
            now,
        );
    let PluginToCore::RawResult(result) = &result[0] else {
        panic!("candidate result expected");
    };
    assert_eq!(result.data[0]["status"], "needs_location");
    assert!(result.data[0].get("current").is_none());
    assert_eq!(result.data[0]["candidates"].as_array().unwrap().len(), 2);
    assert!(service.pipeline.queries.is_empty());
}

#[test]
fn search_empty_is_normal_and_city_admin_retry_uses_remaining_budget() {
    let now = Instant::now();
    let mut service = WeatherService::default();
    let first = service.handle(
        CoreToPlugin::ToolCall {
            id: "search".into(),
            tool_name: "search_locations".into(),
            arguments: json!({"query":"广州/天河区","limit":1}),
            settings: None,
            budget_ms: Some(100),
        },
        now,
    );
    let retry = service.handle(
        geo_response(http_id(&first[0]), "{}"),
        now + Duration::from_millis(30),
    );
    let PluginToCore::HttpRequest(request) = &retry[0] else {
        panic!("unqualified retry expected");
    };
    assert_eq!(request.timeout_ms, 70);
    assert!(!request.url.contains("%2C"));
    let result = service.handle(
        geo_response(request.id.clone(), "{}"),
        now + Duration::from_millis(40),
    );
    let PluginToCore::RawResult(result) = &result[0] else {
        panic!("search result expected");
    };
    assert!(result.error.is_none());
    assert_eq!(result.data[0]["count"], 0);
    assert!(service.pipeline.queries.is_empty());
}

#[test]
fn cancellation_drops_late_geo_without_starting_weather() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let initial = service.handle(tool("tc_1", None), now);
    let http = http_id(&initial[0]);
    assert!(
        service
            .handle(CoreToPlugin::Cancel { id: "tc_1".into() }, now)
            .is_empty()
    );
    assert!(
        service
            .handle(geo_response(http, GEO_UNIQUE), now)
            .is_empty()
    );
    assert!(service.pipeline.queries.is_empty());
}

#[test]
fn remaining_budget_is_shared_and_expired_response_cannot_continue() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let initial = service.handle(tool("tc_1", Some(100)), now);
    let id = http_id(&initial[0]);
    let next = service.handle(
        geo_response(id, GEO_UNIQUE),
        now + Duration::from_millis(60),
    );
    match &next[0] {
        PluginToCore::HttpRequest(r) => assert_eq!(r.timeout_ms, 40),
        _ => panic!("expected weather request"),
    }
    let result = service.handle(
        CoreToPlugin::HttpResponse {
            id: http_id(&next[0]),
            status: 200,
            body: Some("{}".into()),
            error: None,
        },
        now + Duration::from_millis(101),
    );
    match &result[0] {
        PluginToCore::RawResult(r) => assert_eq!(r.error.as_ref().unwrap().code, "timeout"),
        _ => panic!("expected timeout"),
    }
    assert!(service.pipeline.queries.is_empty());
}

#[test]
fn concurrent_requests_have_distinct_ids_and_cancel_does_not_cross_queries() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let a = service.handle(tool("a", None), now);
    let mut other = tool("b", None);
    if let CoreToPlugin::ToolCall { arguments, .. } = &mut other {
        *arguments = json!({"city":"北京"});
    }
    let b = service.handle(other, now);
    assert_ne!(http_id(&a[0]), http_id(&b[0]));
    service.handle(CoreToPlugin::Cancel { id: "a".into() }, now);
    assert_eq!(
        service
            .handle(geo_response(http_id(&b[0]), GEO_UNIQUE), now)
            .len(),
        1
    );
    assert!(
        service
            .handle(geo_response(http_id(&a[0]), GEO_UNIQUE), now)
            .is_empty()
    );
}

#[test]
fn pending_work_is_bounded_and_expiry_releases_capacity() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    for i in 0..MAX_PENDING_QUERIES {
        let mut request = tool(&i.to_string(), Some(10));
        if let CoreToPlugin::ToolCall { arguments, .. } = &mut request {
            *arguments = json!({"location":{"latitude":20.0 + i as f64,"longitude":113.0,"coordinate_system":"wgs84"}});
        }
        service.handle(request, now);
    }
    let busy = service.handle(tool("extra", None), now);
    match &busy[0] {
        PluginToCore::RawResult(r) => assert_eq!(r.error.as_ref().unwrap().code, "busy"),
        _ => panic!("expected busy"),
    }
    let resumed = service.handle(tool("new", None), now + Duration::from_millis(11));
    assert!(matches!(resumed.last(), Some(PluginToCore::HttpRequest(_))));
    assert_eq!(service.pipeline.queries.len(), 1);
}

#[test]
fn complete_tool_flow_keeps_array_and_explicit_unit() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let mut request = tool("tc_1", None);
    if let CoreToPlugin::ToolCall { arguments, .. } = &mut request {
        *arguments = json!({"unit":"fahrenheit"});
    }
    let first = service.handle(request, now);
    let second = service.handle(geo_response(http_id(&first[0]), GEO_UNIQUE), now);
    let final_result = service.handle(
            CoreToPlugin::HttpResponse {
                id: http_id(&second[0]),
                status: 200,
                body: Some(r#"{"current":{"temperature_2m":0,"weather_code":96,"wind_speed_10m":2},"daily":{"time":["2026-10-15"],"temperature_2m_max":[10],"temperature_2m_min":[0]}}"#.into()),
                error: None,
            },
            now,
        );
    match &final_result[0] {
        PluginToCore::RawResult(r) => {
            assert!(r.error.is_none());
            assert_eq!(r.data.as_array().unwrap().len(), 1);
            assert_eq!(r.data[0]["temp"], "32°F");
            assert_eq!(r.data[0]["daily"][0]["temp_max"], "50°F");
            assert_eq!(r.data[0]["status"], "partial");
        }
        _ => panic!("expected raw result"),
    }
    assert!(service.pipeline.queries.is_empty());
}

#[test]
fn unknown_tool_and_zero_budget_never_start_http() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let mut request = tool("x", None);
    if let CoreToPlugin::ToolCall { tool_name, .. } = &mut request {
        *tool_name = "other".into();
    }
    for request in [request, tool("zero", Some(0))] {
        assert!(matches!(
            &service.handle(request, now)[0],
            PluginToCore::RawResult(_)
        ));
    }
    assert!(service.pipeline.queries.is_empty());
}

#[test]
fn http_errors_keep_stable_codes_release_capacity_and_hide_network_details() {
    let now = Instant::now();
    for (status, detail, code) in [
        (401, None, "authentication_failed"),
        (429, None, "rate_limited"),
        (0, Some("busy: private-key"), "busy"),
        (0, Some("timeout: private-key"), "timeout"),
        (0, Some("https://host/private-key"), "network_failed"),
        (200, None, "invalid_response"),
    ] {
        let mut service = WeatherService::default();
        let first = service.handle(tool("x", None), now);
        let result = service.handle(
            CoreToPlugin::HttpResponse {
                id: http_id(&first[0]),
                status,
                body: None,
                error: detail.map(str::to_string),
            },
            now,
        );
        let PluginToCore::RawResult(result) = &result[0] else {
            panic!("expected error");
        };
        let error = result.error.as_ref().unwrap();
        assert_eq!(error.code, code);
        assert!(!error.message.contains("private-key"));
        assert!(service.pipeline.queries.is_empty());
        assert!(matches!(
            service.handle(tool("new", None), now).last(),
            Some(PluginToCore::HttpRequest(_))
        ));
    }
}

#[test]
fn cancellation_after_deadline_is_silent() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    service.handle(tool("x", Some(10)), now);
    assert!(
        service
            .handle(
                CoreToPlugin::Cancel { id: "x".into() },
                now + Duration::from_millis(20)
            )
            .is_empty()
    );
    assert!(service.pipeline.queries.is_empty());
}
