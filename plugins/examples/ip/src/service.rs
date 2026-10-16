//! 单线程状态机管理请求；stdin reader 仅搬运消息，超时独立于 HTTP 回包推进。
use crate::{model::*, presentation, protocol::*, providers::Provider, settings::Settings};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

const CACHE_TTL: Duration = Duration::from_secs(60);
const MAX_JOBS: usize = 4;
const TOTAL_MS: u64 = 6500;
const ATTEMPT_MS: u64 = 1600;

struct Chain {
    providers: Vec<Provider>,
    next: usize,
    done: bool,
    failures: Vec<String>,
}

struct Job {
    generation: u64,
    tool: bool,
    settings: Settings,
    network: NetworkSnapshot,
    deadline: Instant,
    chains: HashMap<Group, Chain>,
    observations: Vec<(Group, Observation)>,
    warnings: Vec<String>,
}

struct Cached {
    settings: Settings,
    network: NetworkSnapshot,
    observation: Observation,
    stored: Instant,
}

#[derive(Default)]
pub struct Engine {
    generation: u64,
    current: Option<(Settings, NetworkSnapshot)>,
    jobs: HashMap<String, Job>,
    http: HashMap<String, (String, Group, Provider)>,
    cache: HashMap<Group, Cached>,
    next_request: HashMap<Provider, Instant>,
    seq: u64,
}

impl Engine {
    pub fn handle(
        &mut self,
        message: CoreToPlugin,
        now: Instant,
        network: Option<NetworkSnapshot>,
    ) -> Vec<PluginToCore> {
        match message {
            CoreToPlugin::Query {
                id,
                query,
                settings,
                budget_ms,
            } => {
                let args = query.trim().to_lowercase();
                if !matches!(args.as_str(), "" | "refresh" | "刷新" | "local" | "本地") {
                    return vec![error(
                        id,
                        false,
                        "invalid_arguments",
                        "输入 ip 查询，ip local 只看本机，ip refresh 刷新出口",
                    )];
                }
                let mut settings = Settings::parse(&settings.unwrap_or(Value::Null), &Value::Null);
                if matches!(args.as_str(), "local" | "本地") {
                    settings.groups.clear();
                }
                self.start(
                    id,
                    false,
                    settings,
                    network.unwrap_or_default(),
                    budget_ms,
                    matches!(args.as_str(), "refresh" | "刷新"),
                    now,
                )
            }
            CoreToPlugin::ToolCall {
                id,
                tool_name,
                arguments,
                settings,
                budget_ms,
            } => {
                if tool_name != "get_ip" {
                    return vec![error(id, true, "unknown_tool", "不支持的 IP 工具")];
                }
                let settings = Settings::parse(&settings.unwrap_or(Value::Null), &arguments);
                self.start(
                    id,
                    true,
                    settings,
                    network.unwrap_or_default(),
                    budget_ms,
                    arguments["refresh"].as_bool().unwrap_or(false),
                    now,
                )
            }
            CoreToPlugin::Cancel { id } => {
                self.jobs.remove(&id);
                self.http.retain(|_, (owner, _, _)| owner != &id);
                Vec::new()
            }
            CoreToPlugin::HttpResponse {
                id,
                status,
                body,
                error,
            } => {
                let Some((owner, group, provider)) = self.http.remove(&id) else {
                    return Vec::new();
                };
                let Some(job) = self.jobs.get_mut(&owner) else {
                    return Vec::new();
                };
                if now >= job.deadline {
                    return self.expire(now);
                }
                if status == 429 && error.is_none() {
                    self.next_request
                        .insert(provider, now + Duration::from_secs(60));
                }
                let parsed = if error.is_some() {
                    Err("请求失败或超时")
                } else if status == 429 {
                    Err("服务限流，请稍后重试")
                } else if status != 200 {
                    Err("服务暂不可用")
                } else {
                    provider.parse(body.as_deref().unwrap_or(""))
                };
                match parsed {
                    Ok(mut observation) => {
                        let chain = job.chains.get_mut(&group).unwrap();
                        observation.fallback = chain.next > 1;
                        chain.done = true;
                        if job.generation == self.generation {
                            self.cache.insert(
                                group,
                                Cached {
                                    settings: job.settings.clone(),
                                    network: job.network.clone(),
                                    observation: observation.clone(),
                                    stored: now,
                                },
                            );
                        }
                        job.observations.push((group, observation));
                        eprintln!(
                            "[ip] detection succeeded scope={group:?} source={}",
                            provider.id()
                        );
                    }
                    Err(reason) => {
                        let chain = job.chains.get_mut(&group).unwrap();
                        chain
                            .failures
                            .push(format!("{}：{reason}", provider.name()));
                        eprintln!(
                            "[ip] source unavailable scope={group:?} source={}",
                            provider.id()
                        );
                    }
                }
                self.advance(&owner, group, now)
            }
        }
    }

