pub mod catalog;
use super::{AppEntry, fuzzy_entry_scores, to_pinyin_full, to_pinyin_initials};
use crate::config::StartMenuConfig;
use blink_infra::platform::system_entries::{
    self as platform, Discovery, DiscoveryState, LaunchTarget,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

#[derive(Clone)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub english: String,
    pub aliases: Vec<String>,
    pub history_key: String,
    pub icon: String,
    pub target: LaunchTarget,
}
#[derive(Clone, Serialize)]
pub struct Status {
    pub state: DiscoveryState,
    pub extra_count: usize,
    pub skipped: usize,
    pub failed_files: usize,
}
struct State {
    config: StartMenuConfig,
    language: String,
    generation: u64,
    building: bool,
    discovered: Vec<Entry>,
    snapshot: Arc<Vec<Entry>>,
    matches: Vec<Arc<Vec<AppEntry>>>,
    discovered_matches: Arc<Vec<AppEntry>>,
    fingerprint: Vec<(String, u64, u64)>,
    status: Status,
}
#[derive(Clone)]
pub struct SystemEntries {
    state: Arc<RwLock<State>>,
    common: Arc<Vec<Entry>>,
    common_matches: Arc<Vec<AppEntry>>,
}
impl SystemEntries {
    pub fn new(config: StartMenuConfig) -> Self {
        let common: Vec<Entry> = catalog::DEFINITIONS
            .iter()
            .filter(|d| platform::available(&d.target()))
            .map(|d| Entry {
                id: format!("system:{}", d.id),
                title: d.zh.into(),
                english: d.en.into(),
                aliases: d.aliases.split(';').map(str::to_string).collect(),
                history_key: d.history_key(),
                icon: d.icon(),
                target: d.target(),
            })
            .collect();
        let common_matches = Arc::new(prepare_matches(&common));
        let service = Self {
            common: Arc::new(common),
            common_matches,
            state: Arc::new(RwLock::new(State {
                config,
                language: "zh".into(),
                generation: 0,
                building: false,
                discovered: Vec::new(),
                snapshot: Arc::new(Vec::new()),
                matches: Vec::new(),
                discovered_matches: Arc::new(Vec::new()),
                fingerprint: Vec::new(),
                status: Status {
                    state: DiscoveryState::Disabled,
                    extra_count: 0,
                    skipped: 0,
                    failed_files: 0,
                },
            })),
        };
        service.rebuild(&mut service.state.write().unwrap());
        service
    }
    fn enabled(state: &State) -> bool {
        state.config.enabled && state.config.discover_system_settings
    }
    fn rebuild(&self, state: &mut State) {
        let mut entries = if state.config.enabled && state.config.include_system_shortcuts {
            self.common.as_ref().clone()
        } else {
            Vec::new()
        };
        let before = entries.len();
        if Self::enabled(state) {
            for discovered in &state.discovered {
                // 同一身份保留常用标题和目标，匹配从两份预计算目录取最高分。
                if !entries.iter().any(|e| e.id == discovered.id) {
                    entries.push(discovered.clone());
                }
            }
        }
        state.status.extra_count = entries.len() - before;
        let mut matches = Vec::new();
        if state.config.enabled && state.config.include_system_shortcuts {
            matches.push(self.common_matches.clone());
        }
        if Self::enabled(state) {
            matches.push(state.discovered_matches.clone());
        }
        state.snapshot = Arc::new(entries);
        state.matches = matches;
    }

