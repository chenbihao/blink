use super::*;
use crate::{
    model::{Adapter, AdapterKind, Address, adapter_kind, public_v4},
    providers::Provider,
};
use serde_json::json;

fn network() -> NetworkSnapshot {
    NetworkSnapshot {
        adapters: vec![
            Adapter {
                name: "以太网".into(),
                kind: AdapterKind::Physical,
                addresses: vec![
                    Address {
                        ip: "192.168.1.2".parse().unwrap(),
                        prefix: 24,
                        valid: true,
                        preferred: true,
                        temporary: false,
                    },
                    Address {
                        ip: "2409::123".parse().unwrap(),
                        prefix: 64,
                        valid: true,
                        preferred: true,
                        temporary: false,
                    },
                    Address {
                        ip: "fe80::1".parse().unwrap(),
                        prefix: 64,
                        valid: true,
                        preferred: true,
                        temporary: false,
                    },
                ],
                gateways: vec!["192.168.1.1".into()],
                metric: 25,
            },
            Adapter {
                name: "Mihomo".into(),
                kind: AdapterKind::Tunnel,
                addresses: vec![Address {
                    ip: "198.18.0.1".parse().unwrap(),
                    prefix: 30,
                    valid: true,
                    preferred: true,
                    temporary: false,
                }],
                gateways: vec![],
                metric: 0,
            },
        ],
        ..Default::default()
    }
}
fn query(id: &str, command: &str, settings: Value) -> CoreToPlugin {
    serde_json::from_value(
        json!({"type":"query","id":id,"query":command,"settings":settings,"budget_ms":6700}),
    )
    .unwrap()
}
fn requests(output: &[PluginToCore]) -> Vec<(String, &'static str, u64)> {
    output
        .iter()
        .filter_map(|m| match m {
            PluginToCore::HttpRequest {
                id,
                url,
                timeout_ms,
                ..
            } => Some((id.clone(), *url, *timeout_ms)),
            _ => None,
        })
        .collect()
}
fn reply(engine: &mut Engine, request: &str, body: &str, now: Instant) -> Vec<PluginToCore> {
    engine.handle(
        CoreToPlugin::HttpResponse {
            id: request.into(),
            status: 200,
            body: Some(body.into()),
            error: None,
        },
        now,
        None,
    )
}
fn fail(engine: &mut Engine, request: &str, now: Instant) -> Vec<PluginToCore> {
    engine.handle(
        CoreToPlugin::HttpResponse {
            id: request.into(),
            status: 500,
            body: Some("secret response".into()),
            error: Some("http://secret-host?key=private".into()),
        },
        now,
        None,
    )
}
fn domestic_body(ip: &str) -> String {
    json!({"ret":"ok","data":{"ip":ip,"location":["中国","广东","广州","","移动"]}}).to_string()
}
fn complete(engine: &mut Engine, output: &[PluginToCore], now: Instant) -> Vec<PluginToCore> {
    let reqs = requests(output);
    assert_eq!(reqs.len(), 2);
    assert!(reply(engine, &reqs[0].0, &domestic_body("1.1.1.1"), now).is_empty());
    reply(engine, &reqs[1].0, r#"{"ip":"8.8.8.8"}"#, now)
}
fn value(output: &[PluginToCore]) -> Value {
    assert_eq!(output.len(), 1);
    serde_json::to_value(&output[0]).unwrap()
}

#[test]
fn starts_two_independent_https_checks_with_bounded_timeouts() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let output = engine.handle(query("a", "", Value::Null), now, Some(network()));
    let reqs = requests(&output);
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].1, Provider::Ipip.url());
    assert_eq!(reqs[1].1, Provider::Ifconfig.url());
    assert!(
        reqs.iter()
            .all(|(_, url, ms)| url.starts_with("https://") && *ms <= 1600)
    );
}

#[test]
fn domestic_falls_back_through_three_sources_without_crossing_groups() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let out = engine.handle(
        query("a", "", json!({"geo_provider":"domestic"})),
        now,
        Some(network()),
    );
    let out = fail(&mut engine, &requests(&out)[0].0, now);
    assert_eq!(requests(&out)[0].1, Provider::Pubyun.url());
    let out = reply(&mut engine, &requests(&out)[0].0, "<html>login</html>", now);
    assert_eq!(requests(&out)[0].1, Provider::Oray.url());
    let out = reply(
        &mut engine,
        &requests(&out)[0].0,
        "Current IP Address: 1.1.1.1\n",
        now,
    );
    let val = value(&out);
    assert!(val.to_string().contains("国内站出口"));
    assert!(val.to_string().contains("已使用备用源"));
    assert!(!val.to_string().contains("secret-host"));
    assert!(engine.http.is_empty() && engine.jobs.is_empty());
}

#[test]
fn international_uses_ifconfig_backup_and_its_optional_location() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let out = engine.handle(
        query(
            "a",
            "",
            json!({"geo_provider":"international","international_order":["ipify","ifconfig"]}),
        ),
        now,
        Some(network()),
    );
    let out = fail(&mut engine, &requests(&out)[0].0, now);
    assert_eq!(requests(&out)[0].1, Provider::Ifconfig.url());
    let out = reply(
        &mut engine,
        &requests(&out)[0].0,
        r#"{"ip":"8.8.8.8","country":"United States","city":"Los Angeles"}"#,
        now,
    );
    assert!(value(&out).to_string().contains("Los Angeles"));
}

