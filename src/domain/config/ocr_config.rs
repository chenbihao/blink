//! OCR 配置分片——第 9 个 KV，key = `"ocr:config"`（0.22.4）。
//!
//! 独立于 AppConfig 门面，独立 opt-in。
//! 老用户首次读拿到 `OcrConfig::default()`，`backend = "auto"`（0.22.10 起），零副作用。
//!
//! ## serde 缺字段回落
//!
//! 所有字段使用 `#[serde(default)]`，旧配置或缺失字段回落到默认值。
//! `backend` 字段使用自定义 deserializer，未知字符串回落到 `Auto`（0.22.10 起默认）。
//!
//! ## 运行时快照
//!
//! `OcrRuntimeSnapshot` 是每次 recognize 开始时的不可变快照，
//! 中途修改配置不改变在途请求。

use serde::{Deserialize, Serialize};

use super::store::ConfigKey;
use crate::domain::ocr::config::{
    ComputePreference, DEFAULT_IDLE_TTL_SECONDS, OcrBackendKind, OcrLifecycle, PaddleModel,
    validate_idle_ttl,
};

/// OCR 配置分片（0.22.4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrConfig {
    /// OCR 后端选择：`"auto"` | `"paddleocr"` | `"windows"`。
    ///
    /// 默认 `"auto"`（0.22.10 起：已安装 PaddleOCR 优先并允许冷启动）。
    /// 未知值回落到 `"auto"`。
    #[serde(default = "default_backend", deserialize_with = "deserialize_backend")]
    pub backend: OcrBackendKind,

    /// PaddleOCR 模型档位（仅 `paddleocr` / `auto` 模式生效）。
    ///
    /// 默认 `"tiny"`（唯一通过生产资格门的候选）。
    #[serde(default)]
    pub paddle_model: PaddleModel,

    /// 生命周期策略。
    #[serde(default)]
    pub lifecycle: OcrLifecycle,

    /// 空闲 TTL（秒，仅 `OnDemand` 模式生效）。
    ///
    /// 默认 300（5 分钟）。范围 10-3600。
    #[serde(default = "default_idle_ttl")]
    pub idle_ttl_seconds: u32,

    /// 计算设备偏好（0.22.4 §3.5）。
    ///
    /// 首版只允许 `auto` | `cpu`。
    /// 未验证的 cuda/vulkan/directml 不得开放。
    #[serde(default = "default_compute_preference")]
    pub compute_preference: ComputePreference,
}

fn default_backend() -> OcrBackendKind {
    OcrBackendKind::Auto
}

fn default_idle_ttl() -> u32 {
    DEFAULT_IDLE_TTL_SECONDS
}

fn default_compute_preference() -> ComputePreference {
    ComputePreference::Auto
}

/// 自定义 deserializer：未知字符串回落到 `Auto`（0.22.10 起默认）。
fn deserialize_backend<'de, D>(deserializer: D) -> Result<OcrBackendKind, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer).unwrap_or_default();
    match s.as_str() {
        "windows" => Ok(OcrBackendKind::Windows),
        "paddleocr" => Ok(OcrBackendKind::PaddleOcr),
        "auto" => Ok(OcrBackendKind::Auto),
        _ => {
            tracing::warn!(
                value = %s,
                "OcrConfig.backend 未知值，回落到 auto"
            );
            Ok(OcrBackendKind::Auto)
        }
    }
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            paddle_model: PaddleModel::default(),
            lifecycle: OcrLifecycle::default(),
            idle_ttl_seconds: default_idle_ttl(),
            compute_preference: default_compute_preference(),
        }
    }
}

impl OcrConfig {
    /// 校验配置完整性。
    ///
    /// 返回 `Err(message)` 如果有无效值。
    pub fn validate(&self) -> Result<(), String> {
        validate_idle_ttl(self.idle_ttl_seconds)?;

        // 0.22.4 §3.5: compute_preference 首版只允许 auto | cpu
        if !matches!(
            self.compute_preference,
            ComputePreference::Auto | ComputePreference::Cpu
        ) {
            return Err(format!(
                "compute_preference {:?} 未开放，首版只允许 auto 或 cpu",
                self.compute_preference
            ));
        }

        // paddleocr 模式只允许 production-ready 模型
        if matches!(
            self.backend,
            OcrBackendKind::PaddleOcr | OcrBackendKind::Auto
        ) && !self.paddle_model.is_production_ready()
        {
            return Err(format!(
                "paddle_model {:} 未通过生产资格门，只有 tiny 可用",
                self.paddle_model
            ));
        }

        Ok(())
    }

