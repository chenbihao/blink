//! search 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use crate::domain::event_names::EventNames;
use tauri::{Emitter, Manager};
use tauri_plugin_dialog::DialogExt;
/// 打开文件选择对话框，返回选中的文件路径（取消时返回 null）。
/// 0.16.2: `input_text` 参数用于 `requires_input=true` 的 chord（如 chat），
/// 把主窗口输入框文本带入 action 执行。None 或空字符串表示无入参。
#[tauri::command]
pub async fn open_file_dialog(
    app: tauri::AppHandle,
    title: String,
    filters: Vec<serde_json::Value>,
) -> Option<String> {
    // 构造过滤器
    let mut dialog = app.dialog().file();
    if !title.is_empty() {
        dialog = dialog.set_title(title);
    }
    // 转换过滤器格式（简化处理，只取第一个扩展名）
    for filter in filters {
        if let Some(name) = filter.get("name").and_then(|v| v.as_str())
            && let Some(exts) = filter.get("extensions").and_then(|v| v.as_array())
        {
            let extensions: Vec<&str> = exts.iter().filter_map(|e| e.as_str()).collect();
            if !extensions.is_empty() {
                dialog = dialog.add_filter(name, &extensions);
            }
        }
    }
    dialog.blocking_pick_file().and_then(|p| match p {
        tauri_plugin_dialog::FilePath::Path(path) => path.to_str().map(|s| s.to_string()),
        tauri_plugin_dialog::FilePath::Url(url) => Some(url.to_string()),
    })
}

/// 打开目录选择对话框，返回选中的目录路径（取消时返回 null）。
#[tauri::command]
pub async fn pick_directory_dialog(app: tauri::AppHandle, title: String) -> Option<String> {
    let mut dialog = app.dialog().file();
    if !title.is_empty() {
        dialog = dialog.set_title(title);
    }
    dialog.blocking_pick_folder().and_then(|p| match p {
        tauri_plugin_dialog::FilePath::Path(path) => path.to_str().map(|s| s.to_string()),
        tauri_plugin_dialog::FilePath::Url(url) => Some(url.to_string()),
    })
}

/// 主窗口 ESC 调用：隐藏主窗口。
#[tauri::command]
pub fn hide_window(app: tauri::AppHandle) {
    // 0.17.6: 隐藏窗口时清除 AI 活跃标志，防止标志残留导致下次唤起时 watchdog 不工作
    crate::infra::platform::window::set_main_ai_active(false);
    crate::infra::platform::window::hide(&app, "ESC");
}

/// 隐藏设置窗口（供设置页的 ESC 调用）。
#[tauri::command]
pub fn hide_settings_window(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.hide();
        tracing::debug!("hide_settings_window: 隐藏设置窗口");
    }
}

/// 前端输入时调用:经 SearchService 多路召回(sync lane 同步返回首批)。
///
/// calc / 应用搜索 / 历史融合等逻辑已下沉到各 SearchEngine + SearchService(见 0.2 设计 §2)。
/// `seq` 为前端递增请求序号,async 增量结果(blink://results)回带同一 seq 供前端校验。
///
/// 0.8.3 §4.3：返回契约 `SearchResponse { entries, suggestion }`——
/// Keyword（0.8.1 输入补全）与 Context（0.8.3 环境感知）Ghost 走同一字段。
#[tauri::command]
pub async fn search_apps(
    query: String,
    seq: u64,
    app: tauri::AppHandle,
) -> crate::domain::search::SearchResponse {
    tracing::debug!(%query, seq, "search_apps: 收到搜索请求");
    let service = app.state::<std::sync::Arc<crate::domain::search::SearchService>>();
    let results = service.search(&query, seq).await;
    tracing::debug!(
        count = results.entries.len(),
        has_suggestion = results.suggestion.is_some(),
        %query,
        "search_apps: 返回结果"
    );
    for (i, item) in results.entries.iter().enumerate() {
        let detail = item.score_detail.as_deref().unwrap_or("");
        tracing::trace!(
            index = i,
            score = if detail.is_empty() {
                format!("{:.4}", item.score)
            } else {
                format!("{:.4} ({})", item.score, detail)
            },
            source = %item.source,
            name = %item.name,
            lnk_path = %item.lnk_path,
            "搜索结果项"
        );
    }
    if let Some(sug) = &results.suggestion {
        tracing::debug!(
            display = %sug.display,
            replacement = %sug.replacement,
            source = ?sug.source,
            confidence = sug.confidence,
            "suggestion"
        );
    }
    results
}