    pub fn update_config(&self, config: StartMenuConfig) {
        {
            let mut state = self.state.write().unwrap();
            if state.config.enabled != config.enabled
                || state.config.discover_system_settings != config.discover_system_settings
            {
                state.generation += 1;
            }
            state.config = config;
            if !Self::enabled(&state) {
                state.status.state = DiscoveryState::Disabled;
            }
            self.rebuild(&mut state);
        }
        self.refresh(false);
    }
    pub fn update_language(&self, language: String) {
        let mut state = self.state.write().unwrap();
        if state.language == language {
            return;
        }
        state.language = language;
        state.generation += 1;
        state.fingerprint.clear();
        drop(state);
        self.refresh(true);
    }
    pub fn resolve(&self, id: &str) -> Option<Entry> {
        self.state
            .read()
            .unwrap()
            .snapshot
            .iter()
            .find(|e| e.id == id)
            .cloned()
    }
    pub fn status(&self) -> Status {
        self.state.read().unwrap().status.clone()
    }
    pub fn search(&self, query: &str, language: &str) -> Vec<(Entry, f32)> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        let (entries, matches) = {
            let state = self.state.read().unwrap();
            (state.snapshot.clone(), state.matches.clone())
        };
        let mut scores = HashMap::<&str, u32>::new();
        for variants in &matches {
            for (score, variant) in fuzzy_entry_scores(query, variants) {
                scores
                    .entry(variant.lnk_path.as_str())
                    .and_modify(|s| *s = (*s).max(score))
                    .or_insert(score);
            }
        }
        let max = scores.values().copied().max().unwrap_or(1).max(1) as f32;
        let mut results: Vec<_> = entries
            .iter()
            .filter_map(|entry| {
                let score = *scores.get(entry.id.as_str())? as f32 / max;
                let mut entry = entry.clone();
                if language.starts_with("en") && !entry.english.is_empty() {
                    entry.title = entry.english.clone();
                }
                Some((entry, score))
            })
            .collect();
        results.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.id.cmp(&b.0.id)));
        results.truncate(50);
        results
    }
    pub fn start(&self) {
        self.refresh(false);
        let service = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await;
                service.refresh(false);
            }
        });
    }
    pub fn refresh(&self, force: bool) {
        let generation = {
            let mut state = self.state.write().unwrap();
            if !Self::enabled(&state) || state.building {
                return;
            }
            state.building = true;
            state.status.state = DiscoveryState::Building;
            state.generation
        };
        let service = self.clone();
        tokio::spawn(async move {
            let cached = service.state.read().unwrap().fingerprint.clone();
            let work = tokio::task::spawn_blocking(move || {
                let fingerprint = platform::fingerprint();
                if !force && !cached.is_empty() && cached == fingerprint {
                    return Outcome::Unchanged;
                }
                let discovered = platform::discover();
                if fingerprint != platform::fingerprint() {
                    return Outcome::Changed;
                }
                match discovered {
                    Ok(Some(discovery)) => {
                        let skipped = discovery.skipped;
                        let failed_files = discovery.failed_files;
                        let entries = convert(discovery);
                        let matches = Arc::new(prepare_matches(&entries));
                        Outcome::Ready {
                            fingerprint,
                            data: Prepared {
                                entries,
                                matches,
                                skipped,
                                failed_files,
                            },
                        }
                    }
                    Ok(None) => Outcome::Unsupported,
                    Err(error) => Outcome::Failed(error),
                }
            })
            .await
            .unwrap_or_else(|error| Outcome::Failed(error.to_string()));
            let stale = service.finish(generation, work);
            if stale {
                service.refresh(false);
            }
        });
    }
    /// 只提交当前代次；测试可注入纯数据完成任务，无真实平台调用。
    fn finish(&self, generation: u64, outcome: Outcome) -> bool {
        let mut state = self.state.write().unwrap();
        state.building = false;
        if state.generation != generation || matches!(outcome, Outcome::Changed) {
            return true;
        }
        if !Self::enabled(&state) {
            return false;
        }
        match outcome {
            Outcome::Unchanged => state.status.state = DiscoveryState::Ready,
            Outcome::Ready { fingerprint, data } => {
                state.status.skipped = data.skipped;
                state.status.failed_files = data.failed_files;
                state.discovered = data.entries;
                state.discovered_matches = data.matches;
                state.status.state = DiscoveryState::Ready;
                state.fingerprint = fingerprint;
            }
            Outcome::Unsupported => {
                state.discovered.clear();
                state.discovered_matches = Arc::new(Vec::new());
                state.fingerprint.clear();
                state.status.state = DiscoveryState::Unsupported;
            }
            Outcome::Failed(error) => {
                tracing::warn!(%error, generation, "系统入口自动发现失败");
                state.status.state = DiscoveryState::Failed;
                state.fingerprint.clear();
            }
            Outcome::Changed => unreachable!(),
        }
        self.rebuild(&mut state);
        tracing::debug!(generation, state = ?state.status.state, extra_count = state.status.extra_count, "系统入口索引状态已更新");
        false
    }
}
enum Outcome {
    Unchanged,
    Ready {
        fingerprint: Vec<(String, u64, u64)>,
        data: Prepared,
    },
    Unsupported,
    Failed(String),
    Changed,
}
struct Prepared {
    entries: Vec<Entry>,
    matches: Arc<Vec<AppEntry>>,
    skipped: usize,
    failed_files: usize,
}
fn prepare_matches(entries: &[Entry]) -> Vec<AppEntry> {
    entries
        .iter()
        .flat_map(|entry| {
            std::iter::once(&entry.title)
                .chain(std::iter::once(&entry.english))
                .chain(entry.aliases.iter())
                .filter(|s| !s.is_empty())
                .map(|name| AppEntry {
                    name: name.clone(),
                    pinyin_name: to_pinyin_initials(name),
                    pinyin_full: to_pinyin_full(name),
                    lnk_path: entry.id.clone(),
                    ..Default::default()
                })
        })
        .collect()
}
fn convert(discovery: Discovery) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for raw in discovery.entries {
        let common = catalog::DEFINITIONS
            .iter()
            .find(|d| d.matches_discovery(&raw.id, &raw.title, &raw.target));
        let id = common
            .map(|d| format!("system:{}", d.id))
            .unwrap_or_else(|| format!("system:discovered:{}", raw.id));
        if let Some(existing) = entries.iter_mut().find(|e| e.id == id) {
            existing.aliases.push(raw.title);
            existing.aliases.extend(raw.keywords);
            continue;
        }
        let history_key = common
            .map(|d| d.history_key())
            .unwrap_or_else(|| id.clone());
        entries.push(Entry {
            id,
            title: raw.title,
            english: String::new(),
            aliases: raw.keywords,
            history_key,
            icon: if raw.icon.is_empty() {
                "stock:30".into()
            } else {
                raw.icon
            },
            target: raw.target,
        });
    }
    // 多份 XML 常重复同一标题/关键词；合并后只预计算一次，避免增加搜索热路径工作量。
    for entry in &mut entries {
        let mut seen = std::collections::HashSet::from([entry.title.to_lowercase()]);
        entry
            .aliases
            .retain(|alias| !alias.trim().is_empty() && seen.insert(alias.to_lowercase()));
    }
    entries
}
#[cfg(test)]
mod tests {
    use super::*;
    fn discovered() -> Entry {
        Entry {
            id: "system:discovered:test".into(),
            title: "测试入口".into(),
            english: String::new(),
            aliases: vec!["probe".into()],
            history_key: "system:discovered:test".into(),
            icon: "stock:30".into(),
            target: LaunchTarget::Shell("shell:ControlPanelFolder".into()),
        }
    }
    fn ready(entries: Vec<Entry>) -> Outcome {
        let matches = Arc::new(prepare_matches(&entries));
        Outcome::Ready {
            fingerprint: vec![("fixture".into(), 1, 1)],
            data: Prepared {
                entries,
                matches,
                skipped: 2,
                failed_files: 0,
            },
        }
    }
    #[tokio::test]
    async fn all_source_combinations_and_stale_discovery_completion() {
        let service = SystemEntries::new(StartMenuConfig::default());
        assert!(!service.state.read().unwrap().building);
        assert!(service.state.read().unwrap().fingerprint.is_empty());
        service.state.write().unwrap().building = true; // 注入一个尚未完成的后台任务。
        let mut cfg = StartMenuConfig {
            discover_system_settings: true,
            ..Default::default()
        };
        service.update_config(cfg.clone());
        let generation = service.state.read().unwrap().generation;
        cfg.discover_system_settings = false;
        service.update_config(cfg.clone());
        assert_eq!(service.status().state, DiscoveryState::Disabled);
        assert!(service.finish(generation, ready(vec![discovered()])));
        assert!(service.resolve("system:discovered:test").is_none());
        cfg.discover_system_settings = true;
        service.state.write().unwrap().building = true;
        service.update_config(cfg.clone());
        let generation = service.state.read().unwrap().generation;
        assert!(!service.finish(generation, ready(vec![discovered()])));
        assert_eq!(service.status().extra_count, 1);
        for master in [false, true] {
            for common in [false, true] {
                for auto in [false, true] {
                    // 保持在途任务，配置切换不能发起第二个扫描；搜索来自注入数据。
                    service.state.write().unwrap().building = true;
                    cfg.enabled = master;
                    cfg.include_system_shortcuts = common;
                    cfg.discover_system_settings = auto;
                    service.update_config(cfg.clone());
                    assert_eq!(
                        service.resolve("system:discovered:test").is_some(),
                        master && auto
                    );
                    assert_eq!(
                        service
                            .search("probe", "zh")
                            .iter()
                            .any(|(e, _)| e.id == "system:discovered:test"),
                        master && auto
                    );
                    if cfg!(windows) {
                        assert_eq!(
                            service.resolve("system:control_panel").is_some(),
                            master && common
                        );
                    }
                }
            }
        }
        cfg.enabled = true;
        cfg.discover_system_settings = true;
        cfg.include_system_shortcuts = true;
        service.update_config(cfg);
        let generation = service.state.read().unwrap().generation;
        let duplicate = service
            .common
            .iter()
            .find(|e| e.id == "system:control_panel")
            .cloned();
        if let Some(mut duplicate) = duplicate {
            duplicate.title = "自动来源标题".into();
            duplicate.aliases.push("fixture_alias".into());
            service.finish(generation, ready(vec![duplicate, discovered()]));
            assert_eq!(service.status().extra_count, 1);
            let hit = service.search("fixture_alias", "zh");
            assert_eq!(hit[0].0.title, "控制面板");
            assert_eq!(
                hit.iter()
                    .filter(|(e, _)| e.id == "system:control_panel")
                    .count(),
                1
            );
        }
    }
    #[tokio::test]
    async fn failure_and_unsupported_keep_common_results_available() {
        let service = SystemEntries::new(StartMenuConfig {
            discover_system_settings: true,
            ..Default::default()
        });
        service.finish(0, Outcome::Failed("fixture".into()));
        assert_eq!(service.status().state, DiscoveryState::Failed);
        assert!(service.state.read().unwrap().fingerprint.is_empty());
        service.finish(0, Outcome::Unsupported);
        assert_eq!(service.status().state, DiscoveryState::Unsupported);
        service.finish(0, ready(Vec::new()));
        assert_eq!(service.status().state, DiscoveryState::Ready);
        assert_eq!(service.status().extra_count, 0);
        if cfg!(windows) {
            assert!(service.resolve("system:control_panel").is_some());
        }
    }
    #[test]
    fn common_aliases_pinyin_languages_and_identity() {
        if !cfg!(windows) {
            return;
        }
        let service = SystemEntries::new(StartMenuConfig::default());
        for (query, id) in [
            ("编辑系统环境变量", "system:system_environment"),
            ("编辑账户的环境变量", "system:environment_variables"),
            ("卸载程序", "system:programs_features"),
            ("kzmb", "system:control_panel"),
            ("Control Panel", "system:control_panel"),
        ] {
            assert_eq!(service.search(query, "zh")[0].0.id, id, "{query}");
        }
        assert_eq!(service.search("控制面板", "en")[0].0.title, "Control Panel");
        let account = service.resolve("system:environment_variables").unwrap();
        assert_eq!(
            account.history_key,
            "rundll32.exe sysdm.cpl,EditEnvironmentVariables"
        );
        let system = service.resolve("system:system_environment").unwrap();
        let advanced = service.resolve("system:advanced_system").unwrap();
        assert_eq!(system.target, advanced.target);
        assert_ne!(system.id, advanced.id);
        assert_ne!(system.history_key, advanced.history_key);
    }
    #[test]
    #[ignore = "本机 Windows 只读目录与搜索快照回放"]
    fn replay_installed_discovery_and_snapshot_search() {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let Some(discovery) = platform::discover().unwrap() else {
            return;
        };
        let service = SystemEntries::new(StartMenuConfig {
            discover_system_settings: true,
            ..Default::default()
        });
        service.finish(0, ready(convert(discovery)));
        assert!(service.resolve("system:system_environment").is_some());
        assert!(service.resolve("system:environment_variables").is_some());
        let mut samples = Vec::new();
        for _ in 0..20 {
            for query in ["编辑系统环境变量", "卸载程序", "kzmb", "声音", "a"] {
                let start = std::time::Instant::now();
                let results = service.search(query, "zh");
                samples.push(start.elapsed().as_micros());
                if query == "编辑系统环境变量" {
                    assert_eq!(results[0].0.id, "system:system_environment");
                }
            }
        }
        samples.sort();
        tracing::info!(
            extra_count = service.status().extra_count,
            snapshot_entries = service.state.read().unwrap().snapshot.len(),
            samples = samples.len(),
            p95_us = samples[94],
            max_us = samples[99],
            "系统入口快照只读回放完成（不含窗口/IPC/历史）"
        );
    }
    #[test]
    fn duplicate_setting_variants_merge_titles_keywords_and_history_identity() {
        let entries = convert(Discovery {
            entries: [
                ("装载卷的访问路径", "添加访问路径"),
                ("创建虚拟磁盘", "创建 vhd 设置"),
                ("初始化磁盘", "初始化硬盘"),
                ("创建虚拟磁盘", "创建 vhd 设置"),
            ]
            .into_iter()
            .map(|(title, keyword)| platform::DiscoveredEntry {
                id: "systemsettings_storagesense_disksandvolumeslink".into(),
                title: title.into(),
                keywords: vec![keyword.into()],
                icon: String::new(),
                target: LaunchTarget::Uri("ms-settings:disksandvolumes".into()),
            })
            .collect(),
            ..Default::default()
        });
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].aliases.len(), 5);
        assert_eq!(entries[0].history_key, entries[0].id);
        let service = SystemEntries::new(StartMenuConfig {
            discover_system_settings: true,
            ..Default::default()
        });
        service.finish(0, ready(entries));
        for query in ["创建虚拟磁盘", "创建 vhd 设置", "初始化硬盘"] {
            assert_eq!(service.search(query, "zh").len(), 1, "{query}");
        }
    }
    #[test]
    fn different_settings_on_same_page_keep_their_own_identity() {
        let discovery = Discovery {
            entries: ["扬声器音量", "麦克风音量"]
                .into_iter()
                .enumerate()
                .map(|(index, title)| platform::DiscoveredEntry {
                    id: format!("sound-{index}"),
                    title: title.into(),
                    keywords: vec![],
                    icon: String::new(),
                    target: LaunchTarget::Uri("ms-settings:sound".into()),
                })
                .collect(),
            ..Default::default()
        };
        let entries = convert(discovery);
        assert_eq!(entries.len(), 2);
        assert_ne!(entries[0].id, entries[1].id);
    }
    #[test]
    fn merging_keeps_semantic_identity_and_old_history() {
        let target = catalog::DEFINITIONS
            .iter()
            .find(|d| d.id == "system_environment")
            .unwrap()
            .target();
        let discovery = Discovery {
            entries: [
                "{e2394c16-f45a-496f-83cc-49e163281662}",
                "{b1fe5142-dedd-409b-bcc8-547ec08de84e}",
            ]
            .into_iter()
            .map(|id| platform::DiscoveredEntry {
                id: id.into(),
                title: "test".into(),
                keywords: vec![],
                icon: String::new(),
                target: target.clone(),
            })
            .collect(),
            ..Default::default()
        };
        let entries = convert(discovery);
        assert_eq!(entries.len(), 2);
        assert_ne!(entries[0].id, entries[1].id);
        assert_eq!(
            catalog::DEFINITIONS[2].history_key(),
            "rundll32.exe sysdm.cpl,EditEnvironmentVariables"
        );
    }
}
