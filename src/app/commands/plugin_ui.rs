//! 插件页面受限桥接命令（0.28.2 §五.2-3）——plugin-* 窗口的唯一 IPC 面。
//!
//! **来源校验**：所有 `plugin_page_*` 命令以 Tauri 注入的**调用窗口**（非页面
//! 自报参数）反查 `PluginSurface` 会话——窗口 label 归属 + 插件句柄代次双重
//! 复核；冒用其他 plugin_id / 旧代次窗口 / 非插件窗口一律拒绝。
//!
//! **能力边界**：`plugin_page_invoke` 只放行本插件 manifest 声明的 tool
//! （capability id 按 owner `plugin:<id>` 复核）——页面拿不到其他插件或宿主
//! 的任意 capability；`open_plugin_page` 为宿主入口（设置页/主窗），插件窗口
//! 自身不得调用（防页面替别的插件开窗）。
//!
//! 敏感命令的插件窗口拒绝门禁（install/uninstall/plugin_config）见
//! [`reject_plugin_window`]——app command 未走 Capability 框架，需要显式守卫。

use std::sync::Arc;

use tauri::Manager;

use crate::app::command_error::CommandError;
use crate::app::plugin_surface;
use crate::domain::capability::{
    CapabilityRegistry, CapabilityResult, InvocationOrigin, InvokeContext, RuntimeCapabilities,
};
use crate::domain::plugin::{plugin_tool_id, PluginEngine};

/// 宿主 SDK（`blink-plugin` 协议 `/@host/sdk.js` 的真源，include 嵌入随二进制）。
pub const PLUGIN_HOST_SDK: &str = include_str!("../../../resources/plugin-host/sdk.js");

/// 页面单次 invoke 的外层硬顶（插件自身 manifest 预算更小时以小者为准，
/// 由 PluginCapabilityAdapter 内部执行；此处只兜底防悬挂）。
const PAGE_INVOKE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// 页面剪贴板写入的单次上限（有界 IPC，spec-backend §7.4 精神）。
const PAGE_CLIPBOARD_MAX_BYTES: usize = 1 << 20;

/// 所有应用命令在分派前统一检查，新增宿主命令不会默认向插件开放。
pub(crate) fn command_allowed(label: &str, command: &str) -> bool {
    !label.starts_with("plugin-") || matches!(command,
        "plugin_page_ready" | "plugin_page_invoke" | "plugin_page_get_settings"
        | "plugin_page_clipboard" | "plugin_page_close")
}

#[cfg(test)]
mod tests {
    use super::command_allowed;

    #[test]
    fn plugin_commands_are_default_deny() {
        for command in ["set_config", "reset_config", "clear_plugin_secret", "run_builtin_action", "gif_start_recording", "future_host_command"] {
            assert!(!command_allowed("plugin-builtin-ip", command));
            assert!(command_allowed("settings", command));
        }
        assert!(command_allowed("plugin-builtin-ip", "plugin_page_ready"));
        assert!(!command_allowed("plugin-builtin-ip", "plugin_page_unrestricted"));
    }
}

/// 插件窗口守卫：插件页面不得调用的宿主命令在入口处拒绝。
///
/// 适用：安装/卸载插件包、写插件配置等生态敏感命令——页面内容是"插件资产"，
/// 不授予其改动插件安装态/其他插件配置的权限（phase §3.2「宿主控制创建/
/// 销毁、权限、来源」）。其余通用 app command 的窗口级隔离是已知边界，
/// 随 0.28.5 官方目录与来源验证收紧。
pub fn reject_plugin_window(window: &tauri::WebviewWindow) -> Result<(), CommandError> {
    if window.label().starts_with("plugin-") {
        return Err(CommandError::new(
            "permission_denied",
            "插件页面不得调用该命令",
            false,
        ));
    }
    Ok(())
}