/// 前端回车/点击时调用：启动选中的应用（普通 lnk 路径）。
///
/// 0.8.0 §1.3 起，内置动作走 `run_builtin_action`（前端 `Action.kind == "run"` 时分派），
/// 此命令只处理真正的文件/应用路径。计算结果无 lnk_path，忽略。
///
/// 0.21.3：打开操作收敛走 `open_path` Capability，不再直接调 `search::launch`。
/// 历史记录仍在此 command 完成（搜索特有副作用，不属于 Capability 职责）。
#[tauri::command]
pub async fn launch_app(app: tauri::AppHandle, lnk_path: String) -> Result<(), String> {
    if lnk_path.is_empty() {
        return Ok(());
    }

    tracing::debug!(%lnk_path, "launch_app: 普通应用启动");

    let pools = app.state::<crate::infra::data::DbPools>();
    // search_history_enabled=false 时跳过记录（隐私/偏好）；该项频率加权随之失效
    let config = crate::app::config::get_config(&pools.config).await;
    if config.search_history_enabled {
        crate::infra::data::history::record_launch(&pools.history, &lnk_path).await;
    }

    // 0.21.3：打开操作统一走 open_path Capability
    let env_arc = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let cap_reg = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    let ctx = crate::domain::capability::InvokeContext {
        env: env_arc.as_ref(),
        origin: crate::domain::capability::InvocationOrigin::LocalSurface,
        runtime: crate::domain::capability::RuntimeCapabilities {
            surface: Some(env_arc.as_ref()),
            main_process: true,
            desktop_session: true,
        },
        deadline: None,
    };
    let args = serde_json::json!({ "path": lnk_path });
    match cap_reg.invoke("open_path", args, &ctx).await {
        Ok(_) => {
            crate::infra::platform::window::hide(&app, "launch");
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "launch_app: open_path Capability 执行失败");
            // 降级：如果 Capability 失败（如 registry 未注册），回退到直接 launch
            // 0.21.7 删除 execution 模块后移除此降级
            crate::domain::search::launch(&lnk_path)?;
            crate::infra::platform::window::hide(&app, "launch");
            Ok(())
        }
    }
}

/// 运行内置动作（0.8.0 §1.3 / 0.8.6 §8.1.1 重构）。
///
/// 前端 `Action.kind === "run"` → `invoke("run_builtin_action", { id, arg })`。
/// `id` 为 `SearchAction::RunAction.id`（即 descriptor 的 `capability_id`）。
///
/// **0.21.1**：删除 ActionRegistry → CapabilityRegistry 双 fallback，
/// 统一走 CapabilityRegistry。13 个旧 Action 已全量迁为 Capability。
///
/// **0.21.13**：
/// - `arg` 已是目标 Capability schema 接收的最终 JSON object（由 `ParamSource::extract`
///   在领域层直接产出），command 层不再二次猜测参数形状。
/// - 诊断文本复制改由显式 `BuiltinResultAction::CopyText` 表达，不再按 capability id 特判。
/// - 复制失败返回错误，不谎报成功。
///
/// 未知 id → 返回 `Err`；前端会打印到控制台，不弹窗。
///
/// **0.14.7 W3**：返回 `CommandError`（结构化错误协议），前端按 code/message 展示。
#[tauri::command]
pub async fn run_builtin_action(
    app: tauri::AppHandle,
    id: String,
    arg: Option<serde_json::Value>,
) -> Result<(), crate::app::command_error::CommandError> {
    tracing::debug!(%id, ?arg, "run_builtin_action: 收到请求");

    // 0.14.6 §2.2：从 state 获取 DomainEnv 桥接器
    let env_arc = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();

    // 0.21.3: 直接查 CapabilityRegistry（descriptor target）。
    // 不再有 Action-first/Capability-fallback——descriptor 的 capability_id 直接指向 Capability。
    let cap_reg = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    if cap_reg.get(&id).is_some() {
        // 0.21.13：arg 已是最终 JSON object，无需 convert_legacy_arg_to_capability_args。
        // 无参能力传 `{}`，参数化能力传 `{ "url": "..." }` / `{ "path": "..." }`。
        let args = arg.unwrap_or_else(|| serde_json::json!({}));
        let ctx = crate::domain::capability::InvokeContext {
            env: env_arc.as_ref(),
            origin: crate::domain::capability::InvocationOrigin::LocalSurface,
            runtime: crate::domain::capability::RuntimeCapabilities {
                surface: Some(env_arc.as_ref()), // 0.21.1: 注入 SurfacePort
                main_process: true,
                desktop_session: true,
            },
            deadline: None,
        };
        // 0.21.0: 走 registry.invoke 执行 origin/runtime 门禁
        match cap_reg.invoke(&id, args, &ctx).await {
            Ok(result) => {
                let projection = cap_reg.get(&id).and_then(|cap| cap.projection());
                tracing::info!(%id, summary = %result.to_display_text(projection.as_ref()), "run_builtin_action: Capability 执行成功");

                // 0.21.13：显式结果动作——按 capability id 查 descriptor 声明的 result_action。
                // 不再按 `blink_print_debug_info` / `blink_debug_inithook` id 特判。
                // 普通 CapabilityResult::Text 不会自动复制——只有 descriptor 声明了
                // CopyText 才执行剪贴板写入。
                if let Some(crate::domain::search::BuiltinResultAction::CopyText { skip_persist }) =
                    crate::domain::search::find_result_action_by_capability_id(&id)
                    && let crate::domain::capability::CapabilityResult::Text { content, .. } =
                        &result
                    && let Err(e) = crate::infra::platform::clipboard::write_text_to_clipboard(
                        content,
                        &id,
                        skip_persist,
                    )
                {
                    tracing::error!(error = %e, %id, "写入结果文本到剪贴板失败");
                    return Err(crate::app::command_error::CommandError::new(
                        "clipboard_error",
                        format!("写入剪贴板失败: {e}"),
                        false,
                    ));
                }
            }
            Err(e) => {
                tracing::error!(%id, error = %e, "run_builtin_action: Capability 执行失败");
                return Err(crate::app::command_error::CommandError::from(e));
            }
        }
        crate::infra::platform::window::hide(&app, "run_builtin_action");
        return Ok(());
    }

    tracing::warn!(%id, "run_builtin_action: 未知 id");
    Err(crate::app::command_error::CommandError::new(
        "not_found",
        format!("未知内置动作 id: {id}"),
        false,
    ))
}

