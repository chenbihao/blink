//! FileEngine: 文件搜索引擎（0.5）。
//!
//! 数据源优先级链（0.26.14 扩展，见 `resolve_source_order`）：
//! 1. 用户 Everything HTTP - 最快，全盘索引，需用户安装 Everything 并开启 HTTP Server
//! 2. blink 托管 Everything 实例 - 用户未装时按需懒启动（`everything_runtime` 治理）
//! 3. 本地目录预扫（0.7.1 walkdir Fallback）- 兜底，仅覆盖常用目录
//! 4. Windows Search COM API - 系统内置，暂未实现（占位）
//!
//! 失败静默降级，不报错、不阻塞其他引擎。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use walkdir::WalkDir;

use crate::config::FileSearchConfig;

use super::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};
use super::everything_runtime::{EverythingRuntime, SEARCH_START_BUDGET};

/// `data_source` 的解析结果（纯函数，链路单测用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceOrder {
    /// `"auto"`：用户 Everything → 托管实例 → 本地兜底。
    Auto,
    /// `"everything"`：仅用户 Everything。
    UserOnly,
    /// `"managed"`：仅托管 Everything 实例。
    ManagedOnly,
    /// `"local"`：仅本地扫描。未知值回落 Auto。
    LocalOnly,
}

/// 解析 `data_source` 字符串（未知值回落 `Auto`，与既有宽容语义一致）。
pub fn resolve_source_order(data_source: &str) -> SourceOrder {
    match data_source {
        "everything" => SourceOrder::UserOnly,
        "managed" => SourceOrder::ManagedOnly,
        "local" => SourceOrder::LocalOnly,
        _ => SourceOrder::Auto,
    }
}

/// Everything 探测状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EverythingStatus {
    /// 未探测
    Unknown,
    /// 可用
    Available,
    /// 不可用
    Unavailable,
}

/// 文件搜索引擎。
pub struct FileEngine {
    /// 配置（运行时可更新，通过 SearchService 注入）
    config: Arc<RwLock<FileSearchConfig>>,
    /// 用户 Everything 探测状态（仅跟踪 `everything_port`；托管实例就绪态归 runtime）
    everything_status: Arc<RwLock<EverythingStatus>>,
    /// 上次探测时间（用于 Unavailable 状态下定期重试）
    last_probe_at: Arc<RwLock<Option<Instant>>>,
    /// reqwest 客户端（复用连接）
    client: reqwest::Client,
    /// 本地目录 Fallback 缓存
    fallback_cache: Arc<Mutex<FallbackCache>>,
    /// 托管 Everything 实例治理（0.26.14，Clone 共享）
    runtime: EverythingRuntime,
}

/// 文件搜索的专用命中结构。
///
/// 通用搜索只消费 `item`；`search_files` Capability 额外透传后端已有的文件元数据。
#[derive(Debug, Clone)]
pub struct FileSearchHit {
    pub item: SearchItem,
    pub size_bytes: Option<u64>,
    pub modified_at: Option<i64>,
}

/// Fallback 缓存条目。
#[derive(Debug, Clone)]
struct CachedFileEntry {
    /// 文件名
    name: String,
    /// 完整路径
    full_path: String,
    /// 父目录路径（用于 subtitle 显示）
    parent_dir: String,
    size_bytes: Option<u64>,
    modified_at: Option<i64>,
}

/// Fallback 缓存状态。
#[derive(Debug)]
struct FallbackCache {
    /// 缓存的文件列表
    entries: Vec<CachedFileEntry>,
    /// 缓存创建时间
    cached_at: Option<Instant>,
    /// 缓存有效期
    ttl: Duration,
    /// 是否正在扫描（防止并发扫描）
    scanning: bool,
}

impl FallbackCache {
    fn new(ttl_secs: u64) -> Self {
        Self {
            entries: Vec::new(),
            cached_at: None,
            ttl: Duration::from_secs(ttl_secs),
            scanning: false,
        }
    }

    /// 缓存是否有效（未过期且非空）
    fn is_valid(&self) -> bool {
        if let Some(at) = self.cached_at {
            at.elapsed() < self.ttl && !self.entries.is_empty()
        } else {
            false
        }
    }

    /// 是否正在扫描中
    fn is_scanning(&self) -> bool {
        self.scanning
    }
}

impl Default for FileEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl FileEngine {
    /// 创建新的文件搜索引擎。
    pub fn new() -> Self {
        Self::with_config(FileSearchConfig::default())
    }