/// 打开（或复用）插件主窗口——宿主入口（设置页插件卡片等）。插件窗口自身禁调。
#[tauri::command]
pub async fn open_plugin_page(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
    page_id: Option<String>,
    payload: Option<serde_json::Value>,
) -> Result<(), CommandError> {
    reject_plugin_window(&window)?;
    plugin_surface::open_plugin_page(&app, &plugin_id, page_id.as_deref(), payload)
        .map_err(|e| CommandError::new("invalid_state", e, true))
}

/// 页面就绪握手：取走初始 payload + 宿主上下文（语言/主题）。
#[tauri::command]
pub async fn plugin_page_ready(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
    on_payload: tauri::ipc::Channel<serde_json::Value>,
) -> Result<serde_json::Value, CommandError> {
    let surface = plugin_surface::surface_state(&app);
    let engine = app.state::<Arc<PluginEngine>>().inner().clone();
    plugin_surface::page_session(&surface, &app, &window, &engine, &plugin_id)
        .map_err(|e| CommandError::new("permission_denied", e, false))?;

    let pool = app.state::<crate::infra::data::DbPools>().config.clone();
    let appearance =
        crate::app::config::ConfigStore::get::<crate::app::config::AppearanceConfig>(&pool).await;
    let payload = plugin_surface::take_ready_payload(&surface, window.label(), on_payload);
    Ok(serde_json::json!({
        "payload": payload,
        "context": {
            "language": appearance.language,
            // auto 必须解析为具体主题名：SDK 直接拼 /@host/theme.css?name=X，
            // 原样回传 auto 会因无 auto.css 恒 404
            "theme": crate::app::commands::resolve_effective_theme(&appearance.theme),
        },
    }))
}

/// 调用本插件声明的 tool（Capability 统一原子执行，owner 复核）。
#[tauri::command]
pub async fn plugin_page_invoke(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
    tool: String,
    args: serde_json::Value,
) -> Result<serde_json::Value, CommandError> {
    let surface = plugin_surface::surface_state(&app);
    let engine = app.state::<Arc<PluginEngine>>().inner().clone();
    let (_, _, handle) = plugin_surface::page_session(
        &surface, &app, &window, &engine, &plugin_id,
    )
    .map_err(|e| CommandError::new("permission_denied", e, false))?;

    // 只放行本插件 manifest 声明的 tool（页面不能臆造 capability id）
    if !handle.manifest().tools.iter().any(|t| t.name == tool) {
        return Err(CommandError::new(
            "invalid_arguments",
            format!("插件 {plugin_id} 未声明 tool: {tool}"),
            false,
        ));
    }
    let cap_id = plugin_tool_id(&plugin_id, &tool);
    let registry = app.state::<Arc<CapabilityRegistry>>().inner().clone();
    // owner 复核：目标 capability 必须仍注册在 plugin:<id> 名下（撤销后拒绝）
    let owner = format!("plugin:{plugin_id}");
    if !registry.ids_owned_by(&owner).contains(&cap_id) {
        return Err(CommandError::new(
            "invalid_state",
            format!("能力 {tool} 当前不可用（插件可能已被禁用或更新）"),
            true,
        ));
    }

    let env = app
        .state::<Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let ctx = InvokeContext {
        env: env.as_ref(),
        origin: InvocationOrigin::LocalSurface,
        runtime: RuntimeCapabilities {
            surface: Some(env.as_ref()),
            main_process: true,
            desktop_session: true,
        },
        deadline: Some(std::time::Instant::now() + PAGE_INVOKE_DEADLINE),
    };
    let result = registry
        .invoke(&cap_id, args, &ctx)
        .await
        .map_err(CommandError::from)?;
    Ok(match result {
        CapabilityResult::Text { content, desc } => serde_json::json!({
            "ok": true, "kind": "text", "content": content, "desc": desc,
        }),
        CapabilityResult::Items { items } => serde_json::json!({
            "ok": true, "kind": "items",
            "items": items.iter().map(|i| serde_json::json!({
                "data": i.data, "desc": i.desc,
            })).collect::<Vec<_>>(),
        }),
        CapabilityResult::Done { summary } => serde_json::json!({
            "ok": true, "kind": "done", "summary": summary,
        }),
        // Blob 不跨页面 IPC（spec-backend §7.4）：只回摘要，页面如需二进制
        // 走受控资源引用（当前无页面型 Blob 插件）
        CapabilityResult::Blob { desc, .. } => serde_json::json!({
            "ok": true, "kind": "blob", "summary": desc.unwrap_or_default(),
        }),
    })
}