/// 对话窗口危险操作确认（0.12.0 §2.4 闭环骨架）。
///
/// 对话窗口前端收到 `blink://chat-confirm-action` 事件后展示确认卡片，
/// 用户确认/拒绝 -> invoke 此 command -> 唤醒 tool_adapter 挂起的 `call`。
///
/// 0.17.6 后主窗口 + 对话窗口共用此 command（旧 `confirm_ai_action` 已删除）。
/// `confirm_chat_action` 只送信号（rig agent loop 内部 `call` 挂起等待，确认后由 `call` 自己执行）。
///
/// 返回 `true` = 信号已送达（confirm_id 有效）；`false` = confirm_id 不存在（超时/过期）。
#[tauri::command]
pub async fn confirm_chat_action(
    app: tauri::AppHandle,
    confirm_id: u64,
    approved: bool,
) -> Result<bool, String> {
    let pending = app.state::<std::sync::Arc<crate::domain::ai::tool_adapter::PendingConfirms>>();
    let delivered = pending.resolve(confirm_id, approved).await;
    if delivered {
        tracing::debug!(confirm_id, approved, "confirm_chat_action: 信号已送达");
    } else {
        tracing::warn!(
            confirm_id,
            "confirm_chat_action: confirm_id 不存在（过期/超时）"
        );
    }
    Ok(delivered)
}

/// 打开设置页并定位到指定 Tab（0.12.4 §6.5）。
///
/// 复用 `open_about` 的 eval 模式：打开设置窗口后延迟 300ms 点击对应 Tab。
/// chat 窗口设置按钮调用此 command 并传 `tab: "ai"`。
#[tauri::command]
pub async fn open_settings_tab(app: tauri::AppHandle, tab: String) -> Result<(), String> {
    // 0.12.8: 白名单校验——eval 字符串拼接存在注入风险
    const ALLOWED_TABS: &[&str] = &[
        "general",
        "engines",
        "plugins",
        "ai-providers",
        "ai-chat",
        "voice",
        "context",
        "chord",
        "hotkey",
        "network",
        "storage",
        "debug",
        "about",
    ];
    if !ALLOWED_TABS.contains(&tab.as_str()) {
        return Err(format!("无效的设置页 tab: {tab}"));
    }

    crate::infra::platform::window::open_settings(&app);
    if let Some(w) = app.get_webview_window("settings") {
        let js = format!(
            "setTimeout(() => document.querySelector('.tab[data-tab=\"{}\"]')?.click(), 300)",
            tab
        );
        let _ = w.eval(&js);
    }
    Ok(())
}

/// 保存文本到指定路径（0.12.5 §5.6 导出对话用）。
///
/// 前端通过 Tauri `dialog.save()` 获取路径后调用此 command 写文件。
/// 仅写 UTF-8 文本文件，不做任何特权操作（无路径穿越风险——路径来自用户主动选择）。
#[tauri::command]
pub async fn save_text_file(path: String, content: String) -> Result<(), String> {
    std::fs::write(&path, content.as_bytes()).map_err(|e| format!("写入文件失败: {e}"))?;
    tracing::info!(%path, bytes = content.len(), "save_text_file: 文件已保存");
    Ok(())
}

