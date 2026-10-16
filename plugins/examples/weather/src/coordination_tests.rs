use super::*;

fn request(id: &str, unit: &str, revision: u64) -> CoreToPlugin {
    CoreToPlugin::ToolCall {
        id: id.into(),
        tool_name: "get_weather".into(),
        arguments: json!({"unit":unit}),
        settings: Some(json!({"settings_revision":revision,"default_location":{
            "latitude":23.13,"longitude":113.26,"name":"广州","coordinate_system":"wgs84"
        }})),
        budget_ms: Some(10_000),
    }
}
fn ids(messages: &[PluginToCore]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| {
            if let PluginToCore::HttpRequest(h) = m {
                Some(h.id.clone())
            } else {
                None
            }
        })
        .collect()
}
fn weather_body() -> String {
    json!({
        "utc_offset_seconds":28800,
        "timezone":"Asia/Shanghai",
        "current":{"temperature_2m":20,"weather_code":1,"wind_speed_10m":5,"time":"2026-10-16T12:00"},
        "daily":{"time":["2026-10-16","2026-10-17","2026-10-18","2026-10-19","2026-10-20","2026-10-21","2026-10-22"],
            "temperature_2m_max":[25,25,25,25,25,25,25],"temperature_2m_min":[15,15,15,15,15,15,15],
            "weather_code":[1,1,1,1,1,1,1],"precipitation_probability_max":[0,0,0,0,0,0,0],"wind_speed_10m_max":[5,5,5,5,5,5,5]}
    }).to_string()
}
fn response(id: &str) -> CoreToPlugin {
    CoreToPlugin::HttpResponse {
        id: id.into(),
        status: 200,
        body: Some(weather_body()),
        error: None,
    }
}

#[test]
fn reader_queue_time_consumes_query_and_tool_budgets() {
    for tool in [false, true] {
        let mut service = WeatherService::default();
        let received = Instant::now();
        let message = |id: &str| {
            if tool {
                request(id, "celsius", 1)
            } else {
                CoreToPlugin::Query {
                    id: id.into(),
                    query: "广州".into(),
                    settings: None,
                    budget_ms: Some(DEFAULT_BUDGET_MS),
                }
            }
        };
        let out = service.handle_received(
            message("queued"),
            received,
            received + Duration::from_secs(3),
        );
        let http = out
            .iter()
            .find_map(|m| match m {
                PluginToCore::HttpRequest(http) => Some(http),
                _ => None,
            })
            .expect("remaining budget must still allow HTTP");
        assert_eq!(http.timeout_ms, 7000);
        let out = service.handle_received(
            message("expired"),
            received,
            received + Duration::from_secs(11),
        );
        assert!(
            ids(&out).is_empty(),
            "expired queued requests must not start HTTP"
        );
        let code = match out.last().unwrap() {
            PluginToCore::RawResult(result) => &result.error.as_ref().unwrap().code,
            PluginToCore::Response(result) => &result.error.as_ref().unwrap().code,
            _ => panic!("expected a caller error"),
        };
        assert_eq!(code, "timeout");
    }
}
fn raw(message: &PluginToCore) -> &RawToolResult {
    match message {
        PluginToCore::RawResult(r) => r,
        _ => panic!("expected raw"),
    }
}

fn fallback_request(id: &str, enabled: bool) -> CoreToPlugin {
    let mut message = request(id, "celsius", 1);
    if let CoreToPlugin::ToolCall {
        settings: Some(settings),
        ..
    } = &mut message
    {
        settings["default_provider"] = json!("qweather");
        settings["qweather_api_host"] = json!("fixture.qweatherapi.com/");
        settings["qweather_api_key"] = json!("fixture-key");
        settings["allow_fallback"] = json!(enabled);
        settings["fallback_order"] = json!(["caiyun", "open_meteo", "qweather"]);
    }
    message
}

fn reject(id: &str) -> CoreToPlugin {
    CoreToPlugin::HttpResponse {
        id: id.into(),
        status: 404,
        body: None,
        error: None,
    }
}