#[test]
fn fallback_off_preserves_local_addresses_and_explains_missing_group() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let out = engine.handle(
        query(
            "a",
            "",
            json!({"geo_provider":"domestic","allow_fallback":false}),
        ),
        now,
        Some(network()),
    );
    let out = fail(&mut engine, &requests(&out)[0].0, now);
    assert!(requests(&out).is_empty());
    let text = value(&out).to_string();
    assert!(text.contains("192.168.1.2") && text.contains("国内站出口检测未完成"));
    assert!(!text.contains("secret response"));
}

#[test]
fn legacy_offline_and_local_command_never_emit_http() {
    for (command, settings) in [
        ("", json!({"geo_provider":"none"})),
        ("local", Value::Null),
        ("本地", Value::Null),
    ] {
        let out = Engine::default().handle(
            query("a", command, settings),
            Instant::now(),
            Some(network()),
        );
        assert!(requests(&out).is_empty());
        assert!(value(&out).to_string().contains("192.168.1.2"));
    }
}

#[test]
fn physical_tunnel_gateway_and_ipv6_are_labelled_and_copy_actual_addresses() {
    let out = Engine::default().handle(
        query(
            "a",
            "local",
            json!({"use_ipv6":true,"enhanced_network":true}),
        ),
        Instant::now(),
        Some(network()),
    );
    let val = value(&out);
    let items = val["items"].as_array().unwrap();
    assert_eq!(items[0]["title"], "本机 IPv4：192.168.1.2");
    assert_eq!(items[0]["action"]["text"], "192.168.1.2");
    assert!(items.iter().any(|i| i["title"] == "网关：192.168.1.1"));
    assert!(items.iter().any(|i| i["title"] == "隧道 IPv4：198.18.0.1"));
    assert!(items.iter().any(|i| i["title"] == "本机 IPv6：2409::123"));
    assert!(!val.to_string().contains("fe80::1"));
}

#[test]
fn basic_mode_keeps_physical_and_hides_gateway_and_virtual() {
    let out = Engine::default().handle(
        query("a", "local", Value::Null),
        Instant::now(),
        Some(network()),
    );
    let text = value(&out).to_string();
    assert!(text.contains("192.168.1.2"));
    assert!(!text.contains("198.18.0.1"));
}

#[test]
fn complete_cache_retains_original_timestamp_and_expires() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let out = engine.handle(query("a", "", Value::Null), now, Some(network()));
    let finished = complete(&mut engine, &out, now);
    let original = engine.cache[&Group::Domestic]
        .observation
        .observed_at
        .clone();
    let cached = engine.handle(
        query("b", "", Value::Null),
        now + Duration::from_secs(50),
        Some(network()),
    );
    assert!(requests(&cached).is_empty());
    assert_eq!(
        engine.cache[&Group::Domestic].observation.observed_at,
        original
    );
    assert!(!value(&cached).to_string().contains(&original));
    assert!(value(&cached).to_string().contains("缓存"));
    assert!(!value(&finished).to_string().contains("缓存"));
    let out = engine.handle(
        query("c", "", Value::Null),
        now + Duration::from_secs(61),
        Some(network()),
    );
    assert_eq!(requests(&out).len(), 2);
}

#[test]
fn refresh_respects_cooldown_then_bypasses_valid_cache() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let out = engine.handle(query("a", "", Value::Null), now, Some(network()));
    complete(&mut engine, &out, now);
    let out = engine.handle(
        query("b", "refresh", Value::Null),
        now + Duration::from_secs(1),
        Some(network()),
    );
    assert!(requests(&out).is_empty());
    assert!(value(&out).to_string().contains("刷新过快"));
    let out = engine.handle(
        query("c", "refresh", Value::Null),
        now + Duration::from_secs(6),
        Some(network()),
    );
    let reqs = requests(&out);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].1, Provider::Ipip.url());
    let cached = reply(
        &mut engine,
        &reqs[0].0,
        &domestic_body("1.1.1.1"),
        now + Duration::from_secs(6),
    );
    assert!(value(&cached).to_string().contains("缓存"));
    let out = engine.handle(
        query("d", "refresh", Value::Null),
        now + Duration::from_secs(61),
        Some(network()),
    );
    assert_eq!(requests(&out).len(), 2);
}

#[test]
fn rate_limited_sources_are_not_retried_within_one_minute_even_after_failure() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let settings = json!({"geo_provider":"domestic","allow_fallback":false,"domestic_order":["pubyun","ipip","oray"]});
    let out = engine.handle(query("a", "", settings.clone()), now, Some(network()));
    assert_eq!(requests(&out)[0].1, Provider::Pubyun.url());
    fail(&mut engine, &requests(&out)[0].0, now);
    let out = engine.handle(
        query("b", "refresh", settings.clone()),
        now + Duration::from_secs(59),
        Some(network()),
    );
    assert!(requests(&out).is_empty());
    assert!(value(&out).to_string().contains("刷新间隔内"));
    let out = engine.handle(
        query("c", "refresh", settings),
        now + Duration::from_secs(61),
        Some(network()),
    );
    assert_eq!(requests(&out).len(), 1);
}