/// 列出所有内置动作元数据 + 当前 enabled 状态（0.8.0 §1.3 / 0.8.6 §8.2.4 i18n）。
#[tauri::command]
pub async fn list_builtin_actions(
    app: tauri::AppHandle,
) -> Vec<crate::domain::search::BuiltinActionInfo> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let disabled = crate::app::config::get_disabled_builtin_actions(pool).await;
    // 0.21.3：不再依赖 ActionRegistry，descriptor 自带双语 title/subtitle
    let config = crate::app::config::get_config(pool).await;
    crate::domain::search::list_builtin_actions(&disabled, &config.language)
}

/// 触发 Chord 动作（0.8.5 §六）。前端 Alt+字母 → invoke 此 command。
///
/// key 为字母（不区分大小写）。未注册 → Err（前端 log，不弹窗）。
///
/// **surface 分派**（0.8.5 §6.4 简化后）：
/// - 截图（Alt+A）/ 剪贴板（Alt+C）/ 其它：action 自己在 execute 内决定 UI 反馈
///   （如 ClipboardHistoryAction 自 emit fill-query），command 层不再统一后处理
///
/// **门禁**（0.8.7 修复）：设置页取消勾选的 Chord 动作 → disabled 列表命中即静默早退。
/// 前端 `list_chord_actions` 已过滤 disabled 只是让"提示条不显示"，但 Alt+字母 触发是
/// **另一条独立路径**（前端 keyboard.js 直接按物理键 invoke），必须在 command 层守门。
///
/// **注意**：Alt+Space 语音输入不走此 command——它由 native hotkey hook 的 hold
/// 状态机直接处理（`HotkeyEvent::Hold` → `VoiceService::start_recording`），
/// chord registry 里的 `voice_input` 条目仅用于提示条显示（display-only）。
/// 0.16.2: `input_text` 参数用于 `requires_input=true` 的 chord（如 chat），
/// 把主窗口输入框文本带入 action 执行。None 或空字符串表示无入参。
///
#[tauri::command]
pub async fn trigger_chord(
    app: tauri::AppHandle,
    key: String,
    input_text: Option<String>,
    origin_ref: Option<String>,
) -> Result<(), String> {
    tracing::debug!(%key, input_len = input_text.as_deref().map(|s| s.len()), "trigger_chord");
    let Some(registry) = app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
    else {
        return Err("chord registry 未就绪".into());
    };
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    // 0.10.7：读 chord 配置（bindings + disabled），键位由 binding 覆盖
    let chord_cfg = crate::app::config::get_chord_config(pool).await;
    let disabled = crate::app::config::get_disabled_chord_actions(pool).await;
    // 查该 key 对应的 action id,若在 disabled 列表 → 早退
    let key_lower = key.to_lowercase();
    if let Some(action_id) = registry.action_id_for_key(&key_lower, &chord_cfg.bindings) {
        if disabled.iter().any(|d| d == action_id) {
            tracing::debug!(%key_lower, %action_id, "chord 已禁用,跳过触发");
            return Ok(());
        }
        // 0.12.1: AI 总开关关闭时 chat 不仅不可见，也不能由旧前端/直接 IPC 绕过。
        if action_id == "chat" && !crate::app::ai_config::get_ai_config().enabled {
            tracing::debug!(%key_lower, "AI 未启用,跳过 chat chord");
            return Ok(());
        }
    }
    // 0.21.2：Chord trigger 改为按 ChordTarget 分派，需要 CapabilityRegistry 和 SurfacePort。
    let env_arc = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let cap_registry = app
        .state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>()
        .inner()
        .clone();
    let _surface = registry
        .trigger(
            &key,
            &chord_cfg.bindings,
            cap_registry.as_ref(),
            env_arc.as_ref(),
            Some(env_arc.as_ref()),
            input_text.as_deref(),
            origin_ref.as_deref(),
        )
        .await?;
    Ok(())
}

/// 列出所有已注册的 Chord 动作元数据（0.8.5 §六 Ghost overlay 提示层渲染用）。
///
/// 每条：`{ id, key, label, surface }`。已 disabled 的跳过；label 按当前 UI 语言解析。
///
/// **功能总开关门禁**：`voice_input` 绑定 STT 总开关，`chat` 绑定 AI 总开关；
/// 未启用时不返回对应条目（提示条不显示）。
#[tauri::command]
pub async fn list_chord_actions(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let chord_cfg = crate::app::config::get_chord_config(pool).await;
    let disabled = crate::app::config::get_disabled_chord_actions(pool).await;
    // 仅取 AppearanceConfig 单分片（1 次 DB），不走 get_config 全量门面（7 次 DB）。
    let language = crate::domain::config::store::ConfigStore::get::<
        crate::domain::config::shards::AppearanceConfig,
    >(pool)
    .await
    .language;
    let stt_enabled = crate::app::stt_config::get_stt_config().enabled;
    let ai_enabled = crate::app::ai_config::get_ai_config().enabled;
    let Some(registry) = app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
    else {
        return Vec::new();
    };
    registry
        .list(&disabled, &chord_cfg.bindings, &language)
        .into_iter()
        .filter(|a| a["id"] != "voice_input" || stt_enabled)
        .filter(|a| a["id"] != "chat" || ai_enabled)
        .collect()
}

