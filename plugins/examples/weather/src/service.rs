//! Weather request coordination: bounded caches and shared producers with
//! independent caller budgets/cancellation. Provider IO stays in the pipeline.
use crate::cache::BoundedCache;
use crate::model::{ResolvedLocation, WeatherSnapshot};
use crate::protocol::*;
use crate::settings::{self, Provider, TemperatureUnit};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

#[path = "pipeline.rs"]
mod pipeline;
use pipeline::{Pipeline, ReplyKind};

const DEFAULT_BUDGET_MS: u64 = 10_000;
const MAX_BUDGET_MS: u64 = 30_000;
const MAX_PENDING_QUERIES: usize = 4;
const MAX_WAITERS_PER_FLIGHT: usize = 8;
const WEATHER_TTL: Duration = Duration::from_secs(5 * 60);
const LOCATION_TTL: Duration = Duration::from_secs(6 * 60 * 60);

// Neither key nor epoch implements Debug: credentials/location are never logged.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Epoch {
    revision: u64,
    digest: [u8; 32],
}
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    epoch: Epoch,
    digest: [u8; 32],
}

fn digest(value: &Value) -> [u8; 32] {
    Sha256::digest(serde_json::to_vec(value).expect("JSON values serialize")).into()
}

#[derive(Clone)]
struct LocationResult {
    resolver: &'static str,
    candidates: Vec<ResolvedLocation>,
    truncated: bool,
    attributions: Vec<String>,
    stored_at: Instant,
}

enum Outcome {
    Weather(WeatherSnapshot),
    Locations(LocationResult),
    NeedsLocation(LocationResult),
}

enum PipelineEvent {
    Http(HttpRequest),
    Complete {
        id: String,
        result: Result<Outcome, PluginError>,
    },
    LocationsReady {
        id: String,
        locations: LocationResult,
    },
    Batch(Vec<PipelineEvent>),
}

struct Waiter {
    id: String,
    reply: ReplyKind,
    unit: TemperatureUnit,
    deadline: Instant,
    location: Option<ResolvedLocation>,
}
struct Flight {
    key: Key,
    geo_key: Option<Key>,
    request_location: Option<ResolvedLocation>,
    hard_deadline: Instant,
    waiters: Vec<Waiter>,
}

pub struct WeatherService {
    pipeline: Pipeline,
    flights: HashMap<String, Flight>,
    by_key: HashMap<Key, Vec<String>>,
    weather: BoundedCache<Key, (WeatherSnapshot, Instant)>,
    locations: BoundedCache<Key, LocationResult>,
    epoch: Option<Epoch>,
    next_flight: u64,
}

impl Default for WeatherService {
    fn default() -> Self {
        Self {
            pipeline: Pipeline::default(),
            flights: HashMap::new(),
            by_key: HashMap::new(),
            weather: BoundedCache::new(64, WEATHER_TTL, 4 * 1024 * 1024),
            locations: BoundedCache::new(128, LOCATION_TTL, 1024 * 1024),
            epoch: None,
            next_flight: 0,
        }
    }
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
            data: Value::Null,
            error: Some(error),
        }),
    }
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenMeteo => "open_meteo",
        Provider::QWeather => "qweather",
        Provider::Caiyun => "caiyun",
    }
}
fn geo_key(epoch: &Epoch, resolver: &str, query: &crate::location::LocationQuery) -> Key {
    Key {
        epoch: epoch.clone(),
        digest: digest(&json!([
            "locations:zh",
            resolver,
            query.query,
            query.country_code,
            query.admin,
            query.limit
        ])),
    }
}
fn weather_key(epoch: &Epoch, provider: &str, location: &ResolvedLocation) -> Key {
    // Metadata is caller-specific. Only fields affecting weather/date/coordinates
    // enter the request key; +/-zero describe the same geographic point.
    let lat = if location.latitude == 0.0 {
        0.0
    } else {
        location.latitude
    };
    let lon = if location.longitude == 0.0 {
        0.0
    } else {
        location.longitude
    };
    Key {
        epoch: epoch.clone(),
        digest: digest(&json!([
            "weather:current+7:zh",
            provider,
            lat,
            lon,
            location.coordinate_system,
            location.in_mainland_china,
            location.timezone
        ])),
    }
}