    fn start(
        &mut self,
        id: String,
        tool: bool,
        settings: Settings,
        network: NetworkSnapshot,
        budget: Option<u64>,
        refresh: bool,
        now: Instant,
    ) -> Vec<PluginToCore> {
        if self.jobs.len() >= MAX_JOBS || self.jobs.contains_key(&id) {
            return vec![error(id, tool, "busy", "IP 查询繁忙，请稍后重试")];
        }
        // A local-only lookup does not change the active detector configuration.
        // Keep successful egress results available when switching back during cooldown.
        if settings.groups.is_empty() {
            return vec![presentation::response(
                id,
                tool,
                &settings,
                &network,
                &[],
                &[],
            )];
        }
        if !self
            .current
            .as_ref()
            .is_some_and(|(old_settings, old_network)| {
                old_settings.same_detection(&settings) && old_network == &network
            })
        {
            self.generation += 1;
            self.current = Some((settings.clone(), network.clone()));
            self.cache.clear();
        }
        let budget = budget.unwrap_or(TOTAL_MS + 200).min(TOTAL_MS + 200);
        // Leave host enough time for the last JSONL frame and result projection.
        let deadline = now + Duration::from_millis(budget.saturating_sub(200));
        let mut job = Job {
            generation: self.generation,
            tool,
            settings: settings.clone(),
            network: network.clone(),
            deadline,
            chains: HashMap::new(),
            observations: Vec::new(),
            warnings: Vec::new(),
        };
        for group in settings.detection_groups() {
            let cached = self.cache.get(&group).filter(|entry| {
                entry.settings.same_detection(&settings)
                    && entry.network == network
                    && now.saturating_duration_since(entry.stored) < CACHE_TTL
            });
            if let Some(entry) = cached {
                let cooling = self.cooling(entry.observation.provider, now);
                if !refresh || cooling {
                    let mut observation = entry.observation.clone();
                    observation.cached = true;
                    job.observations.push((group, observation));
                    if refresh && cooling {
                        job.warnings.push(format!(
                            "{}刷新过快，保留上次结果（{} 秒后可重试）",
                            group.label(),
                            self.cooldown_seconds(entry.observation.provider, now)
                        ));
                    }
                    continue;
                }
            }
            job.chains.insert(
                group,
                Chain {
                    providers: settings.providers(group),
                    next: 0,
                    done: false,
                    failures: Vec::new(),
                },
            );
        }
        self.jobs.insert(id.clone(), job);
        let mut output = Vec::new();
        for group in settings.detection_groups() {
            output.extend(self.advance(&id, group, now));
        }
        output.extend(self.finish_if_ready(&id));
        output
    }

    fn cooling(&self, provider: Provider, now: Instant) -> bool {
        self.next_request
            .get(&provider)
            .is_some_and(|next| now < *next)
    }

    fn cooldown_seconds(&self, provider: Provider, now: Instant) -> u64 {
        self.next_request.get(&provider).map_or(0, |next| {
            next.saturating_duration_since(now)
                .as_millis()
                .div_ceil(1000) as u64
        })
    }

    fn advance(&mut self, id: &str, group: Group, now: Instant) -> Vec<PluginToCore> {
        loop {
            let Some(job) = self.jobs.get_mut(id) else {
                return Vec::new();
            };
            let Some(chain) = job.chains.get_mut(&group) else {
                return self.finish_if_ready(id);
            };
            if chain.done {
                return self.finish_if_ready(id);
            }
            let remaining = job.deadline.saturating_duration_since(now).as_millis() as u64;
            if remaining < 100 || chain.next == chain.providers.len() {
                chain.done = true;
                let reason = if remaining < 100 {
                    "查询时间已用尽".into()
                } else {
                    chain.failures.join("；")
                };
                job.warnings
                    .push(format!("{}检测未完成：{reason}", group.label()));
                return self.finish_if_ready(id);
            }
            let provider = chain.providers[chain.next];
            chain.next += 1;
            if self.cooling(provider, now) {
                let seconds = self.cooldown_seconds(provider, now);
                self.jobs
                    .get_mut(id)
                    .unwrap()
                    .chains
                    .get_mut(&group)
                    .unwrap()
                    .failures
                    .push(format!(
                        "{}：刷新间隔内（{seconds} 秒后可重试，本次未请求）",
                        provider.name()
                    ));
                continue;
            }
            self.next_request.insert(
                provider,
                now + Duration::from_millis(provider.min_interval_ms()),
            );
            self.seq += 1;
            let request_id = format!("ip_http_{}", self.seq);
            self.http
                .insert(request_id.clone(), (id.into(), group, provider));
            eprintln!(
                "[ip] detection started scope={group:?} source={}",
                provider.id()
            );
            return vec![PluginToCore::HttpRequest {
                id: request_id,
                method: "GET",
                url: provider.url(),
                timeout_ms: ATTEMPT_MS.min(remaining.saturating_sub(50)),
                headers: vec![
                    ("User-Agent".into(), "Blink/0.26.6".into()),
                    (
                        "Accept".into(),
                        "application/json,text/plain;q=0.9,text/html;q=0.5".into(),
                    ),
                ],
            }];
        }
    }

