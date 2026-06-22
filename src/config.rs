//! 配置管理：SQLite 持久化 + 默认值 + 类型安全。
//!
//! 配置存储在 SQLite 的 `config` 表中，格式为 (key, value, updated_at)。
//! 本模块提供类型安全的配置读写，以及默认值处理。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

// ── 配置结构体 ──────────────────────────────────────────────────────────────────

/// 快捷键配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    /// 修饰键列表（ctrl, shift, alt, meta/win）
    pub modifiers: Vec<String>,
    /// 主键（字母、数字、功能键等）
    pub key: String,
    /// 显示名称（如 "RightAlt", "Ctrl+Shift+Space"）
    pub display: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            modifiers: vec![],
            key: "ralt".to_string(),
            display: "RightAlt".to_string(),
        }
    }
}

/// 应用配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 快捷键配置
    pub hotkey: HotkeyConfig,
    /// tap 阈值（毫秒）
    pub tap_threshold: u64,
    /// 看门狗 grace period（毫秒）
    pub grace_period: u64,
    /// 开机自启
    pub auto_start: bool,
    /// 语言（zh/en）
    pub language: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: HotkeyConfig::default(),
            tap_threshold: 300,
            grace_period: 500,
            auto_start: false,
            language: "zh".to_string(),
        }
    }
}

// ── 配置操作函数 ────────────────────────────────────────────────────────────────

/// 初始化配置：如果配置不存在，写入默认值。
pub async fn init_config(pool: &SqlitePool) -> Result<(), String> {
    let existing = crate::history::get_all_config(pool).await;
    if existing.is_empty() {
        let config = AppConfig::default();
        save_config(pool, &config).await?;
    }
    Ok(())
}

/// 获取完整配置。
pub async fn get_config(pool: &SqlitePool) -> AppConfig {
    let config_json = crate::history::get_config(pool, "app_config").await;
    match config_json {
        Some(json) => serde_json::from_str(&json).unwrap_or_default(),
        None => AppConfig::default(),
    }
}

/// 保存完整配置。
pub async fn save_config(pool: &SqlitePool, config: &AppConfig) -> Result<(), String> {
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::history::set_config(pool, "app_config", &json).await;
    Ok(())
}

/// 更新快捷键配置。
pub async fn update_hotkey(pool: &SqlitePool, hotkey: HotkeyConfig) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.hotkey = hotkey;
    save_config(pool, &config).await
}

/// 更新 tap 阈值。
pub async fn update_tap_threshold(pool: &SqlitePool, threshold: u64) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.tap_threshold = threshold;
    save_config(pool, &config).await
}

/// 更新 grace period。
pub async fn update_grace_period(pool: &SqlitePool, period: u64) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.grace_period = period;
    save_config(pool, &config).await
}

/// 更新开机自启设置。
pub async fn update_auto_start(pool: &SqlitePool, auto_start: bool) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.auto_start = auto_start;
    save_config(pool, &config).await
}

/// 更新语言设置。
pub async fn update_language(pool: &SqlitePool, language: String) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.language = language;
    save_config(pool, &config).await
}

// ── TODO: 方案 B - Trait 抽象（后续重构）────────────────────────────────────────
//
// 当需要支持多平台时，可以将配置管理抽象为 trait：
//
// ```rust
// pub trait ConfigManager {
//     async fn get_config(&self) -> AppConfig;
//     async fn save_config(&self, config: &AppConfig) -> Result<(), String>;
//     async fn get_hotkey(&self) -> HotkeyConfig;
//     async fn update_hotkey(&self, hotkey: HotkeyConfig) -> Result<(), String>;
//     // ... 其他配置操作
// }
//
// // 每个平台实现自己的 ConfigManager
// pub struct SqliteConfigManager { pool: SqlitePool }
// pub struct FileConfigManager { path: PathBuf }  // macOS/Linux 可能用文件
// ```
//
// 这样可以：
// 1. 支持不同的存储后端（SQLite、文件、注册表等）
// 2. 支持平台特定的配置项
// 3. 更容易进行单元测试（mock ConfigManager）
//