#[test]
fn changing_network_invalidates_cache_and_old_generation_cannot_refill_it() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let first = engine.handle(query("a", "", Value::Null), now, Some(network()));
    let mut changed = network();
    changed.adapters[0].addresses[0].ip = "192.168.2.2".parse().unwrap();
    let later = now + Duration::from_secs(6);
    let second = engine.handle(query("b", "", Value::Null), later, Some(changed.clone()));
    complete(&mut engine, &first, later);
    assert!(engine.cache.is_empty());
    complete(&mut engine, &second, later);
    let cached = engine.handle(
        query("c", "", Value::Null),
        later + Duration::from_millis(1),
        Some(changed),
    );
    assert!(requests(&cached).is_empty());
    assert!(value(&cached).to_string().contains("192.168.2.2"));
}

#[test]
fn changing_configuration_does_not_reuse_disabled_fallback_result() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let out = engine.handle(
        query("a", "", json!({"geo_provider":"domestic"})),
        now,
        Some(network()),
    );
    let out = fail(&mut engine, &requests(&out)[0].0, now);
    reply(&mut engine, &requests(&out)[0].0, "1.1.1.1", now);
    let out = engine.handle(
        query(
            "b",
            "",
            json!({"geo_provider":"domestic","allow_fallback":false}),
        ),
        now + Duration::from_secs(6),
        Some(network()),
    );
    assert_eq!(requests(&out)[0].1, Provider::Ipip.url());
}

#[test]
fn cancelled_request_cannot_cache_or_return_late_results() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let out = engine.handle(query("a", "", Value::Null), now, Some(network()));
    let reqs = requests(&out);
    assert!(
        engine
            .handle(CoreToPlugin::Cancel { id: "a".into() }, now, None)
            .is_empty()
    );
    assert!(reply(&mut engine, &reqs[0].0, &domestic_body("1.1.1.1"), now).is_empty());
    assert!(engine.jobs.is_empty() && engine.http.is_empty() && engine.cache.is_empty());
}

#[test]
fn timeout_returns_partial_before_host_deadline_and_discards_late_http() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let out = engine.handle(query("a", "", Value::Null), now, Some(network()));
    let reqs = requests(&out);
    reply(&mut engine, &reqs[0].0, &domestic_body("1.1.1.1"), now);
    let expired = engine.expire(now + Duration::from_millis(6500));
    let text = value(&expired).to_string();
    assert!(
        text.contains("1.1.1.1")
            && text.contains("192.168.1.2")
            && text.contains("国际站出口检测超时")
    );
    assert!(
        reply(
            &mut engine,
            &reqs[1].0,
            r#"{"ip":"8.8.8.8"}"#,
            now + Duration::from_secs(7)
        )
        .is_empty()
    );
}

#[test]
fn insufficient_budget_returns_local_without_starting_network() {
    let mut message = query("a", "", Value::Null);
    if let CoreToPlugin::Query { budget_ms, .. } = &mut message {
        *budget_ms = Some(250);
    }
    let out = Engine::default().handle(message, Instant::now(), Some(network()));
    assert!(requests(&out).is_empty());
    assert!(value(&out).to_string().contains("时间已用尽"));
}

#[test]
fn same_public_ip_is_merged_with_both_source_provenances() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let message: CoreToPlugin =
        serde_json::from_value(json!({"type":"tool_call","id":"a","tool_name":"get_ip"})).unwrap();
    let out = engine.handle(message, now, Some(network()));
    let reqs = requests(&out);
    reply(&mut engine, &reqs[0].0, &domestic_body("1.1.1.1"), now);
    let out = reply(&mut engine, &reqs[1].0, r#"{"ip":"1.1.1.1"}"#, now);
    let val = value(&out);
    let public: Vec<_> = val["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["kind"] == "public")
        .collect();
    assert_eq!(public.len(), 1);
    assert_eq!(public[0]["sources"].as_array().unwrap().len(), 2);
    assert_eq!(public[0]["type"], "国内/国际站出口");
}

#[test]
fn failed_tool_check_keeps_structured_partial_status_and_warnings() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let message:CoreToPlugin=serde_json::from_value(json!({"type":"tool_call","id":"a","tool_name":"get_ip","settings":{"geo_provider":"domestic","allow_fallback":false}})).unwrap();
    let out = engine.handle(message, now, Some(network()));
    let out = fail(&mut engine, &requests(&out)[0].0, now);
    let val = value(&out);
    assert_eq!(val["data"][0]["status"], "partial");
    assert!(
        val["data"][0]["warnings"][0]
            .as_str()
            .unwrap()
            .contains("国内站出口")
    );
}

#[test]
fn parser_rejects_non_public_and_fake_addresses() {
    for ip in [
        "0.0.0.0",
        "10.0.0.1",
        "127.0.0.1",
        "169.254.1.1",
        "172.16.0.1",
        "192.168.1.1",
        "100.64.0.1",
        "100.127.255.254",
        "198.18.0.1",
        "198.19.1.1",
        "192.0.2.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.1.1.1",
        "255.255.255.255",
    ] {
        assert!(!public_v4(ip.parse().unwrap()), "{ip}");
        assert!(Provider::Pubyun.parse(ip).is_err(), "{ip}");
    }
    for ip in ["1.1.1.1", "8.8.8.8", "223.73.1.1"] {
        assert!(public_v4(ip.parse().unwrap()));
    }
}

