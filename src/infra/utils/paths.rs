//! 统一路径 helper（0.13 架构优化 §9.1）。
//!
//! 全项目 `%APPDATA%\blink\` 路径获取的唯一入口。
//! 消除散落在 `pools.rs` / `commands.rs` / `logging.rs` / `builtin.rs` /
//! `python/mod.rs` / `funasr.rs` / `skill.rs` 中的重复路径拼接逻辑。
//!
//! **设计**：用 `dirs_next::data_dir()`（Windows 下展开为 `%APPDATA%`），
//! 比直接 `std::env::var("APPDATA")` 更健壮（处理未设置 / 异常情况）。

use std::path::PathBuf;

/// Blink 数据根目录：`%APPDATA%\blink`
///
/// 所有持久化数据（DB / 日志 / Python 环境 / Skills / 配置）都在此目录下。
/// 目录不存在时仍返回路径（调用方自行 `create_dir_all`）。
pub fn app_data_dir() -> PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("blink")
}

/// 日志目录：`%APPDATA%\blink\logs`
pub fn logs_dir() -> PathBuf {
    app_data_dir().join("logs")
}

/// Skill 全局目录：`%APPDATA%\blink\skills`
pub fn skills_global_dir() -> PathBuf {
    app_data_dir().join("skills")
}

/// Python 环境目录：`%APPDATA%\blink\python`
pub fn python_dir() -> PathBuf {
    app_data_dir().join("python")
}

/// 模型缓存根目录：`%APPDATA%\blink\models`
///
/// 引擎专属模型缓存使用 `runtime::engine_model_cache_dir(EngineId)` 作为唯一真源。
#[allow(dead_code)] // 预留：当前引擎模型路径由 runtime 模块管理，此函数供未来通用模型管理使用
pub fn models_dir() -> PathBuf {
    app_data_dir().join("models")
}

/// 数据库文件路径：`%APPDATA%\blink\{db_name}`
pub fn db_path(db_name: &str) -> PathBuf {
    app_data_dir().join(db_name)
}
