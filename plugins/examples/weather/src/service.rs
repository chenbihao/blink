//! 单线程查询状态机；输入/HTTP/取消按请求 ID 隔离。
//!
//! 0.26.2 起区分"逻辑查询"与"HTTP 子请求"：一条用户查询（queries，按 query_id）
//! 可同时挂多个在途 HTTP 子请求（http，按 http_id → (query_id, 槽位) 关联）——
//! 和风实况与逐日并行共用同一条查询的总预算，两个子请求都完成前不移除该查询。
//! 查询容量（4 槽）按逻辑查询计，不按 HTTP 计。
use crate::model::{CurrentWeather, DailyWeather, ResolvedLocation, WeatherSnapshot};
use crate::protocol::*;
use crate::providers::{caiyun, open_meteo, qweather};
use crate::settings::{self, Provider, RequestParameters, Resolver, TemperatureUnit};
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

/// 并行子请求的槽位；单请求阶段固定 Only。
#[derive(Clone, Copy)]
enum HttpSlot {
    Only,
    QCurrent,
    QDaily,
}

enum Stage {
    /// 名称解析进行中；resolver 标记实际使用的解析来源。
    Geocoding {
        query: crate::location::LocationQuery,
        resolver: &'static str,
        search_only: bool,
        unqualified: bool,
    },
    WeatherOpenMeteo(ResolvedLocation),
    /// 和风实况/逐日并行；槽位在子请求完成时填充，两侧齐后聚合。
    WeatherQWeather {
        location: ResolvedLocation,
        current: Option<Result<(CurrentWeather, Vec<String>), PluginError>>,
        daily: Option<Result<(Vec<DailyWeather>, Vec<String>), PluginError>>,
    },
    /// 彩云综合 weather 单请求同时覆盖实况+逐日。
    WeatherCaiyun(ResolvedLocation),
}

struct Pending {
    reply: ReplyKind,
    unit: TemperatureUnit,
    deadline: Instant,
    provider: Provider,
    qweather: Option<settings::QWeatherCredentials>,
    caiyun: Option<settings::CaiyunCredentials>,
    stage: Stage,
    inflight: usize,
}