#[test]
fn parses_only_expected_formats_not_login_pages_or_random_ip_text() {
    assert!(
        Provider::Oray
            .parse("Current IP Address: 1.1.1.1\n")
            .is_ok()
    );
    assert!(
        Provider::Oray
            .parse("<html><body>Current IP Address: 1.1.1.1</body></html>")
            .is_ok()
    );
    assert!(Provider::Pubyun.parse("Server IP 1.1.1.1").is_err());
    assert!(
        Provider::Ipip
            .parse(r#"{"ret":"fail","data":{"ip":"1.1.1.1"}}"#)
            .is_err()
    );
    assert!(Provider::Ipify.parse(r#"{"ip":"2409::1"}"#).is_err());
    assert!(Provider::Ipify.parse(&"x".repeat(8193)).is_err());
}

#[test]
fn settings_orders_deduplicate_and_cannot_mix_domestic_and_international() {
    let settings = Settings::parse(
        &json!({"domestic_order":["oray","ipify","oray"],"international_order":"ifconfig,oray,ipify"}),
        &Value::Null,
    );
    assert_eq!(
        settings.domestic,
        vec![Provider::Oray, Provider::Ipip, Provider::Pubyun]
    );
    assert_eq!(
        settings.international,
        vec![Provider::Ifconfig, Provider::Ipify]
    );
    assert!(Settings::parse(&json!({"use_ipv6":false}), &json!({"include_ipv6":true})).ipv6);
}

#[test]
fn virtual_classification_uses_type_name_and_fake_ip_evidence() {
    assert_eq!(
        adapter_kind("Ethernet", "Realtek", 6, &[]),
        AdapterKind::Physical
    );
    assert_eq!(
        adapter_kind("Mihomo", "Meta Tunnel", 6, &[]),
        AdapterKind::Tunnel
    );
    assert_eq!(
        adapter_kind("vEthernet (WSL)", "Hyper-V", 6, &[]),
        AdapterKind::Virtual
    );
    assert_eq!(adapter_kind("VPN", "", 131, &[]), AdapterKind::Tunnel);
}

#[test]
fn invalid_commands_and_unknown_tools_do_not_send_requests() {
    let out = Engine::default().handle(query("a", "nonsense", Value::Null), Instant::now(), None);
    assert_eq!(value(&out)["error"]["code"], "invalid_arguments");
    let message =
        serde_json::from_value(json!({"type":"tool_call","id":"a","tool_name":"unknown"})).unwrap();
    let out = Engine::default().handle(message, Instant::now(), None);
    assert_eq!(value(&out)["error"]["code"], "unknown_tool");
}

#[test]
fn display_preferences_reuse_public_cache_but_render_new_choices() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let out = engine.handle(query("a", "", Value::Null), now, Some(network()));
    complete(&mut engine, &out, now);
    let out = engine.handle(
        query(
            "b",
            "",
            json!({"enhanced_network":false,"show_location":false}),
        ),
        now,
        Some(network()),
    );
    assert!(requests(&out).is_empty());
    let text = value(&out).to_string();
    assert!(!text.contains("2409::123"));
    assert!(!text.contains("198.18.0.1") && !text.contains("中国 · 广东"));
}

#[test]
fn rate_limit_response_backs_off_even_a_fast_refresh_source() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let settings = json!({"geo_provider":"international","allow_fallback":false});
    let out = engine.handle(query("a", "", settings.clone()), now, Some(network()));
    engine.handle(
        CoreToPlugin::HttpResponse {
            id: requests(&out)[0].0.clone(),
            status: 429,
            body: None,
            error: None,
        },
        now,
        None,
    );
    let out = engine.handle(
        query("b", "refresh", settings.clone()),
        now + Duration::from_secs(10),
        Some(network()),
    );
    assert!(requests(&out).is_empty());
    let out = engine.handle(
        query("c", "refresh", settings),
        now + Duration::from_secs(61),
        Some(network()),
    );
    assert_eq!(requests(&out).len(), 1);
}

#[test]
fn cancellation_does_not_remove_another_call() {
    let mut engine = Engine::default();
    let now = Instant::now();
    engine.handle(
        query("a", "", json!({"geo_provider":"domestic"})),
        now,
        Some(network()),
    );
    let other = engine.handle(
        query("b", "", json!({"geo_provider":"international"})),
        now,
        Some(network()),
    );
    engine.handle(CoreToPlugin::Cancel { id: "a".into() }, now, None);
    let out = reply(
        &mut engine,
        &requests(&other)[0].0,
        r#"{"ip":"8.8.8.8"}"#,
        now,
    );
    assert_eq!(value(&out)["id"], "b");
}

#[test]
fn budget_restricts_every_request_and_missing_http_still_finishes() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let mut message = query("a", "", Value::Null);
    if let CoreToPlugin::Query { budget_ms, .. } = &mut message {
        *budget_ms = Some(500);
    }
    let out = engine.handle(message, now, Some(network()));
    assert!(requests(&out).iter().all(|(_, _, ms)| *ms <= 250));
    let out = engine.expire(now + Duration::from_millis(300));
    assert_eq!(value(&out)["id"], "a");
    assert!(engine.jobs.is_empty() && engine.http.is_empty());
}

#[test]
fn enhanced_network_defaults_off_and_legacy_virtual_flag_does_not_enable_it() {
    assert!(!Settings::parse(&Value::Null, &Value::Null).enhanced);
    assert!(!Settings::parse(&json!({"show_virtual":true}), &Value::Null).enhanced);
    assert!(Settings::parse(&json!({"enhanced_network":true}), &Value::Null).enhanced);
}

