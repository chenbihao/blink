//! 配置同步命令层（0.28.6）。
//!
//! 手动同步四入口：`test_sync_connection` / `run_sync`（预览+提交一体，
//! 冲突返回计划由前端裁决后带 resolutions 重入）/ `sync_initialize`（首次
//! 显式方向）/ `reset_sync_overrides`（清覆盖重试）。全部走
//! `config_sync_service` 的互斥锁与 `SyncEngine` 状态机。

use blink_domain::config_sync::{
    engine::SyncRunContext, webdav::normalize_root_url, InitDirection, Resolution,
    ResetOverridesReport, SyncEngine, SyncError, SyncPlan, SyncReport, SyncRunOutcome,
};
use blink_domain::config::SyncSettingsConfig;
use serde::Serialize;
use tauri::Emitter;
use tauri::Manager;

use crate::app::command_error::CommandError;
use crate::app::config_sync_service as service;

fn sync_err(e: SyncError) -> CommandError {
    // 集中落日志：同步命令失败此前只回传前端、控制台无痕，排障只能靠复现。
    // NotConfigured 是用户输入未完成的预期状态（保存前测试），降为 debug。
    if matches!(e, SyncError::NotConfigured) {
        tracing::debug!(code = e.code(), "同步命令返回未配置（预期状态）");
    } else {
        tracing::warn!(code = e.code(), "配置同步命令失败: {}", e.message());
    }
    CommandError::new(e.code(), e.message(), e.retryable())
}

/// 设置视图（密码只回有无 + 全掩码占位，不泄漏任何字符/长度）。
#[derive(Debug, Clone, Serialize)]
pub struct SyncSettingsView {
    pub url: String,
    pub username: String,
    pub has_password: bool,
    /// 全掩码占位（`••••••••`）——WebDAV 密码与 API key 不同，不允许
    /// 首尾字符泄漏；无密码为空。
    pub password_hint: String,
    pub is_http: bool,
    pub last_sync: Option<blink_domain::config::LastSyncRecord>,
    /// 本机覆盖记录数（应用失败保留本机值的项）。
    pub overrides: usize,
    /// 是否有待裁决冲突（上次预览保存的计划仍在）。
    pub has_pending_conflicts: bool,
}

#[tauri::command]
pub async fn get_sync_settings(
    app: tauri::AppHandle,
) -> Result<SyncSettingsView, CommandError> {
    let pool = &app.state::<blink_infra::data::DbPools>().config;
    let settings = service::load_settings(pool).await;
    let password = service::load_password().map_err(sync_err)?;
    let state_path = service::sync_state_dir().join("state.json");
    let state = if state_path.exists() {
        blink_domain::config_sync::SyncState::load(&state_path).ok()
    } else {
        None
    };
    let is_http = settings.url.starts_with("http://");
    Ok(SyncSettingsView {
        url: settings.url,
        username: settings.username,
        has_password: password.is_some(),
        password_hint: password
            .map(|p| blink_infra::platform::secret::format_hint_strict(&p))
            .unwrap_or_default(),
        is_http,
        last_sync: settings.last_sync,
        overrides: state.as_ref().map(|s| s.overrides.len()).unwrap_or(0),
        has_pending_conflicts: state.as_ref().is_some_and(|s| s.pending_plan.is_some()),
    })
}

#[tauri::command]
pub async fn save_sync_settings(
    app: tauri::AppHandle,
    url: String,
    username: String,
    // Some(非空) = 写入新密码；Some(空)/None = 保持不变。
    password: Option<String>,
) -> Result<SyncSettingsView, CommandError> {
    let pool = &app.state::<blink_infra::data::DbPools>().config;
    let normalized = normalize_root_url(&url).map_err(sync_err)?;
    let settings = SyncSettingsConfig {
        url: normalized,
        username: username.trim().to_string(),
        ..service::load_settings(pool).await
    };
    if settings.username.is_empty() {
        return Err(sync_err(SyncError::NotConfigured));
    }
    service::save_settings(pool, &settings).await.map_err(sync_err)?;
    if let Some(pw) = password
        && !pw.is_empty()
    {
        service::save_password(&pw).map_err(sync_err)?;
    }
    tracing::info!(url = %settings.url, "WebDAV 同步设置已保存");
    get_sync_settings(app).await
}

#[tauri::command]
pub async fn clear_sync_password(app: tauri::AppHandle) -> Result<SyncSettingsView, CommandError> {
    service::clear_password().map_err(sync_err)?;
    get_sync_settings(app).await
}

/// 连接测试结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TestConnectionView {
    Ok {
        head_revision: Option<u64>,
        /// 远端已有内容且本机从未同步——需要选择初始化方向。
        needs_init: bool,
    },
    EmptyServer,
}

