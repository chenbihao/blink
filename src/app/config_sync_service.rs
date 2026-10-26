//! 配置同步应用服务（0.28.6）——`SyncLocalEnv` 的 bin 侧实现 + 设置/凭据管理。
//!
//! 应用走**既有 `set_config_internal` 链**（校验/落库/运行时副作用/广播一条不
//! 绕过，phase「UI 与同步共用配置应用服务」）；失败项由引擎转本机覆盖记录。
//! 密码只存 CM（`SecretRef::webdav_password`）；地址/用户名在 `sync:settings`
//! 分片（整分片留本机，不进可移植范围）。

use std::path::PathBuf;

use blink_domain::config_sync::{
    portable, ApplyOp, AppliedItemOutcome, RawLocal, SyncError, SyncLocalEnv,
    WebDavClient, WebDavConfig,
};
use blink_domain::config::SyncSettingsConfig;
use blink_infra::data::prompt_library;
use blink_infra::platform::secret::{load_secret, save_secret, SecretRef};
use blink_infra::utils::paths::app_data_dir;
use tauri::Manager;

/// 同步状态目录：`%APPDATA%\blink\sync`（state.json + backups/）。
pub fn sync_state_dir() -> PathBuf {
    app_data_dir().join("sync")
}

/// 读同步连接设置（不含密码）。
pub async fn load_settings(pool: &sqlx::SqlitePool) -> SyncSettingsConfig {
    blink_domain_config_shim::get(pool).await
}

/// 保存连接设置（仅地址/用户名；密码独立走 CM）。
pub async fn save_settings(pool: &sqlx::SqlitePool, cfg: &SyncSettingsConfig) -> Result<(), SyncError> {
    blink_domain_config_shim::set(pool, cfg)
        .await
        .map_err(|e| SyncError::Io {
            message: format!("保存同步设置失败: {e}"),
        })
}

/// 密码探测（只读 CM；不存在返回 None）。
pub fn load_password() -> Result<Option<String>, SyncError> {
    match load_secret(&SecretRef::webdav_password()) {
        Ok(secret) => Ok(Some(secret.expose().to_string())),
        Err(blink_infra::platform::secret::SecretError::NotFound(_)) => Ok(None),
        Err(e) => Err(SyncError::Io {
            message: format!("读取同步密码失败: {e}"),
        }),
    }
}

pub fn save_password(password: &str) -> Result<(), SyncError> {
    use blink_infra::platform::secret::SecretString;
    save_secret(
        &SecretRef::webdav_password(),
        &SecretString::new(password.to_string()),
    )
    .map_err(|e| SyncError::Io {
        message: format!("保存同步密码失败: {e}"),
    })
}

pub fn clear_password() -> Result<(), SyncError> {
    use blink_infra::platform::secret::delete_secret;
    match delete_secret(&SecretRef::webdav_password()) {
        Ok(()) | Err(blink_infra::platform::secret::SecretError::NotFound(_)) => Ok(()),
        Err(e) => Err(SyncError::Io {
            message: format!("清除同步密码失败: {e}"),
        }),
    }
}

/// 从当前设置构建 WebDAV 客户端（未配置/缺密码 → NotConfigured）。
pub async fn build_client(pool: &sqlx::SqlitePool) -> Result<WebDavClient, SyncError> {
    let settings = load_settings(pool).await;
    if settings.url.is_empty() || settings.username.is_empty() {
        return Err(SyncError::NotConfigured);
    }
    let password = load_password()?.ok_or(SyncError::NotConfigured)?;
    WebDavClient::new(WebDavConfig {
        root: settings.url,
        username: settings.username,
        password,
    })
}

/// bin 侧 `SyncLocalEnv` 实现。
pub struct AppSyncEnv {
    app: tauri::AppHandle,
}

impl AppSyncEnv {
    pub fn new(app: &tauri::AppHandle) -> Self {
        Self { app: app.clone() }
    }

    fn pools_config(&self) -> sqlx::SqlitePool {
        self.app
            .state::<blink_infra::data::DbPools>()
            .config
            .clone()
    }
}

#[async_trait::async_trait]
impl SyncLocalEnv for AppSyncEnv {
    fn state_dir(&self) -> PathBuf {
        sync_state_dir()
    }

    async fn read_raw(&self) -> Result<RawLocal, SyncError> {
        portable::read_raw_local(&self.pools_config()).await
    }

    async fn apply_ops(&self, ops: Vec<ApplyOp>) -> Vec<AppliedItemOutcome> {
        let mut results = Vec::with_capacity(ops.len());
        for op in ops {
            let (shard, items) = op_identity(&op);
            let error = execute_op(&self.app, &op).await.err();
            results.push(AppliedItemOutcome { shard, items, error });
        }
        results
    }

    fn provider_has_secret(&self, provider_id: &str) -> bool {
        load_secret(&SecretRef::ai(provider_id)).is_ok()
    }

