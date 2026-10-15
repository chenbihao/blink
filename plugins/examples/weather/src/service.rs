//! 单线程查询状态机；输入/HTTP/取消按请求 ID 隔离。
use crate::model::ResolvedLocation;
use crate::protocol::*;
use crate::providers::open_meteo;
use crate::settings::{self, RequestParameters};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

const DEFAULT_BUDGET_MS: u64 = 10_000;
const MAX_BUDGET_MS: u64 = 30_000;
const MAX_PENDING_QUERIES: usize = 4;

#[derive(Clone, Copy)]
enum ReplyKind {
    Query,
    Tool,
}

enum Stage {
    Geocoding {
        query: crate::location::LocationQuery,
        search_only: bool,
        unqualified: bool,
    },
    Weather(ResolvedLocation),
}

struct Pending {
    query_id: String,
    reply: ReplyKind,
    unit: crate::settings::TemperatureUnit,
    deadline: Instant,
    stage: Stage,
}

#[derive(Default)]
pub struct WeatherService {
    pending: HashMap<String, Pending>,
    next_http_id: u64,
}

fn error(code: &str, message: &str) -> PluginError {
    PluginError {
        code: code.into(),
        message: message.into(),
    }
}

fn failed(id: String, reply: ReplyKind, error: PluginError) -> PluginToCore {
    eprintln!("[weather][warn] request failed: code={}", error.code);
    match reply {
        ReplyKind::Query => PluginToCore::Response(PluginResponse {
            id,
            items: vec![],
            error: Some(error),
        }),
        ReplyKind::Tool => PluginToCore::RawResult(RawToolResult {
            id,
            data: serde_json::Value::Null,
            error: Some(error),
        }),
    }
}

fn remaining_ms(deadline: Instant, now: Instant) -> u64 {
    deadline
        .saturating_duration_since(now)
        .as_millis()
        .min(u64::MAX as u128) as u64
}

impl WeatherService {
    pub fn handle(&mut self, message: CoreToPlugin, now: Instant) -> Vec<PluginToCore> {
        // 用户主动取消不生成超时/错误回复，也不能续发下一阶段。
        if let CoreToPlugin::Cancel { id } = &message {
            eprintln!("[weather] request cancelled");
            self.pending.retain(|_, pending| pending.query_id != *id);
            return vec![];
        }
        // 清理所有过期请求，防旧宿主不发 cancel 时永久占槽。
        let mut responses = Vec::new();
        self.pending.retain(|_, pending| {
            if remaining_ms(pending.deadline, now) == 0 {
                responses.push(failed(
                    pending.query_id.clone(),
                    pending.reply,
                    error("timeout", "天气查询超时，请稍后重试"),
                ));
                false
            } else {
                true
            }
        });
        let response = match message {
            CoreToPlugin::Cancel { id } => {
                self.pending.retain(|_, pending| pending.query_id != id);
                None
            }
            CoreToPlugin::Query {
                id,
                query,
                settings,
                budget_ms,
            } => {
                let parameters = settings::parse(Some(&query), None, settings.as_ref());
                Some(self.start(id, ReplyKind::Query, parameters, budget_ms, now))
            }
            CoreToPlugin::ToolCall {
                id,
                tool_name,
                arguments,
                settings,
                budget_ms,
            } => {
                if tool_name == "search_locations" {
                    Some(match settings::parse_search(&arguments) {
                        Ok(query) => self.start_search(id, query, budget_ms, now),
                        Err(e) => failed(id, ReplyKind::Tool, e),
                    })
                } else if tool_name != "get_weather" {
                    Some(failed(
                        id,
                        ReplyKind::Tool,
                        error("invalid_arguments", "未知天气工具"),
                    ))
                } else {
                    let parameters = settings::parse(None, Some(&arguments), settings.as_ref());
                    Some(self.start(id, ReplyKind::Tool, parameters, budget_ms, now))
                }
            }
            CoreToPlugin::HttpResponse {
                id,
                status,
                body,
                error: network_error,
            } => self.http_response(&id, status, body.as_deref(), network_error.as_deref(), now),
        };
        if let Some(response) = response {
            responses.push(response);
        }
        responses
    }

