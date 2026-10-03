//! ConfigKey trait + ConfigStore（0.8.6 §8.1.3）——配置分片标识 + 泛型存取。
//!
//! 0.14.6 §2.1 从 `app/config.rs` 迁入此域。所有 ConfigKey impl 随对应 struct 迁入，
//! 此文件只保留 trait 定义 + ConfigStore + 外部类型（ClipboardConfig）的 impl。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

// ── ConfigKey trait + ConfigStore（0.8.6 §8.1.3）────────────────────────────────

/// 配置分片标识 trait（0.8.6 §8.1.3）。
///
/// 每个配置分片实现此 trait，声明自己的 KV key。
/// `ConfigStore<T>` 用 `T::KEY` 做 SQLite 存取。
#[allow(dead_code)]
pub trait ConfigKey:
    Serialize + for<'de> Deserialize<'de> + Default + Send + Sync + 'static
{
    /// SQLite config 表的 key（如 `"app_config"` / `"app.hotkey"`）。
    const KEY: &'static str;
}

/// 泛型配置存取（0.8.6 §8.1.3）。
///
/// `ConfigStore<T>` 是无状态的——所有操作直接走 SQLite，不持连接池。
/// 调用方传 `&SqlitePool`。
#[allow(dead_code)]
pub struct ConfigStore;

impl ConfigStore {
    pub async fn get_checked<T: ConfigKey>(pool: &SqlitePool) -> Result<T, String> {
        match blink_infra::data::config::config_value(pool, T::KEY)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(raw) => {
                serde_json::from_str(&raw).map_err(|e| format!("配置 {} 无效: {e}", T::KEY))
            }
            None => Ok(T::default()),
        }
    }
    /// 只修改目标分片。冲突重读最新值再应用字段变更，保留未知字段。
    /// 闭包可能重复调用，必须无外部副作用；损坏的分片禁止默认值回写。
    pub async fn update<T: ConfigKey>(
        pool: &SqlitePool,
        update: impl Fn(&mut T) -> Result<(), String>,
    ) -> Result<T, String> {
        for _ in 0..16 {
            let previous = blink_infra::data::config::config_value(pool, T::KEY)
                .await
                .map_err(|e| e.to_string())?;
            let mut current: T = match previous.as_deref() {
                Some(raw) => serde_json::from_str(raw)
                    .map_err(|e| format!("配置 {} 无效，未写入: {e}", T::KEY))?,
                None => T::default(),
            };
            update(&mut current)?;
            let mut next = previous
                .as_deref()
                .map(serde_json::from_str::<serde_json::Value>)
                .transpose()
                .map_err(|e| e.to_string())?
                .unwrap_or_else(|| serde_json::json!({}));
            let fields = serde_json::to_value(&current).map_err(|e| e.to_string())?;
            if let (Some(target), Some(fields)) = (next.as_object_mut(), fields.as_object()) {
                target.extend(fields.clone());
            } else {
                next = fields;
            }
            let json = serde_json::to_string(&next).map_err(|e| e.to_string())?;
            if blink_infra::data::config::compare_set_config(
                pool,
                T::KEY,
                previous.as_deref(),
                &json,
            )
            .await
            .map_err(|e| e.to_string())?
            {
                return Ok(current);
            }
        }
        Err(format!(
            "config_conflict: 配置 {} 已变化，请重新读取后重试",
            T::KEY
        ))
    }

    /// 读取配置分片。不存在或解析失败返回 `T::default()`。
    #[allow(dead_code)]
    pub async fn get<T: ConfigKey>(pool: &SqlitePool) -> T {
        blink_infra::data::history::get_config(pool, T::KEY)
            .await
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    /// 写入配置分片。
    #[allow(dead_code)]
    pub async fn set<T: ConfigKey>(pool: &SqlitePool, config: &T) -> Result<(), String> {
        let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
        blink_infra::data::history::set_config(pool, T::KEY, &json)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ── 外部类型的 ConfigKey impl（类型定义不在本域的）─────────────────────────────

impl ConfigKey for blink_infra::data::clipboard::ClipboardConfig {
    /// 0.8.8 §8.7:剪贴板配置从原 `app_config.clipboard` nested 字段独立提升为 KV,
    /// 与 6 个 AppConfig 分片同级(但不属于 `app.*` 命名空间,归到 `clipboard:*`)。
    const KEY: &'static str = "clipboard:config";
}
