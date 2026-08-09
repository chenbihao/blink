//! mcp 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use crate::domain::event_names::EventNames;
use tauri::{Emitter, Manager};
/// 列出所有已配置的 MCP server（含状态）。
#[tauri::command]
pub async fn list_mcp_servers(app: tauri::AppHandle) -> Result<Vec<McpServerListItem>, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    let configs = crate::domain::mcp::McpServerConfigStore::load_all(&pools.config)
        .await
        .map_err(|e| e.to_string())?;

    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    let statuses = manager.get_statuses().await;

    let items = configs
        .into_iter()
        .map(|config| {
            let status = statuses.get(&config.name).cloned().unwrap_or(
                crate::domain::mcp::McpServerStatus::Offline {
                    reason: "未启动".to_string(),
                },
            );
            McpServerListItem { config, status }
        })
        .collect();

    Ok(items)
}

/// 添加或更新 MCP server 配置（按 name 去重）。
#[tauri::command]
pub async fn upsert_mcp_server(
    app: tauri::AppHandle,
    config: crate::domain::mcp::McpServerConfig,
) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::domain::mcp::McpServerConfigStore::upsert(&pools.config, config)
        .await
        .map_err(|e| e.to_string())?;

    // 0.13.8: 广播配置变更事件
    let _ = app.emit(
        EventNames::CONFIG_CHANGED,
        serde_json::json!({ "key": "mcp:servers" }),
    );

    Ok(())
}

/// 删除 MCP server 配置（同时停止已连接的 server）。
#[tauri::command]
pub async fn delete_mcp_server(app: tauri::AppHandle, name: String) -> Result<(), String> {
    // 先停止 server（如果有连接）
    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    manager.stop_server(&name).await;

    let pools = app.state::<crate::infra::data::DbPools>();
    crate::domain::mcp::McpServerConfigStore::delete(&pools.config, &name)
        .await
        .map_err(|e| e.to_string())?;

    // 0.13.8: 广播配置变更事件
    let _ = app.emit(
        EventNames::CONFIG_CHANGED,
        serde_json::json!({ "key": "mcp:servers" }),
    );

    Ok(())
}

/// 设置 MCP server 的 enabled 状态。
///
/// 禁用时同时停止已连接的 server（杀子进程 / 断开 HTTP 连接），
/// 避免禁用后子进程仍在后台运行。启用时不自动拉起（lazy connect 在对话时处理），
/// 前端会调用 test_mcp_connection 探测可用性。
#[tauri::command]
pub async fn set_mcp_server_enabled(
    app: tauri::AppHandle,
    name: String,
    enabled: bool,
) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::domain::mcp::McpServerConfigStore::set_enabled(&pools.config, &name, enabled)
        .await
        .map_err(|e| e.to_string())?;

    if !enabled {
        // 禁用时停止 server（如果有连接）
        let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
        manager.stop_server(&name).await;
        tracing::info!(server = %name, "MCP: server 已禁用并停止");
    }

    // 0.13.8: 广播配置变更事件，让对话窗口 popup 刷新
    let _ = app.emit(
        EventNames::CONFIG_CHANGED,
        serde_json::json!({ "key": "mcp:servers" }),
    );

    Ok(())
}

/// 手动启动 MCP server。
#[tauri::command]
pub async fn start_mcp_server(app: tauri::AppHandle, name: String) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    let configs = crate::domain::mcp::McpServerConfigStore::load_all(&pools.config)
        .await
        .map_err(|e| e.to_string())?;
    let config = configs
        .into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| format!("未找到 server 配置: {name}"))?;

    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    manager.start_server(&config).await
}

/// 手动停止 MCP server。
#[tauri::command]
pub async fn stop_mcp_server(app: tauri::AppHandle, name: String) -> Result<(), String> {
    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    manager.stop_server(&name).await;
    Ok(())
}

/// 重连 MCP server（先停再启）。
#[tauri::command]
pub async fn reconnect_mcp_server(app: tauri::AppHandle, name: String) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    manager.reconnect_server(&name, &pools.config).await
}