    fn start(
        &mut self,
        id: String,
        reply: ReplyKind,
        parameters: Result<RequestParameters, PluginError>,
        budget_ms: Option<u64>,
        now: Instant,
    ) -> PluginToCore {
        let parameters = match parameters {
            Ok(p) => p,
            Err(e) => return failed(id, reply, e),
        };
        if self.pending.len() >= MAX_PENDING_QUERIES
            || self.pending.values().any(|p| p.query_id == id)
        {
            return failed(id, reply, error("busy", "天气查询繁忙，请稍后重试"));
        }
        let budget = budget_ms.unwrap_or(DEFAULT_BUDGET_MS).min(MAX_BUDGET_MS);
        if budget == 0 {
            return failed(id, reply, error("timeout", "天气查询时间预算已用尽"));
        }
        let (stage, url) = if let Some(location) = parameters.location {
            let url = open_meteo::weather_url(&location);
            (Stage::Weather(location), url)
        } else {
            let query = parameters.location_query.expect("name query was validated");
            let url = crate::location::open_meteo::search_url(&query);
            (
                Stage::Geocoding {
                    query,
                    search_only: false,
                    unqualified: false,
                },
                url,
            )
        };
        self.issue(
            Pending {
                query_id: id,
                reply,
                unit: parameters.unit,
                deadline: now + Duration::from_millis(budget),
                stage,
            },
            url,
            now,
        )
    }

    fn start_search(
        &mut self,
        id: String,
        query: crate::location::LocationQuery,
        budget_ms: Option<u64>,
        now: Instant,
    ) -> PluginToCore {
        if self.pending.len() >= MAX_PENDING_QUERIES
            || self.pending.values().any(|p| p.query_id == id)
        {
            return failed(
                id,
                ReplyKind::Tool,
                error("busy", "地点搜索繁忙，请稍后重试"),
            );
        }
        let budget = budget_ms.unwrap_or(DEFAULT_BUDGET_MS).min(MAX_BUDGET_MS);
        if budget == 0 {
            return failed(id, ReplyKind::Tool, error("timeout", "地点搜索预算已用尽"));
        }
        let url = crate::location::open_meteo::search_url(&query);
        self.issue(
            Pending {
                query_id: id,
                reply: ReplyKind::Tool,
                unit: crate::settings::TemperatureUnit::Celsius,
                deadline: now + Duration::from_millis(budget),
                stage: Stage::Geocoding {
                    query,
                    search_only: true,
                    unqualified: false,
                },
            },
            url,
            now,
        )
    }
    fn issue(&mut self, pending: Pending, url: String, now: Instant) -> PluginToCore {
        let timeout_ms = remaining_ms(pending.deadline, now);
        if timeout_ms == 0 {
            return failed(
                pending.query_id,
                pending.reply,
                error("timeout", "天气查询超时，请稍后重试"),
            );
        }
        self.next_http_id += 1;
        let id = format!("weather_http_{}", self.next_http_id);
        self.pending.insert(id.clone(), pending);
        PluginToCore::HttpRequest(HttpRequest {
            id,
            method: "GET".into(),
            url,
            body: None,
            timeout_ms,
            headers: vec![],
        })
    }

