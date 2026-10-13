//! 剪贴板字段更新：复用分片 CAS，禁止整片旧快照覆盖未修改字段。

use blink_infra::data::clipboard::ClipboardConfig;
use serde_json::Value;
use sqlx::SqlitePool;

use super::ConfigStore;

pub async fn update_clipboard_config(
    pool: &SqlitePool,
    patch: Value,
    expected: Option<Value>,
) -> Result<ClipboardConfig, String> {
    let fields = patch.as_object().ok_or("剪贴板配置必须为字段对象")?;
    let expected = expected
        .as_ref()
        .map(|v| v.as_object().ok_or("剪贴板配置基线必须为字段对象"))
        .transpose()?;
    if let Some(mode) = fields.get("wheel_mode")
        && mode != "step"
        && mode != "page"
    {
        return Err("剪贴板滚轮行为必须为 step 或 page".into());
    }
    ConfigStore::update::<ClipboardConfig>(pool, |cfg| {
        let mut current = serde_json::to_value(&*cfg).map_err(|e| e.to_string())?;
        for (key, value) in fields {
            if current.get(key).is_none() {
                return Err(format!("未知剪贴板配置字段: {key}"));
            }
            if let Some(old) = expected.and_then(|map| map.get(key))
                && current.get(key) != Some(old)
            {
                return Err(format!(
                    "config_conflict: 剪贴板配置 {key} 已变化，请重新读取后重试"
                ));
            }
            current[key] = value.clone();
        }
        *cfg = serde_json::from_value(current).map_err(|e| e.to_string())?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use blink_infra::data::{config::config_value, history::set_config};
    use serde_json::json;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL DEFAULT 0)")
            .execute(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn field_patch_preserves_other_preferences_and_unknown_fields() {
        let pool = pool().await;
        let mut saved = serde_json::to_value(ClipboardConfig::default()).unwrap();
        saved["enabled"] = json!(false);
        saved["max_image_items"] = json!(77);
        saved["future_setting"] = json!("keep");
        set_config(&pool, "clipboard:config", &saved.to_string())
            .await
            .unwrap();
        let cfg = update_clipboard_config(
            &pool,
            json!({"wheel_mode":"page"}),
            Some(json!({"wheel_mode":"step"})),
        )
        .await
        .unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_image_items, 77);
        assert_eq!(cfg.wheel_mode, "page");
        let raw: Value = serde_json::from_str(
            &config_value(&pool, "clipboard:config")
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(raw["future_setting"], "keep");
        // 另一字段写入不会重置刚保存的滚轮模式。
        let cfg = update_clipboard_config(&pool, json!({"enabled":true}), None)
            .await
            .unwrap();
        assert_eq!(cfg.wheel_mode, "page");
    }

    #[tokio::test]
    async fn rejects_conflicts_invalid_modes_and_corrupt_config_without_overwrite() {
        let pool = pool().await;
        update_clipboard_config(&pool, json!({"wheel_mode":"page"}), None)
            .await
            .unwrap();
        let before = config_value(&pool, "clipboard:config").await.unwrap();
        let err = update_clipboard_config(
            &pool,
            json!({"wheel_mode":"step"}),
            Some(json!({"wheel_mode":"step"})),
        )
        .await
        .unwrap_err();
        assert!(err.starts_with("config_conflict:"));
        assert!(
            update_clipboard_config(&pool, json!({"wheel_mode":"other"}), None)
                .await
                .is_err()
        );
        assert_eq!(
            config_value(&pool, "clipboard:config").await.unwrap(),
            before
        );
        set_config(&pool, "clipboard:config", "corrupt")
            .await
            .unwrap();
        assert!(
            update_clipboard_config(&pool, json!({"enabled":true}), None)
                .await
                .is_err()
        );
        assert_eq!(
            config_value(&pool, "clipboard:config")
                .await
                .unwrap()
                .as_deref(),
            Some("corrupt")
        );
    }
}