#[test]
fn basic_query_and_tool_exclude_all_extra_details_even_from_full_snapshot() {
    for tool in [false, true] {
        let mut net = network();
        net.proxy = Some("127.0.0.1:7890".into());
        net.adapters.push(Adapter {
            name: "vEthernet".into(),
            kind: AdapterKind::Virtual,
            addresses: vec![Address {
                ip: "172.22.240.1".parse().unwrap(),
                prefix: 20,
                valid: true,
                preferred: true,
                temporary: false,
            }],
            gateways: vec![],
            metric: 0,
        });
        let message = if tool {
            serde_json::from_value(json!({"type":"tool_call","id":"a","tool_name":"get_ip","settings":{"geo_provider":"none"}})).unwrap()
        } else {
            query("a", "local", Value::Null)
        };
        let out = Engine::default().handle(message, Instant::now(), Some(net));
        let text = value(&out).to_string();
        assert!(text.contains("192.168.1.2"));
        for hidden in [
            "192.168.1.1",
            "198.18.0.1",
            "172.22.240.1",
            "127.0.0.1",
            "系统代理",
            "隧道",
            "gateway",
        ] {
            assert!(
                !text.contains(hidden),
                "unexpected extra detail {hidden}: {text}"
            );
        }
    }
}

#[test]
fn enhanced_tool_includes_gateway_tunnel_and_proxy_notes() {
    let mut net = network();
    net.proxy = Some("127.0.0.1:7890".into());
    let message=serde_json::from_value(json!({"type":"tool_call","id":"a","tool_name":"get_ip","settings":{"geo_provider":"none","enhanced_network":true}})).unwrap();
    let out = Engine::default().handle(message, Instant::now(), Some(net));
    let text = value(&out).to_string();
    assert!(
        text.contains("192.168.1.1")
            && text.contains("198.18.0.1")
            && text.contains("系统代理已启用")
    );
    assert!(requests(&out).is_empty());
}

#[test]
fn fresh_result_hides_date_and_timezone_but_tool_preserves_rfc3339_timestamp() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let out = engine.handle(query("a", "", Value::Null), now, Some(network()));
    let finished = complete(&mut engine, &out, now);
    let timestamp = engine.cache[&Group::Domestic]
        .observation
        .observed_at
        .clone();
    assert!(chrono::DateTime::parse_from_rfc3339(&timestamp).is_ok());
    let items = value(&finished)["items"].as_array().unwrap().clone();
    for row in items {
        assert!(!row["subtitle"].as_str().unwrap().contains(&timestamp));
        assert!(!row["subtitle"].as_str().unwrap().contains("+08"));
    }
    let tool =
        serde_json::from_value(json!({"type":"tool_call","id":"b","tool_name":"get_ip"})).unwrap();
    let output = engine.handle(tool, now + Duration::from_millis(1), Some(network()));
    let val = value(&output);
    let public = val["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "public")
        .unwrap();
    assert_eq!(public["sources"][0]["observed_at"], timestamp);
}

#[test]
fn address_lifetimes_filter_expired_even_when_windows_says_preferred() {
    let ip = "2409::123".parse().unwrap();
    assert!(Address::from_lifetimes(ip, 64, 0, 3600, false).is_none());
    let old = Address::from_lifetimes(ip, 64, 3600, 0, true).unwrap();
    assert!(!old.preferred && old.valid && old.temporary);
    assert_eq!(
        Address::from_lifetimes(ip, 64, 3600, 1800, false),
        Address::from_lifetimes(ip, 64, 3599, 1799, false)
    );
}

