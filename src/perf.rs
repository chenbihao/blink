//! 性能统计与监控（0.7.0）：量化核心链路耗时，便于发现瓶颈。
//!
//! 设计（见 production-design/phases/0.7-plugin-ecosystem-local-search.md §七）：
//! - SQLite `performance_metrics` 表持久化，保留 30 天，自动清理
//! - 全局 `once_cell` 持有 pool，各埋点零参数传递
//! - `record()` 异步写入，不阻塞调用方（spawn 到 tokio runtime）
//! - 前端可查询 P50/P90/P99 和慢查询日志

use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// 全局 pool 单例——各埋点直接调 record()，无需传 pool。
static POOL: OnceCell<SqlitePool> = OnceCell::new();

/// 性能指标分类。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MetricCategory {
    /// 启动阶段
    Startup,
    /// 热键唤起
    Hotkey,
    /// 搜索引擎
    SearchEngine,
    /// 插件查询
    Plugin,
    /// 图标提取
    IconExtract,
}

impl std::fmt::Display for MetricCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Startup => write!(f, "startup"),
            Self::Hotkey => write!(f, "hotkey"),
            Self::SearchEngine => write!(f, "search_engine"),
            Self::Plugin => write!(f, "plugin"),
            Self::IconExtract => write!(f, "icon_extract"),
        }
    }
}

/// 单条性能指标。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceMetric {
    pub category: String,
    pub name: String,
    pub value_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    pub created_at: i64,
}

/// 初始化性能统计：建表 + 清理过期数据 + 注册全局 pool。
pub async fn init(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS performance_metrics (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            category TEXT NOT NULL,
            name TEXT NOT NULL,
            value_ms REAL NOT NULL,
            metadata TEXT,
            created_at INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_perf_created ON performance_metrics(created_at)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_perf_cat_name ON performance_metrics(category, name)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    // 注册全局 pool
    let _ = POOL.set(pool.clone());

    // 后台清理过期数据（>30 天）
    let cleanup_pool = pool.clone();
    tokio::spawn(async move {
        cleanup_old(&cleanup_pool).await;
    });

    tracing::debug!("performance_metrics 表已初始化");
    Ok(())
}

/// 记录一条性能指标（异步写入，不阻塞调用方）。
///
/// 如果全局 pool 未初始化（启动早期），静默丢弃。
/// 可在 async 上下文和同步上下文（如 Win32 回调）中调用——内部通过 `tokio::spawn` 异步写入。
pub fn record(category: MetricCategory, name: &str, value_ms: f64, metadata: Option<&str>) {
    let Some(pool) = POOL.get() else {
        return; // pool 未就绪，静默丢弃
    };
    let category = category.to_string();
    let name = name.to_string();
    let metadata = metadata.map(|s| s.to_string());
    let now = chrono::Utc::now().timestamp();
    let pool = pool.clone();

    // setup 阶段可能还没有 Tokio runtime 句柄，静默丢弃而非 panic
    // 注意：setup 阶段的启动时间需要显式用 record_blocking() 记录
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = sqlx::query(
                "INSERT INTO performance_metrics (category, name, value_ms, metadata, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(&category)
            .bind(&name)
            .bind(value_ms)
            .bind(&metadata)
            .bind(now)
            .execute(&pool)
            .await;
        });
    }
}