impl WeatherService {
    fn observe_settings(&mut self, settings: Option<&Value>) -> Result<Epoch, PluginError> {
        let revision = match settings.and_then(|s| s.get("settings_revision")) {
            None => 0,
            Some(value) => value
                .as_u64()
                .ok_or_else(|| error("invalid_arguments", "配置版本无效"))?,
        };
        let epoch = Epoch {
            revision,
            digest: digest(settings.unwrap_or(&Value::Null)),
        };
        // Old snapshots may arrive after a newer one. They remain isolated and
        // cannot roll back the active epoch or refill its caches.
        if self
            .epoch
            .as_ref()
            .is_none_or(|active| revision >= active.revision && active != &epoch)
        {
            self.weather.clear();
            self.locations.clear();
            self.epoch = Some(epoch.clone());
            eprintln!("[weather] cache configuration changed: revision={revision}");
        }
        Ok(epoch)
    }

    pub fn handle(&mut self, message: CoreToPlugin, now: Instant) -> Vec<PluginToCore> {
        // A late caller cancellation is silent even if its deadline has elapsed.
        if let CoreToPlugin::Cancel { id } = message {
            self.remove_waiter(&id);
            return self.poll(now);
        }
        let mut output = self.poll(now);
        let events = match message {
            CoreToPlugin::Query {
                id,
                query,
                settings,
                budget_ms,
            } => self.start(
                id,
                ReplyKind::Query,
                Some(&query),
                None,
                settings.as_ref(),
                budget_ms,
                now,
            ),
            CoreToPlugin::ToolCall {
                id,
                tool_name,
                arguments,
                settings,
                budget_ms,
            } => {
                if tool_name == "get_weather" {
                    self.start(
                        id,
                        ReplyKind::Tool,
                        None,
                        Some(&arguments),
                        settings.as_ref(),
                        budget_ms,
                        now,
                    )
                } else if tool_name == "search_locations" {
                    self.start_search(id, &arguments, settings.as_ref(), budget_ms, now)
                } else {
                    vec![failed(
                        id,
                        ReplyKind::Tool,
                        error("invalid_arguments", "未知天气工具"),
                    )]
                }
            }
            CoreToPlugin::HttpResponse {
                id,
                status,
                body,
                error,
            } => {
                let events = self
                    .pipeline
                    .http_response(&id, status, body.as_deref(), error.as_deref(), now)
                    .into_iter()
                    .collect();
                self.dispatch(events, now)
            }
            CoreToPlugin::Cancel { .. } => unreachable!(),
        };
        output.extend(events);
        output
    }

    pub fn poll(&mut self, now: Instant) -> Vec<PluginToCore> {
        let mut output = vec![];
        let ids: Vec<_> = self.flights.keys().cloned().collect();
        for id in ids {
            let flight = self.flights.get_mut(&id).unwrap();
            let mut expired = vec![];
            flight.waiters.retain(|w| {
                if now >= w.deadline || now >= flight.hard_deadline {
                    expired.push((w.id.clone(), w.reply));
                    false
                } else {
                    true
                }
            });
            output.extend(expired.into_iter().map(|(id, reply)| {
                failed(id, reply, error("timeout", "天气查询超时，请稍后重试"))
            }));
            if flight.waiters.is_empty() {
                self.remove_flight(&id);
            } else {
                self.refresh_deadline(&id);
            }
        }
        output
    }