    /// 带配置创建。
    pub fn with_config(config: FileSearchConfig) -> Self {
        let cache_ttl = config.local_cache_ttl_sec;
        Self {
            config: Arc::new(RwLock::new(config)),
            everything_status: Arc::new(RwLock::new(EverythingStatus::Unknown)),
            last_probe_at: Arc::new(RwLock::new(None)),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap_or_default(),
            fallback_cache: Arc::new(Mutex::new(FallbackCache::new(cache_ttl))),
            runtime: EverythingRuntime::new(),
        }
    }

    /// 托管 Everything 实例治理器（退出清理/设置页状态共用；Clone 共享同一实例）。
    pub fn everything_runtime(&self) -> &EverythingRuntime {
        &self.runtime
    }

    /// 用户 Everything 实例状态投影（0.26.14 设置页统一状态面）。
    ///
    /// `available: None` = 未探测（启动后未查询过），Some(true/false) = 最近探测结果；
    /// 端口来自当前配置（热更新后即时反映）。
    pub async fn user_everything_state(&self) -> (u16, Option<bool>) {
        let port = self.config.read().await.everything_port;
        let status = *self.everything_status.read().await;
        (port, match status {
            EverythingStatus::Unknown => None,
            EverythingStatus::Available => Some(true),
            EverythingStatus::Unavailable => Some(false),
        })
    }

    /// 更新配置（供 SearchService 调用）。
    pub async fn update_config(&self, config: FileSearchConfig) {
        let mut cfg = self.config.write().await;
        *cfg = config;
        tracing::debug!("FileEngine 配置已更新: port={}", cfg.everything_port);
    }