#[test]
fn fallback_skips_missing_credentials_shares_budget_and_reports_source_without_caching() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let first = ids(&service.handle(fallback_request("first", true), now));
    assert_eq!(first.len(), 2);
    assert!(
        service
            .handle(fallback_request("second", true), now)
            .is_empty()
    );
    assert!(
        service
            .handle(reject(&first[0]), now + Duration::from_secs(1))
            .is_empty()
    );
    let fallback = service.handle(reject(&first[1]), now + Duration::from_secs(2));
    let PluginToCore::HttpRequest(http) = &fallback[0] else {
        panic!("expected fallback HTTP")
    };
    assert!(http.url.starts_with("https://api.open-meteo.com/"));
    assert_eq!(http.timeout_ms, 8000);
    let result = service.handle(response(&http.id), now + Duration::from_secs(3));
    assert_eq!(result.len(), 2);
    for message in &result {
        let data = &raw(message).data[0];
        assert_eq!(data["provider"], "open_meteo");
        assert_eq!(data["status"], "ok");
        assert_eq!(data["fallback"]["requested_provider"], "qweather");
        assert_eq!(data["fallback"]["actual_provider"], "open_meteo");
        assert!(data["copy_text"].as_str().unwrap().contains("已降级"));
        assert!(!data.to_string().contains("fixture-key"));
    }
    assert_eq!(
        ids(&service.handle(
            fallback_request("again", true),
            now + Duration::from_secs(4)
        ))
        .len(),
        2
    );
}

#[test]
fn fallback_is_opt_in_and_partial_weather_does_not_switch_provider() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let first = ids(&service.handle(fallback_request("disabled", false), now));
    service.handle(reject(&first[0]), now);
    let result = service.handle(reject(&first[1]), now);
    assert_eq!(
        raw(&result[0]).error.as_ref().unwrap().code,
        "invalid_response"
    );
    let first = ids(&service.handle(fallback_request("partial", true), now));
    service.handle(
        geo(
            &first[0],
            r#"{"temperature":{"value":25,"unit":"°C"},"condition":{"text":"晴","code":"100"}}"#,
        ),
        now,
    );
    let result = service.handle(reject(&first[1]), now);
    assert!(ids(&result).is_empty());
    assert_eq!(raw(&result[0]).data[0]["status"], "partial");
    assert_eq!(raw(&result[0]).data[0]["provider"], "qweather");
    assert!(raw(&result[0]).data[0].get("fallback").is_none());
}

#[test]
fn fallback_cancel_and_expiry_release_requests_without_late_continuation() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let first = ids(&service.handle(fallback_request("cancel", true), now));
    service.handle(reject(&first[0]), now);
    let next = ids(&service.handle(reject(&first[1]), now));
    service.handle(
        CoreToPlugin::Cancel {
            id: "cancel".into(),
        },
        now,
    );
    assert!(service.handle(response(&next[0]), now).is_empty());
    assert!(service.flights.is_empty() && service.pipeline.http.is_empty());
    let first = ids(&service.handle(fallback_request("expire", true), now));
    service.handle(reject(&first[0]), now);
    let expired = service.handle(reject(&first[1]), now + Duration::from_secs(10));
    assert!(ids(&expired).is_empty());
    assert_eq!(raw(&expired[0]).error.as_ref().unwrap().code, "timeout");
    assert!(service.flights.is_empty() && service.pipeline.http.is_empty());
}

#[test]
fn missing_primary_can_fall_back_but_invalid_user_location_cannot() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let mut message = fallback_request("missing", true);
    if let CoreToPlugin::ToolCall {
        settings: Some(settings),
        ..
    } = &mut message
    {
        settings["qweather_api_key"] = json!("");
    }
    let http = ids(&service.handle(message, now));
    assert_eq!(http.len(), 1);
    let result = service.handle(response(&http[0]), now);
    assert_eq!(
        raw(&result[0]).data[0]["fallback"]["attempts"][0]["code"],
        "provider_not_configured"
    );
    let mut invalid = fallback_request("invalid", true);
    if let CoreToPlugin::ToolCall { arguments, .. } = &mut invalid {
        *arguments =
            json!({"location":{"latitude":1000,"longitude":0,"coordinate_system":"wgs84"}});
    }
    let result = service.handle(invalid, now);
    assert!(ids(&result).is_empty());
    assert_eq!(
        raw(&result[0]).error.as_ref().unwrap().code,
        "invalid_arguments"
    );
}

