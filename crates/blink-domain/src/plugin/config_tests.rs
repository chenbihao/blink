use super::*;
use crate::config::PluginConfig;
use crate::plugin::PluginManifest;
use crate::plugin::secret_update::SecretStore;
use blink_infra::platform::secret::SecretString;
use serde_json::{Value, json};
use sqlx::sqlite::SqlitePoolOptions;
use std::path::PathBuf;
use std::sync::Mutex;

const PLUGIN_ID: &str = "builtin.weather";

#[derive(Default)]
struct MemoryState {
    values: HashMap<(String, String), SecretString>,
    reads: usize,
    writes: usize,
    fail_write_at: Option<usize>,
    fail_reads: bool,
}

#[derive(Default)]
struct MemorySecretStore(Mutex<MemoryState>);

impl MemorySecretStore {
    fn seed(&self, plugin_id: &str, key: &str, value: &str) {
        self.0.lock().unwrap().values.insert(
            (plugin_id.to_string(), key.to_string()),
            SecretString::new(value.to_string()),
        );
    }

    fn value(&self, plugin_id: &str, key: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .values
            .get(&(plugin_id.to_string(), key.to_string()))
            .map(|value| value.expose().to_string())
    }

    fn fail_write_at(&self, call: usize) {
        self.0.lock().unwrap().fail_write_at = Some(call);
    }

    fn clear_failures(&self) {
        self.0.lock().unwrap().fail_write_at = None;
    }

    fn counts(&self) -> (usize, usize) {
        let state = self.0.lock().unwrap();
        (state.reads, state.writes)
    }
}

impl SecretStore for MemorySecretStore {
    fn read(&self, plugin_id: &str, key: &str) -> Result<Option<SecretString>, String> {
        let mut state = self.0.lock().unwrap();
        state.reads += 1;
        if state.fail_reads { return Err("injected secret read failure".into()); }
        Ok(state
            .values
            .get(&(plugin_id.to_string(), key.to_string()))
            .map(|value| SecretString::new(value.expose().to_string())))
    }

    fn write(
        &self,
        plugin_id: &str,
        key: &str,
        value: Option<&SecretString>,
    ) -> Result<(), String> {
        let mut state = self.0.lock().unwrap();
        state.writes += 1;
        if state.fail_write_at == Some(state.writes) {
            return Err("injected secret write failure".to_string());
        }

        let reference = (plugin_id.to_string(), key.to_string());
        match value {
            Some(secret) => {
                state
                    .values
                    .insert(reference, SecretString::new(secret.expose().to_string()));
            }
            None => {
                state.values.remove(&reference);
            }
        }
        Ok(())
    }
}

async fn setup_engine(
    initial: Option<PluginConfig>,
    cache_initial: bool,
) -> (PluginEngine, SqlitePool) {
    setup_engine_with_secrets(initial, cache_initial, &["api_key"]).await
}

async fn setup_engine_with_secrets(
    initial: Option<PluginConfig>,
    cache_initial: bool,
    secret_fields: &[&str],
) -> (PluginEngine, SqlitePool) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("内存 SQLite 应可用");
    sqlx::query(
        "CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)",
    )
    .execute(&pool)
    .await
    .expect("创建内存 config 表");

    let schema: Vec<Value> = secret_fields
        .iter()
        .map(|key| json!({"key": key, "type": "string", "title": "API key", "secret": true}))
        .collect();
    let manifest: PluginManifest = serde_json::from_value(json!({
        "schema_version": 1,
        "id": PLUGIN_ID,
        "name": "Test Weather",
        "version": "1.0.0",
        "runtime": {"exec": "never-start.exe"},
        "triggers": [],
        "capabilities": [],
        "settings_schema": schema
    }))
    .expect("测试 manifest 应合法");
    let plugin = Arc::new(PluginHandle::new(Arc::new(manifest), PathBuf::new(), None));
    let engine = PluginEngine::new(vec![plugin], pool.clone(), None);

    if let Some(config) = initial {
        save_config(&pool, &config).await;
        if cache_initial {
            engine
                .configs
                .write()
                .unwrap()
                .insert(PLUGIN_ID.to_string(), config);
        }
    }

    (engine, pool)
}