    /// 探测 Everything HTTP Server 是否可用。
    async fn probe_everything(&self, port: u16) -> bool {
        let url = format!("http://localhost:{port}/?search=__blink_probe__&json=1&count=1");

        match self.client.get(&url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    // 校验返回体是 Everything 的 JSON 结构（防止撞到别的 web 服务）
                    if let Ok(text) = resp.text().await {
                        // Everything 返回的 JSON 以 `{"totalResults":` 开头
                        if text.contains("totalResults") || text.contains("results") {
                            return true;
                        }
                    }
                }
                false
            }
            Err(_) => false,
        }
    }

    /// 搜索用户 Everything HTTP API（带探测/重试状态机，仅跟踪 `everything_port`）。
    async fn search_everything(
        &self,
        port: u16,
        query: &str,
        max_results: u32,
    ) -> Vec<FileSearchHit> {
        // Unavailable 状态下定期重试（每 30 秒一次，兼容后续启动 Everything 的场景）
        const RETRY_INTERVAL: Duration = Duration::from_secs(30);

        {
            let mut status = self.everything_status.write().await;
            let should_retry = match *status {
                EverythingStatus::Unknown => true,
                EverythingStatus::Unavailable => {
                    let last = self.last_probe_at.read().await;
                    last.is_none_or(|t| t.elapsed() >= RETRY_INTERVAL)
                }
                EverythingStatus::Available => false,
            };

            if should_retry {
                let now = Instant::now();
                *self.last_probe_at.write().await = Some(now);
                *status = if self.probe_everything(port).await {
                    tracing::debug!("Everything HTTP Server 探测成功，端口 {port}");
                    EverythingStatus::Available
                } else {
                    tracing::debug!("Everything HTTP Server 探测失败，端口 {port}");
                    EverythingStatus::Unavailable
                };
            }
            if *status == EverythingStatus::Unavailable {
                return Vec::new();
            }
        }

        // 发起搜索（请求失败标记用户实例 Unavailable）
        self.everything_http_query(port, query, max_results, true).await
    }

    /// 直接查询指定端口的 Everything HTTP JSON（托管实例与用户实例共用）。
    ///
    /// `is_user_instance`：连接失败时是否标记用户实例探测状态 Unavailable
    /// （状态机只跟踪用户端口；托管实例就绪态归 runtime 治理，不进状态机）。
    async fn everything_http_query(
        &self,
        port: u16,
        query: &str,
        max_results: u32,
        is_user_instance: bool,
    ) -> Vec<FileSearchHit> {
        // Everything HTTP API 参数:
        // - search: 搜索词
        // - json=1: 返回 JSON 格式
        // - count=N: 返回结果数
        // - path_column=1: 包含完整路径列 (不是 path=1)
        // - size_column=1: 包含文件大小
        // - date_modified_column=1: 包含修改时间（Windows FILETIME）
        let url = format!(
            "http://localhost:{port}/?search={}&json=1&count={max_results}&path_column=1&size_column=1&date_modified_column=1",
            urlencoding::encode(query)
        );

        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("Everything 请求失败: {e}");
                if is_user_instance {
                    // 用户实例：标记不可用，下次探测刷新；托管实例失败归 runtime 退避。
                    *self.everything_status.write().await = EverythingStatus::Unavailable;
                }
                return Vec::new();
            }
        };

        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!("Everything 读取响应失败: {e}");
                return Vec::new();
            }
        };

        let json: serde_json::Value = match serde_json::from_str(&text) {
            Ok(j) => j,
            Err(e) => {
                tracing::debug!("Everything JSON 解析失败: {e}");
                tracing::trace!("失败响应内容: {text}");
                return Vec::new();
            }
        };

        tracing::trace!(
            "Everything JSON keys: {:?}",
            json.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );

        let mut items = Vec::new();
        let results = match json["results"].as_array() {
            Some(r) => r,
            None => {
                tracing::debug!("Everything 响应中没有 results 字段或不是数组");
                tracing::trace!("响应内容: {json}");
                return items;
            }
        };

        for (i, result) in results.iter().enumerate() {
            let name = result["name"].as_str().unwrap_or_default();
            let path = result["path"].as_str().unwrap_or_default();

            if name.is_empty() {
                continue;
            }

            // 处理路径：如果有 path 字段，直接用；否则用 name（当前目录的文件）
            let full_path = if !path.is_empty() {
                if path.ends_with('\\') || path.ends_with('/') {
                    format!("{path}{name}")
                } else {
                    format!("{path}\\{name}")
                }
            } else {
                name.to_string()
            };

            // subtitle: 显示路径（如果有）否则显示文件类型
            let subtitle = if !path.is_empty() {
                path.to_string()
            } else {
                result["type"].as_str().unwrap_or("file").to_string()
            };

            let score = super::scorer::file_search_score(i);

            items.push(FileSearchHit {
                item: SearchItem {
                    id: full_path.clone(),
                    title: name.to_string(),
                    subtitle: Some(subtitle),
                    score,
                    action: SearchAction::Open { path: full_path },
                    source: "file".into(),
                    score_detail: Some(format!("file_rank={}", i)),
                    context_aware: false,
                    color_list_hex: None,
                },
                size_bytes: json_u64(&result["size"]),
                modified_at: json_u64(&result["date_modified"]).and_then(filetime_to_unix_seconds),
            });
        }

        tracing::debug!("Everything 返回 {} 个结果，query={}", items.len(), query);
        items
    }

    /// 从 Fallback 缓存中搜索文件。
    fn search_fallback(&self, query: &str, max_results: u32) -> Vec<FileSearchHit> {
        let cache = self.fallback_cache.lock().unwrap();
        if !cache.is_valid() {
            return Vec::new();
        }

        // 使用 nucleo 做模糊匹配
        use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
        use nucleo::{Config, Matcher, Utf32Str};

        let query_lower = query.to_ascii_lowercase();
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::new(
            &query_lower,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut buf = Vec::new();

        let mut scored: Vec<(u32, &CachedFileEntry)> = cache
            .entries
            .iter()
            .filter_map(|entry| {
                let haystack = Utf32Str::new(&entry.name, &mut buf);
                let score = pattern.score(haystack, &mut matcher)?;
                Some((score, entry))
            })
            .collect();

        scored.sort_by_key(|x| std::cmp::Reverse(x.0));

        let items: Vec<FileSearchHit> = scored
            .into_iter()
            .take(max_results as usize)
            .map(|(_score, entry)| {
                // 显示相对路径作为 subtitle（安全截断，避免多字节字符 panic）
                let subtitle = if entry.parent_dir.chars().count() > 60 {
                    let truncated: String = entry.parent_dir.chars().take(57).collect();
                    format!("{}...", truncated)
                } else {
                    entry.parent_dir.clone()
                };

                FileSearchHit {
                    item: SearchItem {
                        id: entry.full_path.clone(),
                        title: entry.name.clone(),
                        subtitle: Some(subtitle),
                        score: super::scorer::file_search_score(0), // Fallback 结果给统一分数
                        action: SearchAction::Open {
                            path: entry.full_path.clone(),
                        },
                        source: "file_local".into(),
                        score_detail: Some("local_fallback".into()),
                        context_aware: false,
                        color_list_hex: None,
                    },
                    size_bytes: entry.size_bytes,
                    modified_at: entry.modified_at,
                }
            })
            .collect();

        tracing::debug!("Fallback 搜索: query={query}, 返回 {} 个结果", items.len());
        items
    }

    /// 仅本地目录搜索（触发后台扫描 + 从缓存搜索）。
    async fn search_local_only(&self, cfg: &FileSearchConfig, query: &str) -> Vec<FileSearchHit> {
        // 触发后台扫描（如果缓存无效）
        let dirs = cfg.local_dirs.clone();
        let depth = cfg.local_max_depth;
        let cache = self.fallback_cache.clone();

        tokio::spawn(async move {
            let need_scan = {
                let c = cache.lock().unwrap();
                !c.is_valid() && !c.is_scanning()
            };
            if need_scan {
                Self::scan_fallback_dirs_static(&cache, &dirs, depth).await;
            }
        });

        // 从缓存搜索
        let results = self.search_fallback(query, cfg.local_max_results);
        tracing::debug!(
            "FileEngine: 本地搜索 query={query}, 返回 {} 个结果",
            results.len()
        );
        results
    }

    /// 静态方法：扫描本地目录填充缓存（供 spawn 调用）。
    async fn scan_fallback_dirs_static(
        cache: &Arc<Mutex<FallbackCache>>,
        dirs: &[String],
        max_depth: u32,
    ) {
        // 标记正在扫描
        {
            let mut c = cache.lock().unwrap();
            if c.scanning || c.is_valid() {
                return;
            }
            c.scanning = true;
        }

        let cache_clone = cache.clone();
        let dirs = dirs.to_vec();

        let entries = tokio::task::spawn_blocking(move || {
            let start = Instant::now();
            let mut all_entries = Vec::new();

            for dir_name in &dirs {
                let dir_path = resolve_special_dir(dir_name);
                let Some(path) = dir_path else {
                    tracing::debug!("Fallback: 无法解析目录 {dir_name}");
                    continue;
                };

                if !path.exists() {
                    tracing::debug!("Fallback: 目录不存在 {path:?}");
                    continue;
                }

                tracing::debug!("Fallback: 扫描目录 {path:?}, max_depth={max_depth}");

                let walker = WalkDir::new(&path)
                    .max_depth(max_depth as usize)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file());

                for entry in walker {
                    let file_name = entry.file_name().to_string_lossy().to_string();
                    let full_path = entry.path().to_string_lossy().to_string();
                    let parent = entry
                        .path()
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let metadata = entry.metadata().ok();
                    let size_bytes = metadata.as_ref().map(|metadata| metadata.len());
                    let modified_at = metadata
                        .and_then(|metadata| metadata.modified().ok())
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .and_then(|duration| i64::try_from(duration.as_secs()).ok());

                    all_entries.push(CachedFileEntry {
                        name: file_name,
                        full_path,
                        parent_dir: parent,
                        size_bytes,
                        modified_at,
                    });
                }
            }

            let elapsed = start.elapsed();
            tracing::info!(
                count = all_entries.len(),
                elapsed_ms = elapsed.as_millis(),
                "Fallback: 扫描完成"
            );

            all_entries
        })
        .await
        .unwrap_or_default();

        // 更新缓存
        {
            let mut c = cache_clone.lock().unwrap();
            c.entries = entries;
            c.cached_at = Some(Instant::now());
            c.scanning = false;
        }
    }

    /// 给 Capability 使用的带元数据搜索；与通用 SearchEngine 共用同一分流和缓存。
    pub async fn search_with_metadata(&self, query: &str) -> Vec<FileSearchHit> {
        let q = query.trim();
        if q.is_empty() || q.len() < 2 {
            tracing::trace!(query = %q, "FileEngine: 查询太短，跳过");
            return Vec::new();
        }

        let cfg = self.config.read().await;
        if !cfg.enabled {
            tracing::trace!("FileEngine: 已禁用，跳过");
            return Vec::new();
        }

        // ── data_source 优先级链（0.26.14）───────────────────────────────
        // auto = 用户 Everything → 托管实例 → 本地兜底；显式选项固定单一来源。
        match resolve_source_order(&cfg.data_source) {
            SourceOrder::LocalOnly => {
                tracing::debug!(query = %q, "FileEngine: 本地模式");
                self.search_local_only(&cfg, q).await
            }
            SourceOrder::UserOnly => {
                tracing::debug!(query = %q, port = cfg.everything_port, "FileEngine: 仅用户 Everything");
                self.search_everything(cfg.everything_port, q, cfg.max_results).await
            }
            SourceOrder::ManagedOnly => {
                if !cfg.managed_everything_enabled {
                    tracing::trace!("FileEngine: 托管实例已禁用");
                    return Vec::new();
                }
                self.search_managed(&cfg, q).await
            }
            SourceOrder::Auto => {
                let results =
                    self.search_everything(cfg.everything_port, q, cfg.max_results).await;
                if !results.is_empty() {
                    return results;
                }
                // 用户 Everything 不可用（状态机已探测）→ 托管实例 → 本地兜底。
                let user_unavailable = {
                    let status = self.everything_status.read().await;
                    *status == EverythingStatus::Unavailable
                };
                if user_unavailable && cfg.managed_everything_enabled {
                    let managed = self.search_managed(&cfg, q).await;
                    if !managed.is_empty() {
                        return managed;
                    }
                }
                // 注意区分：Everything 已应答但无匹配（实例活着、索引中/确无匹配）
                // 与探测不可用都会走本地兜底——日志如实区分，不把 0 结果谎报为不可用。
                tracing::debug!(
                    user_unavailable,
                    "FileEngine: Everything 无可用结果（不可用或无匹配），降级本地搜索"
                );
                self.search_local_only(&cfg, q).await
            }
        }
    }

    /// 托管实例查询：懒启动（短预算）→ 命中即查；未就绪本轮静默降级。
    async fn search_managed(&self, cfg: &FileSearchConfig, q: &str) -> Vec<FileSearchHit> {
        match self
            .runtime
            .ensure_started(cfg.managed_everything_port, super::everything_runtime::SEARCH_START_BUDGET)
            .await
        {
            Ok(port) => {
                tracing::debug!(query = %q, port, "FileEngine: 托管 Everything 实例");
                self.everything_http_query(port, q, cfg.max_results, false).await
            }
            Err(error) => {
                // 未安装是正常态（用户没点下载）；其余为启动期状态，下一轮重试。
                tracing::debug!(%error, "FileEngine: 托管实例不可用");
                Vec::new()
            }
        }
    }
}