/// 列出所有已注册的 Chord 动作 + enabled 状态（0.8.5.1 §6.6 设置页用）。
///
/// 与 `list_chord_actions` 的区别:不过滤 disabled,而是每条附带 `enabled` 字段。
/// 用于设置页展示"所有可开关的动作",让用户能勾选禁用。
///
/// **功能总开关门禁**：同 `list_chord_actions`；STT/AI 未启用时不返回对应条目，
/// 设置页不显示一个当前不可用功能的 Chord 开关。
#[tauri::command]
pub async fn list_all_chord_actions(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let chord_cfg = crate::app::config::get_chord_config(pool).await;
    let disabled = crate::app::config::get_disabled_chord_actions(pool).await;
    // 仅取 AppearanceConfig 单分片（1 次 DB），不走 get_config 全量门面（7 次 DB）。
    let language = crate::domain::config::store::ConfigStore::get::<
        crate::domain::config::shards::AppearanceConfig,
    >(pool)
    .await
    .language;
    let stt_enabled = crate::app::stt_config::get_stt_config().enabled;
    let ai_enabled = crate::app::ai_config::get_ai_config().enabled;
    let Some(registry) = app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
    else {
        return Vec::new();
    };
    registry
        .list_all(&disabled, &chord_cfg.bindings, &language)
        .into_iter()
        .filter(|a| a["id"] != "voice_input" || stt_enabled)
        .filter(|a| a["id"] != "chat" || ai_enabled)
        .collect()
}

/// 0.16.9：获取当前 awareness 快照中的选区文本。
///
/// 供 chord E/S 在空闲态（空 query、无结果）解析上下文用。
/// 无选区时返回 null。
#[tauri::command]
pub async fn get_awareness_text(app: tauri::AppHandle) -> Option<String> {
    use tauri::Manager;
    let svc = app.state::<std::sync::Arc<crate::domain::search::SearchService>>();
    svc.get_selection_text()
}

/// 列出所有已注册的 context binding + 当前 enabled 状态（0.8.3 §4.6 设置页面板）。
///
/// 每条 binding 描述：`{ key, target_id, trigger_key, target_label, trigger_label, enabled }`。
/// - `key`：`{target_id}::{trigger_key}`，作 disable 列表存储项
/// - `target_label`：从 PluginManifest.name 本地化（缺失时降级 target_id）
/// - `trigger_label`：显示名（如「文本非目标语言 → 翻译」），i18n key（前端翻）
/// - `enabled`：用户配置的启用状态
///
/// 0.11.8：合并 manifest 与 builtin 两路。此前只枚举插件 manifest 的 context trigger，
/// 漏掉内置参数化动作（`open_url`/`open_path`/`reveal_in_explorer`）——这些在 BuiltinEngine
/// 内部自判 context，前端 UI 看不见也关不掉单条。现在 builtin 一路由
/// `list_builtin_context_bindings` 提供，与 manifest 路径字段格式完全对齐。
#[tauri::command]
pub async fn list_context_bindings(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let config = crate::app::config::get_config(pool).await;
    let disabled: std::collections::HashSet<String> =
        config.disabled_context_bindings.iter().cloned().collect();
    let lang = config.language.clone();

    // ── 路径 1：插件 manifest 的 Context trigger（原有逻辑） ───────────────
    let mut bindings = Vec::new();
    if let Some(pe) = app.try_state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>() {
        for manifest in pe.list_manifests() {
            for trigger in &manifest.triggers {
                if let crate::domain::plugin::PluginTrigger::Context { when, .. } = trigger {
                    let ctx_when: crate::domain::context::trigger::ContextTrigger = (*when).into();
                    let trigger_key = crate::domain::intent::trigger_key(&ctx_when);
                    let key = crate::domain::intent::binding_key(&manifest.id, trigger_key);
                    let target_label = manifest.name.resolve(&lang);
                    let enabled = !disabled.contains(&key);
                    bindings.push(serde_json::json!({
                        "key": key,
                        "target_id": manifest.id,
                        "trigger_key": trigger_key,
                        "target_label": target_label,
                        "trigger_label": trigger_key, // 前端按 key 翻译（i18n）
                        "enabled": enabled,
                    }));
                }
            }
        }
    }

    // ── 路径 2：内置参数化动作的 Context binding（0.11.8 补齐） ────────────
    // BuiltinEngine 自判 context、不走 RuleRouter，故需要单独取数。字段格式与路径 1
    // 完全对齐，前端 renderBindingRow 无需区分来源。
    // 0.21.3：不再依赖 ActionRegistry，descriptor 自带双语 title。
    {
        let disabled_vec: Vec<String> = disabled.iter().cloned().collect();
        bindings.extend(crate::domain::search::list_builtin_context_bindings(
            &disabled_vec,
            &lang,
        ));
    }

    bindings
}

