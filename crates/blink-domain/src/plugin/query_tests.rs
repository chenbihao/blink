//! Async search results may commit only while their configuration snapshot is current.
use super::*;

async fn query_test_engine() -> PluginEngine {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("内存 SQLite 应可用");
    let manifest: crate::plugin::PluginManifest = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "id": "test.query-race",
        "name": "Query race test",
        "version": "1",
        "runtime": {"exec": "never-start.exe", "concurrency": 1},
        "triggers": [],
        "capabilities": []
    }))
    .expect("最小测试 manifest 有效");
    let plugin = Arc::new(PluginHandle::new(
        Arc::new(manifest),
        std::path::PathBuf::new(),
        None,
    ));
    let engine = PluginEngine::new(vec![plugin], pool, None);
    engine.configs.write().unwrap().insert(
        "test.query-race".into(),
        crate::config::PluginConfig {
            revision: 1,
            enabled: true,
            settings: serde_json::json!({"endpoint":"test"}),
            ..Default::default()
        },
    );
    engine
}

fn query_test_item() -> PluginItem {
    PluginItem {
        title: "late result".into(),
        subtitle: None,
        score: 0.5,
        action: PluginAction::Copy {
            text: "late result".into(),
        },
        payload: None,
    }
}

#[tokio::test]
async fn query_subset_discards_result_if_plugin_is_disabled_while_query_runs() {
    let engine = Arc::new(query_test_engine().await);
    let query_started = Arc::new(tokio::sync::Notify::new());
    let resume_query = Arc::new(tokio::sync::Notify::new());
    let resume_timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let query = {
        let query_started = Arc::clone(&query_started);
        let resume_query = Arc::clone(&resume_query);
        let resume_timed_out = Arc::clone(&resume_timed_out);
        move |_plugin: Arc<PluginHandle>, _arg, _context, _settings| {
            let query_started = Arc::clone(&query_started);
            let resume_query = Arc::clone(&resume_query);
            let resume_timed_out = Arc::clone(&resume_timed_out);
            async move {
                query_started.notify_one();
                if tokio::time::timeout(std::time::Duration::from_secs(2), resume_query.notified())
                    .await
                    .is_err()
                {
                    resume_timed_out.store(true, std::sync::atomic::Ordering::SeqCst);
                    panic!("test did not release the delayed plugin query before timeout");
                }
                vec![query_test_item()]
            }
        }
    };
    let candidates = vec![("test.query-race".to_owned(), "q".to_owned())];
    let context = PluginQueryContext::default();
    let mut task = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.query_subset_with(&candidates, &context, query).await })
    };

    tokio::select! {
        result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            query_started.notified(),
        ) => {
            result.expect("injected plugin query should start before timeout");
        }
        result = &mut task => {
            panic!("query task ended before signaling start: {result:?}");
        }
    }
    {
        let mut configs = engine.configs.write().unwrap();
        let config = configs.get_mut("test.query-race").unwrap();
        config.revision = 2;
        config.enabled = false;
    }
    resume_query.notify_one();

    let results = tokio::time::timeout(std::time::Duration::from_secs(2), &mut task)
        .await
        .expect("query task should finish after it is released")
        .expect("query task must not panic");
    assert!(
        !resume_timed_out.load(std::sync::atomic::Ordering::SeqCst),
        "the injected query's resume wait must not time out"
    );
    assert!(results.is_empty(), "禁用后完成的查询不得返回迟到结果");
}

#[tokio::test]
async fn query_subset_discards_result_from_an_old_settings_revision() {
    let engine = Arc::new(query_test_engine().await);
    let query_started = Arc::new(tokio::sync::Notify::new());
    let resume_query = Arc::new(tokio::sync::Notify::new());
    let resume_timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_revision = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let query = {
        let query_started = Arc::clone(&query_started);
        let resume_query = Arc::clone(&resume_query);
        let resume_timed_out = Arc::clone(&resume_timed_out);
        let observed_revision = Arc::clone(&observed_revision);
        move |_plugin: Arc<PluginHandle>, _arg, _context, settings: Option<serde_json::Value>| {
            let query_started = Arc::clone(&query_started);
            let resume_query = Arc::clone(&resume_query);
            let resume_timed_out = Arc::clone(&resume_timed_out);
            let observed_revision = Arc::clone(&observed_revision);
            async move {
                observed_revision.store(
                    settings
                        .as_ref()
                        .and_then(|value| value["settings_revision"].as_u64())
                        .unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                query_started.notify_one();
                if tokio::time::timeout(std::time::Duration::from_secs(2), resume_query.notified())
                    .await
                    .is_err()
                {
                    resume_timed_out.store(true, std::sync::atomic::Ordering::SeqCst);
                    panic!("test did not release the delayed plugin query before timeout");
                }
                vec![query_test_item()]
            }
        }
    };
    let candidates = vec![("test.query-race".to_owned(), "q".to_owned())];
    let context = PluginQueryContext::default();
    let mut task = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.query_subset_with(&candidates, &context, query).await })
    };

    tokio::select! {
        result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            query_started.notified(),
        ) => {
            result.expect("injected plugin query should start before timeout");
        }
        result = &mut task => {
            panic!("query task ended before signaling start: {result:?}");
        }
    }
    {
        let mut configs = engine.configs.write().unwrap();
        let config = configs.get_mut("test.query-race").unwrap();
        config.revision = 2;
    }
    resume_query.notify_one();

    let results = tokio::time::timeout(std::time::Duration::from_secs(2), &mut task)
        .await
        .expect("query task should finish after it is released")
        .expect("query task must not panic");
    assert!(
        !resume_timed_out.load(std::sync::atomic::Ordering::SeqCst),
        "the injected query's resume wait must not time out"
    );
    assert_eq!(
        observed_revision.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "插件查询必须持有启动时的 settings revision"
    );
    assert!(results.is_empty(), "旧 revision 的查询不得覆盖新配置结果");
}