#[derive(Default)]
pub struct WeatherService {
    queries: HashMap<String, Pending>,
    http: HashMap<String, (String, HttpSlot)>,
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
            self.cancel(id);
            return vec![];
        }
        // 清理所有过期请求，防旧宿主不发 cancel 时永久占槽。
        let mut responses = Vec::new();
        let expired: Vec<String> = self
            .queries
            .iter()
            .filter(|(_, pending)| remaining_ms(pending.deadline, now) == 0)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            if let Some(pending) = self.queries.remove(&id) {
                self.http.retain(|_, (owner, _)| owner != &id);
                responses.push(failed(
                    id.clone(),
                    pending.reply,
                    error("timeout", "天气查询超时，请稍后重试"),
                ));
            }
        }
        let mut incoming = match message {
            CoreToPlugin::Cancel { id } => {
                self.cancel(&id);
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
                        Ok(search) => {
                            match Self::resolve_resolver(search.resolver, settings.as_ref()) {
                                Ok(resolver) => self.start_search(
                                    id,
                                    search.query,
                                    resolver,
                                    settings.as_ref(),
                                    budget_ms,
                                    now,
                                ),
                                Err(e) => vec![failed(id, ReplyKind::Tool, e)],
                            }
                        }
                        Err(e) => vec![failed(id, ReplyKind::Tool, e)],
                    })
                } else if tool_name != "get_weather" {
                    Some(vec![failed(
                        id,
                        ReplyKind::Tool,
                        error("invalid_arguments", "未知天气工具"),
                    )])
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
            } => self
                .http_response(&id, status, body.as_deref(), network_error.as_deref(), now)
                .map(|message| vec![message]),
        };
        if let Some(mut incoming) = incoming.take() {
            responses.append(&mut incoming);
        }
        responses
    }

    fn cancel(&mut self, id: &str) {
        self.queries.remove(id);
        self.http.retain(|_, (owner, _)| owner != id);
    }

    /// resolver 参数 + settings → 实际解析来源。显式和风缺配置明确报错，
    /// 不静默换源；auto 只在和风已配置时优先和风。
    fn resolve_resolver(
        requested: Resolver,
        settings: Option<&serde_json::Value>,
    ) -> Result<&'static str, PluginError> {
        let qweather_ready =
            settings::qweather_credentials(settings.and_then(serde_json::Value::as_object))?;
        match requested {
            Resolver::QWeather => {
                if qweather_ready.is_none() {
                    return Err(error(
                        "provider_not_configured",
                        "和风地点搜索需要先在设置中配置 API Host 与 API Key",
                    ));
                }
                Ok("qweather")
            }
            Resolver::OpenMeteo => Ok("open_meteo"),
            Resolver::Auto => Ok(if qweather_ready.is_some() {
                "qweather"
            } else {
                "open_meteo"
            }),
        }
    }

    fn start(
        &mut self,
        id: String,
        reply: ReplyKind,
        parameters: Result<RequestParameters, PluginError>,
        budget_ms: Option<u64>,
        now: Instant,
    ) -> Vec<PluginToCore> {
        let parameters = match parameters {
            Ok(p) => p,
            Err(e) => return vec![failed(id, reply, e)],
        };
        if self.queries.len() >= MAX_PENDING_QUERIES || self.queries.contains_key(&id) {
            return vec![failed(id, reply, error("busy", "天气查询繁忙，请稍后重试"))];
        }
        let budget = budget_ms.unwrap_or(DEFAULT_BUDGET_MS).min(MAX_BUDGET_MS);
        if budget == 0 {
            return vec![failed(
                id,
                reply,
                error("timeout", "天气查询时间预算已用尽"),
            )];
        }
        let mut requests: Vec<(String, Vec<(String, String)>, HttpSlot)> = Vec::new();
        let stage = if let Some(location) = parameters.location {
            match parameters.provider {
                Provider::OpenMeteo => {
                    requests.push((open_meteo::weather_url(&location), vec![], HttpSlot::Only));
                    Stage::WeatherOpenMeteo(location)
                }
                Provider::QWeather => {
                    let credentials = parameters.qweather.as_ref().expect("validated");
                    requests.push((
                        qweather::current_url(credentials, &location),
                        credentials.headers(),
                        HttpSlot::QCurrent,
                    ));
                    requests.push((
                        qweather::daily_url(credentials, &location),
                        credentials.headers(),
                        HttpSlot::QDaily,
                    ));
                    Stage::WeatherQWeather {
                        location,
                        current: None,
                        daily: None,
                    }
                }
                Provider::Caiyun => {
                    let credentials = parameters.caiyun.as_ref().expect("validated");
                    // 区域未确认/签名失败不占槽，直接按 invalid_arguments 反馈
                    let signed = crate::location::coordinates::caiyun_request_coordinates(
                        &location,
                    )
                    .and_then(|coordinates| caiyun::weather_request(credentials, &coordinates));
                    match signed {
                        Ok(request) => {
                            requests.push((request.url, request.headers, HttpSlot::Only));
                            Stage::WeatherCaiyun(location)
                        }
                        Err(e) => return vec![failed(id, reply, e)],
                    }
                }
            }
        } else {
            // 名称解析来源独立于天气来源：已配置和风时优先和风，否则 Open-Meteo。
            let resolver = if parameters.qweather.is_some() {
                "qweather"
            } else {
                "open_meteo"
            };
            let query = parameters.location_query.expect("name query was validated");
            match resolver {
                "qweather" => {
                    let credentials = parameters.qweather.as_ref().expect("checked above");
                    requests.push((
                        crate::location::qweather::search_url(&query, &credentials.host),
                        credentials.headers(),
                        HttpSlot::Only,
                    ));
                }
                _ => requests.push((
                    crate::location::open_meteo::search_url(&query),
                    vec![],
                    HttpSlot::Only,
                )),
            }
            Stage::Geocoding {
                query,
                resolver,
                search_only: false,
                unqualified: false,
            }
        };
        let inflight = requests.len();
        self.queries.insert(
            id.clone(),
            Pending {
                reply,
                unit: parameters.unit,
                deadline: now + Duration::from_millis(budget),
                provider: parameters.provider,
                qweather: parameters.qweather.clone(),
                caiyun: parameters.caiyun.clone(),
                stage,
                inflight,
            },
        );
        self.flush_http(&id, requests, now, reply)
    }

    fn start_search(
        &mut self,
        id: String,
        query: crate::location::LocationQuery,
        resolver: &'static str,
        settings: Option<&serde_json::Value>,
        budget_ms: Option<u64>,
        now: Instant,
    ) -> Vec<PluginToCore> {
        if self.queries.len() >= MAX_PENDING_QUERIES || self.queries.contains_key(&id) {
            return vec![failed(
                id,
                ReplyKind::Tool,
                error("busy", "地点搜索繁忙，请稍后重试"),
            )];
        }
        let budget = budget_ms.unwrap_or(DEFAULT_BUDGET_MS).min(MAX_BUDGET_MS);
        if budget == 0 {
            return vec![failed(
                id,
                ReplyKind::Tool,
                error("timeout", "地点搜索预算已用尽"),
            )];
        }
        let (url, headers) = if resolver == "qweather" {
            let credentials =
                settings::qweather_credentials(settings.and_then(serde_json::Value::as_object))
                    .ok()
                    .flatten()
                    .expect("qweather resolver implies configured credentials");
            (
                crate::location::qweather::search_url(&query, &credentials.host),
                credentials.headers(),
            )
        } else {
            (crate::location::open_meteo::search_url(&query), vec![])
        };
        self.queries.insert(
            id.clone(),
            Pending {
                reply: ReplyKind::Tool,
                unit: TemperatureUnit::Celsius,
                deadline: now + Duration::from_millis(budget),
                provider: Provider::OpenMeteo,
                qweather: None,
                caiyun: None,
                stage: Stage::Geocoding {
                    query,
                    resolver,
                    search_only: true,
                    unqualified: false,
                },
                inflight: 1,
            },
        );
        self.flush_http(
            &id,
            vec![(url, headers, HttpSlot::Only)],
            now,
            ReplyKind::Tool,
        )
    }

    /// 发出已排队的子请求；任何一个发不出（预算耗尽）即终止整条查询。
    fn flush_http(
        &mut self,
        query_id: &str,
        requests: Vec<(String, Vec<(String, String)>, HttpSlot)>,
        now: Instant,
        reply: ReplyKind,
    ) -> Vec<PluginToCore> {
        let mut messages = Vec::new();
        for (url, headers, slot) in requests {
            match self.issue(query_id, url, headers, slot, now) {
                Ok(message) => messages.push(message),
                Err(e) => {
                    self.queries.remove(query_id);
                    self.http.retain(|_, (owner, _)| owner != query_id);
                    return vec![failed(query_id.to_owned(), reply, e)];
                }
            }
        }
        messages
    }

    fn issue(
        &mut self,
        query_id: &str,
        url: String,
        headers: Vec<(String, String)>,
        slot: HttpSlot,
        now: Instant,
    ) -> Result<PluginToCore, PluginError> {
        let deadline = self
            .queries
            .get(query_id)
            .map(|pending| pending.deadline)
            .ok_or_else(|| error("busy", "天气查询繁忙，请稍后重试"))?;
        let timeout_ms = remaining_ms(deadline, now);
        if timeout_ms == 0 {
            return Err(error("timeout", "天气查询超时，请稍后重试"));
        }
        self.next_http_id += 1;
        let http_id = format!("weather_http_{}", self.next_http_id);
        self.http
            .insert(http_id.clone(), (query_id.to_owned(), slot));
        Ok(PluginToCore::HttpRequest(HttpRequest {
            id: http_id,
            method: "GET".into(),
            url,
            body: None,
            timeout_ms,
            headers,
        }))
    }

    fn http_response(
        &mut self,
        http_id: &str,
        status: u16,
        body: Option<&str>,
        network_error: Option<&str>,
        now: Instant,
    ) -> Option<PluginToCore> {
        let (query_id, slot) = self.http.remove(http_id)?;
        let pending = self.queries.get_mut(&query_id)?;
        pending.inflight -= 1;
        let stage_label = match &pending.stage {
            Stage::Geocoding { resolver, .. } => format!("geocoding:{resolver}"),
            Stage::WeatherOpenMeteo(_) => "weather:open_meteo".to_owned(),
            Stage::WeatherQWeather { .. } => "weather:qweather".to_owned(),
            Stage::WeatherCaiyun(_) => "weather:caiyun".to_owned(),
        };
        eprintln!(
            "[weather] HTTP completed: stage={stage_label} http_id={http_id} status={status}"
        );

        // 和风双请求：先把子结果放入槽位；两侧都到齐才聚合回复。
        if let Stage::WeatherQWeather { current, daily, .. } = &mut pending.stage {
            match slot {
                HttpSlot::QCurrent => {
                    *current = Some(parse_qweather_current(status, body, network_error))
                }
                HttpSlot::QDaily => {
                    *daily = Some(parse_qweather_daily(status, body, network_error))
                }
                HttpSlot::Only => unreachable!("qweather stage never issues single requests"),
            }
            if pending.inflight > 0 {
                return None;
            }
            let pending = self.queries.remove(&query_id)?;
            return Some(self.finish_qweather(query_id, pending));
        }

        // 彩云综合 weather：单请求即聚合点，错误按彩云错误表细分。
        if matches!(pending.stage, Stage::WeatherCaiyun(_)) {
            let pending = self.queries.remove(&query_id)?;
            return Some(self.finish_caiyun(query_id, pending, status, body, network_error));
        }

        let pending = self.queries.remove(&query_id)?;
        let body = match classify_http(status, body, network_error) {
            Ok(body) => body,
            Err(e) => return Some(failed(query_id, pending.reply, e)),
        };
        let Pending {
            reply,
            unit,
            deadline,
            provider,
            qweather,
            caiyun,
            stage,
            ..
        } = pending;
        match stage {
            Stage::Geocoding {
                query,
                resolver,
                search_only,
                unqualified,
            } => self.finish_geocoding(
                query_id,
                reply,
                unit,
                deadline,
                provider,
                qweather,
                caiyun,
                body,
                query,
                resolver,
                search_only,
                unqualified,
                now,
            ),
            Stage::WeatherOpenMeteo(location) => match open_meteo::parse_weather(&body, location) {
                Ok(snapshot) => Some(self.complete(query_id, reply, &snapshot, unit)),
                Err(e) => Some(failed(query_id, reply, e)),
            },
            Stage::WeatherQWeather { .. } | Stage::WeatherCaiyun(_) => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_geocoding(
        &mut self,
        query_id: String,
        reply: ReplyKind,
        unit: TemperatureUnit,
        deadline: Instant,
        provider: Provider,
        qweather: Option<settings::QWeatherCredentials>,
        caiyun: Option<settings::CaiyunCredentials>,
        body: String,
        query: crate::location::LocationQuery,
        resolver: &'static str,
        search_only: bool,
        unqualified: bool,
        now: Instant,
    ) -> Option<PluginToCore> {
        let parsed = if resolver == "qweather" {
            crate::location::qweather::parse_candidates(&body, &query).map(|candidates| {
                (
                    candidates,
                    crate::location::qweather::possibly_truncated(&body, &query),
                    crate::location::qweather::parse_attributions(&body),
                )
            })
        } else {
            crate::location::open_meteo::parse_candidates(&body, &query).map(|candidates| {
                (
                    candidates,
                    crate::location::open_meteo::possibly_truncated(&body),
                    Vec::new(),
                )
            })
        };
        let (candidates, truncated, attributions) = match parsed {
            Ok(value) => value,
            Err(e) => return Some(failed(query_id, reply, e)),
        };
        // Open-Meteo 的 admin 限定搜索无结果时，去限定重试一次再本地过滤（共用总预算）。
        if resolver == "open_meteo"
            && candidates.is_empty()
            && query.admin.is_some()
            && !unqualified
        {
            let url = crate::location::open_meteo::unqualified_search_url(&query);
            self.queries.insert(
                query_id.clone(),
                Pending {
                    stage: Stage::Geocoding {
                        query,
                        resolver,
                        search_only,
                        unqualified: true,
                    },
                    inflight: 1,
                    reply,
                    unit,
                    deadline,
                    provider,
                    qweather,
                    caiyun,
                },
            );
            return self.issue(&query_id, url, vec![], HttpSlot::Only, now).ok();
        }
        if search_only {
            let data = crate::presentation::search_envelope(
                resolver,
                &candidates,
                truncated,
                &attributions,
            );
            return Some(PluginToCore::RawResult(RawToolResult {
                id: query_id,
                data,
                error: None,
            }));
        }
        if candidates.is_empty() {
            return Some(failed(
                query_id,
                reply,
                error(
                    "location_not_found",
                    "未找到符合名称与限定的地点，请尝试更完整名称",
                ),
            ));
        }
        if candidates.len() == 1
            && crate::location::can_auto_resolve(&query, &candidates)
            && !truncated
        {
            let location = candidates.into_iter().next().unwrap();
            // 名称解析唯一确认后按本次请求的天气来源进入天气阶段。
            let mut requests: Vec<(String, Vec<(String, String)>, HttpSlot)> = Vec::new();
            let stage = match provider {
                Provider::OpenMeteo => {
                    requests.push((open_meteo::weather_url(&location), vec![], HttpSlot::Only));
                    Stage::WeatherOpenMeteo(location)
                }
                Provider::QWeather => {
                    let credentials = qweather.as_ref().expect("validated at start");
                    requests.push((
                        qweather::current_url(credentials, &location),
                        credentials.headers(),
                        HttpSlot::QCurrent,
                    ));
                    requests.push((
                        qweather::daily_url(credentials, &location),
                        credentials.headers(),
                        HttpSlot::QDaily,
                    ));
                    Stage::WeatherQWeather {
                        location,
                        current: None,
                        daily: None,
                    }
                }
                Provider::Caiyun => {
                    let credentials = caiyun.as_ref().expect("validated at start");
                    let signed = crate::location::coordinates::caiyun_request_coordinates(
                        &location,
                    )
                    .and_then(|coordinates| caiyun::weather_request(credentials, &coordinates));
                    match signed {
                        Ok(request) => {
                            requests.push((request.url, request.headers, HttpSlot::Only));
                            Stage::WeatherCaiyun(location)
                        }
                        Err(e) => return Some(failed(query_id, reply, e)),
                    }
                }
            };
            self.queries.insert(
                query_id.clone(),
                Pending {
                    stage,
                    inflight: requests.len(),
                    reply,
                    unit,
                    deadline,
                    provider,
                    qweather,
                    caiyun,
                },
            );
            return self
                .flush_http(&query_id, requests, now, reply)
                .into_iter()
                .next();
        }
        let (items, data) = crate::presentation::needs_location(&candidates, resolver);
        Some(match reply {
            ReplyKind::Query => PluginToCore::Response(PluginResponse {
                id: query_id,
                items,
                error: None,
            }),
            ReplyKind::Tool => PluginToCore::RawResult(RawToolResult {
                id: query_id,
                data,
                error: None,
            }),
        })
    }

    /// 和风聚合：两侧都 Ok 才算完整；一侧失败时另一侧可用数据仍可构成 partial，
    /// 两侧都失败则返回先到的错误。
    fn finish_qweather(&mut self, query_id: String, pending: Pending) -> PluginToCore {
        let Stage::WeatherQWeather {
            location,
            current,
            daily,
        } = pending.stage
        else {
            unreachable!("finish_qweather requires qweather stage");
        };
        let (current, daily) = match (current, daily) {
            (Some(current), Some(daily)) => (current, daily),
            _ => unreachable!("both slots settle before aggregation"),
        };
        let snapshot = match (current, daily) {
            (Ok(current), Ok(daily)) => {
                let mut warnings = Vec::new();
                if current.0.temperature_c.is_none()
                    && current.0.condition.is_none()
                    && current.0.wind_speed_kmh.is_none()
                {
                    warnings.push("和风实况字段不可用，仅提供预报".into());
                }
                if daily.0.is_empty() {
                    warnings.push("逐日预报不可用，仅提供实况".into());
                } else if daily.0.len() < 7 {
                    warnings.push(format!("未来预报可用 {} 天（请求 7 天）", daily.0.len()));
                }
                let mut attributions = current.1;
                for attribution in daily.1 {
                    if !attributions.contains(&attribution) {
                        attributions.push(attribution);
                    }
                }
                WeatherSnapshot {
                    location,
                    provider: "qweather",
                    fetched_at: chrono::Utc::now().to_rfc3339(),
                    current: Some(current.0),
                    daily: daily.0,
                    warnings,
                    attributions,
                }
            }
            (Err(e), _) | (_, Err(e)) => return failed(query_id, pending.reply, e),
        };
        eprintln!(
            "[weather] completed: provider=qweather status={} daily_days={}",
            snapshot.status(),
            snapshot.daily.len()
        );
        self.complete(query_id, pending.reply, &snapshot, pending.unit)
    }

    /// 彩云聚合：单请求覆盖实况+逐日；网络/超时先行，HTTP 错误按彩云错误表
    /// 细分（含重定向拒绝的 3xx），成功进统一快照（partial 语义在 provider 内）。
    fn finish_caiyun(
        &mut self,
        query_id: String,
        pending: Pending,
        status: u16,
        body: Option<&str>,
        network_error: Option<&str>,
    ) -> PluginToCore {
        let Stage::WeatherCaiyun(location) = pending.stage else {
            unreachable!("finish_caiyun requires caiyun stage");
        };
        let snapshot = if network_error.is_some() || status != 200 {
            if network_error.is_some_and(|e| e.starts_with("busy:")) {
                Err(error("busy", "宿主天气请求繁忙，请稍后重试"))
            } else if network_error.is_some_and(|e| e.starts_with("timeout:")) {
                Err(error("timeout", "天气服务请求超时，请稍后重试"))
            } else {
                Err(caiyun::error_from_response(status, body.unwrap_or("")))
            }
        } else {
            caiyun::parse_weather(body.unwrap_or(""), location)
        };
        match snapshot {
            Ok(snapshot) => {
                eprintln!(
                    "[weather] completed: provider=caiyun status={} daily_days={}",
                    snapshot.status(),
                    snapshot.daily.len()
                );
                self.complete(query_id, pending.reply, &snapshot, pending.unit)
            }
            Err(e) => failed(query_id, pending.reply, e),
        }
    }

    fn complete(
        &self,
        query_id: String,
        reply: ReplyKind,
        snapshot: &WeatherSnapshot,
        unit: TemperatureUnit,
    ) -> PluginToCore {
        let (items, data) = crate::presentation::render(snapshot, unit);
        match reply {
            ReplyKind::Query => PluginToCore::Response(PluginResponse {
                id: query_id,
                items,
                error: None,
            }),
            ReplyKind::Tool => PluginToCore::RawResult(RawToolResult {
                id: query_id,
                data,
                error: None,
            }),
        }
    }
}

/// 网络错误/状态码 → 统一错误分类（原始网络错误文本不透出）。
fn classify_http(
    status: u16,
    body: Option<&str>,
    network_error: Option<&str>,
) -> Result<String, PluginError> {
    if network_error.is_some() || status != 200 {
        if network_error.is_some_and(|e| e.starts_with("busy:")) {
            return Err(error("busy", "宿主天气请求繁忙，请稍后重试"));
        }
        if network_error.is_some_and(|e| e.starts_with("timeout:")) {
            return Err(error("timeout", "天气服务请求超时，请稍后重试"));
        }
        return Err(match status {
            401 | 403 => error("authentication_failed", "天气服务拒绝请求，请检查服务配置"),
            429 => error("rate_limited", "天气服务请求过于频繁，请稍后重试"),
            400 => error(
                "invalid_arguments",
                "天气服务无法处理该查询，请检查城市名称",
            ),
            _ => error("network_failed", "天气服务连接失败，请检查网络后重试"),
        });
    }
    body.map(str::to_owned)
        .ok_or_else(|| error("invalid_response", "天气服务没有返回数据"))
}

/// 和风子请求的错误分类：网络/超时先行，HTTP 错误交给供应商适配器按
/// problem+json type 细分；成功保留原始 body 供聚合时解析。
fn parse_qweather_current(
    status: u16,
    body: Option<&str>,
    network_error: Option<&str>,
) -> Result<(CurrentWeather, Vec<String>), PluginError> {
    if network_error.is_some() || status != 200 {
        if network_error.is_some_and(|e| e.starts_with("busy:")) {
            return Err(error("busy", "宿主天气请求繁忙，请稍后重试"));
        }
        if network_error.is_some_and(|e| e.starts_with("timeout:")) {
            return Err(error("timeout", "天气服务请求超时，请稍后重试"));
        }
        return Err(qweather::error_from_response(status, body.unwrap_or("")));
    }
    qweather::parse_current(body.ok_or_else(|| error("invalid_response", "和风天气没有返回数据"))?)
}

fn parse_qweather_daily(
    status: u16,
    body: Option<&str>,
    network_error: Option<&str>,
) -> Result<(Vec<DailyWeather>, Vec<String>), PluginError> {
    if network_error.is_some() || status != 200 {
        if network_error.is_some_and(|e| e.starts_with("busy:")) {
            return Err(error("busy", "宿主天气请求繁忙，请稍后重试"));
        }
        if network_error.is_some_and(|e| e.starts_with("timeout:")) {
            return Err(error("timeout", "天气服务请求超时，请稍后重试"));
        }
        return Err(qweather::error_from_response(status, body.unwrap_or("")));
    }
    qweather::parse_daily(body.ok_or_else(|| error("invalid_response", "和风天气没有返回数据"))?)
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

    const GEO_UNIQUE: &str = r#"{"results":[{"name":"广州","latitude":23.13,"longitude":113.26,"feature_code":"PPLA"}]}"#;

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
        assert!(service.queries.is_empty() && service.http.is_empty());
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
        assert!(request
            .headers
            .iter()
            .all(|(_, value)| !value.contains("test-app-secret")));
        // 原始地点仍是 WGS84 原值，未被派生坐标覆盖
        let pending = service.queries.get("cy").unwrap();
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
        assert!(raw.data[0]["copy_text"].as_str().unwrap().contains("当天主导现象"));

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
        assert!(service.queries.is_empty() && service.http.is_empty());
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
        assert!(request.url.starts_with("https://geocoding-api.open-meteo.com/"));
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
        assert!(request.url.starts_with("https://api.caiyunapp.com/v2.6/test-app-key/"));
        assert!(request.url.contains("/weather?dailysteps=7"));
        assert!(request
            .headers
            .iter()
            .any(|(name, _)| name == "x-cy-signature"));
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
        assert_eq!(service.queries.len(), 1, "两个子请求只占一条逻辑查询");
        assert_eq!(service.http.len(), 2);
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
        assert!(service.queries.contains_key("qw"));
        // 第二份（current）到达：聚合完成
        let result = service.handle(geo_response(ids[0].clone(), QW_CURRENT), now);
        let PluginToCore::RawResult(raw) = &result[0] else {
            panic!("expected aggregated result");
        };
        assert_eq!(raw.data[0]["provider"], "qweather");
        assert_eq!(raw.data[0]["temp"], "22°C");
        assert_eq!(raw.data[0]["condition"], "阴");
        assert_eq!(raw.data[0]["daily"][0]["condition"], "白天小雨/夜间多云");
        assert!(service.queries.is_empty());
        assert!(service.http.is_empty());
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
        assert!(service.queries.is_empty() && service.http.is_empty());
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
        assert!(service.queries.is_empty() && service.http.is_empty());
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
                    arguments: qweather_location(),
                    settings: Some(qweather_settings()),
                    budget_ms: Some(10_000),
                },
                now,
            );
            assert_eq!(requests.len(), 2);
        }
        assert_eq!(service.queries.len(), MAX_PENDING_QUERIES);
        let busy = service.handle(
            CoreToPlugin::ToolCall {
                id: "extra".into(),
                tool_name: "get_weather".into(),
                arguments: qweather_location(),
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
        assert!(service.queries.is_empty());
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
        assert!(service.queries.is_empty());
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
        assert!(service.queries.is_empty());
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
        assert!(service.queries.is_empty());
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
        assert!(service.queries.is_empty());
    }

    #[test]
    fn concurrent_requests_have_distinct_ids_and_cancel_does_not_cross_queries() {
        let mut service = WeatherService::default();
        let now = Instant::now();
        let a = service.handle(tool("a", None), now);
        let b = service.handle(tool("b", None), now);
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
            service.handle(tool(&i.to_string(), Some(10)), now);
        }
        let busy = service.handle(tool("extra", None), now);
        match &busy[0] {
            PluginToCore::RawResult(r) => assert_eq!(r.error.as_ref().unwrap().code, "busy"),
            _ => panic!("expected busy"),
        }
        let resumed = service.handle(tool("new", None), now + Duration::from_millis(11));
        assert!(matches!(resumed.last(), Some(PluginToCore::HttpRequest(_))));
        assert_eq!(service.queries.len(), 1);
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
        assert!(service.queries.is_empty());
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
        assert!(service.queries.is_empty());
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
            assert!(service.queries.is_empty());
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
        assert!(service.queries.is_empty());
    }
}