#[test]
fn ipv6_parsers_reject_ipv4_private_reserved_and_login_pages() {
    use crate::model::public_v6;
    for ip in [
        "::",
        "::1",
        "fe80::1",
        "fc00::1",
        "ff02::1",
        "::ffff:8.8.8.8",
        "2001:db8::1",
        "2001:2::1",
        "2002::1",
        "3fff::1",
    ] {
        assert!(!public_v6(ip.parse().unwrap()), "{ip}");
        assert!(
            Provider::IpifyV6
                .parse(&json!({"ip":ip}).to_string())
                .is_err()
        );
    }
    assert!(Provider::IpifyV6.parse(r#"{"ip":"8.8.8.8"}"#).is_err());
    assert!(Provider::IpifyV6.parse("<html>2409::1</html>").is_err());
    assert!(Provider::Ipify.parse(r#"{"ip":"2409::1"}"#).is_err());
    assert_eq!(
        Provider::IpifyV6
            .parse(r#"{"ip":"2001:4860::1"}"#)
            .unwrap()
            .ip
            .to_string(),
        "2001:4860::1"
    );
}

fn v6_network() -> NetworkSnapshot {
    let mut net = network();
    let rows = &mut net.adapters[0].addresses;
    rows.push(
        Address::from_lifetimes("2409::456".parse().unwrap(), 128, 3600, 1800, true).unwrap(),
    );
    rows.push(Address::from_lifetimes("2409::789".parse().unwrap(), 128, 3600, 0, true).unwrap());
    rows.push(Address {
        ip: "2409::999".parse().unwrap(),
        prefix: 64,
        valid: false,
        preferred: true,
        temporary: false,
    });
    rows.push(
        Address::from_lifetimes("2409::234".parse().unwrap(), 64, 3600, 1800, false).unwrap(),
    );
    net
}

#[test]
fn basic_ipv6_shows_non_temporary_and_temporary_addresses_and_enhanced_shows_valid_extras() {
    for enhanced in [false, true] {
        let out = Engine::default().handle(
            query(
                "a",
                "local",
                json!({"use_ipv6":true,"enhanced_network":enhanced}),
            ),
            Instant::now(),
            Some(v6_network()),
        );
        let val = value(&out);
        let text = val.to_string();
        assert!(text.contains("2409::123"));
        assert!(text.contains("2409::456"));
        assert_eq!(text.contains("2409::789"), enhanced);
        assert_eq!(text.contains("2409::234"), enhanced);
        assert!(!text.contains("2409::999") && !text.contains("fe80::1"));
        if enhanced {
            assert!(text.contains("临时地址") && text.contains("已弃用"));
            let rows = val["items"].as_array().unwrap();
            assert!(
                rows.iter()
                    .any(|row| row["title"] == "本机 IPv6（其他）：2409::234")
            );
            assert!(
                rows.iter()
                    .any(|row| row["title"] == "本机 IPv6（临时，已弃用）：2409::789")
            );
        }
    }
}

#[test]
fn ipv6_can_fall_back_to_temporary_but_never_deprecated_for_primary() {
    let mut net = v6_network();
    net.adapters[0]
        .addresses
        .retain(|a| a.ip.to_string() != "2409::123");
    let out = Engine::default().handle(
        query("a", "local", json!({"use_ipv6":true})),
        Instant::now(),
        Some(net),
    );
    let text = value(&out).to_string();
    assert!(text.contains("2409::456") && !text.contains("2409::789"));
}

#[test]
fn ipv6_probes_one_unified_group_with_fallback_and_ipv4_first_sorting() {
    for tool in [false, true] {
        let now = Instant::now();
        let mut engine = Engine::default();
        let msg = if tool {
            serde_json::from_value(json!({"type":"tool_call","id":"a","tool_name":"get_ip","arguments":{"include_ipv6":true}})).unwrap()
        } else {
            query("a", "", json!({"use_ipv6":true}))
        };
        let out = engine.handle(msg, now, Some(v6_network()));
        let reqs = requests(&out);
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[2].1, Provider::TestIpv6.url());
        assert!(reply(&mut engine, &reqs[0].0, &domestic_body("1.1.1.1"), now).is_empty());
        assert!(reply(&mut engine, &reqs[1].0, r#"{"ip":"8.8.8.8"}"#, now).is_empty());
        let fallback = fail(&mut engine, &reqs[2].0, now);
        assert_eq!(requests(&fallback)[0].1, Provider::IpifyV6.url());
        let out = reply(
            &mut engine,
            &requests(&fallback)[0].0,
            r#"{"ip":"2001:4860::1"}"#,
            now,
        );
        let val = value(&out);
        let addresses: Vec<_> = if tool {
            val["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["ip"].as_str().unwrap())
                .collect()
        } else {
            val["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["action"]["text"].as_str().unwrap())
                .collect()
        };
        assert_eq!(
            addresses,
            [
                "192.168.1.2",
                "1.1.1.1",
                "8.8.8.8",
                "2409::123",
                "2409::456",
                "2001:4860::1"
            ]
        );
        if tool {
            assert_eq!(
                val["data"].as_array().unwrap().last().unwrap()["sources"][0]["scope"],
                "ipv6"
            );
        } else {
            assert!(
                val.to_string().contains("IPv6 出口：") && val.to_string().contains("已使用备用源")
            );
        }
        assert!(
            !val.to_string().contains("国内站 IPv6") && !val.to_string().contains("国际站 IPv6")
        );
        assert!(engine.jobs.is_empty() && engine.http.is_empty());
    }
}

#[test]
fn enabling_ipv6_preserves_ipv4_cache_and_disabling_it_sends_no_ipv6_requests() {
    let mut engine = Engine::default();
    let now = Instant::now();
    let out = engine.handle(query("a", "", Value::Null), now, Some(network()));
    complete(&mut engine, &out, now);
    let out = engine.handle(
        query("b", "", json!({"use_ipv6":true})),
        now,
        Some(network()),
    );
    let reqs = requests(&out);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].1, Provider::TestIpv6.url());
    let out = reply(
        &mut engine,
        &reqs[0].0,
        r#"callback({"ip":"2409::555","type":"ipv6"})"#,
        now,
    );
    assert!(value(&out).to_string().contains("缓存于"));
    let out = engine.handle(query("c", "", Value::Null), now, Some(network()));
    assert!(requests(&out).is_empty());
    assert!(!value(&out).to_string().contains("IPv6"));
}

#[test]
fn ipv6_failures_and_timeout_keep_ipv4_and_local_results_and_ignore_late_replies() {
    for timeout in [false, true] {
        let now = Instant::now();
        let mut engine = Engine::default();
        let out = engine.handle(
            query("a", "", json!({"use_ipv6":true,"allow_fallback":false})),
            now,
            Some(network()),
        );
        let reqs = requests(&out);
        assert_eq!(reqs.len(), 3);
        assert!(reply(&mut engine, &reqs[0].0, &domestic_body("1.1.1.1"), now).is_empty());
        assert!(reply(&mut engine, &reqs[1].0, r#"{"ip":"8.8.8.8"}"#, now).is_empty());
        let out = if timeout {
            engine.expire(now + Duration::from_secs(7))
        } else {
            reply(
                &mut engine,
                &reqs[2].0,
                r#"callback({"ip":"8.8.8.8","type":"ipv4"})"#,
                now,
            )
        };
        let text = value(&out).to_string();
        assert!(text.contains("1.1.1.1") && text.contains("2409::123"));
        assert!(text.contains(if timeout {
            "IPv6 出口检测超时"
        } else {
            "IPv6 出口检测未完成"
        }));
        assert!(
            reply(
                &mut engine,
                &reqs[2].0,
                r#"callback({"ip":"2409::555","type":"ipv6"})"#,
                now + Duration::from_secs(8)
            )
            .is_empty()
        );
        assert!(!engine.cache.contains_key(&Group::Ipv6));
    }
}

#[test]
fn ipv6_offline_and_legacy_orders_preserve_single_group_and_prefer_test_ipv6() {
    for (scope, expected) in [
        ("none", vec![]),
        ("domestic", vec![Group::Domestic, Group::Ipv6]),
        ("international", vec![Group::International, Group::Ipv6]),
        (
            "ip-api.com",
            vec![Group::Domestic, Group::International, Group::Ipv6],
        ),
    ] {
        let settings = Settings::parse(
            &json!({"use_ipv6":true,"geo_provider":scope,"domestic_ipv6_order":["ipw_v6","ipip_v6"],"international_ipv6_order":["ipify_v6","test_ipv6"]}),
            &Value::Null,
        );
        assert_eq!(settings.detection_groups(), expected);
        assert_eq!(
            settings.providers(Group::Ipv6),
            [Provider::TestIpv6, Provider::IpifyV6]
        );
    }
}

#[test]
fn test_ipv6_parses_only_expected_jsonp_with_public_ipv6_and_matching_type() {
    let body = r#"callback({"ip":"2409::555","type":"ipv6","padding":""})"#;
    let observation = Provider::TestIpv6.parse(body).unwrap();
    assert_eq!(observation.ip.to_string(), "2409::555");
    assert!(observation.location.is_none());
    assert!(Provider::TestIpv6.parse(&format!("\n{body};\n")).is_ok());
    for body in [
        r#"callback({"ip":"8.8.8.8","type":"ipv4"})"#,
        r#"callback({"ip":"8.8.8.8","type":"ipv6"})"#,
        r#"callback({"ip":"2409::555","type":"ipv4"})"#,
        r#"callback({"ip":"fe80::1","type":"ipv6"})"#,
        r#"callback({"ip":"2001:2::1","type":"ipv6"})"#,
        r#"other({"ip":"2409::555","type":"ipv6"})"#,
        r#"callback({"ip":"2409::555","type":"ipv6"});alert(1)"#,
        r#"callback({"ip":"2409::555","type":"ipv6"},{})"#,
        r#"<html>callback({"ip":"2409::555","type":"ipv6"})</html>"#,
    ] {
        assert!(Provider::TestIpv6.parse(body).is_err(), "{body}");
    }
    assert!(Provider::TestIpv6.parse(&"x".repeat(8193)).is_err());
}

#[test]
fn unified_ipv6_falls_back_to_ipify_without_changing_ipv4_scope() {
    let now = Instant::now();
    let mut engine = Engine::default();
    let out = engine.handle(
        query(
            "a",
            "",
            json!({"geo_provider":"international","use_ipv6":true}),
        ),
        now,
        Some(network()),
    );
    let reqs = requests(&out);
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1].1, Provider::TestIpv6.url());
    assert!(reply(&mut engine, &reqs[0].0, r#"{"ip":"8.8.8.8"}"#, now).is_empty());
    let out = fail(&mut engine, &reqs[1].0, now);
    assert_eq!(requests(&out)[0].1, Provider::IpifyV6.url());
    let out = reply(
        &mut engine,
        &requests(&out)[0].0,
        r#"{"ip":"2409::555"}"#,
        now,
    );
    let val = value(&out);
    assert!(
        val.to_string().contains("IPv6 出口：2409::555")
            && val.to_string().contains("已使用备用源")
    );
    assert_eq!(
        engine.cache[&Group::Ipv6].observation.provider,
        Provider::IpifyV6
    );
    assert!(!engine.cache.contains_key(&Group::Domestic));
}

#[test]
fn unified_ipv6_order_is_configurable_isolated_and_respects_fallback_switch() {
    let settings = Settings::parse(
        &json!({"ipv6_order":["ipify_v6","test_ipv6","ipify_v6","ipw_v6"]}),
        &Value::Null,
    );
    assert_eq!(
        settings.providers(Group::Ipv6),
        [Provider::IpifyV6, Provider::TestIpv6]
    );
    assert_eq!(
        settings.providers(Group::International),
        [Provider::Ifconfig, Provider::Ipify]
    );
    let settings = Settings::parse(
        &json!({"ipv6_order":["test_ipv6"],"allow_fallback":false}),
        &Value::Null,
    );
    assert_eq!(settings.providers(Group::Ipv6), [Provider::TestIpv6]);
    let changed = Settings::parse(&json!({"ipv6_order":["ipify_v6"]}), &Value::Null);
    assert!(!Settings::parse(&Value::Null, &Value::Null).same_detection(&changed));
}

#[test]
fn test_ipv6_success_is_cached_and_refresh_uses_one_minute_cooldown() {
    let now = Instant::now();
    let mut engine = Engine::default();
    let settings =
        json!({"geo_provider":"international","use_ipv6":true,"ipv6_order":["test_ipv6"]});
    let out = engine.handle(query("a", "", settings.clone()), now, Some(network()));
    let reqs = requests(&out);
    assert_eq!(reqs[1].1, Provider::TestIpv6.url());
    assert!(reply(&mut engine, &reqs[0].0, r#"{"ip":"8.8.8.8"}"#, now).is_empty());
    reply(
        &mut engine,
        &reqs[1].0,
        r#"callback({"ip":"2409::555","type":"ipv6"})"#,
        now,
    );
    let out = engine.handle(
        query("b", "refresh", settings.clone()),
        now + Duration::from_secs(10),
        Some(network()),
    );
    assert!(requests(&out).is_empty());
    let val = value(&out);
    assert!(val.to_string().contains("2409::555") && val.to_string().contains("刷新过快"));
    let out = engine.handle(
        query("c", "", settings),
        now + Duration::from_secs(61),
        Some(network()),
    );
    assert!(
        requests(&out)
            .iter()
            .any(|(_, url, _)| *url == Provider::TestIpv6.url())
    );
}

#[test]
fn test_ipv6_returning_ipv4_is_failure_instead_of_fake_ipv6_success() {
    let now = Instant::now();
    let mut engine = Engine::default();
    let out = engine.handle(query("a", "", json!({"geo_provider":"international","use_ipv6":true,"ipv6_order":["test_ipv6"],"allow_fallback":false})), now, Some(network()));
    let reqs = requests(&out);
    assert!(reply(&mut engine, &reqs[0].0, r#"{"ip":"8.8.8.8"}"#, now).is_empty());
    let out = reply(
        &mut engine,
        &reqs[1].0,
        r#"callback({"ip":"8.8.8.8","type":"ipv4"})"#,
        now,
    );
    let text = value(&out).to_string();
    assert!(text.contains("国际站出口：8.8.8.8"));
    assert!(text.contains("IPv6 出口检测未完成") && text.contains("返回地址类型不匹配"));
    assert!(!engine.cache.contains_key(&Group::Ipv6));
}

#[test]
fn local_lookup_preserves_successful_egress_cache_during_provider_cooldown() {
    let now = Instant::now();
    let mut engine = Engine::default();
    let settings = json!({"geo_provider":"international","use_ipv6":true});
    let out = engine.handle(query("online", "", settings.clone()), now, Some(network()));
    let reqs = requests(&out);
    reply(&mut engine, &reqs[0].0, r#"{"ip":"8.8.8.8"}"#, now);
    reply(
        &mut engine,
        &reqs[1].0,
        r#"callback({"ip":"2409::555","type":"ipv6"})"#,
        now,
    );
    let local = engine.handle(
        query("local", "local", settings.clone()),
        now + Duration::from_secs(1),
        Some(network()),
    );
    assert!(requests(&local).is_empty());
    assert!(!value(&local).to_string().contains("2409::555"));
    let online = engine.handle(
        query("again", "", settings),
        now + Duration::from_secs(2),
        Some(network()),
    );
    assert!(requests(&online).is_empty());
    let text = value(&online).to_string();
    assert!(text.contains("8.8.8.8") && text.contains("2409::555"));
    assert!(text.contains("缓存") && !text.contains("检测未完成"));
}

#[test]
fn cooldown_without_a_successful_ip_reports_remaining_wait_and_skipped_request() {
    let now = Instant::now();
    let mut engine = Engine::default();
    let settings =
        json!({"geo_provider":"domestic","domestic_order":["pubyun"],"allow_fallback":false});
    let out = engine.handle(query("first", "", settings.clone()), now, Some(network()));
    fail(&mut engine, &requests(&out)[0].0, now);
    let out = engine.handle(
        query("again", "refresh", settings),
        now + Duration::from_millis(10_001),
        Some(network()),
    );
    assert!(requests(&out).is_empty());
    let text = value(&out).to_string();
    assert!(text.contains("50 秒后可重试") && text.contains("本次未请求"));
    assert!(text.contains("检测未完成"));
}

#[test]
fn basic_temporary_ipv6_is_labelled_copied_and_sorted_in_query_and_tool() {
    for tool in [false, true] {
        let mut net = v6_network();
        net.adapters[0].addresses.push(
            Address::from_lifetimes("2409::567".parse().unwrap(), 128, 3600, 1800, true).unwrap(),
        );
        let msg = if tool {
            serde_json::from_value(json!({"type":"tool_call","id":"a","tool_name":"get_ip","arguments":{"include_ipv6":true},"settings":{"geo_provider":"none"}})).unwrap()
        } else {
            query("a", "local", json!({"use_ipv6":true}))
        };
        let out = Engine::default().handle(msg, Instant::now(), Some(net));
        let val = value(&out);
        if tool {
            let rows = val["data"].as_array().unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[2]["ip"], "2409::456");
            assert_eq!(rows[2]["temporary"], true);
            assert_eq!(rows[2]["type"], "本机 IPv6（临时）");
        } else {
            let rows = val["items"].as_array().unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[2]["title"], "本机 IPv6（临时）：2409::456");
            assert_eq!(rows[2]["action"]["text"], "2409::456");
            assert!(rows[2]["subtitle"].as_str().unwrap().contains("临时地址"));
        }
        assert!(!val.to_string().contains("2409::567") && !val.to_string().contains("2409::789"));
    }
}
