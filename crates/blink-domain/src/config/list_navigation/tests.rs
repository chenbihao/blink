use super::*;
use blink_infra::data::config::set_config;

async fn pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL DEFAULT 0)")
        .execute(&pool).await.unwrap();
    pool
}
async fn put(pool: &SqlitePool, key: &str, value: Value) {
    set_config(pool, key, &value.to_string()).await.unwrap();
}
async fn read(pool: &SqlitePool, key: &str) -> Value {
    serde_json::from_str(&config_value(pool, key).await.unwrap().unwrap()).unwrap()
}

#[tokio::test]
async fn migration_precedence_defaults_and_idempotence() {
    for (new, old, expected) in [
        (None, Some(json!("page")), "page"),
        (None, Some(json!("step")), "step"),
        (None, None, "step"),
        (None, Some(json!("invalid")), "step"),
        (None, Some(json!(12)), "step"),
        (Some("step"), Some(json!("page")), "step"),
        (Some("page"), Some(json!("step")), "page"),
    ] {
        let pool = pool().await;
        let mut search = json!({"page_size":7,"future_search":true});
        if let Some(mode) = new {
            search["wheel_mode"] = json!(mode);
        }
        put(&pool, "app.search", search).await;
        let mut clip = json!({"max_items":456,"future_clip":"keep"});
        if let Some(mode) = old {
            clip["wheel_mode"] = mode;
        }
        put(&pool, "clipboard:config", clip).await;
        migrate_wheel_mode(&pool).await.unwrap();
        let search = read(&pool, "app.search").await;
        let clip = read(&pool, "clipboard:config").await;
        assert_eq!(search["wheel_mode"], expected);
        assert_eq!(search["page_size"], 7);
        assert_eq!(search["future_search"], true);
        assert_eq!(clip["max_items"], 456);
        assert_eq!(clip["future_clip"], "keep");
        assert!(clip.get("wheel_mode").is_none());
        migrate_wheel_mode(&pool).await.unwrap();
        assert_eq!(read(&pool, "app.search").await, search);
        assert_eq!(read(&pool, "clipboard:config").await, clip);
    }
}

#[tokio::test]
async fn fresh_install_and_clipboard_only_install() {
    let pool = pool().await;
    migrate_wheel_mode(&pool).await.unwrap();
    assert!(config_value(&pool, "app.search").await.unwrap().is_none());
    put(
        &pool,
        "clipboard:config",
        json!({"wheel_mode":"page","max_items":88}),
    )
    .await;
    super::super::app_config::init_config(&pool).await.unwrap();
    assert_eq!(read(&pool, "app.search").await["wheel_mode"], "page");
    assert_eq!(read(&pool, "clipboard:config").await["max_items"], 88);
}

#[tokio::test]
async fn legacy_single_key_also_preserves_clipboard_preference() {
    let pool = pool().await;
    let mut legacy = serde_json::to_value(super::super::app_config::AppConfig::default()).unwrap();
    legacy.as_object_mut().unwrap().remove("wheel_mode");
    legacy["clipboard"]["wheel_mode"] = json!("page");
    put(&pool, "app_config", legacy).await;
    super::super::app_config::init_config(&pool).await.unwrap();
    assert_eq!(read(&pool, "app.search").await["wheel_mode"], "page");
    assert!(
        read(&pool, "clipboard:config")
            .await
            .get("wheel_mode")
            .is_none()
    );
    // 单 key 删除前崩溃留下旧记录，也不能覆盖已经保存的新偏好。
    put(&pool, "app_config", {
        let mut old = serde_json::to_value(super::super::app_config::AppConfig::default()).unwrap();
        old.as_object_mut().unwrap().remove("wheel_mode");
        old["clipboard"]["wheel_mode"] = json!("page");
        old
    })
    .await;
    put(&pool, "app.search", json!({"wheel_mode":"step"})).await;
    super::super::app_config::init_config(&pool).await.unwrap();
    assert_eq!(read(&pool, "app.search").await["wheel_mode"], "step");
}