    fn remove_flight(&mut self, id: &str) -> Option<Flight> {
        let flight = self.flights.remove(id)?;
        if let Some(ids) = self.by_key.get_mut(&flight.key) {
            ids.retain(|owner| owner != id);
            if ids.is_empty() {
                self.by_key.remove(&flight.key);
            }
        }
        self.pipeline.cancel(id);
        Some(flight)
    }
    fn remove_waiter(&mut self, caller: &str) {
        let owner = self
            .flights
            .iter()
            .find(|(_, f)| f.waiters.iter().any(|w| w.id == caller))
            .map(|(id, _)| id.clone());
        if let Some(owner) = owner {
            let flight = self.flights.get_mut(&owner).unwrap();
            flight.waiters.retain(|w| w.id != caller);
            if flight.waiters.is_empty() {
                self.remove_flight(&owner);
            } else {
                self.refresh_deadline(&owner);
            }
        }
    }
    fn refresh_deadline(&mut self, id: &str) {
        let flight = &self.flights[id];
        if let Some(pending) = self.pipeline.queries.get_mut(id) {
            pending.deadline = flight
                .waiters
                .iter()
                .map(|w| w.deadline)
                .max()
                .unwrap()
                .min(flight.hard_deadline);
        }
    }
    fn waiter(
        &self,
        id: String,
        reply: ReplyKind,
        unit: TemperatureUnit,
        location: Option<ResolvedLocation>,
        budget_ms: Option<u64>,
        now: Instant,
    ) -> Result<Waiter, PluginError> {
        if self
            .flights
            .values()
            .any(|f| f.waiters.iter().any(|w| w.id == id))
        {
            return Err(error("busy", "请求 ID 已在使用"));
        }
        let budget = budget_ms.unwrap_or(DEFAULT_BUDGET_MS).min(MAX_BUDGET_MS);
        if budget == 0 {
            return Err(error("timeout", "天气查询时间预算已用尽"));
        }
        Ok(Waiter {
            id,
            reply,
            unit,
            location,
            deadline: now + Duration::from_millis(budget),
        })
    }
    // Return the producer ID only for a new flight. Followers keep their own
    // deadline/format and never emit a second upstream request.
    fn admit(
        &mut self,
        key: Key,
        geo_key: Option<Key>,
        waiter: Waiter,
        _now: Instant,
    ) -> Result<Option<String>, (Waiter, PluginError)> {
        // An already issued HTTP timeout cannot be extended. Only join a flight
        // whose transport budget covers this caller; a longer budget gets an
        // independent producer instead of inheriting an early timeout.
        let compatible = self
            .by_key
            .get(&key)
            .and_then(|ids| {
                ids.iter()
                    .filter(|id| self.flights[*id].hard_deadline >= waiter.deadline)
                    .min_by_key(|id| self.flights[*id].waiters.len())
            })
            .cloned();
        if let Some(id) = compatible {
            let flight = self.flights.get_mut(&id).unwrap();
            if flight.waiters.len() >= MAX_WAITERS_PER_FLIGHT {
                return Err((waiter, error("busy", "同一天气查询等待者已满")));
            }
            flight.waiters.push(waiter);
            self.refresh_deadline(&id);
            eprintln!("[weather] request joined existing flight");
            return Ok(None);
        }
        if self.flights.len() >= MAX_PENDING_QUERIES {
            return Err((waiter, error("busy", "天气查询繁忙，请稍后重试")));
        }
        self.next_flight += 1;
        let id = format!("weather_flight_{}", self.next_flight);
        self.by_key.entry(key.clone()).or_default().push(id.clone());
        let hard_deadline = waiter.deadline;
        let request_location = waiter.location.clone();
        self.flights.insert(
            id.clone(),
            Flight {
                key,
                geo_key,
                request_location,
                hard_deadline,
                waiters: vec![waiter],
            },
        );
        Ok(Some(id))
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        id: String,
        reply: ReplyKind,
        query: Option<&str>,
        args: Option<&Value>,
        settings: Option<&Value>,
        budget_ms: Option<u64>,
        now: Instant,
    ) -> Vec<PluginToCore> {
        let prepared = (|| {
            let epoch = self.observe_settings(settings)?;
            let mut parameters = settings::parse(query, args, settings)?;
            let mut location_key = None;
            let mut ambiguity = None;
            if let Some(query) = parameters.location_query.as_ref() {
                let resolver = if parameters.qweather.is_some() {
                    "qweather"
                } else {
                    "open_meteo"
                };
                let key = geo_key(&epoch, resolver, query);
                if let Some(locations) = self.locations.get(&key, now) {
                    if !locations.truncated
                        && crate::location::can_auto_resolve(query, &locations.candidates)
                    {
                        parameters.location = locations.candidates.into_iter().next();
                        parameters.location_query = None;
                    } else {
                        ambiguity = Some(locations);
                    }
                }
                location_key = Some(key);
            }
            let waiter = self.waiter(
                id.clone(),
                reply,
                parameters.unit,
                parameters.location.clone(),
                budget_ms,
                now,
            )?;
            Ok::<_, PluginError>((epoch, parameters, location_key, waiter, ambiguity))
        })();
        let (epoch, parameters, location_key, waiter, ambiguity) = match prepared {
            Ok(p) => p,
            Err(e) => return vec![failed(id, reply, e)],
        };
        if let Some(locations) = ambiguity {
            let age_ms = now
                .saturating_duration_since(locations.stored_at)
                .as_millis() as u64;
            return vec![render(
                &Outcome::NeedsLocation(locations),
                &waiter,
                true,
                age_ms,
            )];
        }
        let key = if let Some(location) = parameters.location.as_ref() {
            weather_key(&epoch, provider_name(parameters.provider), location)
        } else {
            Key {
                epoch: epoch.clone(),
                digest: digest(&json!([
                    "weather:name",
                    provider_name(parameters.provider),
                    location_key.as_ref().unwrap().digest
                ])),
            }
        };
        if let Some((snapshot, stored)) = self.weather.get(&key, now) {
            eprintln!(
                "[weather] weather cache hit: provider={}",
                snapshot.provider
            );
            return vec![render(
                &Outcome::Weather(snapshot),
                &waiter,
                true,
                now.saturating_duration_since(stored).as_millis() as u64,
            )];
        }
        let producer = match self.admit(key, location_key, waiter, now) {
            Ok(Some(id)) => id,
            Ok(None) => return vec![],
            Err((w, e)) => return vec![failed(w.id, w.reply, e)],
        };
        let events = self
            .pipeline
            .start(producer, reply, Ok(parameters), budget_ms, now);
        self.dispatch(events, now)
    }

