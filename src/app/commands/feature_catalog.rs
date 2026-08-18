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
/// 前端调用后在"能力管理"设置页按分组展示（六个功能域 + Chord 快捷入口 + 其他插件）。
/// AI/MCP 出口状态从用户授权真源（allowlist / exposed_capabilities）投影，
/// 供目录页三列开关显示实际状态。
/// 配置变更后前端订阅 `blink://config-changed` 重新调用此命令刷新。
#[tauri::command]
pub async fn list_feature_catalog(
    app: tauri::AppHandle,
) -> Result<Vec<FeatureCatalogItem>, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let config = crate::app::config::get_config(pool).await;

    // AI allowlist 真源（0.21.5 落地，key `ai.capability_access`）
    let ai_cfg =
        crate::domain::config::ai_capability_access::AiCapabilityAccessStore::load(pool).await;
    let ai_allowlist: std::collections::HashSet<String> =
        ai_cfg.enabled_capabilities.into_iter().collect();

    // MCP exposed 真源
    let mcp_config = crate::domain::mcp::McpServerModeConfigStore::load(pool)
        .await
        .map_err(|e| e.to_string())?;
    let mcp_exposed: std::collections::HashSet<String> =
        mcp_config.exposed_capabilities.into_iter().collect();

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
        Some(&ai_allowlist),
        &mcp_exposed,
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
    let _ = app.emit(
        EventNames::CONFIG_CHANGED,
        serde_json::json!({ "source": "feature_catalog" }),
    );

    Ok(results)
}

/// MCP 能力摘要——返回已暴露/总数计数（0.21.6）。
///
/// 供 MCP Server 设置页显示 "已暴露 N/M" 摘要，替代旧的全量清单。
#[tauri::command]
pub async fn get_catalog_mcp_summary(
    app: tauri::AppHandle,
) -> Result<McpCapabilitySummary, String> {
    let cap_registry = app
        .state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>()
        .inner()
        .clone();

    let pools = app.state::<crate::infra::data::DbPools>();

    // 从 MCP server config store 读取已暴露的 capability id 集合
    let mcp_config = crate::domain::mcp::McpServerModeConfigStore::load(&pools.config)
        .await
        .map_err(|e| e.to_string())?;
    let exposed_count = mcp_config.exposed_capabilities.len();

    // 统计所有可暴露的 capability 总数（policy 不禁止 MCP 的）
    let total_count = cap_registry
        .entries()
        .iter()
        .filter(|(_, cap)| {
            use crate::domain::capability::policy::{DangerClass, McpDefault};
            let policy = cap.policy();
            policy.danger != DangerClass::Dangerous && policy.mcp_default != McpDefault::Forbidden
        })
        .count();

    Ok(McpCapabilitySummary {
        exposed_count,
        total_count,
    })
}

/// MCP 能力摘要数据结构
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpCapabilitySummary {
    pub exposed_count: usize,
    pub total_count: usize,
}