async fn save_config(pool: &SqlitePool, config: &PluginConfig) {
    let value = serde_json::to_string(config).expect("测试配置应可序列化");
    sqlx::query("INSERT INTO config (key, value, updated_at) VALUES (?1, ?2, 1)")
        .bind(format!("plugin:{PLUGIN_ID}"))
        .bind(value)
        .execute(pool)
        .await
        .expect("写入内存 config 表");
}

async fn database_config(pool: &SqlitePool) -> PluginConfig {
    let raw: (String,) = sqlx::query_as("SELECT value FROM config WHERE key = ?1")
        .bind(format!("plugin:{PLUGIN_ID}"))
        .fetch_one(pool)
        .await
        .expect("配置行应存在");
    serde_json::from_str(&raw.0).expect("存储的测试配置应可解析")
}

fn config(revision: u64, settings: Value) -> PluginConfig {
    PluginConfig {
        revision,
        settings,
        ..PluginConfig::default()
    }
}

fn weather_settings() -> Value {
    json!({
        "api_key": "",
        "default_city": "Osaka",
        "default_location": {
            "name": "Osaka",
            "latitude": 34.6937,
            "longitude": 135.5023,
            "coordinate_system": "wgs84",
            "country_code": "JP",
            "administrative_areas": ["Osaka"]
        },
        "unknown_future_setting": {"preserve": true}
    })
}

#[tokio::test]
async fn returned_revision_can_save_settings_after_a_trigger_only_update() {
    let initial = config(4, weather_settings());
    // 无 secret schema，不产生任何凭据读取/写入；持久化仅使用内存 SQLite。
    let (engine, pool) = setup_engine_with_secrets(Some(initial.clone()), true, &[]).await;
    let mut triggers = initial.clone();
    triggers.settings = json!({});
    triggers.disabled_default_triggers.push("天气".into());
    let revision = engine
        .update_config(PLUGIN_ID, triggers, None)
        .await
        .unwrap();
    assert_eq!(revision, 5);

    let stale = config(4, json!({"temperature_unit":"fahrenheit"}));
    assert!(
        engine
            .update_config(PLUGIN_ID, stale, None)
            .await
            .unwrap_err()
            .starts_with("config_conflict")
    );
    let mut settings = engine.get_config(PLUGIN_ID).unwrap();
    settings.revision = revision;
    settings.settings = json!({"temperature_unit":"fahrenheit"});
    assert_eq!(
        engine
            .update_config(PLUGIN_ID, settings, None)
            .await
            .unwrap(),
        6
    );
    let saved = database_config(&pool).await;
    assert_eq!(saved.settings["temperature_unit"], "fahrenheit");
    assert_eq!(
        saved.settings["default_location"],
        initial.settings["default_location"]
    );
    assert_eq!(saved.disabled_default_triggers, vec!["天气".to_string()]);
}