#[test]
fn same_weather_shares_upstream_and_converts_each_waiter_unit() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let http = ids(&s.handle(request("c", "celsius", 1), now));
    assert_eq!(http.len(), 1);
    assert!(s.handle(request("f", "fahrenheit", 1), now).is_empty());
    let result = s.handle(response(&http[0]), now);
    assert_eq!(result.len(), 2);
    assert_eq!(raw(&result[0]).data[0]["temp"], "20°C");
    assert_eq!(raw(&result[1]).data[0]["temp"], "68°F");
    let hot = s.handle(
        request("hot", "fahrenheit", 1),
        now + Duration::from_secs(1),
    );
    assert_eq!(ids(&hot).len(), 0);
    assert_eq!(raw(&hot[0]).data[0]["cache"]["hit"], true);
    assert_eq!(raw(&hot[0]).data[0]["temp"], "68°F");
    assert_eq!(
        raw(&hot[0]).data[0]["fetched_at"],
        raw(&result[0]).data[0]["fetched_at"]
    );
}

#[test]
fn explicit_open_meteo_resolver_is_independent_of_invalid_qweather_credentials() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let message = CoreToPlugin::ToolCall {
        id: "search".into(),
        tool_name: "search_locations".into(),
        arguments: json!({"query":"广州","resolver":"open_meteo"}),
        settings: Some(json!({"qweather_api_host":"not-a-host","qweather_api_key":"fixture-key"})),
        budget_ms: None,
    };
    let output = service.handle(message, now);
    assert_eq!(ids(&output).len(), 1);
    let PluginToCore::HttpRequest(http) = &output[0] else {
        panic!("expected HTTP")
    };
    assert!(
        http.url
            .starts_with("https://geocoding-api.open-meteo.com/")
    );
    assert!(http.headers.is_empty());
}

#[test]
fn sparse_caiyun_seven_day_response_remains_partial_and_is_never_cached() {
    let mut service = WeatherService::default();
    let now = Instant::now();
    let mut first = request("cold", "celsius", 1);
    if let CoreToPlugin::ToolCall {
        settings: Some(settings),
        ..
    } = &mut first
    {
        settings["default_provider"] = json!("caiyun");
        settings["caiyun_app_key"] = json!("fixture-key");
        settings["caiyun_app_secret"] = json!("fixture-secret");
        settings["default_location"]["in_mainland_china"] = json!(true);
    }
    let CoreToPlugin::ToolCall { settings, .. } = &first else {
        unreachable!()
    };
    let next = CoreToPlugin::ToolCall {
        id: "repeat".into(),
        tool_name: "get_weather".into(),
        arguments: json!({}),
        settings: settings.clone(),
        budget_ms: Some(10_000),
    };
    let http = ids(&service.handle(first, now));
    let body = json!({"status":"ok","result":{
        "realtime":{"status":"ok","temperature":25},
        "daily":{"status":"ok","temperature":(16..23).map(|day|json!({
            "date":format!("2026-10-{day}"),"max":30
        })).collect::<Vec<_>>()}
    }})
    .to_string();
    let cold = service.handle(geo(&http[0], &body), now);
    let result = &raw(&cold[0]).data[0];
    assert_eq!(result["status"], "partial");
    assert_eq!(result["daily"].as_array().unwrap().len(), 7);
    assert_eq!(result["current"]["temperature_c"].as_f64(), Some(25.0));
    assert_eq!(result["daily"][0]["temp_min"], "未知");
    assert_eq!(result["warnings"].as_array().unwrap().len(), 2);
    assert_eq!(
        ids(&service.handle(next, now + Duration::from_millis(1))).len(),
        1
    );
}