    fn http_response(
        &mut self,
        id: &str,
        status: u16,
        body: Option<&str>,
        network_error: Option<&str>,
        now: Instant,
    ) -> Option<PluginToCore> {
        let pending = self.pending.remove(id)?;
        let stage = match &pending.stage {
            Stage::Geocoding { .. } => "geocoding",
            Stage::Weather(_) => "weather",
        };
        eprintln!(
            "[weather] HTTP completed: provider=open_meteo stage={stage} http_id={id} status={status}"
        );
        let result = if network_error.is_some() || status != 200 {
            if network_error.is_some_and(|e| e.starts_with("busy:")) {
                return Some(failed(
                    pending.query_id,
                    pending.reply,
                    error("busy", "宿主天气请求繁忙，请稍后重试"),
                ));
            }
            if network_error.is_some_and(|e| e.starts_with("timeout:")) {
                return Some(failed(
                    pending.query_id,
                    pending.reply,
                    error("timeout", "天气服务请求超时，请稍后重试"),
                ));
            }
            Err(match status {
                401 | 403 => error("authentication_failed", "天气服务拒绝请求，请检查服务配置"),
                429 => error("rate_limited", "天气服务请求过于频繁，请稍后重试"),
                400 => error(
                    "invalid_arguments",
                    "天气服务无法处理该查询，请检查城市名称",
                ),
                _ => error("network_failed", "天气服务连接失败，请检查网络后重试"),
            })
        } else {
            body.ok_or_else(|| error("invalid_response", "天气服务没有返回数据"))
        };
        let body = match result {
            Ok(body) => body,
            Err(e) => return Some(failed(pending.query_id, pending.reply, e)),
        };
        match pending.stage {
            Stage::Geocoding {
                query,
                search_only,
                unqualified,
            } => match crate::location::open_meteo::parse_candidates(body, &query) {
                Ok(candidates)
                    if candidates.is_empty() && query.admin.is_some() && !unqualified =>
                {
                    let url = crate::location::open_meteo::unqualified_search_url(&query);
                    Some(self.issue(
                        Pending {
                            stage: Stage::Geocoding {
                                query,
                                search_only,
                                unqualified: true,
                            },
                            ..pending
                        },
                        url,
                        now,
                    ))
                }
                Ok(candidates) if search_only => Some(PluginToCore::RawResult(RawToolResult {
                    id: pending.query_id,
                    data: serde_json::json!([{"resolver":"open_meteo","summary":format!("找到 {} 个地点候选",candidates.len()),"count":candidates.len(),"truncated":candidates.len()>=query.limit || crate::location::open_meteo::possibly_truncated(body),"candidates":candidates}]),
                    error: None,
                })),
                Ok(candidates) if candidates.is_empty() => Some(failed(
                    pending.query_id,
                    pending.reply,
                    error(
                        "location_not_found",
                        "未找到符合名称与限定的地点，请尝试更完整名称",
                    ),
                )),
                Ok(mut candidates)
                    if crate::location::can_auto_resolve(&query, &candidates)
                        && !crate::location::open_meteo::possibly_truncated(body) =>
                {
                    let location = candidates.remove(0);
                    let url = open_meteo::weather_url(&location);
                    Some(self.issue(
                        Pending {
                            stage: Stage::Weather(location),
                            ..pending
                        },
                        url,
                        now,
                    ))
                }
                Ok(candidates) => {
                    let (items, data) = crate::presentation::needs_location(&candidates);
                    Some(match pending.reply {
                        ReplyKind::Query => PluginToCore::Response(PluginResponse {
                            id: pending.query_id,
                            items,
                            error: None,
                        }),
                        ReplyKind::Tool => PluginToCore::RawResult(RawToolResult {
                            id: pending.query_id,
                            data,
                            error: None,
                        }),
                    })
                }
                Err(e) => Some(failed(pending.query_id, pending.reply, e)),
            },
            Stage::Weather(location) => match open_meteo::parse_weather(body, location) {
                Ok(snapshot) => {
                    eprintln!(
                        "[weather] completed: provider={} status={} daily_days={}",
                        snapshot.provider,
                        snapshot.status(),
                        snapshot.daily.len()
                    );
                    let (items, data) = crate::presentation::render(&snapshot, pending.unit);
                    Some(match pending.reply {
                        ReplyKind::Query => PluginToCore::Response(PluginResponse {
                            id: pending.query_id,
                            items,
                            error: None,
                        }),
                        ReplyKind::Tool => PluginToCore::RawResult(RawToolResult {
                            id: pending.query_id,
                            data,
                            error: None,
                        }),
                    })
                }
                Err(e) => Some(failed(pending.query_id, pending.reply, e)),
            },
        }
    }
}