#[tokio::test]
async fn same_revision_concurrent_saves_only_commit_one() {
    let initial = config(0, json!({"api_key": ""}));
    let (engine, pool) = setup_engine(Some(initial.clone()), true).await;
    let store = MemorySecretStore::default();
    let mut first = config(0, json!({"api_key": "first-secret"}));
    let mut second = config(0, json!({"api_key": "second-secret"}));

    let (first_result, second_result) = tokio::join!(
        engine.update_config_with_store(PLUGIN_ID, &mut first, &store, vec![]),
        engine.update_config_with_store(PLUGIN_ID, &mut second, &store, vec![]),
    );

    assert_eq!(
        first_result.is_ok() as usize + second_result.is_ok() as usize,
        1,
        "同一 revision 并发保存只能有一个成功"
    );
    let first_won = first_result.is_ok();
    let conflict = if first_won {
        second_result.unwrap_err()
    } else {
        first_result.unwrap_err()
    };
    assert!(conflict.starts_with("config_conflict:"));
    assert_eq!(database_config(&pool).await.revision, 1);
    assert_eq!(engine.get_config(PLUGIN_ID).unwrap().revision, 1);
    assert_eq!(store.counts(), (1, 1));
    let winner_secret = if first_won {
        "first-secret"
    } else {
        "second-secret"
    };
    assert_eq!(
        store.value(PLUGIN_ID, "api_key").as_deref(),
        Some(winner_secret),
        "只有获胜请求的密钥应被写入"
    );
}

#[tokio::test]
async fn stale_revision_conflicts_before_secret_store_access() {
    let baseline = config(4, json!({"api_key": "", "default_city": "Osaka"}));
    let (engine, pool) = setup_engine(Some(baseline.clone()), true).await;
    let store = MemorySecretStore::default();
    store.seed(PLUGIN_ID, "api_key", "old-secret");
    let mut stale = config(3, json!({"api_key": "incoming-secret"}));

    let error = engine
        .update_config_with_store(PLUGIN_ID, &mut stale, &store, vec![])
        .await
        .unwrap_err();

    assert!(error.starts_with("config_conflict:"));
    assert_eq!(store.counts(), (0, 0));
    assert_eq!(database_config(&pool).await.revision, 4);
    assert_eq!(engine.get_config(PLUGIN_ID).unwrap().revision, 4);
}

#[tokio::test]
async fn settings_patch_preserves_structured_fields_and_keeps_explicit_clears() {
    let baseline = config(9, weather_settings());
    let expected_location = baseline.settings["default_location"].clone();
    let expected_unknown = baseline.settings["unknown_future_setting"].clone();
    let (engine, pool) = setup_engine(Some(baseline), true).await;
    let store = MemorySecretStore::default();

    let mut patch = config(9, json!({"temperature_unit": "celsius"}));
    engine
        .update_config_with_store(PLUGIN_ID, &mut patch, &store, vec![])
        .await
        .unwrap();
    let after_patch = database_config(&pool).await;
    assert_eq!(after_patch.settings["default_location"], expected_location);
    assert_eq!(
        after_patch.settings["unknown_future_setting"],
        expected_unknown
    );
    assert_eq!(after_patch.settings["default_city"], "Osaka");
    assert_eq!(after_patch.settings["temperature_unit"], "celsius");

    let mut clear = config(10, json!({"default_location": null, "default_city": ""}));
    engine
        .update_config_with_store(PLUGIN_ID, &mut clear, &store, vec![])
        .await
        .unwrap();
    let after_clear = database_config(&pool).await;
    assert!(after_clear.settings["default_location"].is_null());
    assert_eq!(after_clear.settings["default_city"], "");

    let mut unrelated = config(11, json!({"temperature_unit": "fahrenheit"}));
    engine
        .update_config_with_store(PLUGIN_ID, &mut unrelated, &store, vec![])
        .await
        .unwrap();
    let after_unrelated_patch = database_config(&pool).await;
    assert!(after_unrelated_patch.settings["default_location"].is_null());
    assert_eq!(after_unrelated_patch.settings["default_city"], "");
    assert_eq!(
        after_unrelated_patch.settings["unknown_future_setting"],
        expected_unknown
    );
    assert_eq!(store.counts(), (0, 0));
}

