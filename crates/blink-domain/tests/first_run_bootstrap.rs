//! 首次运行核心链路：空配置库 → 默认配置 → 随包插件配置 → 能力发布。
//! 不创建 GUI、不启动插件进程、不访问真实 AppData 或 Credential Manager。

use std::{path::Path, sync::Arc, time::Duration};

use blink_domain::{
    capability::CapabilityRegistry,
    config::{self, ConfigStore},
    intent::RuleRouter,
    plugin::{self, PluginEngine, PluginRuntimePublisher},
};

async fn initialize_from_empty_database(plugins_dir: &Path) {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();

    config::init_config(&pool).await.unwrap();
    // 欢迎页 get_config command 所有 checked 读取必须可用。
    ConfigStore::get_checked::<config::AppearanceConfig>(&pool).await.unwrap();
    ConfigStore::get_checked::<config::HotkeyConfig>(&pool).await.unwrap();
    ConfigStore::get_checked::<config::ChordConfig>(&pool).await.unwrap();
    ConfigStore::get_checked::<config::DisableConfig>(&pool).await.unwrap();
    ConfigStore::get_checked::<config::SearchConfig>(&pool).await.unwrap();
    ConfigStore::get_checked::<blink_infra::data::clipboard::ClipboardConfig>(&pool)
        .await
        .unwrap();
    let app_config = config::get_config(&pool).await;
    assert_eq!(app_config.onboarding_version, 0);

    let plugins = plugin::load_builtin_plugins(plugins_dir, None);
    let engine = Arc::new(PluginEngine::new(plugins.clone(), pool.clone(), None));
    engine.init_configs().await;
    for handle in &plugins {
        let saved = config::get_plugin_config(&pool, handle.id()).await.unwrap();
        assert_eq!(saved.enabled, handle.manifest().default_enabled);
        assert_eq!(engine.get_config(handle.id()).unwrap().settings, saved.settings);
    }

    let registry = Arc::new(CapabilityRegistry::default());
    let router = Arc::new(RuleRouter::new(app_config.surface_takeover_enabled));
    let publisher = PluginRuntimePublisher::new(engine.clone(), registry, router);
    publisher.publish(plugins.clone()).await.unwrap();
    assert_eq!(engine.all_plugins().len(), plugins.len());

    // 重启复读仍成功，不依赖先前进程的内存配置。
    let restarted = PluginEngine::new(plugins, pool.clone(), None);
    restarted.init_configs().await;
    ConfigStore::get_checked::<config::AppearanceConfig>(&pool).await.unwrap();
}

#[tokio::test]
async fn fresh_install_with_real_bundled_manifests() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/builtin");
    assert_eq!(plugin::load_builtin_plugins(&root, None).len(), 3);
    tokio::time::timeout(Duration::from_secs(5), initialize_from_empty_database(&root))
        .await
        .expect("首装配置及随包插件初始化超时");
}

#[tokio::test]
async fn fresh_install_without_plugin_directory() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing-plugins");
    assert!(plugin::load_builtin_plugins(&missing, None).is_empty());
    tokio::time::timeout(Duration::from_secs(5), initialize_from_empty_database(&missing))
        .await
        .expect("无插件时首装初始化超时");
}