#[test]
fn cancelling_first_waiter_keeps_shared_http_and_other_waiter() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let http = ids(&s.handle(request("first", "celsius", 1), now));
    s.handle(request("second", "fahrenheit", 1), now);
    assert!(
        s.handle(CoreToPlugin::Cancel { id: "first".into() }, now)
            .is_empty()
    );
    let result = s.handle(response(&http[0]), now);
    assert_eq!(result.len(), 1);
    assert_eq!(raw(&result[0]).id, "second");
    assert!(s.flights.is_empty() && s.pipeline.http.is_empty());
}

#[test]
fn individual_deadline_expires_without_killing_other_waiter() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let mut first = request("short", "celsius", 1);
    if let CoreToPlugin::ToolCall { budget_ms, .. } = &mut first {
        *budget_ms = Some(10)
    }
    let http = ids(&s.handle(first, now));
    let long_http = ids(&s.handle(
        request("long", "celsius", 1),
        now + Duration::from_millis(1),
    ));
    assert_eq!(long_http.len(), 1, "longer budget needs its own transport");
    assert_ne!(http[0], long_http[0]);
    let expired = s.poll(now + Duration::from_millis(11));
    assert_eq!(raw(&expired[0]).id, "short");
    assert_eq!(raw(&expired[0]).error.as_ref().unwrap().code, "timeout");
    let late = s.handle(
        CoreToPlugin::HttpResponse {
            id: http[0].clone(),
            status: 0,
            body: None,
            error: Some("timeout: HTTP request".into()),
        },
        now + Duration::from_millis(11),
    );
    assert!(late.is_empty());
    let result = s.handle(response(&long_http[0]), now + Duration::from_millis(12));
    assert_eq!(raw(&result[0]).id, "long");
    assert!(raw(&result[0]).error.is_none());
}

#[test]
fn cancelled_all_and_late_response_cannot_fill_cache() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let http = ids(&s.handle(request("one", "celsius", 1), now));
    s.handle(CoreToPlugin::Cancel { id: "one".into() }, now);
    assert!(s.handle(response(&http[0]), now).is_empty());
    assert_eq!(ids(&s.handle(request("next", "celsius", 1), now)).len(), 1);
}

#[test]
fn old_configuration_completion_never_refills_new_epoch() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let old = ids(&s.handle(request("old", "celsius", 1), now));
    let new = ids(&s.handle(request("new", "celsius", 2), now));
    assert_eq!(new.len(), 1);
    s.handle(response(&old[0]), now);
    let result = s.handle(response(&new[0]), now);
    assert_eq!(raw(&result[0]).id, "new");
    let hot = s.handle(request("hot", "celsius", 2), now);
    assert_eq!(raw(&hot[0]).data[0]["cache"]["hit"], true);
    let stale = s.handle(request("stale", "celsius", 1), now);
    assert_eq!(ids(&stale).len(), 1);
    s.handle(response(&ids(&stale)[0]), now);
    let current = s.handle(request("current", "celsius", 2), now);
    assert_eq!(raw(&current[0]).data[0]["cache"]["hit"], true);
}

#[test]
fn follower_limit_and_ttl_are_bounded() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let http = ids(&s.handle(request("first", "celsius", 1), now));
    for i in 1..MAX_WAITERS_PER_FLIGHT {
        assert!(
            s.handle(request(&i.to_string(), "celsius", 1), now)
                .is_empty()
        );
    }
    let busy = s.handle(request("overflow", "celsius", 1), now);
    assert_eq!(raw(&busy[0]).error.as_ref().unwrap().code, "busy");
    s.handle(response(&http[0]), now);
    assert_eq!(
        ids(&s.handle(request("expired", "celsius", 1), now + WEATHER_TTL)).len(),
        1
    );
}