/// 读本插件设置（脱敏版——密钥字段只有空占位，明文永不回页面）。
#[tauri::command]
pub async fn plugin_page_get_settings(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
) -> Result<serde_json::Value, CommandError> {
    let surface = plugin_surface::surface_state(&app);
    let engine = app.state::<Arc<PluginEngine>>().inner().clone();
    plugin_surface::page_session(&surface, &app, &window, &engine, &plugin_id)
        .map_err(|e| CommandError::new("permission_denied", e, false))?;
    Ok(engine.get_settings(&plugin_id).unwrap_or(serde_json::json!({})))
}

/// 页面写宿主剪贴板（译文复制等；进入剪贴板历史，与主窗复制一致）。
#[tauri::command]
pub async fn plugin_page_clipboard(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
    text: String,
) -> Result<(), CommandError> {
    let surface = plugin_surface::surface_state(&app);
    let engine = app.state::<Arc<PluginEngine>>().inner().clone();
    plugin_surface::page_session(&surface, &app, &window, &engine, &plugin_id)
        .map_err(|e| CommandError::new("permission_denied", e, false))?;
    if text.len() > PAGE_CLIPBOARD_MAX_BYTES {
        return Err(CommandError::new(
            "invalid_arguments",
            "文本超出页面复制上限",
            false,
        ));
    }
    crate::infra::platform::clipboard::write_text_to_clipboard(&text, "plugin-page", false)
        .map_err(|e| CommandError::new("internal", e, true))
}

/// 隐藏本插件窗口（会话保留；关闭按钮同效）。
#[tauri::command]
pub async fn plugin_page_close(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
) -> Result<(), CommandError> {
    let surface = plugin_surface::surface_state(&app);
    let engine = app.state::<Arc<PluginEngine>>().inner().clone();
    plugin_surface::page_session(&surface, &app, &window, &engine, &plugin_id)
        .map_err(|e| CommandError::new("permission_denied", e, false))?;
    window
        .hide()
        .map_err(|e| CommandError::new("internal", format!("隐藏窗口失败: {e}"), true))
}