/// 设置页-存储：清空历史记录。
#[tauri::command]
pub async fn clear_history(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::history::clear(pool).await;
    let _ = crate::infra::data::compact(pool).await; // VACUUM + WAL checkpoint 回收空间
    Ok(())
}

/// 调整主窗口大小（前端调用，用于弹性窗口）。
/// 设置大小后若窗口底部超出显示器工作区，自动上移使其完整可见。
#[tauri::command]
pub async fn resize_window(app: tauri::AppHandle, width: f64, height: f64) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("main") {
        let size = tauri::LogicalSize::new(width, height);
        win.set_size(size).map_err(|e| e.to_string())?;
        crate::infra::platform::window::clamp_to_work_area(&win);
    }
    Ok(())
}

/// 获取默认快捷键配置（设置页「恢复默认」按钮调用）。
///
/// **单一数据源**：默认值只在 `HotkeyConfig::default()`（`src/app/config.rs`）一处定义。
/// 前端「恢复默认」按钮调此命令拿到真·默认值，避免前端硬编码字面量与后端漂移
/// （历史 bug：0.x 时期前端曾把 `"RightAlt"` 当默认值，被 `HotkeyConfig::default()` 的
/// doc 注释举例误导，与后端实际默认 `Alt+Space` 不一致）。
#[tauri::command]
pub fn get_default_hotkey() -> serde_json::Value {
    let hk = crate::app::config::HotkeyConfig::default();
    serde_json::to_value(&hk).unwrap_or_else(|_| {
        serde_json::json!({
            "modifiers": ["alt"],
            "key": " ",
            "display": "Alt+Space",
        })
    })
}

/// 获取应用搜索配置。
#[tauri::command]
pub async fn get_start_menu_config(app: tauri::AppHandle) -> crate::app::config::StartMenuConfig {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::app::config::get_start_menu_config(pool).await
}

/// 获取计算器配置。
#[tauri::command]
pub async fn get_calc_config(app: tauri::AppHandle) -> crate::app::config::CalcConfig {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::app::config::get_calc_config(pool).await
}

/// 探测 Everything HTTP Server 状态。
#[tauri::command]
pub async fn probe_everything(port: u16) -> bool {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap_or_default();
    let url = format!("http://localhost:{port}/?search=__blink_probe__&json=1&count=1");
    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// 禁用/恢复某个默认触发词。
#[tauri::command]
pub async fn toggle_default_trigger(
    app: tauri::AppHandle,
    plugin_id: String,
    keyword: String,
    disabled: bool,
) -> Result<(), String> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();

    // 读取现有配置
    let mut config = engine.get_config(&plugin_id).unwrap_or_default();

    if disabled {
        // 加入禁用列表
        if !config.disabled_default_triggers.contains(&keyword) {
            config.disabled_default_triggers.push(keyword.clone());
        }
    } else {
        // 从禁用列表移除
        config.disabled_default_triggers.retain(|k| k != &keyword);
    }

    engine
        .update_config(&plugin_id, config, Some(&router))
        .await?;
    tracing::info!(plugin_id, keyword, disabled, "默认触发词状态已更新");
    Ok(())
}

/// 添加一个自定义触发词。
#[tauri::command]
pub async fn add_custom_trigger(
    app: tauri::AppHandle,
    plugin_id: String,
    keyword: String,
) -> Result<(), String> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();

    let mut config = engine.get_config(&plugin_id).unwrap_or_default();

    // 检查是否已存在（不区分大小写，简单重复检查）
    let keyword_lower = keyword.to_lowercase();
    if config
        .custom_triggers
        .iter()
        .any(|t| t.keyword.to_lowercase() == keyword_lower)
    {
        return Err(format!("触发词 '{keyword}' 已存在"));
    }

    // 添加新触发词
    config
        .custom_triggers
        .push(crate::app::config::CustomTrigger {
            keyword: keyword.clone(),
            enabled: true,
            surface: None,
        });

    engine
        .update_config(&plugin_id, config, Some(&router))
        .await?;
    tracing::info!(plugin_id, keyword, "自定义触发词已添加");
    Ok(())
}