#[tokio::test]
async fn secret_write_failure_does_not_advance_database_or_memory_config() {
    let baseline = config(4, json!({"api_key": "", "default_city": "Osaka"}));
    let (engine, pool) = setup_engine(Some(baseline.clone()), true).await;
    let store = MemorySecretStore::default();
    store.seed(PLUGIN_ID, "api_key", "old-secret");
    store.fail_write_at(1);
    let mut incoming = config(4, json!({"api_key": "new-secret"}));

    let error = engine
        .update_config_with_store(PLUGIN_ID, &mut incoming, &store, vec![])
        .await
        .unwrap_err();

    assert!(error.contains("secret_write_failed"));
    let persisted = database_config(&pool).await;
    let cached = engine.get_config(PLUGIN_ID).unwrap();
    assert_eq!(persisted.revision, baseline.revision);
    assert_eq!(persisted.settings, baseline.settings);
    assert_eq!(cached.revision, baseline.revision);
    assert_eq!(cached.settings, baseline.settings);
    assert_eq!(
        store.value(PLUGIN_ID, "api_key").as_deref(),
        Some("old-secret")
    );
    assert_eq!(store.counts(), (1, 2));
}

#[tokio::test]
async fn sqlite_write_failure_restores_old_secret_and_keeps_config_snapshot() {
    let baseline = config(2, json!({"api_key": "", "default_city": "Osaka"}));
    let (engine, pool) = setup_engine(Some(baseline.clone()), true).await;
    let store = MemorySecretStore::default();
    store.seed(PLUGIN_ID, "api_key", "old-secret");
    sqlx::query(
        r#"CREATE TRIGGER reject_plugin_config BEFORE UPDATE ON config
           WHEN OLD.key = 'plugin:builtin.weather'
           BEGIN SELECT RAISE(FAIL, 'injected config failure'); END"#,
    )
    .execute(&pool)
    .await
    .expect("安装 SQLite 写失败触发器");
    let mut incoming = config(2, json!({"api_key": "new-secret"}));

    let error = engine
        .update_config_with_store(PLUGIN_ID, &mut incoming, &store, vec![])
        .await
        .unwrap_err();

    assert!(error.contains("plugin_config_write_failed"));
    let persisted = database_config(&pool).await;
    let cached = engine.get_config(PLUGIN_ID).unwrap();
    assert_eq!(persisted.revision, baseline.revision);
    assert_eq!(persisted.settings, baseline.settings);
    assert_eq!(cached.revision, baseline.revision);
    assert_eq!(cached.settings, baseline.settings);
    assert_eq!(
        store.value(PLUGIN_ID, "api_key").as_deref(),
        Some("old-secret")
    );
    assert_eq!(store.counts(), (1, 2));
}