#[test]
fn query_and_tool_share_producer_but_keep_protocol_and_units() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let first = request("tool", "fahrenheit", 1);
    let settings = match &first {
        CoreToPlugin::ToolCall { settings, .. } => settings.clone(),
        _ => unreachable!(),
    };
    let http = ids(&s.handle(first, now));
    assert!(
        s.handle(
            CoreToPlugin::Query {
                id: "query".into(),
                query: " ".into(),
                settings,
                budget_ms: None
            },
            now
        )
        .is_empty()
    );
    let result = s.handle(response(&http[0]), now);
    assert_eq!(raw(&result[0]).data[0]["temp"], "68°F");
    let PluginToCore::Response(query) = &result[1] else {
        panic!("expected Query response")
    };
    assert_eq!(query.id, "query");
    assert_eq!(query.items[0].payload.as_ref().unwrap()["temp"], "20°C");
    assert!(
        query
            .items
            .iter()
            .any(|i| matches!(i.action, PluginAction::Open { .. }))
    );
    assert!(matches!(query.items[0].action, PluginAction::Copy { .. }));
}

#[test]
fn cached_weather_preserves_provider_timezone_and_each_callers_name() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let mut first = request("one", "celsius", 1);
    if let CoreToPlugin::ToolCall { arguments, .. } = &mut first {
        arguments["location"] = json!({"latitude":23.13,"longitude":113.26,"name":"工作地点","coordinate_system":"wgs84"});
    }
    let http = ids(&s.handle(first, now));
    s.handle(response(&http[0]), now);
    let mut second = request("two", "celsius", 1);
    if let CoreToPlugin::ToolCall { arguments, .. } = &mut second {
        arguments["location"] = json!({"latitude":23.13,"longitude":113.26,"name":"另一个标签","coordinate_system":"wgs84"});
    }
    let result = s.handle(second, now);
    assert_eq!(ids(&result).len(), 0);
    assert_eq!(raw(&result[0]).data[0]["city"], "另一个标签");
    assert_eq!(
        raw(&result[0]).data[0]["location"]["timezone"],
        "Asia/Shanghai"
    );
    assert_eq!(raw(&result[0]).data[0]["cache"]["hit"], true);
}

#[test]
fn no_revision_credential_change_invalidates_success_cache_too() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let mutate = |id: &str, key: &str| {
        let mut r = request(id, "celsius", 0);
        if let CoreToPlugin::ToolCall {
            settings: Some(settings),
            ..
        } = &mut r
        {
            settings
                .as_object_mut()
                .unwrap()
                .remove("settings_revision");
            settings["qweather_api_host"] = json!("https://fixture.qweatherapi.com");
            settings["qweather_api_key"] = json!(key);
        }
        r
    };
    let http = ids(&s.handle(mutate("one", "old-key"), now));
    s.handle(response(&http[0]), now);
    assert_eq!(ids(&s.handle(mutate("two", "new-key"), now)).len(), 1);
}

#[test]
fn provider_switch_or_cleared_key_cannot_reuse_previous_success() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let http = ids(&s.handle(request("one", "celsius", 1), now));
    s.handle(response(&http[0]), now);
    let mut changed = request("two", "celsius", 2);
    if let CoreToPlugin::ToolCall {
        settings: Some(settings),
        ..
    } = &mut changed
    {
        settings["default_provider"] = json!("qweather");
        settings["qweather_api_host"] = json!("https://fixture.qweatherapi.com");
        settings["qweather_api_key"] = json!("");
    }
    let result = s.handle(changed, now);
    assert_eq!(
        raw(&result[0]).error.as_ref().unwrap().code,
        "provider_not_configured"
    );
    assert!(ids(&result).is_empty());
}