    fn plugin_installed(&self, plugin_id: &str) -> bool {
        self.app
            .try_state::<std::sync::Arc<blink_domain::plugin::PluginEngine>>()
            .is_some_and(|engine| {
                engine
                    .all_plugins()
                    .iter()
                    .any(|p| p.id() == plugin_id)
            })
    }
}

fn op_identity(op: &ApplyOp) -> (String, Vec<String>) {
    match op {
        ApplyOp::SetConfig { shard, items, .. } | ApplyOp::PluginConfig { shard, items, .. } => {
            (shard.clone(), items.clone())
        }
        ApplyOp::PromptUpsert { shard, item, .. } | ApplyOp::PromptDelete { shard, item, .. } => {
            (shard.clone(), vec![item.clone()])
        }
    }
}

/// 逐条执行应用操作；返回 Err(message) 的条目由引擎转本机覆盖记录。
async fn execute_op(
    app: &tauri::AppHandle,
    op: &ApplyOp,
) -> Result<(), String> {
    match op {
        ApplyOp::SetConfig { key, value, .. } => {
            crate::app::commands::config::set_config_internal(app.clone(), key.clone(), value.clone(), None)
                .await
                .map_err(|e| e.message)
        }
        ApplyOp::PluginConfig {
            plugin_id,
            enabled,
            settings,
            custom_triggers,
            disabled_default_triggers,
            expected_revision,
            ..
        } => {
            let mut payload = serde_json::json!({
                "pluginId": plugin_id,
                "enabled": enabled,
                "settings": settings,
                "expectedRevision": expected_revision,
            });
            if let Some(map) = payload.as_object_mut() {
                if let Some(custom) = custom_triggers {
                    map.insert("customTriggers".into(), custom.clone());
                }
                if let Some(disabled) = disabled_default_triggers {
                    map.insert("disabledDefaultTriggers".into(), disabled.clone());
                }
            }
            crate::app::commands::config::set_config_internal(
                app.clone(),
                "plugin_config".into(),
                payload,
                None,
            )
            .await
            .map_err(|e| e.message)
        }
        ApplyOp::PromptUpsert { id, row, .. } => {
            apply_prompt_upsert(app, id, row).await
        }
        ApplyOp::PromptDelete { id, .. } => {
            let pool = &app
                .state::<blink_infra::data::DbPools>()
                .config;
            prompt_library::delete(pool, id)
                .await
                .map_err(|e| e.to_string())?;
            emit_prompt_changed(app, id);
            Ok(())
        }
    }
}

async fn apply_prompt_upsert(
    app: &tauri::AppHandle,
    id: &str,
    row: &serde_json::Value,
) -> Result<(), String> {
    let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let content = row.get("content").and_then(|v| v.as_str());
    // 校验（内置行 content 不漫游——用 override 或空串做体积校验）。
    let effective = content
        .or(row.get("override_content").and_then(|v| v.as_str()))
        .unwrap_or("");
    blink_domain::prompt_library::validate_entry(name, effective).map_err(|e| format!("提示词条目 {id} 无效: {e}"))?;
    let pool = &app.state::<blink_infra::data::DbPools>().config;
    prompt_library::upsert_sync(
        pool,
        id,
        name,
        content,
        row.get("override_content").and_then(|v| v.as_str()),
        row.get("scope").and_then(|v| v.as_str()).unwrap_or("chat"),
        row.get("sort_order").and_then(|v| v.as_i64()).unwrap_or(0),
        row.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false),
    )
    .await
    .map_err(|e| format!("提示词条目 {id} 落库失败: {e}"))?;
    emit_prompt_changed(app, id);
    Ok(())
}

fn emit_prompt_changed(app: &tauri::AppHandle, id: &str) {
    use tauri::Emitter;
    let _ = app.emit(
        blink_infra::event_names::EventNames::PROMPT_LIBRARY_CHANGED,
        serde_json::json!({ "id": id }),
    );
}

/// 同步互斥锁——同一时刻只允许一次同步流程（预览/提交/初始化共用）。
pub static SYNC_RUN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());


/// 记录最近一次同步结果（仅展示）。
pub async fn record_last_sync(
    pool: &sqlx::SqlitePool,
    revision: u64,
    outcome: &str,
) {
    let mut settings = load_settings(pool).await;
    settings.last_sync = Some(blink_domain::config::LastSyncRecord {
        revision,
        at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        outcome: outcome.to_string(),
    });
    let _ = save_settings(pool, &settings).await;
}

// ConfigStore 入口（domain re-export 的 blink-domain-config store）。
mod blink_domain_config_shim {
    use blink_domain::config::store::ConfigStore;
    use blink_domain::config::SyncSettingsConfig;

    pub async fn get(pool: &sqlx::SqlitePool) -> SyncSettingsConfig {
        ConfigStore::get::<SyncSettingsConfig>(pool).await
    }

    pub async fn set(
        pool: &sqlx::SqlitePool,
        cfg: &SyncSettingsConfig,
    ) -> Result<(), String> {
        ConfigStore::set(pool, cfg).await
    }
}
