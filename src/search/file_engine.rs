//! FileEngine: 文件搜索引擎（0.5）。
//!
//! 三层回退架构（按速度/覆盖率排序）：
//! 1. Everything HTTP API - 最快，全盘索引，需用户安装 Everything 并开启 HTTP Server
//! 2. 本地目录预扫 - 兜底，仅覆盖常用目录（Desktop/Documents/Downloads）
//! 3. Windows Search COM API - 系统内置，暂未实现（占位）
//!
//! 失败静默降级，不报错、不阻塞其他引擎。

use std::{sync::Arc, time::Duration};

use tokio::sync::RwLock;

use crate::config::FileSearchConfig;

use super::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};

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
    /// Everything 探测状态
    everything_status: Arc<RwLock<EverythingStatus>>,
    /// reqwest 客户端（复用连接）
    client: reqwest::Client,
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
        Self {
            config: Arc::new(RwLock::new(config)),
            everything_status: Arc::new(RwLock::new(EverythingStatus::Unknown)),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap_or_default(),
        }
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

    /// 搜索 Everything HTTP API。
    async fn search_everything(&self, port: u16, query: &str, max_results: u32) -> Vec<SearchItem> {
        // 先探测（首次搜索时）
        {
            let mut status = self.everything_status.write().await;
            if *status == EverythingStatus::Unknown {
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

        // 发起搜索
        // Everything HTTP API 参数:
        // - search: 搜索词
        // - json=1: 返回 JSON 格式
        // - count=N: 返回结果数
        // - path_column=1: 包含完整路径列 (不是 path=1)
        // - size=1: 包含文件大小
        // - date_modified=1: 包含修改时间
        let url = format!(
            "http://localhost:{port}/?search={}&json=1&count={max_results}&path_column=1&size=1&date_modified=1",
            urlencoding::encode(query)
        );

        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("Everything 请求失败: {e}");
                // 标记为不可用，下次不再重试（后台定时探测会刷新）
                *self.everything_status.write().await = EverythingStatus::Unavailable;
                return Vec::new();
            }
        };

        // 先读取原始文本用于调试
        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!("Everything 读取响应失败: {e}");
                return Vec::new();
            }
        };

        // tracing::trace!("Everything 原始响应(前500字符): {}", &text.chars().take(500).collect::<String>());

        let json: serde_json::Value = match serde_json::from_str(&text) {
            Ok(j) => j,
            Err(e) => {
                tracing::debug!("Everything JSON 解析失败: {e}");
                tracing::trace!("失败响应内容: {text}");
                return Vec::new();
            }
        };

        // 调试：打印 JSON 结构
        tracing::trace!("Everything JSON keys: {:?}", json.as_object().map(|o| o.keys().collect::<Vec<_>>()));

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

            items.push(SearchItem {
                id: full_path.clone(),
                title: name.to_string(),
                subtitle: Some(subtitle),
                score,
                action: SearchAction::Open { path: full_path },
                source: "file".into(),
            });
        }

        tracing::debug!("Everything 返回 {} 个结果，query={}", items.len(), query);
        for (i, item) in items.iter().enumerate() {
            tracing::debug!(
                index = i,
                name = %item.title,
                score = %item.score,
                "文件搜索结果项"
            );
        }
        items
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

    fn start(&self) {
        // 后台异步探测 Everything 状态
        let status = self.everything_status.clone();
        let client = self.client.clone();
        let config = self.config.clone();

        tokio::spawn(async move {
            let cfg = config.read().await;
            let port = cfg.everything_port;
            let url = format!("http://localhost:{port}/?search=__blink_probe__&json=1&count=1");
            let available = match client.get(&url).send().await {
                Ok(resp) => resp.status().is_success(),
                Err(_) => false,
            };

            let mut s = status.write().await;
            *s = if available {
                tracing::info!("Everything HTTP Server 可用，端口 {port}");
                EverythingStatus::Available
            } else {
                tracing::info!("Everything HTTP Server 不可用，文件搜索降级");
                EverythingStatus::Unavailable
            };
        });
    }

    async fn search(&self, query: &str, _ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        let q = query.trim();
        if q.is_empty() || q.len() < 2 {
            tracing::trace!("FileEngine: 查询太短，跳过: {q}");
            return Vec::new();
        }

        let cfg = self.config.read().await;
        if !cfg.enabled {
            tracing::trace!("FileEngine: 已禁用，跳过");
            return Vec::new();
        }

        tracing::debug!("FileEngine: 搜索 Everything，query={q}, port={}, max_results={}", cfg.everything_port, cfg.max_results);
        let results = self.search_everything(cfg.everything_port, q, cfg.max_results).await;
        tracing::debug!("FileEngine: 返回 {} 个结果", results.len());
        results
    }
}

// TODO: 0.5.1 本地目录扫描实现
// TODO: 0.5.x Everything SDK/IPC 通道（无需开 HTTP Server）
// TODO: 0.5.x Windows Search COM API