#[tauri::command]
pub async fn test_sync_connection(
    app: tauri::AppHandle,
    // 表单直测参数：提供非空 url+username 时用输入框当前值（密码空回退
    // 已存密码），保存前即可测；缺省/为空回退已保存设置。
    url: Option<String>,
    username: Option<String>,
    password: Option<String>,
) -> Result<TestConnectionView, CommandError> {
    let form_ready = url.as_deref().is_some_and(|u| !u.trim().is_empty())
        && username.as_deref().is_some_and(|u| !u.trim().is_empty());
    let client = if form_ready {
        service::build_client_from(
            url.as_deref().unwrap_or_default(),
            username.as_deref().unwrap_or_default(),
            password.as_deref(),
        )
        .await
        .map_err(sync_err)?
    } else {
        let pool = &app.state::<blink_infra::data::DbPools>().config;
        service::build_client(pool).await.map_err(sync_err)?
    };
    let head = client.test_connection().await.map_err(sync_err)?;
    match head {
        Some(info) => Ok(TestConnectionView::Ok {
            head_revision: Some(info.head.revision),
            needs_init: false,
        }),
        None => Ok(TestConnectionView::EmptyServer),
    }
}

/// `run_sync` 返回视图（serde 给前端分派）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SyncRunView {
    UpToDate { revision: u64 },
    NeedsInit {
        remote_revision: Option<u64>,
        remote_device: Option<String>,
        remote_updated_at: Option<i64>,
        local_items: usize,
    },
    AwaitingResolution { plan: SyncPlan },
    Done { report: SyncReport },
}

#[tauri::command]
pub async fn run_sync(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    resolutions: Option<Vec<Resolution>>,
    plan_id: Option<String>,
) -> Result<SyncRunView, CommandError> {
    // 同步是宿主敏感面——插件页面不得触发。
    super::plugin_ui::reject_plugin_window(&window)?;
    run_sync_internal(app, resolutions.unwrap_or_default(), plan_id).await
}

pub(crate) async fn run_sync_internal(
    app: tauri::AppHandle,
    resolutions: Vec<Resolution>,
    plan_id: Option<String>,
) -> Result<SyncRunView, CommandError> {
    let _guard = service::SYNC_RUN_LOCK.lock().await;
    let pool = &app.state::<blink_infra::data::DbPools>().config;
    let store = service::build_client(pool).await.map_err(sync_err)?;
    let env = service::AppSyncEnv::new(&app);
    let outcome = SyncEngine::run(SyncRunContext {
        store: &store,
        env: &env,
        resolutions,
        plan_id,
    })
    .await
    .map_err(sync_err)?;
    let view = match outcome {
        SyncRunOutcome::UpToDate { revision } => {
            service::record_last_sync(pool, revision, "up_to_date").await;
            SyncRunView::UpToDate { revision }
        }
        SyncRunOutcome::NeedsInit { remote_head, local_items } => {
            SyncRunView::NeedsInit {
                remote_revision: remote_head.as_ref().map(|h| h.head.revision),
                remote_device: remote_head.as_ref().map(|h| h.head.device_id.clone()),
                remote_updated_at: remote_head.as_ref().map(|h| h.head.updated_at),
                local_items,
            }
        }
        SyncRunOutcome::AwaitingResolution(plan) => {
            service::record_last_sync(pool, plan.target_revision.saturating_sub(1), "awaiting_resolution").await;
            SyncRunView::AwaitingResolution { plan }
        }
        SyncRunOutcome::Done(report) => {
            service::record_last_sync(pool, report.revision, "done").await;
            // 广播一次同步完成（设置页与其他窗口可据此刷新；载荷无敏感内容）。
            let _ = app.emit(
                blink_infra::event_names::EventNames::CONFIG_SYNC_DONE,
                serde_json::json!({ "revision": report.revision, "changed": report.changed }),
            );
            SyncRunView::Done { report }
        }
    };
    Ok(view)
}

#[tauri::command]
pub async fn sync_initialize(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    direction: InitDirection,
) -> Result<SyncReport, CommandError> {
    super::plugin_ui::reject_plugin_window(&window)?;
    let _guard = service::SYNC_RUN_LOCK.lock().await;
    let pool = &app.state::<blink_infra::data::DbPools>().config;
    let store = service::build_client(pool).await.map_err(sync_err)?;
    let env = service::AppSyncEnv::new(&app);
    let report = SyncEngine::initialize(&store, &env, direction)
        .await
        .map_err(sync_err)?;
    service::record_last_sync(pool, report.revision, "done").await;
    Ok(report)
}

#[tauri::command]
pub async fn reset_sync_overrides(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
) -> Result<ResetOverridesReport, CommandError> {
    super::plugin_ui::reject_plugin_window(&window)?;
    let env = service::AppSyncEnv::new(&app);
    SyncEngine::reset_overrides(&env).await.map_err(sync_err)
}