    /// 是否需要 PaddleOCR 引擎（backend 为 paddleocr 或 auto）。
    #[allow(dead_code)] // 预留：供未来路由逻辑使用
    pub fn needs_paddleocr(&self) -> bool {
        matches!(
            self.backend,
            OcrBackendKind::PaddleOcr | OcrBackendKind::Auto
        )
    }

    /// 转为运行时快照（不可变，每次 recognize 开始时快照）。
    pub fn to_snapshot(&self) -> OcrRuntimeSnapshot {
        OcrRuntimeSnapshot {
            backend: self.backend,
            paddle_model: self.paddle_model,
            lifecycle: self.lifecycle,
            idle_ttl_seconds: self.idle_ttl_seconds,
            compute_preference: self.compute_preference,
        }
    }
}

/// OCR 运行时快照（不可变）。
///
/// 每次 recognize 开始时从 `OcrConfig` 快照，
/// 中途修改配置不改变在途请求。
#[derive(Debug, Clone, Copy)]
pub struct OcrRuntimeSnapshot {
    pub backend: OcrBackendKind,
    /// 预留：当前快照逻辑不读取此字段，保留供未来诊断使用。
    #[allow(dead_code)]
    pub paddle_model: PaddleModel,
    pub lifecycle: OcrLifecycle,
    pub idle_ttl_seconds: u32,
    /// 预留：当前快照逻辑不读取此字段，保留供未来诊断使用。
    #[allow(dead_code)]
    pub compute_preference: ComputePreference,
}

impl OcrRuntimeSnapshot {
    /// 是否需要 PaddleOCR。
    pub fn needs_paddleocr(&self) -> bool {
        matches!(
            self.backend,
            OcrBackendKind::PaddleOcr | OcrBackendKind::Auto
        )
    }
}

impl From<&OcrConfig> for OcrRuntimeSnapshot {
    fn from(cfg: &OcrConfig) -> Self {
        cfg.to_snapshot()
    }
}

impl ConfigKey for OcrConfig {
    const KEY: &'static str = "ocr:config";
}

// ── 配置缓存 ──────────────────────────────────────────────────────────────

// Task 15: 改用 LazyLock<RwLock<OcrConfig>>——缓存始终存在，不会因 OnceLock set
// 失败而静默保留旧值。启动加载通过 replace 写入，更新通过 write 写入。
use std::sync::{LazyLock, RwLock};

static CONFIG_CACHE: LazyLock<RwLock<OcrConfig>> =
    LazyLock::new(|| RwLock::new(OcrConfig::default()));

/// 初始化配置缓存（main.rs 启动时调用）。
///
/// 从 SQLite ConfigStore 异步读取后注入缓存。
///
/// **Task 15**: 使用 `write().replace()` 替代 `OnceLock::set()`，
/// 保证多次调用不会静默失败——第二次调用会替换值而非被忽略。
pub fn init_cache(config: OcrConfig) {
    if let Ok(mut guard) = CONFIG_CACHE.write() {
        *guard = config;
    } else {
        tracing::error!("OcrConfig 缓存初始化失败：RwLock write poisoned");
    }
}

/// 从 SQLite ConfigStore 加载并初始化缓存。
///
/// **Task 15**: 可测试的启动加载 helper——与 main.rs 启动路径调用相同的逻辑。
/// 测试可传入 SQLite pool，验证加载 → 缓存 → coordinator snapshot 全链路。
///
/// `ConfigStore::get` 在不存在或解析失败时返回 `OcrConfig::default()`，
/// 所以此函数不会失败——但调用方可用返回值判断是否为默认值。
pub async fn load_and_init_cache(pool: &sqlx::SqlitePool) -> OcrConfig {
    let config: OcrConfig = crate::domain::config::store::ConfigStore::get(pool).await;
    init_cache(config.clone());
    config
}

/// 更新配置缓存（`set_ocr_config` 命令调用后同步）。
///
/// **Task 15**: 不再静默忽略写入失败——如果 RwLock poison 则记录错误。
pub fn update_cache(config: &OcrConfig) {
    if let Ok(mut guard) = CONFIG_CACHE.write() {
        *guard = config.clone();
    } else {
        tracing::error!("OcrConfig 缓存更新失败：RwLock write poisoned");
    }
}