/// 插件窗口面状态（设置页"已安装/获取"数据源，0.28.2 §五.5）：
/// 每插件 pages / readiness / 安装记录合并视图。
#[tauri::command]
pub async fn get_plugin_surface_status(
    app: tauri::AppHandle,
) -> Result<Vec<serde_json::Value>, CommandError> {
    let engine = app.state::<Arc<PluginEngine>>().inner().clone();
    let pool = app.state::<crate::infra::data::DbPools>().config.clone();
    let lang = crate::app::config::get_config(&pool).await.language;

    // 安装记录（无 installer 的环境如 CLI 不炸——空表）
    let installs: std::collections::HashMap<String, blink_infra::plugin_storage::InstallRecord> =
        app.try_state::<Arc<crate::domain::plugin::PluginInstaller>>()
            .map(|installer| {
                installer
                    .list_installed()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| (r.plugin_id.clone(), r))
                    .collect()
            })
            .unwrap_or_default();

    // 数据包记录（0.28.4；按 "plugin_id/dataset_id" 键转按插件分组）
    let mut dataset_records: std::collections::HashMap<
        String,
        Vec<blink_infra::plugin_storage::DatasetRecord>,
    > = std::collections::HashMap::new();
    if let Some(dataset_installer) =
        app.try_state::<Arc<crate::domain::plugin::DatasetInstaller>>()
    {
        for record in dataset_installer.list().unwrap_or_default() {
            dataset_records
                .entry(record.plugin_id.clone())
                .or_default()
                .push(record);
        }
    }

    let mut rows: Vec<serde_json::Value> = engine
        .all_plugins()
        .iter()
        .map(|handle| {
            let manifest = handle.manifest();
            let readiness = engine
                .readiness(handle.id(), &lang)
                .unwrap_or(serde_json::json!({"ready": true, "blockers": []}));
            let install = installs.get(handle.id()).map(|r| {
                serde_json::json!({
                    "version": r.version,
                    "installed_at": r.installed_at,
                    "state": match r.state {
                        blink_infra::plugin_storage::InstallState::Active => "active",
                        blink_infra::plugin_storage::InstallState::PendingRemove => "pending_remove",
                    },
                    "source": match &r.source {
                        blink_infra::plugin_storage::InstallSource::LocalDev { origin } => {
                            serde_json::json!({"kind": "local_dev", "origin": origin})
                        }
                        blink_infra::plugin_storage::InstallSource::Official { catalog_url } => {
                            serde_json::json!({"kind": "official", "catalog_url": catalog_url})
                        }
                    },
                })
            });
            // manifest 声明的数据包逐个合并安装记录（缺装/版本/损坏状态）
            let installed_ds = dataset_records.get(handle.id());
            let datasets: Vec<serde_json::Value> = manifest
                .datasets
                .iter()
                .map(|def| {
                    let record = installed_ds.and_then(|rows| {
                        rows.iter().find(|r| r.dataset_id == def.id)
                    });
                    serde_json::json!({
                        "id": def.id,
                        "optional": def.optional,
                        "min_version": def.min_version,
                        "installed": record.map(|r| serde_json::json!({
                            "version": r.version,
                            "installed_at": r.installed_at,
                            "state": match r.state {
                                blink_infra::plugin_storage::InstallState::Active => "active",
                                blink_infra::plugin_storage::InstallState::PendingRemove => "pending_remove",
                            },
                            "source": match &r.source {
                                blink_infra::plugin_storage::InstallSource::LocalDev { origin } => {
                                    serde_json::json!({"kind": "local_dev", "origin": origin})
                                }
                                blink_infra::plugin_storage::InstallSource::Official { catalog_url } => {
                                    serde_json::json!({"kind": "official", "catalog_url": catalog_url})
                                }
                            },
                        })),
                    })
                })
                .collect();
            serde_json::json!({
                "plugin_id": handle.id(),
                "dev_mode": cfg!(debug_assertions),
                "version": manifest.version,
                // 0.28.12：随包身份标记——有安装记录的覆盖版卸载后可恢复随包版；
                // 仅随包插件无卸载入口（前端只显示禁用开关）。
                "bundled": crate::app::commands::has_bundled_version(&app, handle.id()),
                "pages": manifest.pages.iter().map(|p| serde_json::json!({
                    "id": p.id, "entry": p.entry,
                })).collect::<Vec<_>>(),
                "readiness": readiness,
                "install": install,
                "datasets": datasets,
            })
        })
        .collect();
    // 0.28.12：待清理插件（卸载时文件被占用）已从运行时撤销——引擎集合无
    // 句柄，按安装记录追加占位行，前端如实展示"待清理（重启后完成）"。
    for (pid, record) in &installs {
        if record.state != blink_infra::plugin_storage::InstallState::PendingRemove
            || rows.iter().any(|r| r["plugin_id"] == *pid)
        {
            continue;
        }
        rows.push(serde_json::json!({
            "plugin_id": pid,
            "dev_mode": cfg!(debug_assertions),
            "bundled": crate::app::commands::has_bundled_version(&app, pid),
            "pages": [],
            "readiness": {"ready": false, "blockers": []},
            "install": {
                "version": record.version,
                "installed_at": record.installed_at,
                "state": "pending_remove",
                "source": match &record.source {
                    blink_infra::plugin_storage::InstallSource::LocalDev { origin } => {
                        serde_json::json!({"kind": "local_dev", "origin": origin})
                    }
                    blink_infra::plugin_storage::InstallSource::Official { catalog_url } => {
                        serde_json::json!({"kind": "official", "catalog_url": catalog_url})
                    }
                },
            },
            "datasets": [],
        }));
    }
    Ok(rows)
}
