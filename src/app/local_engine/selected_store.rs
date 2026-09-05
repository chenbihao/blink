//! 本地 STT selected 模型存储——生产实现（0.22.9 Handoff 08）。
//!
//! 实现 `EngineManager::switch_model` 事务的 [`SelectedModelStore`] 端口：
//! - 读：`SttConfig` 内存缓存（与 `EngineManager::read_selected_model` 同真源）；
//! - 写：三个字段同步（`local_stt_selection` / `local_model_id` /
//!   `local_engine.funasr_model`）→ ConfigStore 持久化 → 缓存更新 →
//!   `CONFIG_CHANGED` 广播——与 `set_local_stt_selection` 命令的保存语义一致。
//!
//! manager 侧不接触 DB/AppHandle；wiring 层（main.rs）在构造 EngineManager
//! 后注入本实现。

use tauri::{Emitter, Manager};

use crate::domain::event_names::EventNames;

use super::manager::SelectedModelStore;

/// 生产 selected 存储（Tauri + SQLite ConfigStore）。
pub struct SttSelectedModelStore {
    app: tauri::AppHandle,
}

impl SttSelectedModelStore {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait::async_trait]
impl SelectedModelStore for SttSelectedModelStore {
    fn read_selected(&self) -> Option<String> {
        let m = crate::app::stt_config::get_stt_config()
            .local_engine
            .funasr_model;
        if m.is_empty() { None } else { Some(m) }
    }

    async fn commit_selected(&self, model_id: &str) -> Result<(), String> {
        let mut config = crate::app::stt_config::get_stt_config();
        config.local_stt_selection = Some(crate::app::stt_config::LocalSttSelection::new(
            crate::app::stt_config::LocalSttSelection::FUNASR_ENGINE_ID,
            model_id,
        ));
        config.local_model_id = Some(model_id.to_string());
        config.local_engine.funasr_model = model_id.to_string();

        let pool = self.app.state::<crate::infra::data::DbPools>();
        crate::domain::config::store::ConfigStore::set(&pool.config, &config)
            .await
            .map_err(|e| format!("保存 STT 配置失败: {e}"))?;

        // 更新内存缓存（供 STT 引擎等同步读取）
        crate::app::stt_config::update_cache(&config);

        // 广播配置变更（前端刷新选择态）
        let _ = self.app.emit(
            EventNames::CONFIG_CHANGED,
            serde_json::json!({ "key": "stt:config", "scope": "local_stt_selection" }),
        );

        tracing::info!(
            engine_id = crate::app::local_engine::funasr::FUNASR_ENGINE_ID,
            model_id = %model_id,
            "本地 STT selected 已由切换事务提交"
        );
        Ok(())
    }
}