/// 删除一个自定义触发词。
#[tauri::command]
pub async fn delete_custom_trigger(
    app: tauri::AppHandle,
    plugin_id: String,
    keyword: String,
) -> Result<(), String> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();

    let mut config = engine.get_config(&plugin_id).unwrap_or_default();
    let before_len = config.custom_triggers.len();
    config.custom_triggers.retain(|t| t.keyword != keyword);

    if config.custom_triggers.len() == before_len {
        return Err(format!("触发词 '{keyword}' 不存在"));
    }

    engine
        .update_config(&plugin_id, config, Some(&router))
        .await?;
    tracing::info!(plugin_id, keyword, "自定义触发词已删除");
    Ok(())
}

/// 获取引擎配置（通用 API）。
#[tauri::command]
pub async fn get_engine_config(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<serde_json::Value, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    Ok(crate::app::config::get_engine_config(pool, &engine_id)
        .await
        .unwrap_or_else(|| serde_json::json!({})))
}

/// 获取 Context 层配置（设置页用）。优先读内存 state（最新），兜底读 DB。
#[tauri::command]
pub async fn get_context_config(
    app: tauri::AppHandle,
) -> Result<crate::app::config::ContextConfig, String> {
    if let Some(mem) =
        app.try_state::<std::sync::Arc<std::sync::RwLock<crate::app::config::ContextConfig>>>()
    {
        return Ok(mem.read().unwrap().clone());
    }
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    Ok(crate::app::config::get_context_config(pool).await)
}

/// 打开文件/快捷方式所在文件夹（explorer /select 定位选中）。
/// §5 约束：lnk_path 不归一化，透传原路径字符串。
/// 但 explorer /select 对正斜杠路径解析异常（会打开"文档"等默认位置），需归一化为反斜杠。
#[tauri::command]
pub async fn open_containing_folder(path: String) -> Result<(), String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("路径为空".into());
    }
    // explorer /select 不认正斜杠，统一为反斜杠
    let normalized = path.replace('/', "\\");
    tracing::info!(original = %path, normalized = %normalized, "open_containing_folder");

    // 用 ShellExecuteW 直接调 explorer——绕过 std::process::Command 的参数拼接，
    // 避免 CreateProcessW 对含空格/特殊字符路径的转义问题。
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    let arg = format!("/select,{normalized}");
    let arg_wide: Vec<u16> = arg.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("explorer"),
            PCWSTR(arg_wide.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 返回值 > 32 表示成功
    if result.0 as i32 <= 32 {
        return Err(format!("ShellExecuteW 失败，返回值: {}", result.0 as i32));
    }
    Ok(())
}

/// 解析 .lnk 快捷方式目标，用 explorer /select 定位到目标文件。
/// 非文件路径的快捷方式（URL、UWP 等）会返回错误。
#[tauri::command]
pub async fn open_lnk_target(lnk_path: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
        use windows::Win32::System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize, IPersistFile,
        };
        use windows::Win32::UI::Shell::{IShellLinkW, ShellExecuteW};
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use windows::core::{GUID, Interface, PCWSTR, w};

        // CLSID_ShellLink（00021401-0000-0000-C000-000000000046）
        const CLSID_SHELLLINK: GUID = GUID::from_u128(0x00021401_0000_0000_C000_000000000046);

        // COM 初始化（与 icon.rs 同模式：APARTMENTTHREADED，已初始化则跳过）
        let com_hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let should_uninit = com_hr.is_ok();
        struct ComUninit(bool);
        impl Drop for ComUninit {
            fn drop(&mut self) {
                if self.0 {
                    unsafe { CoUninitialize() };
                }
            }
        }
        let _com = ComUninit(should_uninit);

        unsafe {
            // 创建 ShellLink COM 对象
            let link: IShellLinkW = CoCreateInstance(&CLSID_SHELLLINK, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("创建 ShellLink 失败: {e}"))?;

            // 加载 .lnk 文件
            let persist: IPersistFile = link
                .cast()
                .map_err(|e| format!("获取 IPersistFile 失败: {e}"))?;
            let lnk_wide: Vec<u16> = lnk_path.encode_utf16().chain(std::iter::once(0)).collect();
            persist
                .Load(
                    PCWSTR(lnk_wide.as_ptr()),
                    windows::Win32::System::Com::STGM_READ,
                )
                .map_err(|e| format!("加载 .lnk 失败: {e}"))?;

            // 解析目标路径
            let mut buf = [0u16; 1024];
            let mut find_data: WIN32_FIND_DATAW = std::mem::zeroed();
            link.GetPath(&mut buf, &mut find_data as *mut _, 0)
                .map_err(|e| format!("获取目标路径失败: {e}"))?;

            let target = PCWSTR(buf.as_ptr())
                .to_string()
                .map_err(|e| format!("路径转换失败: {e}"))?;
            let target = target.trim();

            if target.is_empty() {
                return Err("快捷方式未指向文件路径（可能是 URL 或 UWP 应用）".into());
            }

            // 用 explorer /select 定位到目标文件
            let normalized = target.replace('/', "\\");
            let arg = format!("/select,{normalized}");
            let arg_wide: Vec<u16> = arg.encode_utf16().chain(std::iter::once(0)).collect();
            let result = ShellExecuteW(
                None,
                w!("open"),
                w!("explorer"),
                PCWSTR(arg_wide.as_ptr()),
                None,
                SW_SHOWNORMAL,
            );
            if result.0 as i32 <= 32 {
                return Err(format!("ShellExecuteW 失败，返回值: {}", result.0 as i32));
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("spawn_blocking 失败: {e}"))?
}