pub fn run() {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut service = WeatherService::default();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let message = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => {
                eprintln!("[weather][warn] invalid JSONL message");
                continue;
            }
        };
        for response in service.handle(message, Instant::now()) {
            if serde_json::to_writer(&mut stdout, &response).is_err()
                || writeln!(stdout).is_err()
                || stdout.flush().is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_coordinates_start_weather_without_geocoding() {
        let mut service = WeatherService::default();
        let result=service.handle(CoreToPlugin::ToolCall{id:"coordinates".into(),tool_name:"get_weather".into(),arguments:json!({}),settings:Some(json!({"default_location":{"latitude":23.13,"longitude":113.26,"coordinate_system":"wgs84","name":"广州"},"default_city":"wrong"})),budget_ms:None},Instant::now());
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
    fn ambiguous_city_returns_candidates_without_weather_or_config_writes() {
        let now = Instant::now();
        let mut service = WeatherService::default();
        let first = service.handle(tool("ambiguity", None), now);
        let result=service.handle(CoreToPlugin::HttpResponse{id:http_id(&first[0]),status:200,error:None,body:Some(r#"{"results":[{"id":1,"name":"广州","latitude":23,"longitude":113,"feature_code":"PPL"},{"id":2,"name":"广州","latitude":24,"longitude":114,"feature_code":"PPL"}]}"#.into())},now);
        let PluginToCore::RawResult(result) = &result[0] else {
            panic!("candidate result expected");
        };
        assert_eq!(result.data[0]["status"], "needs_location");
        assert!(result.data[0].get("current").is_none());
        assert_eq!(result.data[0]["candidates"].as_array().unwrap().len(), 2);
        assert!(service.pending.is_empty());
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
            CoreToPlugin::HttpResponse {
                id: http_id(&first[0]),
                status: 200,
                body: Some("{}".into()),
                error: None,
            },
            now + Duration::from_millis(30),
        );
        let PluginToCore::HttpRequest(request) = &retry[0] else {
            panic!("unqualified retry expected");
        };
        assert_eq!(request.timeout_ms, 70);
        assert!(!request.url.contains("%2C"));
        let result = service.handle(
            CoreToPlugin::HttpResponse {
                id: request.id.clone(),
                status: 200,
                body: Some("{}".into()),
                error: None,
            },
            now + Duration::from_millis(40),
        );
        let PluginToCore::RawResult(result) = &result[0] else {
            panic!("search result expected");
        };
        assert!(result.error.is_none());
        assert_eq!(result.data[0]["count"], 0);
        assert!(service.pending.is_empty());
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
    fn http_id(message: &PluginToCore) -> String {
        match message {
            PluginToCore::HttpRequest(r) => r.id.clone(),
            _ => panic!("expected HTTP request"),
        }
    }
    fn geo(id: String) -> CoreToPlugin {
        CoreToPlugin::HttpResponse {
            id,
            status: 200,
            body: Some(
                r#"{"results":[{"name":"广州","latitude":23.13,"longitude":113.26,"feature_code":"PPLA"}]}"#.into(),
            ),
            error: None,
        }
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
        assert!(service.handle(geo(http), now).is_empty());
        assert!(service.pending.is_empty());
    }
    #[test]
    fn remaining_budget_is_shared_and_expired_response_cannot_continue() {
        let mut service = WeatherService::default();
        let now = Instant::now();
        let initial = service.handle(tool("tc_1", Some(100)), now);
        let id = http_id(&initial[0]);
        let next = service.handle(geo(id), now + Duration::from_millis(60));
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
        assert!(service.pending.is_empty());
    }
    #[test]
    fn concurrent_requests_have_distinct_ids_and_cancel_does_not_cross_queries() {
        let mut service = WeatherService::default();
        let now = Instant::now();
        let a = service.handle(tool("a", None), now);
        let b = service.handle(tool("b", None), now);
        assert_ne!(http_id(&a[0]), http_id(&b[0]));
        service.handle(CoreToPlugin::Cancel { id: "a".into() }, now);
        assert_eq!(service.handle(geo(http_id(&b[0])), now).len(), 1);
        assert!(service.handle(geo(http_id(&a[0])), now).is_empty());
    }
    #[test]
    fn pending_work_is_bounded_and_expiry_releases_capacity() {
        let mut service = WeatherService::default();
        let now = Instant::now();
        for i in 0..MAX_PENDING_QUERIES {
            service.handle(tool(&i.to_string(), Some(10)), now);
        }
        let busy = service.handle(tool("extra", None), now);
        match &busy[0] {
            PluginToCore::RawResult(r) => assert_eq!(r.error.as_ref().unwrap().code, "busy"),
            _ => panic!("expected busy"),
        }
        let resumed = service.handle(tool("new", None), now + Duration::from_millis(11));
        assert!(matches!(resumed.last(), Some(PluginToCore::HttpRequest(_))));
        assert_eq!(service.pending.len(), 1);
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
        let second = service.handle(geo(http_id(&first[0])), now);
        let final_result=service.handle(CoreToPlugin::HttpResponse{id:http_id(&second[0]),status:200,body:Some(r#"{"current":{"temperature_2m":0,"weather_code":96,"wind_speed_10m":2},"daily":{"time":["2026-10-15"],"temperature_2m_max":[10],"temperature_2m_min":[0]}}"#.into()),error:None},now);
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
        assert!(service.pending.is_empty());
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
        assert!(service.pending.is_empty());
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
            assert!(service.pending.is_empty());
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
        assert!(service.pending.is_empty());
    }
}