/// 0.13.7: 测试 MCP server 连接——连接 + 拉 tool 列表 + 立即断开。
///
/// 用于设置页的「测试连接」按钮——验证 server 是否可用，不保持子进程。
/// 返回 tool 列表供前端预览。
#[tauri::command]
pub async fn test_mcp_connection(
    app: tauri::AppHandle,
    name: String,
) -> Result<Vec<crate::domain::mcp::McpToolInfo>, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    let configs = crate::domain::mcp::McpServerConfigStore::load_all(&pools.config)
        .await
        .map_err(|e| e.to_string())?;
    let config = configs
        .into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| format!("未找到 server 配置: {name}"))?;

    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    manager.test_connection(&config).await
}

/// 获取单个 MCP server 的 tool 列表（供前端预览）。
#[tauri::command]
pub async fn get_mcp_server_tools(
    app: tauri::AppHandle,
    name: String,
) -> Result<Vec<crate::domain::mcp::McpToolInfo>, String> {
    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    manager
        .get_server_tools(&name)
        .await
        .ok_or_else(|| format!("server {name} 未连接或不存在"))
}

/// 更新 MCP server 的 disabled_tools（tool 粒度开关）。
#[tauri::command]
pub async fn set_mcp_server_disabled_tools(
    app: tauri::AppHandle,
    name: String,
    disabled_tools: Vec<String>,
) -> Result<(), String> {
    // 更新配置库
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::domain::mcp::McpServerConfigStore::set_disabled_tools(
        &pools.config,
        &name,
        disabled_tools.clone(),
    )
    .await
    .map_err(|e| e.to_string())?;

    // 更新运行时缓存
    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    manager.update_disabled_tools(&name, disabled_tools).await;

    // 0.13.8: 广播配置变更事件
    let _ = app.emit(
        EventNames::CONFIG_CHANGED,
        serde_json::json!({ "key": "mcp:servers" }),
    );

    Ok(())
}

/// 探测指定 agent 的 MCP 配置文件路径（0.13.6）。
///
/// 返回 None 表示文件不存在（该 agent 可能未安装）。
#[tauri::command]
pub async fn detect_mcp_config_file(
    source: crate::domain::mcp::McpImportSource,
) -> Result<Option<String>, String> {
    Ok(crate::domain::mcp::import::detect_config_file_path(source))
}

/// 从指定 agent 的配置文件读取并解析 MCP 配置（0.13.6）。
///
/// 返回待导入的 server 列表（不含去重——前端展示时标注「已存在」）。
#[tauri::command]
pub async fn import_mcp_from_agent(
    source: crate::domain::mcp::McpImportSource,
) -> Result<Vec<crate::domain::mcp::McpServerConfig>, String> {
    let path = crate::domain::mcp::import::detect_config_file_path(source)
        .ok_or_else(|| format!("未找到 {} 的配置文件", source.display_name()))?;
    let json = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("读取配置文件失败: {e}"))?;
    crate::domain::mcp::import::parse_external_mcp_config(source, &json)
}

/// 解析用户粘贴的 JSON 配置（0.13.6）。
#[tauri::command]
pub async fn import_mcp_from_json(
    json: String,
) -> Result<Vec<crate::domain::mcp::McpServerConfig>, String> {
    crate::domain::mcp::import::parse_external_mcp_config(
        crate::domain::mcp::McpImportSource::Json,
        &json,
    )
}

/// 批量导入 server 配置（0.13.6）。
///
/// `overwrite=true` 覆盖同名，`false` 跳过同名。
/// 只写配置，不拉起子进程——用户在列表页手动「启动」。
#[tauri::command]
pub async fn batch_import_mcp_servers(
    app: tauri::AppHandle,
    configs: Vec<crate::domain::mcp::McpServerConfig>,
    overwrite: bool,
) -> Result<crate::domain::mcp::ImportResult, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    let existing = crate::domain::mcp::McpServerConfigStore::load_all(&pools.config)
        .await
        .map_err(|e| e.to_string())?;
    let existing_names: std::collections::HashSet<&str> =
        existing.iter().map(|c| c.name.as_str()).collect();

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut overwritten = 0usize;
    let mut names = Vec::new();

    for config in configs {
        let is_existing = existing_names.contains(config.name.as_str());
        if is_existing && !overwrite {
            skipped += 1;
            continue;
        }
        if is_existing {
            overwritten += 1;
        } else {
            imported += 1;
        }
        names.push(config.name.clone());
        crate::domain::mcp::McpServerConfigStore::upsert(&pools.config, config)
            .await
            .map_err(|e| e.to_string())?;
    }

    Ok(crate::domain::mcp::ImportResult {
        imported,
        skipped,
        overwritten,
        names,
    })
}