#[test]
fn partial_and_error_are_not_cached_and_failures_release_flights() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let http = ids(&s.handle(request("one", "celsius", 1), now));
    let partial = s.handle(
        CoreToPlugin::HttpResponse {
            id: http[0].clone(),
            status: 200,
            body: Some(r#"{"current":{"temperature_2m":20}}"#.into()),
            error: None,
        },
        now,
    );
    assert_eq!(raw(&partial[0]).data[0]["status"], "partial");
    let next = ids(&s.handle(request("two", "celsius", 1), now));
    assert_eq!(next.len(), 1);
    let result = s.handle(
        CoreToPlugin::HttpResponse {
            id: next[0].clone(),
            status: 401,
            body: None,
            error: None,
        },
        now,
    );
    assert_eq!(
        raw(&result[0]).error.as_ref().unwrap().code,
        "authentication_failed"
    );
    assert!(s.flights.is_empty() && s.by_key.is_empty());
    assert_eq!(ids(&s.handle(request("three", "celsius", 1), now)).len(), 1);
}

fn name_request(id: &str) -> CoreToPlugin {
    CoreToPlugin::ToolCall {
        id: id.into(),
        tool_name: "get_weather".into(),
        arguments: json!({"city":"广州"}),
        settings: None,
        budget_ms: None,
    }
}
fn geo(id: &str, body: &str) -> CoreToPlugin {
    CoreToPlugin::HttpResponse {
        id: id.into(),
        status: 200,
        body: Some(body.into()),
        error: None,
    }
}
const UNIQUE: &str =
    r#"{"results":[{"name":"广州","latitude":23.13,"longitude":113.26,"feature_code":"PPLA"}]}"#;

#[test]
fn cached_location_and_weather_avoid_both_name_and_forecast_requests() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let first = ids(&s.handle(name_request("one"), now));
    let weather = ids(&s.handle(geo(&first[0], UNIQUE), now));
    assert_eq!(weather.len(), 1);
    let cold = s.handle(response(&weather[0]), now);
    assert_eq!(raw(&cold[0]).data[0]["status"], "ok");
    let hot = s.handle(name_request("two"), now + Duration::from_secs(1));
    assert!(ids(&hot).is_empty());
    assert_eq!(raw(&hot[0]).data[0]["cache"]["hit"], true);
    // Weather TTL expires independently; the still-valid resolver result skips
    // geocoding and sends only the forecast request.
    let expired = s.handle(name_request("three"), now + WEATHER_TTL);
    let PluginToCore::HttpRequest(request) = &expired[0] else {
        panic!("expected forecast")
    };
    assert!(
        request
            .url
            .starts_with("https://api.open-meteo.com/v1/forecast")
    );
}

#[test]
fn ambiguous_location_cache_keeps_needs_location_and_limit_boundaries() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let query = |id: &str, limit: u64| CoreToPlugin::ToolCall {
        id: id.into(),
        tool_name: "search_locations".into(),
        arguments: json!({"query":"广州","limit":limit}),
        settings: None,
        budget_ms: None,
    };
    let http = ids(&s.handle(query("search", 20), now));
    let body = r#"{"results":[{"name":"广州","latitude":23.13,"longitude":113.26,"feature_code":"PPLA"},{"name":"广州","latitude":24.0,"longitude":113.0,"feature_code":"PPLA"}]}"#;
    s.handle(geo(&http[0], body), now);
    let needs = s.handle(name_request("weather"), now);
    assert_eq!(raw(&needs[0]).data[0]["status"], "needs_location");
    assert!(raw(&needs[0]).data[0].get("temp").is_none());
    assert!(ids(&needs).is_empty());
    let search = s.handle(query("again", 20), now);
    assert_eq!(raw(&search[0]).data[0]["count"], 2);
    assert_eq!(ids(&s.handle(query("different_limit", 1), now)).len(), 1);
}