/// 同步记录一条性能指标（setup 阶段专用，此时没有 Tokio runtime 句柄）。
/// 需要传入 pool 并在 block_on 中调用。
pub async fn record_blocking(
    pool: &sqlx::SqlitePool,
    category: MetricCategory,
    name: &str,
    value_ms: f64,
    metadata: Option<&str>,
) {
    let category = category.to_string();
    let metadata = metadata.map(|s| s.to_string());
    let now = chrono::Utc::now().timestamp();

    let _ = sqlx::query(
        "INSERT INTO performance_metrics (category, name, value_ms, metadata, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(&category)
    .bind(name)
    .bind(value_ms)
    .bind(&metadata)
    .bind(now)
    .execute(pool)
    .await;
}

/// 计时器 RAII guard：drop 时自动记录耗时。
///
/// 用法：
/// ```ignore
/// let _timer = perf::Timer::new(MetricCategory::Startup, "config_load");
/// // ... 执行操作 ...
/// // _timer drop 时自动记录
/// ```
pub struct Timer {
    category: MetricCategory,
    name: String,
    start: std::time::Instant,
    metadata: Option<String>,
}

impl Timer {
    pub fn new(category: MetricCategory, name: &str) -> Self {
        Self {
            category,
            name: name.to_string(),
            start: std::time::Instant::now(),
            metadata: None,
        }
    }

    #[allow(dead_code)]
    pub fn with_metadata(category: MetricCategory, name: &str, metadata: &str) -> Self {
        Self {
            category,
            name: name.to_string(),
            start: std::time::Instant::now(),
            metadata: Some(metadata.to_string()),
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed().as_secs_f64() * 1000.0;
        record(
            self.category,
            &self.name,
            elapsed,
            self.metadata.as_deref(),
        );
    }
}

/// 查询最近 N 条指标。
pub async fn query_recent(pool: &SqlitePool, limit: i64) -> Vec<PerformanceMetric> {
    sqlx::query_as::<_, (String, String, f64, Option<String>, i64)>(
        "SELECT category, name, value_ms, metadata, created_at FROM performance_metrics ORDER BY created_at DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(category, name, value_ms, metadata, created_at)| PerformanceMetric {
        category,
        name,
        value_ms,
        metadata,
        created_at,
    })
    .collect()
}

/// 查询指定类别的 P50/P90/P99 分位值（最近 N 条）。
pub async fn query_percentiles(
    pool: &SqlitePool,
    category: &str,
    name: &str,
    limit: i64,
) -> serde_json::Value {
    let rows: Vec<f64> = sqlx::query_as::<_, (f64,)>(
        "SELECT value_ms FROM performance_metrics WHERE category = ?1 AND name = ?2 ORDER BY created_at DESC LIMIT ?3",
    )
    .bind(category)
    .bind(name)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(v,)| v)
    .collect();

    if rows.is_empty() {
        return serde_json::json!({ "count": 0 });
    }

    let mut sorted = rows.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let len = sorted.len();

    let p50 = sorted[len * 50 / 100];
    let p90 = sorted[len * 90 / 100];
    let p99 = sorted[len.min(len * 99 / 100)];

    serde_json::json!({
        "count": len,
        "p50": format!("{:.2}", p50),
        "p90": format!("{:.2}", p90),
        "p99": format!("{:.2}", p99),
        "min": format!("{:.2}", sorted.first().unwrap_or(&0.0)),
        "max": format!("{:.2}", sorted.last().unwrap_or(&0.0)),
        "avg": format!("{:.2}", sorted.iter().sum::<f64>() / len as f64),
    })
}

/// 查询慢查询日志（超过阈值的请求）。
pub async fn query_slow(
    pool: &SqlitePool,
    category: &str,
    threshold_ms: f64,
    limit: i64,
) -> Vec<PerformanceMetric> {
    sqlx::query_as::<_, (String, String, f64, Option<String>, i64)>(
        "SELECT category, name, value_ms, metadata, created_at FROM performance_metrics \
         WHERE category = ?1 AND value_ms > ?2 ORDER BY value_ms DESC LIMIT ?3",
    )
    .bind(category)
    .bind(threshold_ms)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(category, name, value_ms, metadata, created_at)| PerformanceMetric {
        category,
        name,
        value_ms,
        metadata,
        created_at,
    })
    .collect()
}

/// 清理超过 30 天的指标数据。
async fn cleanup_old(pool: &SqlitePool) {
    let cutoff = chrono::Utc::now().timestamp() - 30 * 86400;
    match sqlx::query("DELETE FROM performance_metrics WHERE created_at < ?1")
        .bind(cutoff)
        .execute(pool)
        .await
    {
        Ok(r) => {
            let rows = r.rows_affected();
            if rows > 0 {
                tracing::info!(rows, cutoff, "清理过期性能指标");
            }
        }
        Err(e) => tracing::warn!(error = %e, "清理过期性能指标失败"),
    }
}

/// 清除全部性能指标数据。
pub async fn clear_all(pool: &SqlitePool) -> Result<u64, String> {
    let r = sqlx::query("DELETE FROM performance_metrics")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    let rows = r.rows_affected();
    tracing::info!(rows, "清除全部性能指标");
    Ok(rows)
}

/// 获取性能统计概览（供前端调试 Tab 展示）。
pub async fn get_overview(pool: &SqlitePool) -> serde_json::Value {
    // 启动耗时
    let startup = query_percentiles(pool, "startup", "total", 100).await;

    // 热键唤起
    let hotkey = query_percentiles(pool, "hotkey", "key_to_show", 100).await;

    // 搜索引擎耗时
    let search = query_percentiles(pool, "search_engine", "total", 100).await;

    // 慢查询统计
    let slow_hotkey = query_slow(pool, "hotkey", 100.0, 10).await;
    let slow_search = query_slow(pool, "search_engine", 200.0, 10).await;

    serde_json::json!({
        "startup": startup,
        "hotkey": hotkey,
        "search": search,
        "slow_hotkey": slow_hotkey,
        "slow_search": slow_search,
    })
}

/// 导出性能报告（JSON 格式）。
pub async fn export_report(pool: &SqlitePool) -> serde_json::Value {
    let all = query_recent(pool, 10000).await;
    serde_json::json!({
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "total_records": all.len(),
        "metrics": all,
    })
}