    fn start_search(
        &mut self,
        id: String,
        arguments: &Value,
        settings: Option<&Value>,
        budget_ms: Option<u64>,
        now: Instant,
    ) -> Vec<PluginToCore> {
        let prepared = (|| {
            let epoch = self.observe_settings(settings)?;
            let search = settings::parse_search(arguments)?;
            let resolver = Pipeline::resolve_resolver(search.resolver, settings)?;
            let waiter = self.waiter(
                id.clone(),
                ReplyKind::Tool,
                TemperatureUnit::Celsius,
                None,
                budget_ms,
                now,
            )?;
            Ok::<_, PluginError>((epoch, search.query, resolver, waiter))
        })();
        let (epoch, query, resolver, waiter) = match prepared {
            Ok(p) => p,
            Err(e) => return vec![failed(id, ReplyKind::Tool, e)],
        };
        let key = geo_key(&epoch, resolver, &query);
        if let Some(locations) = self.locations.get(&key, now) {
            eprintln!("[weather] location cache hit: resolver={resolver}");
            let age_ms = now
                .saturating_duration_since(locations.stored_at)
                .as_millis() as u64;
            return vec![render(
                &Outcome::Locations(locations),
                &waiter,
                true,
                age_ms,
            )];
        }
        let producer = match self.admit(key.clone(), Some(key), waiter, now) {
            Ok(Some(id)) => id,
            Ok(None) => return vec![],
            Err((w, e)) => return vec![failed(w.id, w.reply, e)],
        };
        let events = self
            .pipeline
            .start_search(producer, query, resolver, settings, budget_ms, now);
        self.dispatch(events, now)
    }

