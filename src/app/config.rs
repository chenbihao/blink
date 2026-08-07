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