#[tokio::test]
async fn corrupt_legacy_shapes_are_retained_without_defaults() {
    for raw in ["broken", "null", "[]", "12", "\"text\""] {
        let pool = pool().await;
        set_config(&pool, "app_config", raw).await.unwrap();
        assert!(super::super::app_config::init_config(&pool).await.is_err());
        assert_eq!(
            config_value(&pool, "app_config").await.unwrap().as_deref(),
            Some(raw)
        );
        assert!(config_value(&pool, "app.search").await.unwrap().is_none());
    }
}

#[tokio::test]
async fn corrupt_shards_and_read_errors_do_not_write_defaults() {
    for (key, invalid) in [
        ("app.search", "broken"),
        ("app.search", "[]"),
        ("app.search", r#"{"wheel_mode":"unknown"}"#),
        ("clipboard:config", "broken"),
        (
            "clipboard:config",
            r#"{"max_items":"broken","wheel_mode":"page"}"#,
        ),
    ] {
        let pool = pool().await;
        put(&pool, "app.search", json!({"page_size":7})).await;
        put(&pool, "clipboard:config", json!({"wheel_mode":"page"})).await;
        set_config(&pool, key, invalid).await.unwrap();
        let before_search = config_value(&pool, "app.search").await.unwrap();
        let before_clip = config_value(&pool, "clipboard:config").await.unwrap();
        assert!(migrate_wheel_mode(&pool).await.is_err());
        assert_eq!(
            config_value(&pool, "app.search").await.unwrap(),
            before_search
        );
        assert_eq!(
            config_value(&pool, "clipboard:config").await.unwrap(),
            before_clip
        );
    }
    let empty = SqlitePool::connect("sqlite::memory:").await.unwrap();
    assert!(migrate_wheel_mode(&empty).await.is_err());
}

#[tokio::test]
async fn failed_second_write_and_stale_baseline_roll_back_first_write() {
    let pool = pool().await;
    put(&pool, "app.search", json!({"page_size":7})).await;
    put(&pool, "clipboard:config", json!({"wheel_mode":"page"})).await;
    let search = config_value(&pool, "app.search").await.unwrap();
    let clip = config_value(&pool, "clipboard:config").await.unwrap();
    sqlx::query("CREATE TRIGGER reject_clip BEFORE UPDATE ON config WHEN NEW.key = 'clipboard:config' BEGIN SELECT RAISE(ABORT, 'test rejection'); END")
        .execute(&pool).await.unwrap();
    assert!(migrate_wheel_mode(&pool).await.is_err());
    assert_eq!(config_value(&pool, "app.search").await.unwrap(), search);
    assert_eq!(config_value(&pool, "clipboard:config").await.unwrap(), clip);
    sqlx::query("DROP TRIGGER reject_clip")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !compare_set_configs(
            &pool,
            &[
                ("app.search".into(), search.clone(), "{}".into()),
                ("clipboard:config".into(), Some("stale".into()), "{}".into()),
            ]
        )
        .await
        .unwrap()
    );
    assert_eq!(config_value(&pool, "app.search").await.unwrap(), search);
    migrate_wheel_mode(&pool).await.unwrap();
    assert_eq!(read(&pool, "app.search").await["wheel_mode"], "page");
}

#[tokio::test]
async fn field_updates_preserve_other_preferences_and_check_conflicts() {
    let pool = pool().await;
    put(
        &pool,
        "app.search",
        json!({"wheel_mode":"step","future":123}),
    )
    .await;
    let (wheel, page) = tokio::join!(
        update_wheel_mode(&pool, WheelMode::Page, Some(WheelMode::Step)),
        ConfigStore::update::<SearchConfig>(&pool, |cfg| {
            cfg.page_size = 7;
            Ok(())
        }),
    );
    wheel.unwrap();
    page.unwrap();
    assert_eq!(read(&pool, "app.search").await["wheel_mode"], "page");
    assert_eq!(read(&pool, "app.search").await["page_size"], 7);
    assert_eq!(read(&pool, "app.search").await["future"], 123);
    assert!(
        update_wheel_mode(&pool, WheelMode::Step, Some(WheelMode::Step))
            .await
            .unwrap_err()
            .starts_with("config_conflict:")
    );
    assert!(serde_json::from_value::<WheelMode>(json!("invalid")).is_err());
}
