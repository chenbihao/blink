//! FeatureCatalog IPC 命令（0.21.4）。
//!
//! 只读 command `list_feature_catalog()` 和批量写 `apply_binding_batch(ops)`。
//! 目录刷新由前端订阅 `blink://config-changed`，不新增专用事件（§5.5 第 8 条）。

use tauri::Manager;

use crate::domain::event_names::EventNames;
use crate::domain::feature_catalog::{
    ApplyBindingResult, BindingOp, FeatureCatalogAggregator, FeatureCatalogItem,
    apply_binding_batch,
};

/// 列出功能目录——聚合 builtin descriptor、chord binding 和 capability。
///
/// 前端调用后在"能力与操作"设置页按六组展示。
/// 配置变更后前端订阅 `blink://config-changed` 重新调用此命令刷新。
#[tauri::command]
pub async fn list_feature_catalog(
    app: tauri::AppHandle,
) -> Result<Vec<FeatureCatalogItem>, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let config = crate::app::config::get_config(pool).await;

    let cap_registry = app
        .state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>()
        .inner()
        .clone();

    let chord_registry = app
        .try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
        .map(|r| r.inner().clone())
        .ok_or_else(|| "ChordRegistry 未就绪".to_string())?;

    let plugin_engine = app
        .try_state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>()
        .map(|pe| pe.inner().clone());

    let items = FeatureCatalogAggregator::aggregate(
        &config.disabled_builtin_actions,
        &config.disabled_chord_actions,
        &config.disabled_context_bindings,
        &config.language,
        cap_registry.as_ref(),
        chord_registry.as_ref(),
        plugin_engine.as_deref(),
    );

    Ok(items)
}

/// 批量执行 binding 操作——写回各 binding store 并广播配置变更。
///
/// 成功后广播 `blink://config-changed`，前端所有订阅模块（含功能目录）自动刷新。
#[tauri::command]
pub async fn apply_binding_ops(
    app: tauri::AppHandle,
    ops: Vec<BindingOp>,
) -> Result<Vec<ApplyBindingResult>, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let results = apply_binding_batch(pool, &ops).await;

    // 广播配置变更——前端所有按 key 订阅的模块自动刷新
    use tauri::Emitter;
    let _ = app.emit(EventNames::CONFIG_CHANGED, serde_json::json!({ "source": "feature_catalog" }));

    Ok(results)
}
