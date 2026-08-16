//! 配置 re-export 垫片。
//!
//! 所有配置类型 + 操作函数已迁入 `domain::config`。
//! 此文件保留 re-export 以避免 app 层大量 import 改动。

use std::time::Duration;

use tauri::Manager;

pub use crate::domain::config::app_config::*;
pub use crate::domain::config::plugin_config::*;
pub use crate::domain::config::shards::*;
pub use crate::domain::config::store::*;

// ── 输入配置快照刷新 ──────────────────────────────────────────────────────────

/// 刷新输入配置快照并推送到 hook 线程。
///
/// 从 DB 读取热键/chord 配置，从 `ChordRegistry` 派生 `exclusive_tap_keys`，
/// 构建完整 `InputConfigSnapshot` 并通过 `InputController::update_config` 发送。
///
/// 启动时及 `hotkey`/`tap_threshold`/`chord_toggles`/`chord_bindings`/
/// `disabled_chord_actions` 任一更新成功后必须调用此函数；
/// 禁止各 command 局部 patch Hook 状态。
pub async fn refresh_input_config(app: &tauri::AppHandle) {
    refresh_input_config_with_registry(app, None).await;
}

/// 刷新输入配置快照（可显式传入 ChordRegistry，绕过 app.state 时序问题）。
///
/// 启动时 ChordRegistry 尚未 `app.manage`，`try_state` 返回 None。
/// HotkeyService::start 通过 `ctx.chord_registry` 显式传入，避免空集。
pub async fn refresh_input_config_with_registry(
    app: &tauri::AppHandle,
    registry: Option<&std::sync::Arc<crate::domain::chord::ChordRegistry>>,
) {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let config = get_config(pool).await;
    let chord_cfg = get_chord_config(pool).await;
    let disabled = get_disabled_chord_actions(pool).await;

    let exclusive_tap_keys = if let Some(reg) = registry {
        reg.exclusive_tap_keys(&chord_cfg.bindings, &disabled)
    } else {
        app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
            .map(|reg| reg.exclusive_tap_keys(&chord_cfg.bindings, &disabled))
            .unwrap_or_else(|| {
                tracing::warn!(
                    "refresh_input_config: ChordRegistry 未就绪，exclusive_tap_keys 为空"
                );
                Default::default()
            })
    };

    let snapshot = crate::infra::platform::hotkey::InputConfigSnapshot {
        revision: 0,
        hotkey: crate::infra::platform::hotkey::NormalizedHotkey {
            modifiers: config.hotkey.modifiers.clone(),
            key: config.hotkey.key.clone(),
        },
        tap_threshold: Duration::from_millis(config.tap_threshold),
        chord_enabled: chord_cfg.chord_enabled,
        exclusive_tap_keys,
        voice_hold_enabled: true,
    };

    crate::infra::platform::hotkey::InputController::update_config(snapshot);
}

// ── 0.21.14：从 infra/data/config.rs 上移的迁移函数 ─────────────────────────
//
// infra 层不反向依赖 domain::plugin::PluginHandle / domain::config::PluginConfig。
// 迁移逻辑只使用 infra::data::config 的 get/set_config primitive + domain 类型构造。

/// 0.4→0.5 自动迁移：为每个插件初始化默认配置（`plugin:<id>` 不存在则写入默认）。
/// 迁移完成后写 marker，下次不再执行。
pub async fn migrate_0_4_to_0_5(
    pool: &sqlx::SqlitePool,
    plugins: &[std::sync::Arc<crate::domain::plugin::PluginHandle>],
) {
    const MARKER_KEY: &str = "migration_0_5_done";

    if crate::infra::data::config::get_config(pool, MARKER_KEY)
        .await
        .is_some()
    {
        return;
    }
    tracing::info!("开始执行 0.4→0.5 配置迁移");

    for plugin in plugins {
        let plugin_id = plugin.id();
        let key = format!("plugin:{plugin_id}");
        if crate::infra::data::config::get_config(pool, &key)
            .await
            .is_none()
        {
            let default_config = crate::domain::config::PluginConfig {
                settings: plugin.manifest().default_settings(),
                ..Default::default()
            };
            match serde_json::to_string(&default_config) {
                Ok(json) => {
                    if let Err(e) = crate::infra::data::config::set_config(pool, &key, &json).await
                    {
                        tracing::warn!(plugin = %plugin_id, error = %e, "插件配置写入失败");
                    } else {
                        tracing::info!(plugin = %plugin_id, "初始化插件默认配置");
                    }
                }
                Err(e) => tracing::warn!(plugin = %plugin_id, error = %e, "插件配置初始化失败"),
            }
        }
    }

    if let Err(e) = crate::infra::data::config::set_config(pool, MARKER_KEY, "1").await {
        tracing::warn!(error = %e, "迁移标记写入失败");
    }
    tracing::info!("0.4→0.5 配置迁移完成");
}