fn json_u64(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
}

fn filetime_to_unix_seconds(filetime: u64) -> Option<i64> {
    const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;
    let ticks = filetime.checked_sub(WINDOWS_TO_UNIX_EPOCH_100NS)?;
    i64::try_from(ticks / 10_000_000).ok()
}

/// 解析特殊目录名为实际路径。
fn resolve_special_dir(name: &str) -> Option<PathBuf> {
    match name.to_ascii_lowercase().as_str() {
        "desktop" => dirs_next::desktop_dir(),
        "documents" => dirs_next::document_dir(),
        "downloads" => dirs_next::download_dir(),
        "startmenu" => {
            // 开始菜单：用户目录 + 公共目录
            let user = dirs_next::data_dir()
                .map(|d| d.join("Microsoft").join("Windows").join("Start Menu"));
            let public = std::env::var("ProgramData").ok().map(|d| {
                PathBuf::from(d)
                    .join("Microsoft")
                    .join("Windows")
                    .join("Start Menu")
            });
            // 返回用户目录（主要位置）
            user.or(public)
        }
        _ => {
            // 尝试作为绝对路径
            let path = PathBuf::from(name);
            if path.exists() { Some(path) } else { None }
        }
    }
}

#[async_trait::async_trait]
impl SearchEngine for FileEngine {
    fn id(&self) -> &'static str {
        "file"
    }

    fn lane(&self) -> Lane {
        // HTTP 请求放 Async 通道，不阻塞首批结果
        Lane::Async
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn start(&self) {
        // 后台异步探测 Everything 状态（带重试，兼容 Everything 比 Blink 启动慢的场景）
        let status = self.everything_status.clone();
        let last_probe_at = self.last_probe_at.clone();
        let client = self.client.clone();
        let config = self.config.clone();

        // 0.26.14：已安装且启用时启动即拉起托管实例——app 重启（含 dev 热重载）
        // 会终止实例，只在首搜懒启动会让「装好的引擎重启后消失」，实机反馈不佳；
        // 配置读取与就绪探测全部在 spawn 内 await（start() 跑在 runtime 线程，
        // 禁用 blocking_read——曾致启动期 panic），未就绪由首搜/设置页兜底。
        let runtime = self.runtime.clone();
        let managed_config = self.config.clone();
        tokio::spawn(async move {
            let cfg = managed_config.read().await;
            let needed = cfg.managed_everything_enabled
                && matches!(
                    resolve_source_order(&cfg.data_source),
                    SourceOrder::Auto | SourceOrder::ManagedOnly
                )
                && runtime.snapshot().installed;
            let port = cfg.managed_everything_port;
            drop(cfg);
            if !needed {
                return;
            }
            match runtime.ensure_started(port, SEARCH_START_BUDGET).await {
                Ok(ready_port) => {
                    tracing::info!(port = ready_port, "启动时托管 Everything 实例已拉起");
                }
                Err(error) => {
                    tracing::debug!(%error, "启动时托管实例未就绪（首搜/设置页再试）");
                }
            }
        });

        tokio::spawn(async move {
            let cfg = config.read().await;

            // 仅本地/仅托管模式不需要探测用户 Everything
            if matches!(
                resolve_source_order(&cfg.data_source),
                SourceOrder::LocalOnly | SourceOrder::ManagedOnly
            ) {
                tracing::info!("FileEngine: 数据源不使用用户 Everything，跳过探测");
                let mut s = status.write().await;
                *s = EverythingStatus::Unavailable;
                return;
            }

            let port = cfg.everything_port;
            drop(cfg); // 释放读锁

            // 最多重试 3 次，间隔 2 秒（兼容 Everything 比 Blink 启动慢的场景）
            let max_retries = 3u32;
            for attempt in 1..=max_retries {
                let url = format!("http://localhost:{port}/?search=__blink_probe__&json=1&count=1");
                let available = match client.get(&url).send().await {
                    Ok(resp) => resp.status().is_success(),
                    Err(_) => false,
                };

                if available {
                    tracing::info!(
                        "Everything HTTP Server 可用，端口 {port}（第 {attempt} 次探测）"
                    );
                    *status.write().await = EverythingStatus::Available;
                    *last_probe_at.write().await = Some(Instant::now());
                    return;
                }

                if attempt < max_retries {
                    tracing::debug!(
                        "Everything HTTP Server 探测失败，{attempt}/{max_retries}，2 秒后重试"
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }

            tracing::info!(
                "Everything HTTP Server 不可用（已重试 {max_retries} 次），文件搜索降级"
            );
            *status.write().await = EverythingStatus::Unavailable;
            *last_probe_at.write().await = Some(Instant::now());
        });
    }

    async fn search(&self, query: &str, _ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        self.search_with_metadata(query)
            .await
            .into_iter()
            .map(|hit| hit.item)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_order_maps_data_source_values() {
        assert_eq!(resolve_source_order("auto"), SourceOrder::Auto);
        assert_eq!(resolve_source_order("everything"), SourceOrder::UserOnly);
        assert_eq!(resolve_source_order("managed"), SourceOrder::ManagedOnly);
        assert_eq!(resolve_source_order("local"), SourceOrder::LocalOnly);
        // 未知值回落 Auto（宽容语义与既有行为一致）
        assert_eq!(resolve_source_order("wat"), SourceOrder::Auto);
        assert_eq!(resolve_source_order(""), SourceOrder::Auto);
    }

    #[test]
    fn managed_fields_default_on_with_dedicated_port() {
        let cfg = FileSearchConfig::default();
        assert!(cfg.managed_everything_enabled, "auto 链默认启用托管实例");
        assert_eq!(cfg.managed_everything_port, 52819);
        // 旧分片（无 managed 字段）反序列化必须得到默认值
        let legacy = r#"{"enabled":true,"data_source":"auto","everything_port":80}"#;
        let cfg: FileSearchConfig = serde_json::from_str(legacy).unwrap();
        assert!(cfg.managed_everything_enabled);
        assert_eq!(cfg.managed_everything_port, 52819);
    }

    #[test]
    fn parses_everything_numeric_fields() {
        assert_eq!(json_u64(&serde_json::json!(123)), Some(123));
        assert_eq!(json_u64(&serde_json::json!("456")), Some(456));
        assert_eq!(json_u64(&serde_json::Value::Null), None);
    }

    #[test]
    fn converts_windows_filetime_to_unix_seconds() {
        assert_eq!(filetime_to_unix_seconds(116_444_736_000_000_000), Some(0));
        assert_eq!(filetime_to_unix_seconds(116_444_736_010_000_000), Some(1));
        assert_eq!(filetime_to_unix_seconds(1), None);
    }
}

// TODO: 0.5.1 本地目录扫描实现
// TODO: 0.5.x Everything SDK/IPC 通道（无需开 HTTP Server）
// TODO: 0.5.x Windows Search COM API
