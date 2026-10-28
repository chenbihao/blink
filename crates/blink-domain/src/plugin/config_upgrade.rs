//! 配置加载阶段的一次性升级；查询不改配置，CAS 防止覆盖同时发生的人工修改。
use crate::config::PluginConfig;
use serde_json::Value;
use sqlx::SqlitePool;

const MARKER: &str = "target_lang_defaults_version";

fn upgrade_settings(id: &str, settings: &mut Value) -> bool {
    if id != "builtin.translate" { return false; }
    let Some(obj) = settings.as_object_mut() else { return false };
    if obj.get(MARKER).and_then(Value::as_u64).unwrap_or(0) >= 1 { return false; }
    // 旧版默认 zh 已落库，改 manifest 默认无法生效；只升级一次，后续人工选 zh 保留。
    if obj.get("target_lang").and_then(Value::as_str).is_none_or(|lang| lang == "zh") {
        obj.insert("target_lang".into(), Value::String("auto".into()));
    }
    obj.insert(MARKER.into(), Value::from(1));
    true
}

pub(super) async fn translation_defaults(
    pool: &SqlitePool,
    id: &str,
    fallback: PluginConfig,
) -> PluginConfig {
    if id != "builtin.translate" { return fallback; }
    let key = format!("plugin:{id}");
    let mut latest = fallback;
    for _ in 0..3 {
        let baseline = match blink_infra::data::config::config_value(pool, &key).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(plugin = id, %error, "翻译默认方向升级读取失败");
                return latest;
            }
        };
        if let Some(raw) = baseline.as_deref() {
            match serde_json::from_str::<PluginConfig>(raw) {
                Ok(config) => latest = config,
                Err(_) => {
                    tracing::warn!(plugin = id, "翻译默认方向升级跳过无效配置");
                    return latest;
                }
            }
        }
        let mut upgraded = latest.clone();
        if !upgrade_settings(id, &mut upgraded.settings) { return latest; }
        let Some(revision) = latest.revision.checked_add(1) else { return latest };
        upgraded.revision = revision;
        let Ok(serialized) = serde_json::to_string(&upgraded) else { return latest };
        match blink_infra::data::config::compare_set_config(pool, &key, baseline.as_deref(), &serialized).await {
            Ok(true) => {
                tracing::info!(plugin = id, revision, "翻译默认方向兼容升级已完成");
                return upgraded;
            }
            Ok(false) => continue,
            Err(error) => {
                tracing::warn!(plugin = id, %error, "翻译默认方向升级写入失败");
                return latest;
            }
        }
    }
    tracing::warn!(plugin = id, "翻译默认方向升级遇到并发修改，下次加载重试");
    latest
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_chinese_upgrades_once_and_later_explicit_choices_survive() {
        let mut settings = json!({"target_lang":"zh", "default_engine":"ali", "api_key":"", "future":{"keep":true}});
        assert!(upgrade_settings("builtin.translate", &mut settings));
        assert_eq!(settings["target_lang"], "auto");
        assert_eq!(settings["future"]["keep"], true);
        settings["target_lang"] = json!("zh");
        assert!(!upgrade_settings("builtin.translate", &mut settings));
        assert_eq!(settings["target_lang"], "zh");
        for target in ["en", "ja", "ko", "auto"] {
            let mut settings = json!({"target_lang":target});
            assert!(upgrade_settings("builtin.translate", &mut settings));
            assert_eq!(settings["target_lang"], target);
            settings["target_lang"] = json!("zh");
            assert!(!upgrade_settings("builtin.translate", &mut settings));
        }
        let mut other = json!({"target_lang":"zh"});
        assert!(!upgrade_settings("builtin.other", &mut other));
        assert_eq!(other, json!({"target_lang":"zh"}));
    }

    #[tokio::test]
    async fn startup_upgrade_commits_revision_and_is_idempotent() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
            .connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)")
            .execute(&pool).await.unwrap();
        let original = PluginConfig {revision: 7, enabled: true,
            settings: json!({"target_lang":"zh","default_engine":"ali"}), ..Default::default()};
        crate::config::set_plugin_config(&pool, "builtin.translate", &original).await.unwrap();
        let upgraded = translation_defaults(&pool, "builtin.translate", original).await;
        assert_eq!(upgraded.revision, 8);
        assert_eq!(upgraded.settings["target_lang"], "auto");
        let stored = crate::config::get_plugin_config(&pool, "builtin.translate").await.unwrap();
        assert_eq!(stored.settings, upgraded.settings);
        assert_eq!(stored.revision, 8);
        assert_eq!(translation_defaults(&pool, "builtin.translate", stored).await.revision, 8);
    }

    #[tokio::test]
    async fn engine_load_exposes_upgraded_target_to_queries_and_pages() {
        use super::super::{PluginEngine, PluginHandle, PluginManifest, PluginSettingResolver};
        use std::sync::Arc;

        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
            .connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)")
            .execute(&pool).await.unwrap();
        let manifest: PluginManifest = serde_json::from_value(json!({
            "schema_version":1, "id":"builtin.translate", "name":"Translate", "version":"0.4.1",
            "runtime":{"exec":"never-start.exe"}, "triggers":[], "capabilities":[], "settings_schema":[]
        })).unwrap();
        let handle = Arc::new(PluginHandle::new(Arc::new(manifest), std::path::PathBuf::new(), None));
        let original = PluginConfig {revision: 7, enabled: true,
            settings: json!({"target_lang":"zh"}), ..Default::default()};
        crate::config::set_plugin_config(&pool, "builtin.translate", &original).await.unwrap();
        let engine = PluginEngine::new(vec![handle.clone()], pool.clone(), None);
        engine.init_configs().await;
        assert_eq!(engine.get_settings("builtin.translate").unwrap()["target_lang"], "auto");
        assert_eq!(engine.get_string("builtin.translate", "target_lang").as_deref(), Some("auto"));
        assert_eq!(engine.resolved_settings("builtin.translate").await.unwrap()["target_lang"], "auto");

        // 之后人工再选固定中文：补丁保留升级标记，下次启动不得再覆盖。
        let mut fixed = engine.get_config("builtin.translate").unwrap();
        fixed.settings = json!({"target_lang":"zh"});
        engine.update_config("builtin.translate", fixed, None).await.unwrap();
        let next = PluginEngine::new(vec![handle.clone()], pool.clone(), None);
        next.init_configs().await;
        assert_eq!(next.get_settings("builtin.translate").unwrap()["target_lang"], "zh");

        // 运行期安装路径也必须完成同一升级，而不是只有主启动路径生效。
        crate::config::set_plugin_config(&pool, "builtin.translate", &original).await.unwrap();
        let installed = PluginEngine::new(vec![handle.clone()], pool, None);
        installed.ensure_config_for(&handle).await;
        assert_eq!(installed.get_settings("builtin.translate").unwrap()["target_lang"], "auto");
    }

    #[tokio::test]
    async fn write_failure_keeps_original_default_and_revision() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
            .connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)")
            .execute(&pool).await.unwrap();
        let original = PluginConfig {revision: 7, settings: json!({"target_lang":"zh"}), ..Default::default()};
        crate::config::set_plugin_config(&pool, "builtin.translate", &original).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_upgrade BEFORE UPDATE ON config BEGIN SELECT RAISE(FAIL, 'test failure'); END")
            .execute(&pool).await.unwrap();
        let result = translation_defaults(&pool, "builtin.translate", original).await;
        assert_eq!(result.revision, 7);
        assert_eq!(result.settings, json!({"target_lang":"zh"}));
    }
}