fn qw(id: &str, args: Value) -> CoreToPlugin {
    CoreToPlugin::ToolCall {
        id: id.into(),
        tool_name: "get_weather".into(),
        arguments: args,
        settings: Some(
            json!({"default_provider":"qweather","qweather_api_host":"https://fixture.qweatherapi.com","qweather_api_key":"key"}),
        ),
        budget_ms: None,
    }
}
#[test]
fn resolved_qweather_name_emits_both_parallel_requests() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let first = ids(&s.handle(qw("one", json!({"city":"广州"})), now));
    let parsed=s.handle(geo(&first[0],r#"{"code":"200","location":[{"name":"广州","id":"1","lat":"23.13","lon":"113.26","type":"city","adm1":"广东","adm2":"广州","country":"中国"}]}"#),now);
    assert_eq!(ids(&parsed).len(), 2);
    assert!(
        parsed
            .iter()
            .any(|m| matches!(m,PluginToCore::HttpRequest(h) if h.url.contains("/current/")))
    );
    assert!(
        parsed
            .iter()
            .any(|m| matches!(m,PluginToCore::HttpRequest(h) if h.url.contains("/daily/")))
    );
}

#[test]
fn qweather_single_side_failure_returns_useful_partial_without_caching() {
    for failure in [401, 500] {
        let mut s = WeatherService::default();
        let now = Instant::now();
        let args =
            json!({"location":{"latitude":23.13,"longitude":113.26,"coordinate_system":"wgs84"}});
        let http = ids(&s.handle(qw("one", args.clone()), now));
        s.handle(
            geo(
                &http[0],
                r#"{"temperature":{"value":20,"unit":"°C"},"condition":{"text":"晴","code":100}}"#,
            ),
            now,
        );
        let partial = s.handle(
            CoreToPlugin::HttpResponse {
                id: http[1].clone(),
                status: failure,
                body: None,
                error: None,
            },
            now,
        );
        assert_eq!(raw(&partial[0]).data[0]["status"], "partial");
        assert_eq!(raw(&partial[0]).data[0]["temp"], "20°C");
        assert_eq!(ids(&s.handle(qw("two", args), now)).len(), 2);
    }
}

#[test]
fn invalid_budget_duplicate_id_and_final_expiry_leave_no_orphan_flight() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let first = ids(&s.handle(request("one", "celsius", 1), now));
    let duplicate = s.handle(request("one", "fahrenheit", 1), now);
    assert_eq!(raw(&duplicate[0]).error.as_ref().unwrap().code, "busy");
    assert_eq!(s.flights.len(), 1);
    let expired = s.poll(now + Duration::from_secs(11));
    assert_eq!(expired.len(), 1);
    assert!(
        s.flights.is_empty()
            && s.by_key.is_empty()
            && s.pipeline.queries.is_empty()
            && s.pipeline.http.is_empty()
    );
    assert!(
        s.handle(response(&first[0]), now + Duration::from_secs(11))
            .is_empty()
    );
}

#[test]
fn a_shorter_waiter_joins_then_expires_without_stopping_the_shared_transport() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let http = ids(&s.handle(request("long", "celsius", 1), now));
    let mut short = request("short", "celsius", 1);
    if let CoreToPlugin::ToolCall { budget_ms, .. } = &mut short {
        *budget_ms = Some(50)
    }
    assert!(s.handle(short, now + Duration::from_millis(1)).is_empty());
    assert_eq!(s.flights.len(), 1);
    let expired = s.poll(now + Duration::from_millis(51));
    assert_eq!(raw(&expired[0]).id, "short");
    let result = s.handle(response(&http[0]), now + Duration::from_millis(52));
    assert_eq!(raw(&result[0]).id, "long");
    assert!(raw(&result[0]).error.is_none());
}

#[test]
fn a_full_compatible_flight_does_not_hide_another_producers_free_waiter_slot() {
    let mut s = WeatherService::default();
    let now = Instant::now();
    let with_budget = |id: &str, budget: u64| {
        let mut r = request(id, "celsius", 1);
        if let CoreToPlugin::ToolCall { budget_ms, .. } = &mut r {
            *budget_ms = Some(budget)
        }
        r
    };
    s.handle(with_budget("short", 100), now);
    for i in 1..MAX_WAITERS_PER_FLIGHT {
        s.handle(with_budget(&i.to_string(), 100), now);
    }
    assert_eq!(ids(&s.handle(with_budget("long", 1000), now)).len(), 1);
    assert!(s.handle(with_budget("fits_long", 50), now).is_empty());
    assert_eq!(s.flights.len(), 2);
    assert_eq!(
        s.flights.values().map(|f| f.waiters.len()).sum::<usize>(),
        10
    );
}