    fn dispatch(&mut self, events: Vec<PipelineEvent>, now: Instant) -> Vec<PluginToCore> {
        let mut queue = VecDeque::from(events);
        let mut output = vec![];
        while let Some(event) = queue.pop_front() {
            match event {
                PipelineEvent::Batch(events) => {
                    for e in events.into_iter().rev() {
                        queue.push_front(e);
                    }
                }
                PipelineEvent::Http(request) => output.push(PluginToCore::HttpRequest(request)),
                PipelineEvent::LocationsReady { id, locations } => {
                    if let Some(flight) = self.flights.get_mut(&id) {
                        if locations.candidates.len() == 1 && !locations.truncated {
                            flight.request_location = locations.candidates.first().cloned();
                        }
                        if self.epoch.as_ref() == Some(&flight.key.epoch)
                            && !locations.candidates.is_empty()
                        {
                            if let Some(key) = &flight.geo_key {
                                let weight = serde_json::to_vec(&locations.candidates)
                                    .map_or(usize::MAX, |v| v.len())
                                    .saturating_add(
                                        locations
                                            .attributions
                                            .iter()
                                            .map(String::len)
                                            .sum::<usize>(),
                                    );
                                self.locations.insert(key.clone(), locations, weight, now);
                            }
                        }
                    }
                }
                PipelineEvent::Complete { id, result } => {
                    let Some(flight) = self.remove_flight(&id) else {
                        continue;
                    };
                    if self.epoch.as_ref() == Some(&flight.key.epoch) {
                        if let Ok(Outcome::Weather(snapshot)) = &result {
                            // Never cache partial/errors: a broken credential/permission
                            // must remain visible on the next request.
                            if snapshot.status() == "ok"
                                && snapshot.current.is_some()
                                && !snapshot.daily.is_empty()
                            {
                                let key = weather_key(
                                    &flight.key.epoch,
                                    snapshot.provider,
                                    flight
                                        .request_location
                                        .as_ref()
                                        .unwrap_or(&snapshot.location),
                                );
                                let weight =
                                    serde_json::to_vec(snapshot).map_or(usize::MAX, |v| v.len());
                                self.weather
                                    .insert(key, (snapshot.clone(), now), weight, now);
                            }
                        }
                    }
                    for waiter in flight.waiters {
                        output.push(match &result {
                            Ok(outcome) => render(outcome, &waiter, false, 0),
                            Err(e) => failed(waiter.id, waiter.reply, e.clone()),
                        });
                    }
                }
            }
        }
        output
    }
}

fn render(outcome: &Outcome, waiter: &Waiter, hit: bool, age_ms: u64) -> PluginToCore {
    let (mut items, mut data) = match outcome {
        Outcome::Weather(snapshot) => {
            let mut snapshot = snapshot.clone();
            if let Some(location) = &waiter.location {
                let timezone = snapshot.location.timezone.clone();
                snapshot.location = location.clone();
                if timezone.is_some() {
                    snapshot.location.timezone = timezone;
                }
            }
            crate::presentation::render(&snapshot, waiter.unit)
        }
        Outcome::NeedsLocation(locations) => {
            crate::presentation::needs_location(&locations.candidates, locations.resolver)
        }
        Outcome::Locations(locations) => (
            vec![],
            crate::presentation::search_envelope(
                locations.resolver,
                &locations.candidates,
                locations.truncated,
                &locations.attributions,
            ),
        ),
    };
    if let Some(value) = data.get_mut(0).and_then(Value::as_object_mut) {
        value.insert("cache".into(), json!({"hit":hit,"age_ms":age_ms}));
    }
    for item in &mut items {
        if let Some(value) = item.payload.as_mut().and_then(Value::as_object_mut) {
            value.insert("cache".into(), json!({"hit":hit,"age_ms":age_ms}));
        }
    }
    match waiter.reply {
        ReplyKind::Query => PluginToCore::Response(PluginResponse {
            id: waiter.id.clone(),
            items,
            error: None,
        }),
        ReplyKind::Tool => PluginToCore::RawResult(RawToolResult {
            id: waiter.id.clone(),
            data,
            error: None,
        }),
    }
}

pub fn run() {
    // One bounded reader queue lets the coordinator expire callers even when no
    // further JSONL arrives. The reader owns stdin; all state stays on one thread.
    let (tx, rx) = std::sync::mpsc::sync_channel(16);
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line) {
                Ok(message) => {
                    if tx.send(message).is_err() {
                        break;
                    }
                }
                Err(_) => eprintln!("[weather][warn] invalid JSONL message"),
            }
        }
    });
    let mut stdout = io::stdout().lock();
    let mut service = WeatherService::default();
    loop {
        let messages = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(message) => service.handle(message, Instant::now()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => service.poll(Instant::now()),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        for response in messages {
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
#[path = "coordination_tests.rs"]
mod coordination_tests;
#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