#[tokio::test]
async fn config_read_failure_is_not_treated_as_missing_default() {
    let baseline = config(7, json!({"api_key": "", "default_city": "Osaka"}));
    let (engine, pool) = setup_engine(Some(baseline.clone()), false).await;
    let original = sqlx::query_scalar::<_, String>("SELECT value FROM config WHERE key = ?1")
        .bind(format!("plugin:{PLUGIN_ID}"))
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE config RENAME COLUMN value TO payload")
        .execute(&pool)
        .await
        .expect("模拟配置读取错误");

    let store = MemorySecretStore::default();
    let mut incoming = config(0, json!({"api_key": "must-not-be-written"}));
    let error = engine
        .update_config_with_store(PLUGIN_ID, &mut incoming, &store, vec![])
        .await
        .unwrap_err();

    assert!(error.contains("plugin_config_read_failed"));
    assert_eq!(store.counts(), (0, 0));
    assert_eq!(incoming.revision, 0);
    assert!(engine.get_config(PLUGIN_ID).is_none());
    let persisted = sqlx::query_scalar::<_, String>("SELECT payload FROM config WHERE key = ?1")
        .bind(format!("plugin:{PLUGIN_ID}"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(persisted, original);
}

/// 0.26.3：天气插件同时持有和风 Key 与彩云 App Secret 两个 secret 字段。
/// 一起更新时第二个 CM 写失败 → 两个字段都恢复旧值、revision 不推进；
/// 成功路径下一次快照读到的是同一代次的两个新值（不出现一新一旧）。
#[tokio::test]
async fn two_secret_fields_update_fails_together_and_rolls_back_both() {
    let fields = ["qweather_api_key", "caiyun_app_secret"];
    let baseline = config(
        3,
        json!({"qweather_api_key": "", "caiyun_app_secret": "", "default_city": "Osaka"}),
    );
    let (engine, pool) =
        setup_engine_with_secrets(Some(baseline.clone()), true, &fields).await;
    let store = MemorySecretStore::default();
    store.seed(PLUGIN_ID, "qweather_api_key", "old-qw-key");
    store.seed(PLUGIN_ID, "caiyun_app_secret", "old-cy-secret");
    // 第二个写失败（字段顺序来自 manifest secret_field_keys：qweather 先写成功）
    store.fail_write_at(2);
    let mut incoming = config(
        3,
        json!({"qweather_api_key": "new-qw-key", "caiyun_app_secret": "new-cy-secret"}),
    );

    let error = engine
        .update_config_with_store(PLUGIN_ID, &mut incoming, &store, vec![])
        .await
        .unwrap_err();
    assert_eq!(error, "secret_write_failed");

    // 两个密钥都恢复旧值；DB/内存 revision 与 settings 保持基线
    assert_eq!(
        store.value(PLUGIN_ID, "qweather_api_key").as_deref(),
        Some("old-qw-key")
    );
    assert_eq!(
        store.value(PLUGIN_ID, "caiyun_app_secret").as_deref(),
        Some("old-cy-secret")
    );
    let persisted = database_config(&pool).await;
    assert_eq!(persisted.revision, baseline.revision);
    assert_eq!(persisted.settings, baseline.settings);

    // 重试成功：revision 推进，两个字段同代次生效
    store.clear_failures();
    let mut incoming = config(
        3,
        json!({"qweather_api_key": "new-qw-key", "caiyun_app_secret": "new-cy-secret"}),
    );
    engine
        .update_config_with_store(PLUGIN_ID, &mut incoming, &store, vec![])
        .await
        .unwrap();
    assert_eq!(
        store.value(PLUGIN_ID, "qweather_api_key").as_deref(),
        Some("new-qw-key")
    );
    assert_eq!(
        store.value(PLUGIN_ID, "caiyun_app_secret").as_deref(),
        Some("new-cy-secret")
    );
    assert_eq!(database_config(&pool).await.revision, 4);
    assert_eq!(
        engine.get_config(PLUGIN_ID).unwrap().settings,
        json!({"qweather_api_key": "", "caiyun_app_secret": "", "default_city": "Osaka"})
    );
}


fn resolve_cached_for_test(engine: &PluginEngine, store: &MemorySecretStore) -> Value {
    let config = engine.get_config(PLUGIN_ID).unwrap();
    let mut settings = config.settings;
    engine.secret_cache.restore(PLUGIN_ID, config.revision, engine.plugin_set().generation,
        &engine.secret_field_keys(PLUGIN_ID), &mut settings, store);
    settings
}

#[tokio::test]
async fn plugin_secret_snapshot_reads_once_and_never_enters_public_config() {
    let (engine, _) = setup_engine_with_secrets(Some(config(4, json!({"api_key":"", "optional_key":""}))),
        true, &["api_key", "optional_key"]).await;
    let store = MemorySecretStore::default();
    store.seed(PLUGIN_ID, "api_key", "first-secret");
    for _ in 0..8 {
        let settings = resolve_cached_for_test(&engine, &store);
        assert_eq!(settings["api_key"], "first-secret");
        assert_eq!(settings["optional_key"], "");
    }
    assert_eq!(store.counts(), (2, 0), "configured and missing keys are each read once");
    assert_eq!(engine.get_settings(PLUGIN_ID).unwrap()["api_key"], "");
    assert!(engine.secret_cache.hint(PLUGIN_ID, 4, engine.plugin_set().generation, "api_key").unwrap().is_some());
    assert_eq!(engine.secret_cache.hint(PLUGIN_ID, 4, engine.plugin_set().generation, "optional_key"), Some(None));
    let reads = store.counts().0;
    engine.replace_plugins(engine.all_plugins());
    resolve_cached_for_test(&engine, &store);
    assert_eq!(store.counts().0, reads + 2, "runtime publication invalidates the snapshot");
}

#[tokio::test]
async fn credential_save_clear_disable_and_failed_update_invalidate_cache() {
    let (engine, _) = setup_engine(Some(config(4, json!({"api_key":""}))), true).await;
    let store = MemorySecretStore::default();
    store.seed(PLUGIN_ID, "api_key", "old-secret");
    assert_eq!(resolve_cached_for_test(&engine, &store)["api_key"], "old-secret");
    let mut saved = config(4, json!({"api_key":"new-secret"}));
    engine.update_config_with_store(PLUGIN_ID, &mut saved, &store, vec![]).await.unwrap();
    assert_eq!(engine.secret_cache.hint(PLUGIN_ID, 4, engine.plugin_set().generation, "api_key"), None);
    assert_eq!(resolve_cached_for_test(&engine, &store)["api_key"], "new-secret");
    let mut cleared = config(5, json!({"api_key":""}));
    engine.update_config_with_store(PLUGIN_ID, &mut cleared, &store,
        vec![("api_key".into(), None)]).await.unwrap();
    assert_eq!(resolve_cached_for_test(&engine, &store)["api_key"], "");
    let mut disabled = config(6, json!({})); disabled.enabled = false;
    engine.update_config_with_store(PLUGIN_ID, &mut disabled, &store, vec![]).await.unwrap();
    assert_eq!(engine.secret_cache.hint(PLUGIN_ID, 6, engine.plugin_set().generation, "api_key"), None);

    store.seed(PLUGIN_ID, "api_key", "restored-secret");
    resolve_cached_for_test(&engine, &store);
    store.fail_write_at(store.counts().1 + 1);
    let mut failed = config(7, json!({"api_key":"must-not-survive"}));
    assert!(engine.update_config_with_store(PLUGIN_ID, &mut failed, &store, vec![]).await.is_err());
    assert_eq!(resolve_cached_for_test(&engine, &store)["api_key"], "restored-secret");
}

#[tokio::test]
async fn failed_credential_read_is_retried_instead_of_cached_as_missing() {
    let (engine, _) = setup_engine(Some(config(1, json!({"api_key":""}))), true).await;
    let store = MemorySecretStore::default();
    store.seed(PLUGIN_ID, "api_key", "available-after-retry");
    store.0.lock().unwrap().fail_reads = true;
    assert_eq!(resolve_cached_for_test(&engine, &store)["api_key"], "");
    store.0.lock().unwrap().fail_reads = false;
    assert_eq!(resolve_cached_for_test(&engine, &store)["api_key"], "available-after-retry");
    let reads = store.counts().0;
    resolve_cached_for_test(&engine, &store);
    assert_eq!(store.counts().0, reads);
}


#[tokio::test]
async fn manifest_secret_fields_are_part_of_the_cached_snapshot_identity() {
    let (engine, _) = setup_engine(Some(config(1, json!({"api_key":""}))), true).await;
    let store = MemorySecretStore::default();
    store.seed(PLUGIN_ID, "api_key", "original");
    store.seed(PLUGIN_ID, "new_field", "new-value");
    resolve_cached_for_test(&engine, &store);
    let mut settings = json!({});
    engine.secret_cache.restore(PLUGIN_ID, 1, engine.plugin_set().generation,
        &["api_key".into(), "new_field".into()], &mut settings, &store);
    assert_eq!(settings["new_field"], "new-value");
    assert_eq!(store.counts().0, 3, "a manifest change cannot reuse a different secret field set");
}