#[test]
fn full_qweather_and_caiyun_snapshots_are_cached_in_their_own_provider_scope() {
    for provider in ["qweather", "caiyun"] {
        let mut s = WeatherService::default();
        let now = Instant::now();
        let mut first = request("cold", "celsius", 1);
        if let CoreToPlugin::ToolCall {
            settings: Some(settings),
            ..
        } = &mut first
        {
            settings["default_provider"] = json!(provider);
            settings["qweather_api_host"] = json!("https://fixture.qweatherapi.com");
            settings["qweather_api_key"] = json!("test-key");
            settings["caiyun_app_key"] = json!("test-key");
            settings["caiyun_app_secret"] = json!("test-secret");
            settings["default_location"]["in_mainland_china"] = json!(false);
        }
        let clone_request = |id: &str, unit: &str| {
            let CoreToPlugin::ToolCall { settings, .. } = &first else {
                unreachable!()
            };
            CoreToPlugin::ToolCall {
                id: id.into(),
                tool_name: "get_weather".into(),
                arguments: json!({"unit":unit}),
                settings: settings.clone(),
                budget_ms: None,
            }
        };
        let http = ids(&s.handle(clone_request("cold", "celsius"), now));
        let dates: Vec<_> = (16..23)
            .map(|d| format!("2026-10-{d}T00:00+08:00"))
            .collect();
        let cold = if provider == "qweather" {
            assert_eq!(http.len(), 2);
            let daily=json!({"days":dates.iter().map(|date|json!({"forecastStartTime":date,
                "temperatureMax":{"value":25,"unit":"°C"},"temperatureMin":{"value":15,"unit":"°C"},
                "daytime":{"condition":{"text":"晴","code":100},"wind":{"speed":{"value":5,"unit":"km/h"}},"precipitation":{"probability":0.2}},
                "nighttime":{"condition":{"text":"多云","code":101},"wind":{"speed":{"value":3,"unit":"km/h"}},"precipitation":{"probability":0.1}}
            })).collect::<Vec<_>>()}).to_string();
            s.handle(geo(&http[0],r#"{"temperature":{"value":20,"unit":"°C"},"condition":{"text":"晴","code":100},"wind":{"speed":{"value":5,"unit":"km/h"}}}"#),now);
            s.handle(geo(&http[1], &daily), now)
        } else {
            assert_eq!(http.len(), 1);
            let body=json!({"status":"ok","api_version":"v2.6","unit":"metric:v2","timezone":"Asia/Shanghai","result":{
                "realtime":{"status":"ok","temperature":20,"wind":{"speed":5},"skycon":"CLEAR_DAY"},
                "daily":{"status":"ok",
                    "temperature":dates.iter().map(|date|json!({"date":date,"max":25,"min":15})).collect::<Vec<_>>(),
                    "wind":dates.iter().map(|date|json!({"date":date,"max":{"speed":5}})).collect::<Vec<_>>(),
                    "skycon":dates.iter().map(|date|json!({"date":date,"value":"CLEAR_DAY"})).collect::<Vec<_>>(),
                    "precipitation":dates.iter().map(|date|json!({"date":date,"probability":0.2})).collect::<Vec<_>>()
                }
            }}).to_string();
            s.handle(geo(&http[0], &body), now)
        };
        assert_eq!(
            raw(&cold[0]).data[0]["status"],
            "ok",
            "{}",
            raw(&cold[0]).data
        );
        let hot = s.handle(
            clone_request("hot", "fahrenheit"),
            now + Duration::from_millis(1),
        );
        assert_eq!(ids(&hot).len(), 0);
        assert_eq!(raw(&hot[0]).data[0]["provider"], provider);
        assert_eq!(raw(&hot[0]).data[0]["cache"]["hit"], true);
        assert_eq!(raw(&hot[0]).data[0]["temp"], "68°F");
    }
}
