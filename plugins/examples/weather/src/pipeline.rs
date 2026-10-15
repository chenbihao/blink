//! 单线程查询状态机；输入/HTTP/取消按请求 ID 隔离。
//!
//! 0.26.2 起区分"逻辑查询"与"HTTP 子请求"：一条用户查询（queries，按 query_id）
//! 可同时挂多个在途 HTTP 子请求（http，按 http_id → (query_id, 槽位) 关联）——
//! 和风实况与逐日并行共用同一条查询的总预算，两个子请求都完成前不移除该查询。
//! 查询容量（4 槽）按逻辑查询计，不按 HTTP 计。
use super::{LocationResult, Outcome, PipelineEvent};
use crate::model::{CurrentWeather, DailyWeather, ResolvedLocation, WeatherSnapshot};
use crate::protocol::{HttpRequest, PluginError};
use crate::providers::{caiyun, open_meteo, qweather};
use crate::settings::{self, Provider, RequestParameters, Resolver, TemperatureUnit};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const DEFAULT_BUDGET_MS: u64 = 10_000;
const MAX_BUDGET_MS: u64 = 30_000;
const MAX_PENDING_QUERIES: usize = 4;

#[derive(Clone, Copy)]
pub(super) enum ReplyKind {
    Query,
    Tool,
}

/// 并行子请求的槽位；单请求阶段固定 Only。
#[derive(Clone, Copy)]
pub(super) enum HttpSlot {
    Only,
    QCurrent,
    QDaily,
}

pub(super) enum Stage {
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

pub(super) struct Pending {
    reply: ReplyKind,
    unit: TemperatureUnit,
    pub(super) deadline: Instant,
    provider: Provider,
    qweather: Option<settings::QWeatherCredentials>,
    caiyun: Option<settings::CaiyunCredentials>,
    pub(super) stage: Stage,
    inflight: usize,
}

#[derive(Default)]
pub(super) struct Pipeline {
    pub(super) queries: HashMap<String, Pending>,
    pub(super) http: HashMap<String, (String, HttpSlot)>,
    next_http_id: u64,
}

fn error(code: &str, message: &str) -> PluginError {
    PluginError {
        code: code.into(),
        message: message.into(),
    }
}

fn failed(id: String, _reply: ReplyKind, error: PluginError) -> PipelineEvent {
    // Intermediate provider failures may be handled by explicit fallback; the
    // service logs a warning only when the caller ultimately receives an error.
    eprintln!("[weather] request failed: code={}", error.code);
    PipelineEvent::Complete {
        id,
        result: Err(error),
    }
}

fn remaining_ms(deadline: Instant, now: Instant) -> u64 {
    deadline
        .saturating_duration_since(now)
        .as_millis()
        .min(u64::MAX as u128) as u64
}

impl Pipeline {
    pub(super) fn cancel(&mut self, id: &str) {
        self.queries.remove(id);
        self.http.retain(|_, (owner, _)| owner != id);
    }

    /// resolver 参数 + settings → 实际解析来源。显式和风缺配置明确报错，
    /// 不静默换源；auto 只在和风已配置时优先和风。
    pub(super) fn resolve_resolver(
        requested: Resolver,
        settings: Option<&serde_json::Value>,
    ) -> Result<&'static str, PluginError> {
        if requested == Resolver::OpenMeteo {
            return Ok("open_meteo");
        }
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

