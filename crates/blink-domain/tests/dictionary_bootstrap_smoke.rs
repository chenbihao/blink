//! 显式运行的真实插件进程冒烟；存储全部注入临时目录，不触碰用户 AppData。
use std::{path::PathBuf, sync::Arc, time::Duration};

use blink_domain::plugin::{
    BeginOutcome, BootstrapManager, DatasetInstaller, HttpResourceTransport, OpPhase, PluginEngine,
    PluginHandle, PluginManifest,
};
use blink_infra::plugin_storage::PluginStorage;

#[tokio::test]
#[ignore = "需要 cargo xtask pack 生成的真实词典 exe 和联网下载锁定源"]
async fn real_dictionary_bootstrap_smoke() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let package = root.join("plugins/dictionary");
    let executable = package.join("bin/blink-plugin-dictionary.exe");
    assert!(executable.is_file(), "先运行 cargo xtask pack");
    let mut manifest = PluginManifest::from_path(&package.join("manifest.json")).unwrap();
    // 直指本次打包的 release exe，避免 debug 路径兜底误选旧二进制。
    manifest.runtime.exec = executable
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let temp = tempfile::tempdir().unwrap();
    let storage = PluginStorage::new(temp.path());
    storage.ensure_layout().unwrap();
    let handle = Arc::new(PluginHandle::new(Arc::new(manifest), package, None));
    handle.set_dirs(blink_domain::plugin::PluginDirs {
        data: storage
            .data_dir("builtin.dictionary")
            .to_string_lossy()
            .into_owned(),
        cache: storage
            .cache_dir("builtin.dictionary")
            .to_string_lossy()
            .into_owned(),
        datasets: storage
            .datasets_dir("builtin.dictionary")
            .to_string_lossy()
            .into_owned(),
    });
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let engine = Arc::new(PluginEngine::new(vec![handle.clone()], pool, None));
    let installer = Arc::new(DatasetInstaller::new(storage.clone()));
    let manager = BootstrapManager::new(
        engine.clone(),
        storage,
        installer,
        Arc::new(HttpResourceTransport),
        tokio::runtime::Handle::current(),
    );
    engine.set_bootstrap_sender(manager.aux_sender());
    let mut progress = manager.subscribe();
    // 与安装后自动触发一致，同时准备基础库和词源库。
    for dataset in ["dictionary-basic", "dictionary-etymology"] {
        assert!(matches!(
            manager
                .begin("builtin.dictionary", dataset, "auto_install")
                .await
                .unwrap(),
            BeginOutcome::Started(_)
        ));
    }
    let result = tokio::time::timeout(Duration::from_secs(600), async {
        loop {
            let states = manager.status("builtin.dictionary");
            if states.len() == 2 && states.iter().all(|s| s.phase.is_terminal()) {
                for state in &states {
                    assert!(
                        matches!(state.phase, OpPhase::Succeeded { .. }),
                        "{state:?}"
                    );
                }
                break;
            }
            match tokio::time::timeout(Duration::from_secs(2), progress.recv()).await {
                Ok(Ok(snapshot)) => println!("{}: {:?}", snapshot.dataset_id, snapshot.phase),
                _ => {}
            }
        }
        for (tool, args, expected) in [
            (
                "dictionary_lookup",
                serde_json::json!({"word":"hello"}),
                "hello",
            ),
            (
                "dictionary_lookup",
                serde_json::json!({"word":"went"}),
                "go",
            ),
            (
                "dictionary_search_zh",
                serde_json::json!({"term":"苹果"}),
                "apple",
            ),
        ] {
            let result = handle
                .execute_tool_raw(tool, &args, None, None)
                .await
                .unwrap();
            assert!(result.error.is_none(), "{result:?}");
            assert!(result.data.to_string().contains(expected), "{result:?}");
            println!("PASS {tool} {args}");
        }
        // 回归 Windows 同版本重建：上述查询已让真实插件持有旧 DB 连接。
        let rebuilt = manager
            .begin("builtin.dictionary", "dictionary-basic", "manual")
            .await
            .unwrap();
        let BeginOutcome::Started(rebuilt) = rebuilt else {
            panic!("显式重建必须创建新操作: {rebuilt:?}");
        };
        loop {
            let state = manager.status_for("builtin.dictionary", "dictionary-basic").unwrap();
            assert_eq!(state.op_id, rebuilt.op_id);
            if state.phase.is_terminal() {
                assert!(matches!(state.phase, OpPhase::Succeeded { .. }), "{state:?}");
                break;
            }
            if let Ok(Ok(snapshot)) = tokio::time::timeout(Duration::from_secs(2), progress.recv()).await {
                println!("REBUILD {}: {:?}", snapshot.dataset_id, snapshot.phase);
            }
        }
        let result = handle.execute_tool_raw("dictionary_lookup", &serde_json::json!({"word":"hello"}), None, None)
            .await.unwrap();
        assert!(result.error.is_none(), "{result:?}");
        assert!(result.data.to_string().contains("hello"), "{result:?}");
        println!("PASS same-version rebuild after querying the mounted database");
    })
    .await;
    manager.revoke_plugin(&handle, "smoke finished").await;
    assert!(result.is_ok(), "真实下载/构建/挂载冒烟超过 600s");
}