/// 同步读取配置缓存。
///
/// **Task 15**: LazyLock 保证缓存始终存在，不会返回 "未初始化" 的 default。
/// 默认值仍为 `backend = auto`（0.22.10 起）。
pub fn get_ocr_config() -> OcrConfig {
    CONFIG_CACHE.read().map(|g| g.clone()).unwrap_or_default()
}

/// **Task 15**: 仅供测试——重置缓存为 default。
#[cfg(test)]
pub fn reset_cache_for_test() {
    if let Ok(mut guard) = CONFIG_CACHE.write() {
        *guard = OcrConfig::default();
    }
}

/// **Task 15**: 仅供测试——直接注入缓存值。
#[cfg(test)]
pub fn set_cache_for_test(config: OcrConfig) {
    if let Ok(mut guard) = CONFIG_CACHE.write() {
        *guard = config;
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_auto_backend() {
        let cfg = OcrConfig::default();
        assert_eq!(cfg.backend, OcrBackendKind::Auto);
        assert_eq!(cfg.paddle_model, PaddleModel::Tiny);
        assert_eq!(cfg.lifecycle, OcrLifecycle::OnDemand);
        assert_eq!(cfg.idle_ttl_seconds, 300);
        assert_eq!(cfg.compute_preference, ComputePreference::Auto);
    }

    #[test]
    fn serde_roundtrip() {
        let cfg = OcrConfig {
            backend: OcrBackendKind::PaddleOcr,
            paddle_model: PaddleModel::Tiny,
            lifecycle: OcrLifecycle::OnDemand,
            idle_ttl_seconds: 600,
            compute_preference: ComputePreference::Cpu,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: OcrConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.backend, back.backend);
        assert_eq!(cfg.paddle_model, back.paddle_model);
        assert_eq!(cfg.idle_ttl_seconds, back.idle_ttl_seconds);
        assert_eq!(cfg.compute_preference, back.compute_preference);
    }

    #[test]
    fn serde_unknown_backend_falls_back_to_auto() {
        let json = r#"{"backend":"unknown_value"}"#;
        let cfg: OcrConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.backend, OcrBackendKind::Auto);
    }

    #[test]
    fn serde_missing_all_fields_uses_defaults() {
        let json = r#"{}"#;
        let cfg: OcrConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.backend, OcrBackendKind::Auto);
        assert_eq!(cfg.paddle_model, PaddleModel::Tiny);
        assert_eq!(cfg.idle_ttl_seconds, 300);
    }

    #[test]
    fn validate_accepts_defaults() {
        let cfg = OcrConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_non_production_model_with_paddleocr() {
        let cfg = OcrConfig {
            backend: OcrBackendKind::PaddleOcr,
            paddle_model: PaddleModel::Small,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_invalid_idle_ttl() {
        let cfg = OcrConfig {
            idle_ttl_seconds: 5,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn needs_paddleocr_for_paddleocr_and_auto() {
        assert!(
            OcrConfig {
                backend: OcrBackendKind::PaddleOcr,
                ..Default::default()
            }
            .needs_paddleocr()
        );

        assert!(
            OcrConfig {
                backend: OcrBackendKind::Auto,
                ..Default::default()
            }
            .needs_paddleocr()
        );

        assert!(
            !OcrConfig {
                backend: OcrBackendKind::Windows,
                ..Default::default()
            }
            .needs_paddleocr()
        );
    }

    #[test]
    fn snapshot_is_immutable_copy() {
        let cfg = OcrConfig {
            backend: OcrBackendKind::Auto,
            idle_ttl_seconds: 120,
            ..Default::default()
        };
        let snap = cfg.to_snapshot();
        assert_eq!(snap.backend, OcrBackendKind::Auto);
        assert_eq!(snap.idle_ttl_seconds, 120);
        assert!(snap.needs_paddleocr());
    }

    #[test]
    fn validate_rejects_unsupported_compute_preference() {
        // cuda/vulkan/directml 在 0.22.4 未开放
        let cfg = OcrConfig {
            compute_preference: ComputePreference::Cuda,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());

        let cfg = OcrConfig {
            compute_preference: ComputePreference::Vulkan,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());

        let cfg = OcrConfig {
            compute_preference: ComputePreference::Directml,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_accepts_auto_and_cpu_only() {
        let cfg = OcrConfig {
            compute_preference: ComputePreference::Auto,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());

        let cfg = OcrConfig {
            compute_preference: ComputePreference::Cpu,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn serde_compute_preference_roundtrip() {
        let cfg = OcrConfig {
            compute_preference: ComputePreference::Cpu,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: OcrConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.compute_preference, back.compute_preference);
    }

    #[tokio::test]
    async fn sqlite_persistence_survives_restart() {
        // Task 3: 写入 paddleocr/auto，模拟重启后保持原值
        use crate::domain::config::store::ConfigStore;
        use sqlx::sqlite::SqlitePoolOptions;

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL DEFAULT 0
        )",
        )
        .execute(&pool)
        .await
        .expect("create config table");

        let config = OcrConfig {
            backend: OcrBackendKind::PaddleOcr,
            paddle_model: PaddleModel::Tiny,
            lifecycle: OcrLifecycle::KeepRunning,
            idle_ttl_seconds: 600,
            compute_preference: ComputePreference::Cpu,
        };

        // 写入
        ConfigStore::set(&pool, &config).await.expect("写入失败");

        // 模拟重启——清空内存缓存后重新读取
        let loaded: OcrConfig = ConfigStore::get(&pool).await;
        assert_eq!(loaded.backend, config.backend);
        assert_eq!(loaded.paddle_model, config.paddle_model);
        assert_eq!(loaded.lifecycle, config.lifecycle);
        assert_eq!(loaded.idle_ttl_seconds, config.idle_ttl_seconds);
        assert_eq!(loaded.compute_preference, config.compute_preference);
    }

    /// Task 15: 验证 load_and_init_cache 全链路——
    /// SQLite 写入 → load_and_init_cache → get_ocr_config 读取 → 值一致。
    #[tokio::test]
    async fn load_and_init_cache_full_chain() {
        use crate::domain::config::store::ConfigStore;
        use sqlx::sqlite::SqlitePoolOptions;

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL DEFAULT 0
        )",
        )
        .execute(&pool)
        .await
        .expect("create config table");

        // 写入非默认配置到 SQLite
        let config = OcrConfig {
            backend: OcrBackendKind::PaddleOcr,
            paddle_model: PaddleModel::Tiny,
            lifecycle: OcrLifecycle::KeepRunning,
            idle_ttl_seconds: 600,
            compute_preference: ComputePreference::Cpu,
        };
        ConfigStore::set(&pool, &config).await.expect("写入失败");

        // Task 15: 通过 load_and_init_cache 加载（与 main.rs 启动路径相同）
        let loaded = load_and_init_cache(&pool).await;
        assert_eq!(loaded.backend, config.backend);
        assert_eq!(loaded.compute_preference, config.compute_preference);

        // 验证内存缓存与 SQLite 一致
        let cached = get_ocr_config();
        assert_eq!(cached.backend, config.backend);
        assert_eq!(cached.compute_preference, config.compute_preference);
    }

    /// Task 15: 验证 default 值——SQLite 为空时 load_and_init_cache 返回 default。
    #[tokio::test]
    async fn load_and_init_cache_defaults_when_empty() {
        use sqlx::sqlite::SqlitePoolOptions;

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL DEFAULT 0
        )",
        )
        .execute(&pool)
        .await
        .expect("create config table");

        // 重置缓存为非默认值，验证 load 会覆盖
        set_cache_for_test(OcrConfig {
            backend: OcrBackendKind::PaddleOcr,
            ..Default::default()
        });

        // SQLite 无配置——load_and_init_cache 返回 default（0.22.10 起为 Auto）
        let loaded = load_and_init_cache(&pool).await;
        assert_eq!(loaded.backend, OcrBackendKind::Auto);
        assert_eq!(loaded.compute_preference, ComputePreference::Auto);

        // 缓存也应为 default
        let cached = get_ocr_config();
        assert_eq!(cached.backend, OcrBackendKind::Auto);

        // 清理
        reset_cache_for_test();
    }

    /// Task 15: 验证 update_cache 不静默失败——多次调用 init_cache 可覆盖。
    #[test]
    fn init_cache_replaceable() {
        // 第一次 init
        init_cache(OcrConfig {
            backend: OcrBackendKind::PaddleOcr,
            ..Default::default()
        });
        assert_eq!(get_ocr_config().backend, OcrBackendKind::PaddleOcr);

        // 第二次 init——应该覆盖，不被忽略
        init_cache(OcrConfig {
            backend: OcrBackendKind::Auto,
            ..Default::default()
        });
        assert_eq!(get_ocr_config().backend, OcrBackendKind::Auto);

        // update_cache 也应覆盖
        update_cache(&OcrConfig {
            backend: OcrBackendKind::Windows,
            ..Default::default()
        });
        assert_eq!(get_ocr_config().backend, OcrBackendKind::Windows);

        // 清理
        reset_cache_for_test();
    }
}