    pub(super) fn start(
        &mut self,
        id: String,
        reply: ReplyKind,
        parameters: Result<RequestParameters, PluginError>,
        budget_ms: Option<u64>,
        now: Instant,
    ) -> Vec<PipelineEvent> {
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
                    let signed =
                        crate::location::coordinates::caiyun_request_coordinates(&location)
                            .and_then(|coordinates| {
                                caiyun::weather_request(credentials, &coordinates)
                            });
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

    pub(super) fn start_search(
        &mut self,
        id: String,
        query: crate::location::LocationQuery,
        resolver: &'static str,
        settings: Option<&serde_json::Value>,
        budget_ms: Option<u64>,
        now: Instant,
    ) -> Vec<PipelineEvent> {
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
    ) -> Vec<PipelineEvent> {
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
    ) -> Result<PipelineEvent, PluginError> {
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
        Ok(PipelineEvent::Http(HttpRequest {
            id: http_id,
            method: "GET".into(),
            url,
            body: None,
            timeout_ms,
            headers,
        }))
    }

    pub(super) fn http_response(
        &mut self,
        http_id: &str,
        status: u16,
        body: Option<&str>,
        network_error: Option<&str>,
        now: Instant,
    ) -> Option<PipelineEvent> {
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
    ) -> Option<PipelineEvent> {
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
        let locations = LocationResult {
            resolver,
            candidates: candidates.clone(),
            truncated,
            attributions,
            stored_at: now,
        };
        let ready = PipelineEvent::LocationsReady {
            id: query_id.clone(),
            locations: locations.clone(),
        };
        if search_only {
            return Some(PipelineEvent::Batch(vec![
                ready,
                PipelineEvent::Complete {
                    id: query_id,
                    result: Ok(Outcome::Locations(locations)),
                },
            ]));
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
                    let signed =
                        crate::location::coordinates::caiyun_request_coordinates(&location)
                            .and_then(|coordinates| {
                                caiyun::weather_request(credentials, &coordinates)
                            });
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
            let mut events = vec![ready];
            events.extend(self.flush_http(&query_id, requests, now, reply));
            return Some(PipelineEvent::Batch(events));
        }
        Some(PipelineEvent::Batch(vec![
            ready,
            PipelineEvent::Complete {
                id: query_id,
                result: Ok(Outcome::NeedsLocation(locations)),
            },
        ]))
    }

    /// 和风聚合：两侧都 Ok 才算完整；一侧失败时另一侧可用数据仍可构成 partial，
    /// 两侧都失败则返回先到的错误。
    fn finish_qweather(&mut self, query_id: String, pending: Pending) -> PipelineEvent {
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
                let current_usable = current.0.temperature_c.is_some()
                    || current.0.condition.is_some()
                    || current.0.wind_speed_kmh.is_some();
                if !current_usable {
                    warnings.push("和风实况字段不可用，仅提供预报".into());
                } else if current.0.temperature_c.is_none()
                    || current.0.condition.is_none()
                    || current.0.wind_speed_kmh.is_none()
                {
                    warnings.push("和风实况部分字段不可用".into());
                }
                if daily.0.is_empty() {
                    warnings.push("逐日预报不可用，仅提供实况".into());
                } else if daily.0.len() < 7 {
                    warnings.push(format!("未来预报可用 {} 天（请求 7 天）", daily.0.len()));
                }
                if daily.0.iter().any(|day| {
                    day.temperature_max_c.is_none()
                        || day.temperature_min_c.is_none()
                        || day.wind_max_kmh.is_none()
                        || day.precipitation_probability_percent.is_none()
                        || day.day_condition.is_none()
                        || day.night_condition.is_none()
                }) {
                    warnings.push("和风逐日预报部分字段不可用".into());
                }
                if !current_usable && daily.0.is_empty() {
                    return failed(
                        query_id,
                        pending.reply,
                        error("invalid_response", "和风实况与预报均无可用数据"),
                    );
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
                    current: current_usable.then_some(current.0),
                    daily: daily.0,
                    warnings,
                    attributions,
                }
            }
            (Ok((current, attributions)), Err(e))
                if current.temperature_c.is_some()
                    || current.condition.is_some()
                    || current.wind_speed_kmh.is_some() =>
            {
                WeatherSnapshot {
                    location,
                    provider: "qweather",
                    fetched_at: chrono::Utc::now().to_rfc3339(),
                    current: Some(current),
                    daily: vec![],
                    attributions,
                    warnings: vec![format!("逐日预报不可用（{}）：{}", e.code, e.message)],
                }
            }
            (Err(e), Ok((daily, attributions))) if !daily.is_empty() => WeatherSnapshot {
                location,
                provider: "qweather",
                fetched_at: chrono::Utc::now().to_rfc3339(),
                current: None,
                daily,
                attributions,
                warnings: vec![format!("实况不可用，仅预报（{}）：{}", e.code, e.message)],
            },
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
    ) -> PipelineEvent {
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
        _reply: ReplyKind,
        snapshot: &WeatherSnapshot,
        _unit: TemperatureUnit,
    ) -> PipelineEvent {
        PipelineEvent::Complete {
            id: query_id,
            result: Ok(Outcome::Weather(snapshot.clone())),
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