/// 批量设置 MCP server 的 enabled 状态（0.13.6）。
#[tauri::command]
pub async fn batch_set_mcp_enabled(
    app: tauri::AppHandle,
    names: Vec<String>,
    enabled: bool,
) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    for name in &names {
        let _ = crate::domain::mcp::McpServerConfigStore::set_enabled(&pools.config, name, enabled)
            .await;
    }
    Ok(())
}

/// 0.13.8: 触发 MCP lazy connect——持久连接所有 enabled 但尚未连接的 server。
///
/// 供对话窗口打开时调用，让 popup 能显示正确的在线状态。
/// 与 `ensure_provider` 中的 `ensure_connected` 调用相同，但此处不依赖 ChatService——
/// 对话窗口 init 时即可调用，不需要等用户发消息。
/// 单个 server 连接失败只记状态（Offline），不影响其他 server。
#[tauri::command]
pub async fn ensure_mcp_connected(app: tauri::AppHandle) -> Result<(), String> {
    if app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .is_some_and(|chat| chat.is_pure_chat_mode())
    {
        tracing::debug!("MCP: 纯对话模式跳过对话窗口 lazy connect");
        return Ok(());
    }
    let pools = app.state::<crate::infra::data::DbPools>();
    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    manager.ensure_connected(&pools.config).await;
    Ok(())
}

/// 获取对话窗口 tool 池规模（内置 + MCP，供前端显示）。
#[tauri::command]
pub async fn get_mcp_tool_pool_size(app: tauri::AppHandle) -> serde_json::Value {
    if app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .is_some_and(|chat| chat.is_pure_chat_mode())
    {
        return serde_json::json!({ "builtin": 0, "mcp": 0, "total": 0 });
    }
    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    // 0.13.8: 用轻量 count_tools() 替代 collect_tools().len()，
    // 避免仅为取一个数字就构造完整的 McpTool（clone + Box 分配）
    let mcp_tools = manager.count_tools().await;

    // 0.14 Capability-only：内置 tool 数只来自 CapabilityRegistry。
    let cap_registry = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    let builtin_tools = cap_registry.len();

    serde_json::json!({
        "builtin": builtin_tools,
        "mcp": mcp_tools,
        "total": builtin_tools + mcp_tools,
    })
}

/// 获取所有已连接 MCP server 的 tool 名称列表（供前端区分工具来源）。
#[tauri::command]
pub async fn get_mcp_tool_names(app: tauri::AppHandle) -> Vec<String> {
    if app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .is_some_and(|chat| chat.is_pure_chat_mode())
    {
        return Vec::new();
    }
    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    manager.get_all_tool_names().await
}

/// 0.13.6: 获取 MCP tool 来源信息（含 server 名 + transport 类型）。
///
/// 供前端工具卡片增强——显示 MCP 工具来自哪个 server、用哪种协议。
#[tauri::command]
pub async fn get_mcp_tool_sources(
    app: tauri::AppHandle,
) -> Vec<crate::domain::mcp::client::McpToolSource> {
    if app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .is_some_and(|chat| chat.is_pure_chat_mode())
    {
        return Vec::new();
    }
    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    manager.get_tool_sources().await
}

/// 加载 MCP server 配置（总开关 + 暴露能力清单）。
#[tauri::command]
pub async fn get_mcp_server_config(
    app: tauri::AppHandle,
) -> Result<crate::domain::mcp::McpServerModeConfig, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::domain::mcp::McpServerModeConfigStore::load(&pools.config)
        .await
        .map_err(|e| e.to_string())
}

/// 保存 MCP server 配置。
#[tauri::command]
pub async fn set_mcp_server_config(
    app: tauri::AppHandle,
    config: crate::domain::mcp::McpServerModeConfig,
) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::domain::mcp::McpServerModeConfigStore::save(&pools.config, &config)
        .await
        .map_err(|e| e.to_string())
}

// ── 辅助类型（从 commands.rs 迁移）──

#[derive(Clone, Debug, serde::Serialize)]
pub struct McpServerListItem {
    #[serde(flatten)]
    pub config: crate::domain::mcp::McpServerConfig,
    /// 运行时状态（online / offline / connecting）。
    pub status: crate::domain::mcp::McpServerStatus,
}