    fn finish_if_ready(&mut self, id: &str) -> Vec<PluginToCore> {
        if !self
            .jobs
            .get(id)
            .is_some_and(|job| job.chains.values().all(|chain| chain.done))
        {
            return Vec::new();
        }
        let job = self.jobs.remove(id).unwrap();
        self.http.retain(|_, (owner, _, _)| owner != id);
        let mut observations = job.observations;
        observations.sort_by_key(|(g, _)| match g {
            Group::Domestic => 0,
            Group::International => 1,
            Group::Ipv6 => 2,
        });
        vec![presentation::response(
            id.into(),
            job.tool,
            &job.settings,
            &job.network,
            &observations,
            &job.warnings,
        )]
    }

    pub fn expire(&mut self, now: Instant) -> Vec<PluginToCore> {
        let expired: Vec<_> = self
            .jobs
            .iter()
            .filter(|(_, job)| now >= job.deadline)
            .map(|(id, _)| id.clone())
            .collect();
        let mut output = Vec::new();
        for id in expired {
            let job = self.jobs.get_mut(&id).unwrap();
            for (&group, chain) in &mut job.chains {
                if !chain.done {
                    job.warnings
                        .push(format!("{}检测超时，已保留可用地址", group.label()));
                    chain.done = true;
                }
            }
            output.extend(self.finish_if_ready(&id));
        }
        output
    }
}

fn error(id: String, tool: bool, code: &'static str, message: &str) -> PluginToCore {
    let error = Some(PluginError {
        code,
        message: message.into(),
    });
    if tool {
        PluginToCore::RawResult {
            id,
            data: Value::Null,
            error,
        }
    } else {
        PluginToCore::Response {
            id,
            items: Vec::new(),
            error,
        }
    }
}

pub fn run() {
    let (sender, receiver) = mpsc::sync_channel(64);
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            match serde_json::from_str::<CoreToPlugin>(&line) {
                Ok(message) => {
                    if sender.send((message, Instant::now())).is_err() {
                        break;
                    }
                }
                Err(_) => eprintln!("[ip] invalid JSONL message"),
            }
        }
    });
    let mut engine = Engine::default();
    let mut stdout = io::stdout().lock();
    loop {
        if emit(&mut stdout, engine.expire(Instant::now())).is_err() {
            return;
        }
        // Idle processes block instead of polling. Any stdin message wakes this wait;
        // only an active query needs a timer at its nearest total deadline.
        let received = if let Some(deadline) = engine.jobs.values().map(|job| job.deadline).min() {
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        } else {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        match received {
            Ok((mut message, received_at)) => {
                let network = match &message {
                    CoreToPlugin::Query { settings, .. }
                    | CoreToPlugin::ToolCall { settings, .. } => {
                        let enhanced =
                            Settings::enhanced_enabled(settings.as_ref().unwrap_or(&Value::Null));
                        Some(crate::network::snapshot(enhanced))
                    }
                    _ => None,
                };
                if let CoreToPlugin::Query { budget_ms, .. }
                | CoreToPlugin::ToolCall { budget_ms, .. } = &mut message
                {
                    let spent = received_at.elapsed().as_millis() as u64;
                    *budget_ms = Some(budget_ms.unwrap_or(TOTAL_MS + 200).saturating_sub(spent));
                }
                if emit(&mut stdout, engine.handle(message, Instant::now(), network)).is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn emit(writer: &mut impl Write, messages: Vec<PluginToCore>) -> io::Result<()> {
    for message in messages {
        serde_json::to_writer(&mut *writer, &message).map_err(io::Error::other)?;
        writeln!(writer)?;
    }
    writer.flush()
}

#[cfg(test)]
mod tests;