/// 重置某项的历史记录权重（右键菜单「重置该项记录」，0.5.3）。
#[tauri::command]
pub async fn reset_item_history(app: tauri::AppHandle, lnk_path: String) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::history::reset_weight(pool, &lnk_path).await;
    tracing::debug!(path = %lnk_path, "已重置该项历史权重");
    Ok(())
}

/// 列出当前有可见窗口的运行中进程（设置页「敏感应用」选择器用）。
/// spawn_blocking 隔离 Win32 枚举，避免阻塞 async runtime。
#[tauri::command]
pub async fn list_running_processes() -> Vec<crate::infra::platform::context::RunningProcess> {
    tokio::task::spawn_blocking(crate::infra::platform::context::list_running_processes)
        .await
        .unwrap_or_default()
}

/// 录制快捷键（阻塞，直到用户按下组合键或超时）。
#[tauri::command]
pub async fn record_hotkey() -> Result<serde_json::Value, String> {
    // 在阻塞线程中等待录制（事件由 ll_proc 喂入 recorder 状态机）
    let result =
        tokio::task::spawn_blocking(crate::infra::platform::hotkey::record_hotkey_blocking)
            .await
            .map_err(|e| e.to_string())?;

    match result {
        Some(record) => {
            let val = serde_json::json!({
                "modifiers": record.modifiers,
                "key": record.key,
                "display": record.display,
            });
            tracing::debug!("record_hotkey: → Ok display={}", record.display);
            Ok(val)
        }
        None => {
            tracing::warn!("record_hotkey: → Err (None)");
            Err("录制超时或取消".to_string())
        }
    }
}

/// 记录剪贴板命中（用户选择粘贴某条历史）。
#[tauri::command]
pub async fn record_clipboard_hit(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::record_hit(pool, &id).await;
    Ok(())
}

/// 在外部浏览器打开 URL。
///
/// **0.19.0**：改经 `CapabilityRegistry` 调 `OpenUrl` Capability，消除双入口
/// （旧实现直调 `ShellExecuteW`，与 Capability 路径走 `open::that` 是两套独立底层）。
#[tauri::command]
pub async fn open_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    tracing::debug!(%url, "open_url");

    let cap_reg = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();

    let env_arc = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();

    let args = serde_json::json!({ "url": url });
    let ctx = crate::domain::capability::InvokeContext {
        env: env_arc.as_ref(),
        origin: crate::domain::capability::InvocationOrigin::LocalCommand,
        runtime: crate::domain::capability::RuntimeCapabilities {
            surface: None,
            main_process: true,
            desktop_session: true,
        },
        deadline: None,
    };

    // 0.21.0: 走 registry.invoke 执行 origin/runtime 门禁
    cap_reg
        .invoke("open_url", args, &ctx)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 列出所有可暴露的 Capability（设置页勾选用）。
/// 返回 (id, description, sensitive) 三元组。
#[tauri::command]
pub async fn list_exposable_capabilities(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let cap_registry = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    cap_registry
        .list()
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "id": s.name,
                "description": s.description,
                "sensitive": s.sensitive,
            })
        })
        .collect()
}

/// 0.13.6: 在资源管理器中打开指定目录路径。
///
/// 供设置页 Skill 卡片「打开目录」按钮调用——打开单个 Skill 所在的目录。
#[tauri::command]
pub async fn open_dir_in_explorer(path: String) -> Result<(), String> {
    let dir = std::path::Path::new(&path);
    if !dir.exists() {
        return Err(format!("目录不存在: {path}"));
    }
    std::process::Command::new("explorer.exe")
        .arg(dir)
        .spawn()
        .map_err(|e| format!("打开资源管理器失败: {e}"))?;
    Ok(())
}

/// 结束一个截图会话（0.11.7-f helper）：清标注模式 + 隐藏 overlay + 清 SESSION。
///
/// `screenshot_copy/pin/save/cancel` 都以此收尾，一处修改多处受益。
mod context_menu;
mod media;
pub use context_menu::*;
pub use media::*;
