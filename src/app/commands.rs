//! Tauri command 层：前端 invoke 入口，组合 core/search/history 能力。
//!
//! 命令保持轻量——编排逻辑，不含业务实现。

use tauri::{Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

// SttEngine trait needed for LocalSttEngine::finalize() in diagnose_stt
use crate::domain::stt::SttEngine;

/// 打开文件选择对话框，返回选中的文件路径（取消时返回 null）。
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
        if let Some(name) = filter.get("name").and_then(|v| v.as_str()) {
            if let Some(exts) = filter.get("extensions").and_then(|v| v.as_array()) {
                let extensions: Vec<&str> = exts.iter().filter_map(|e| e.as_str()).collect();
                if !extensions.is_empty() {
                    dialog = dialog.add_filter(name, &extensions);
                }
            }
        }
    }
    dialog.blocking_pick_file().and_then(|p| match p {
        tauri_plugin_dialog::FilePath::Path(path) => path.to_str().map(|s| s.to_string()),
        tauri_plugin_dialog::FilePath::Url(url) => Some(url.to_string()),
    })
}

/// 主窗口 ESC 调用：隐藏主窗口。
#[tauri::command]
pub fn hide_window(app: tauri::AppHandle) {
    crate::infra::platform::window::hide(&app, "ESC");
}

/// **临时**（0.11.7-f 调试用）：前端把 console 日志转发到后端 tracing。
///
/// TODO(0.11.7 收尾)：0.11.7 稳定后删除此 command 与前端 `frontendLog()` 封装。
/// 前端诊断转由 devtools 完成。
#[tauri::command]
pub fn frontend_log(level: String, message: String) {
    match level.as_str() {
        "error" => tracing::error!(target: "blink::frontend", "{message}"),
        "warn" => tracing::warn!(target: "blink::frontend", "{message}"),
        "info" => tracing::info!(target: "blink::frontend", "{message}"),
        "debug" => tracing::debug!(target: "blink::frontend", "{message}"),
        _ => tracing::trace!(target: "blink::frontend", "{message}"),
    }
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

/// 前端按 Tab 采纳 AI Ghost Suggestion 时调用(0.9.2 Phase 5b)。
///
/// **为什么单独命令而非走 search_apps 的 debounce 路径**:
/// - AI 调用相对昂贵(几百 ms 到几秒)且消耗 token,不能因打字过程反复触发
/// - 用户显式按 Tab 才走 → 单次调用充分执行,避免 h2 stream 堆积/自 cancel
///
/// 参数:
/// - `query`:要问 AI 的原文(前端保存的 `suggestion.replacement`)
/// - `seq`:与 search 复用同一自增计数,让后续 emit 的结果能被 results.js 正确匹配
#[tauri::command]
pub async fn trigger_ai(query: String, seq: u64, app: tauri::AppHandle) -> Result<(), String> {
    tracing::debug!(
        target: crate::infra::utils::perf::ai_slo::TARGET,
        "AI trigger: seq={seq} qlen={}",
        query.chars().count(),
    );
    let service = app.state::<std::sync::Arc<crate::domain::search::SearchService>>();
    service.trigger_ai(query, seq);
    Ok(())
}

/// 前端回车/点击时调用：启动选中的应用（普通 lnk 路径）。
///
/// 0.8.0 §1.3 起，内置动作走 `run_builtin_action`（前端 `Action.kind == "run"` 时分派），
/// 此命令只处理真正的文件/应用路径。计算结果无 lnk_path，忽略。
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
    crate::domain::search::launch(&lnk_path)?;
    crate::infra::platform::window::hide(&app, "launch");
    Ok(())
}

// 0.8.6 重构：execute_builtin_action 已迁移到 domain::execution 的各 Action struct。
// run_builtin_action 现在通过 ActionRegistry 查找并执行。

/// 运行内置动作（0.8.0 §1.3 / 0.8.6 §8.1.1 重构）。
///
/// 前端 `Action.kind === "run"` → `invoke("run_builtin_action", { id, arg })`。
/// `id` 为内置动作注册表 key（如 `"open_settings"`），后端按 id 从 `ActionRegistry` 查找
/// 对应的 `Action` 实现并执行。
///
/// 0.8.6 重构：原 `BuiltinActionKind` match 分支迁移到 `domain::execution` 的
/// 各 `Action` struct 实现，本函数变为薄委托层。
///
/// 未知 id → 返回 `Err`；前端会打印到控制台，不弹窗。
#[tauri::command]
pub async fn run_builtin_action(
    app: tauri::AppHandle,
    id: String,
    arg: Option<serde_json::Value>,
) -> Result<(), String> {
    tracing::debug!(%id, ?arg, "run_builtin_action: 收到请求");

    let registry = app.state::<std::sync::Arc<crate::domain::execution::ActionRegistry>>();
    let Some(action) = registry.get(&id) else {
        let msg = format!("未知内置动作 id: {id}");
        tracing::warn!(%id, "run_builtin_action: 未知 id");
        return Err(msg);
    };

    let cx = crate::domain::execution::ActionContext::new(&app, arg);
    match action.execute(&cx).await {
        Ok(_outcome) => {
            // 内置动作全部返回 Nop；outcome 为未来扩展预留
        }
        Err(e) => {
            tracing::error!(%id, error = %e, "内置动作执行失败");
            return Err(e.to_string());
        }
    }

    // 所有内置动作都隐藏主窗口；设置窗口在 OpenSettings 分支里已单独显示。
    crate::infra::platform::window::hide(&app, "run_builtin_action");
    Ok(())
}

/// AI Dangerous 动作确认执行（0.9.2 第二步）。
///
/// 前端收到 `blink://ai-confirm-action` 事件后展示确认卡片,
/// 用户按 Enter 确认 → invoke 此 command → 后端执行动作。
///
/// **安全**:与 `run_builtin_action` 同样的查找 + 执行路径,
/// 但 arguments 来自 AI 的 `ToolCall.arguments`(结构化 JSON Object),
/// 走 `ActionContext::from_arguments` 而非 `ActionContext::new`。
///
/// **审计**（0.11.4 补）:用户确认执行后写入 `ai_tool_audit` 表,
/// turn=0 标记"用户确认执行"路径（区别于 Turn 1/Turn 2 自动执行）。
#[tauri::command]
pub async fn confirm_ai_action(
    app: tauri::AppHandle,
    action_name: String,
    arguments: serde_json::Value,
) -> Result<(), String> {
    tracing::debug!(%action_name, ?arguments, "confirm_ai_action: 用户确认 AI 动作");

    let registry = app.state::<std::sync::Arc<crate::domain::execution::ActionRegistry>>();
    let Some(action) = registry.get(&action_name) else {
        let msg = format!("未知动作 id: {action_name}");
        tracing::warn!(%action_name, "confirm_ai_action: 未知 id");
        return Err(msg);
    };

    let cx = crate::domain::execution::ActionContext::from_arguments(&app, arguments.clone());
    match action.execute(&cx).await {
        Ok(outcome) => {
            tracing::info!(%action_name, "confirm_ai_action: 执行成功");

            let pool = &app.state::<crate::infra::data::DbPools>().ai;
            let (provider_kind_str, model_id_str) =
                match app.try_state::<std::sync::Arc<crate::domain::ai::AIProviderRegistry>>() {
                    Some(reg) => match reg.resolve(crate::app::ai_config::Tier::Router) {
                        Ok((provider, _tier)) => (
                            provider.kind().as_serde_str().to_string(),
                            provider.model_id().to_string(),
                        ),
                        Err(_) => (String::new(), String::new()),
                    },
                    None => (String::new(), String::new()),
                };
            let summary = format!(
                "用户确认执行: {}",
                crate::domain::search::outcome_to_summary(&outcome)
            );
            crate::infra::data::ai_audit::save_audit_log(
                &pool,
                &action_name,
                &arguments,
                &summary,
                &provider_kind_str,
                &model_id_str,
                0,
            )
            .await;
        }
        Err(e) => {
            tracing::error!(%action_name, error = %e, "confirm_ai_action: 执行失败");
            return Err(e.to_string());
        }
    }

    crate::infra::platform::window::hide(&app, "confirm_ai_action");
    Ok(())
}

/// 对话窗口危险操作确认（0.12.0 §2.4 闭环骨架）。
///
/// 对话窗口前端收到 `blink://chat-confirm-action` 事件后展示确认卡片，
/// 用户确认/拒绝 -> invoke 此 command -> 唤醒 tool_adapter 挂起的 `call`。
///
/// **与 `confirm_ai_action` 的区别**：主窗口的 `confirm_ai_action` 重新执行 action
/// （外部调度，service.rs 的 tool loop 不阻塞）；对话窗口的 `confirm_chat_action`
/// 只送信号（rig agent loop 内部 `call` 挂起等待，确认后由 `call` 自己执行）。
/// 两者事件名 / payload / 闭环路径都不同，故分离。
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

/// 隐藏独立 chat 窗口（0.12.1 Phase 3A）。
///
/// 窗口层保证先中止 active request，再隐藏 WebView。窗口不存在时 no-op。
#[tauri::command]
pub fn hide_chat_window(app: tauri::AppHandle) {
    crate::infra::platform::window::hide_chat_window(&app);
}

/// 启动对话 prompt（Phase 4）。
///
/// 调用 `ChatService::prompt()` 获取流式 chunk receiver，spawn 后台 task 逐 chunk
/// 包装成 `ChatStreamEvent` 后定向 emit 到 chat 窗口（`blink://chat-stream`）。
///
/// 返回 `request_id`，前端据此过滤已中止请求的尾部 chunk。
/// 若已有 active request，返回错误。
#[tauri::command]
pub async fn chat_prompt(
    app: tauri::AppHandle,
    conversation_id: String,
    message: String,
) -> Result<u64, String> {
    let chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .ok_or("ChatService 未注册")?;

    let handle = chat
        .prompt(conversation_id, message)
        .await
        .map_err(|e| e.to_string())?;
    let request_id = handle.request_id;
    let conv_id = handle.conversation_id.clone();
    let mut chunks = handle.chunks;

    // spawn 后台 task 消费 chunk 流并定向 emit 到 chat 窗口
    let app_clone = app.clone();
    let conv_id_clone = conv_id.clone();
    tokio::spawn(async move {
        while let Some(chunk) = chunks.recv().await {
            let is_done = matches!(
                chunk,
                crate::domain::ai::agent_provider::ChatStreamChunk::Done { .. }
                    | crate::domain::ai::agent_provider::ChatStreamChunk::Error { .. }
            );
            let event = crate::domain::ai::chat_service::ChatStreamEvent {
                request_id,
                conversation_id: conv_id_clone.clone(),
                chunk,
            };
            let _ = app_clone.emit_to(
                tauri::EventTarget::window("chat"),
                "blink://chat-stream",
                &event,
            );
            if is_done {
                break;
            }
        }
        // 注意：自然完成时 ChatService::prompt 内部的 task 已调 clear_if(request_id)，
        // 此处无需再调 abort，避免冗余。
    });

    tracing::debug!(
        request_id,
        conversation_id = %conv_id,
        "chat_prompt: 后台 stream task 已启动"
    );
    Ok(request_id)
}

/// 中止指定的对话请求（Phase 4）。
///
/// 返回 `true` = 已中止；`false` = request_id 不存在（已完成或已中止）。
#[tauri::command]
pub fn chat_abort(app: tauri::AppHandle, request_id: u64) -> bool {
    if let Some(chat) =
        app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
    {
        chat.abort(request_id)
    } else {
        false
    }
}

/// 获取对话服务状态（Phase 4）。
///
/// 返回 `{ active, provider_configured, provider_name?, model_name? }`。
/// 0.12.2：`provider_name`/`model_name` 反映当前生效模型（selected 优先，Main 回落）。
#[tauri::command]
pub fn get_chat_status(app: tauri::AppHandle) -> crate::domain::ai::chat_service::ChatStatus {
    if let Some(chat) =
        app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
    {
        chat.status()
    } else {
        crate::domain::ai::chat_service::ChatStatus {
            active: None,
            provider_configured: false,
            provider_name: None,
            model_name: None,
        }
    }
}

/// chat 窗口可选模型项（0.12.2 §4.4）。
///
/// `id` 形如 `"{provider_id}:{model_id}"`，前端选中后回传给 `select_chat_model`。
/// `is_main` / `is_light` 标注该模型是否是当前 Main / Light 档（供下拉置顶快捷项）。
/// `is_selected` 标注该模型是否是 chat 当前生效模型（selected 优先，否则 Main 档）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct ChatModelOption {
    pub id: String,
    pub provider_name: String,
    pub model_name: String,
    pub is_main: bool,
    pub is_light: bool,
    pub is_selected: bool,
}

/// 列出 chat 可选的所有 Chat 能力模型（0.12.2 §4.4）。
///
/// 从 `AIConfig.providers` 遍历，按 `ModelCapability::Chat` 过滤，标注 Main/Light 档
/// 和当前 selected。前端据此渲染模型选择器下拉。
#[tauri::command]
pub fn get_chat_models(app: tauri::AppHandle) -> Vec<ChatModelOption> {
    use crate::app::ai_config::{ModelCapability, Tier};

    let Some(registry) =
        app.try_state::<std::sync::Arc<crate::domain::ai::registry::AIProviderRegistry>>()
    else {
        return Vec::new();
    };
    let config = registry.config_snapshot();

    // 解析 Main / Light 档当前指向（悬空则 None）
    let main_pair = config.resolve_tier(Tier::Main).map(|(p, m, _)| (p.id.clone(), m.id.clone()));
    let light_pair = config
        .resolve_tier(Tier::Light)
        .map(|(p, m, _)| (p.id.clone(), m.id.clone()));

    // 当前生效 selected（None 时回落 Main 档）
    let selected_pair = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .and_then(|chat| chat.current_selection())
        .map(|sel| (sel.provider_id, sel.model_id))
        .or_else(|| main_pair.clone());

    let mut options = Vec::new();
    for provider in &config.providers {
        for model in &provider.models {
            // 只列 Chat 能力 + enabled 的模型
            if !model.enabled || !model.capabilities.contains(&ModelCapability::Chat) {
                continue;
            }
            let id = format!("{}:{}", provider.id, model.id);
            let model_name = if model.display_name.is_empty() {
                model.id.clone()
            } else {
                model.display_name.clone()
            };
            let is_main = main_pair
                .as_ref()
                .is_some_and(|(pid, mid)| *pid == provider.id && *mid == model.id);
            let is_light = light_pair
                .as_ref()
                .is_some_and(|(pid, mid)| *pid == provider.id && *mid == model.id);
            let is_selected = selected_pair
                .as_ref()
                .is_some_and(|(pid, mid)| *pid == provider.id && *mid == model.id);
            options.push(ChatModelOption {
                id,
                provider_name: provider.display_name.clone(),
                model_name,
                is_main,
                is_light,
                is_selected,
            });
        }
    }
    options
}

/// 设置 chat 运行时选中模型（0.12.2 §4.4）。
///
/// - `selection_id = None` 或空字符串：恢复 Main 档默认。
/// - `selection_id = Some("{provider_id}:{model_id}")`：切换到指定模型。
///
/// 返回 `true` = 切换成功；`false` = id 格式错误或 model 不存在/无 Chat 能力。
/// 切换成功后 ChatService 清 cached_agent，下次 prompt 按新模型重建 AgentProvider。
#[tauri::command]
pub async fn select_chat_model(
    app: tauri::AppHandle,
    selection_id: Option<String>,
) -> Result<bool, String> {
    use crate::app::ai_config::ModelCapability;

    let Some(chat) =
        app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
    else {
        return Err("ChatService 未注册".to_string());
    };
    let Some(registry) =
        app.try_state::<std::sync::Arc<crate::domain::ai::registry::AIProviderRegistry>>()
    else {
        return Err("AIProviderRegistry 未注册".to_string());
    };

    // None / 空字符串 = 恢复 Main 档
    let selection_id = match selection_id {
        None => {
            chat.select_model(None);
            return Ok(true);
        }
        Some(s) if s.trim().is_empty() => {
            chat.select_model(None);
            return Ok(true);
        }
        Some(s) => s,
    };

    // 解析 "{provider_id}:{model_id}"——注意 model_id 可能含冒号，只按第一个冒号切
    let Some((provider_id, model_id)) = selection_id.split_once(':') else {
        return Ok(false);
    };
    if provider_id.is_empty() || model_id.is_empty() {
        return Ok(false);
    }

    // 校验存在 + Chat 能力
    let config = registry.config_snapshot();
    let provider = config
        .providers
        .iter()
        .find(|p| p.id == provider_id)
        .ok_or_else(|| format!("provider 不存在: {provider_id}"))?;
    let model = provider
        .models
        .iter()
        .find(|m| m.id == model_id && m.enabled)
        .ok_or_else(|| format!("model 不存在或已禁用: {model_id}"))?;
    if !model.capabilities.contains(&ModelCapability::Chat) {
        return Ok(false);
    }

    let model_name = if model.display_name.is_empty() {
        model.id.clone()
    } else {
        model.display_name.clone()
    };
    let selection = crate::domain::ai::chat_service::ChatModelSelection {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        provider_display_name: provider.display_name.clone(),
        model_display_name: model_name,
    };
    chat.select_model(Some(selection));
    Ok(true)
}

/// 列出所有内置动作元数据 + 当前 enabled 状态（0.8.0 §1.3 / 0.8.6 §8.2.4 i18n）。
#[tauri::command]
pub async fn list_builtin_actions(
    app: tauri::AppHandle,
) -> Vec<crate::domain::search::BuiltinActionInfo> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let disabled = crate::app::config::get_disabled_builtin_actions(&pool).await;
    let registry = app.state::<std::sync::Arc<crate::domain::execution::ActionRegistry>>();
    // 读当前语言（从 AppConfig 快照取）
    let config = crate::app::config::get_config(&pool).await;
    crate::domain::search::list_builtin_actions(&disabled, &registry, &config.language)
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
#[tauri::command]
pub async fn trigger_chord(app: tauri::AppHandle, key: String) -> Result<(), String> {
    tracing::debug!(%key, "trigger_chord");
    let Some(registry) = app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
    else {
        return Err("chord registry 未就绪".into());
    };
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    // 0.10.7：读 chord 配置（bindings + disabled），键位由 binding 覆盖
    let chord_cfg = crate::app::config::get_chord_config(&pool).await;
    let disabled = crate::app::config::get_disabled_chord_actions(&pool).await;
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
    // surface 现已无 command 层消费者（MiniBall 划词已移除，各 action 自管 UI），
    // 保留 trigger 返回值以备未来扩展。
    let _surface = registry.trigger(&key, &chord_cfg.bindings, &app).await?;
    Ok(())
}

/// 结束一个截图会话（0.11.7-f helper）：清标注模式 + 隐藏 overlay + 清 SESSION。
///
/// `screenshot_copy/pin/save/cancel` 都以此收尾，一处修改多处受益。
fn finish_screenshot_session(app: &tauri::AppHandle) {
    crate::infra::platform::screenshot::set_annotation_mode(false);
    crate::infra::platform::window::hide_screenshot_overlay(app);
}

/// 0.11.7-f：接收前端合成后的 PNG（裁剪区 + 标注），写入剪贴板，结束会话。
///
/// **替代** 0.8.7 `capture_region`（后端从 SESSION 裁剪）——现在前端一份合成路径
/// 走通所有输出（复制/保存/钉图），双击全屏也走这里。
///
/// **异步执行**（0.11.7 review 修）：PNG 解码 + BGRA swap + Win32 剪贴板写入都是
/// 同步 CPU/syscall 密集操作，全屏 2560x1440 约 50-100ms。放在异步命令直接跑会
/// 阻塞 tokio 工作线程，影响其他并发任务。用 `spawn_blocking` 挪到阻塞线程池。
///
/// **快路径**：如果只需要复制选区（无标注、无全屏合成），前端应走 `screenshot_copy_region`
/// 直接传坐标——避开前端 toBlob PNG 编码 + 后端 PNG 解码的双重开销，全屏路径
/// 快 ~150-250ms。有标注 / 全屏合成时才走本命令。
#[tauri::command]
pub async fn screenshot_copy(app: tauri::AppHandle, png_data: Vec<u8>) -> Result<(), String> {
    let bytes_len = png_data.len();
    tokio::task::spawn_blocking(move || {
        crate::infra::platform::clipboard::write_png_to_clipboard(&png_data)
    })
    .await
    .map_err(|e| format!("spawn_blocking join 失败: {e}"))??;
    finish_screenshot_session(&app);
    tracing::info!(bytes = bytes_len, "截图已保存到剪贴板");
    Ok(())
}

/// 0.11.7 快路径：直接从 SESSION 裁剪 BGRA → 写剪贴板，跳过 PNG 编解码往返。
///
/// **适用场景**：无标注（前端 `annot.hasAnnotations() == false`）+ 有选区。
///
/// 相比 `screenshot_copy(png_data)` 的收益：
/// - 前端省 `canvas.toBlob('image/png')`（2560x1440 ~150ms）
/// - 后端省 `image::load_from_memory` PNG 解码（~50-100ms）
/// - IPC payload 从 PNG (~几 MB) 变成 16 字节坐标
///
/// 坐标是物理像素、SESSION 坐标系（虚拟屏幕原点为 (0,0)）——前端 mouseup 时
/// 已按 DPR 转换过。裁剪越界会被 `crop()` 自身 clamp。
#[tauri::command]
pub async fn screenshot_copy_region(
    app: tauri::AppHandle,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> Result<(), String> {
    // BGRA 裁剪 + 剪贴板写入都同步，挪到阻塞线程池
    tokio::task::spawn_blocking(move || -> Result<(u32, u32), String> {
        let (bgra, cw, ch) = crate::infra::platform::screenshot::crop(x, y, w, h)
            .ok_or_else(|| "SESSION 为空或选区越界".to_string())?;
        crate::infra::platform::clipboard::write_bgra_to_clipboard(&bgra, cw, ch)?;
        Ok((cw, ch))
    })
    .await
    .map_err(|e| format!("spawn_blocking join 失败: {e}"))?
    .map(|(cw, ch)| tracing::info!(w = cw, h = ch, "截图选区已直传剪贴板（快路径）"))?;
    finish_screenshot_session(&app);
    Ok(())
}

/// 0.11.7-f：取消截图，结束会话，不保存。
#[tauri::command]
pub fn screenshot_cancel(app: tauri::AppHandle) {
    finish_screenshot_session(&app);
    tracing::info!("截图已取消");
}

/// 0.11.7-f：钉图——接收前端合成后的 PNG，创建钉图窗口。
///
/// `screen_x`/`screen_y` 为选区左上角的**虚拟屏幕物理坐标**，
/// 让钉图窗口定位到截图原位（"就地贴住"）。
#[tauri::command]
pub fn screenshot_pin(
    app: tauri::AppHandle,
    png_data: Vec<u8>,
    screen_x: i32,
    screen_y: i32,
) -> Result<(), String> {
    crate::infra::platform::window::show_pin_window(&app, png_data, screen_x, screen_y)?;
    finish_screenshot_session(&app);
    tracing::info!(screen_x, screen_y, "截图已钉到屏幕");
    Ok(())
}

/// 0.11.7-f：保存截图选区为文件（PNG/JPEG）。
///
/// `path=None` 弹出保存对话框；用户取消时返回 Err，前端应识别 "用户取消了保存"
/// 字符串以避免噪音。
#[tauri::command]
pub async fn screenshot_save(
    app: tauri::AppHandle,
    png_data: Vec<u8>,
    path: Option<String>,
) -> Result<String, String> {
    use std::io::Write;

    let file_path = match path {
        Some(p) => p,
        None => {
            let timestamp = chrono::Local::now().format("截图_%Y%m%d_%H%M%S");
            let default_name = format!("{}.png", timestamp);
            let dialog = app.dialog().file();
            let picked = dialog
                .add_filter("PNG 图片", &["png"])
                .add_filter("JPEG 图片", &["jpg", "jpeg"])
                .set_file_name(&default_name)
                .blocking_save_file();
            match picked {
                Some(path) => path.to_string(),
                None => return Err("用户取消了保存".to_string()),
            }
        }
    };

    let mut file = std::fs::File::create(&file_path).map_err(|e| format!("创建文件失败: {e}"))?;
    file.write_all(&png_data)
        .map_err(|e| format!("写入文件失败: {e}"))?;

    finish_screenshot_session(&app);
    tracing::info!(path = %file_path, "截图已保存到文件");
    Ok(file_path)
}

/// 0.11.7-d：隐藏钉图窗口（hide 而非 close，保留窗口实例供下次钉图复用）。
#[tauri::command]
pub fn screenshot_pin_hide(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("chord-pin") {
        let _ = win.hide();
    }
}

/// 0.11.8：钉图窗口一次性设置位置 + 尺寸（缩放/拖动/onload 跟随共用）。
///
/// 走 Win32 `SetWindowPos` 原子地设位置+尺寸，绕开 Tauri 逻辑像素 DPI 竞态。
/// 参数均为**屏幕物理像素**：
/// - `win_x`/`win_y`：窗口左上角屏幕坐标（= 图片左上 - PIN_PAD）
/// - `win_w`/`win_h`：窗口尺寸（= 图片显示尺寸 + 2×PIN_PAD，含发光区）
///
/// 前端在缩放/拖动时算好这 4 个值一次性传入，避免多次 set_position/set_size 竞态。
#[tauri::command]
pub fn screenshot_pin_transform(
    app: tauri::AppHandle,
    win_x: i32,
    win_y: i32,
    win_w: u32,
    win_h: u32,
) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    if let Some(win) = app.get_webview_window("chord-pin") {
        if let Ok(hwnd) = win.hwnd() {
            crate::infra::platform::window::place_at_physical(
                HWND(hwnd.0 as _),
                win_x,
                win_y,
                win_w,
                win_h,
            );
        }
    }
    Ok(())
}

/// 0.11.7-c：OCR 识别图片中的文字，返回 `{text, lines}`。
///
/// 0.11.7-f：改走 `ocr_engine::backend()` 注入的后端（测试可替换）。
#[tauri::command]
pub async fn ocr_image(
    _app: tauri::AppHandle,
    png_data: Vec<u8>,
) -> Result<serde_json::Value, String> {
    let backend = crate::domain::capability::builtins::ocr_engine::backend();
    let result = backend
        .recognize(&png_data)
        .await
        .map_err(|e| format!("OCR 识别失败: {e}"))?;

    let json = serde_json::to_value(&result).map_err(|e| format!("序列化 OCR 结果失败: {e}"))?;
    tracing::debug!(text_len = result.text.len(), "OCR 识别完成");
    Ok(json)
}

/// 0.11.9-d：翻译文本命令——OCR 面板/工具栏"翻译"按钮的后端入口。
///
/// **绕过 AI 路径**：翻译是确定性动作(用户主动点按钮),不该经过 AI 意图判断
/// + 网络往返。直接走 `ActionRegistry` 找 translate 插件的 `translate` tool
/// (id = `builtin.translate:translate`),用 `ExecArg::UserExplicit(json_args)`
/// 执行,拿 `ActionOutcome::Items` 返 `items[0].title` 即译文。
///
/// **参数**：
/// - `text`: 待翻译文本（必填）
/// - `target_lang`: 目标语言代码(zh/en/ja/ko);`None` 时插件读 setting 默认值
///
/// **失败模式**：
/// - 插件未启用 / manifest 未加载 → 返 `"翻译插件未安装或未启用"`
/// - 插件返回空/错误 → 传递原错误信息
/// - 插件返回非 Items outcome → `"翻译插件返回意外的结果类型"`(理论不会,防御)
#[tauri::command]
pub async fn translate_text(
    app: tauri::AppHandle,
    text: String,
    target_lang: Option<String>,
) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("翻译文本不能为空".into());
    }

    let registry = app.state::<std::sync::Arc<crate::domain::execution::ActionRegistry>>();
    // translate 插件的 tool 注册 id = "{plugin_id}:{tool_name}" = "builtin.translate:translate"
    const TRANSLATE_ACTION_ID: &str = "builtin.translate:translate";
    let Some(action) = registry.get(TRANSLATE_ACTION_ID) else {
        tracing::warn!("translate_text: 翻译插件未注册");
        return Err("翻译插件未安装或未启用".into());
    };

    // 构造插件 tool arguments —— text 必填,target_lang 有值才传(None 让插件读 setting)
    let mut args = serde_json::Map::new();
    args.insert(
        "text".into(),
        serde_json::Value::String(trimmed.to_string()),
    );
    if let Some(lang) = target_lang
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        args.insert(
            "target_lang".into(),
            serde_json::Value::String(lang.to_string()),
        );
    }
    let arguments = serde_json::Value::Object(args);

    tracing::debug!(
        text_len = trimmed.chars().count(),
        ?target_lang,
        "translate_text: 调翻译插件"
    );

    let cx = crate::domain::execution::ActionContext::from_arguments(&app, arguments);
    let outcome = action
        .execute(&cx)
        .await
        .map_err(|e| format!("翻译执行失败: {e}"))?;

    match outcome {
        crate::domain::execution::ActionOutcome::Items { items } => {
            // 优先读 payload.translated（干净译文）；title 是 UI 展示用的，
            // 插件会给它加前缀 emoji（如 "📝 {result}"），不能当数据用。
            let translated = items
                .first()
                .and_then(|it| {
                    it.payload
                        .get("translated")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "翻译插件返回空结果".to_string())?;
            tracing::info!(
                src_len = trimmed.chars().count(),
                dst_len = translated.chars().count(),
                "translate_text 完成"
            );
            Ok(translated)
        }
        crate::domain::execution::ActionOutcome::Copy { text, .. } => {
            // 兼容:如果插件未来改走 Copy outcome,也取到译文
            Ok(text)
        }
        other => {
            tracing::warn!(?other, "translate_text: 翻译插件返回意外的 outcome");
            Err("翻译插件返回意外的结果类型".into())
        }
    }
}

/// 从 translate_batch 的首项 payload 读取保序结果。
fn parse_translate_batch_payload(
    outcome: &crate::domain::execution::ActionOutcome,
    expected: usize,
) -> Option<Vec<String>> {
    let crate::domain::execution::ActionOutcome::Items { items } = outcome else {
        return None;
    };
    let results = items.first()?.payload.get("results")?.as_array()?;
    if results.len() != expected {
        return None;
    }
    results
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect()
}

/// 0.11.10-g:批量翻译多行文本。
///
/// 首选一次调用插件 `translate_batch` tool，由插件加 tag 后单次请求翻译引擎并保序拆回。
/// 插件版本不匹配、tag 被引擎破坏或结构化结果异常时，降级为并发单行 `translate_text`，
/// 保证截图翻译功能不因批量优化失败而不可用。
#[tauri::command]
pub async fn translate_lines(
    app: tauri::AppHandle,
    lines: Vec<String>,
    target_lang: Option<String>,
) -> Result<Vec<String>, String> {
    if lines.is_empty() {
        return Ok(Vec::new());
    }
    let n = lines.len();
    tracing::debug!(count = n, ?target_lang, "translate_lines: 批量翻译开始");
    let started = std::time::Instant::now();

    // 空行不送插件，保留原索引；插件契约只接收非空文本。
    let non_empty: Vec<(usize, String)> = lines
        .iter()
        .enumerate()
        .filter(|(_, text)| !text.trim().is_empty())
        .map(|(idx, text)| (idx, text.clone()))
        .collect();
    if non_empty.is_empty() {
        return Ok(lines);
    }

    const TRANSLATE_BATCH_ACTION_ID: &str = "builtin.translate:translate_batch";
    let registry = app.state::<std::sync::Arc<crate::domain::execution::ActionRegistry>>();
    if let Some(action) = registry.get(TRANSLATE_BATCH_ACTION_ID) {
        let texts: Vec<String> = non_empty.iter().map(|(_, text)| text.clone()).collect();
        let mut args = serde_json::Map::new();
        args.insert("texts".into(), serde_json::json!(texts));
        if let Some(lang) = target_lang
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            args.insert(
                "target_lang".into(),
                serde_json::Value::String(lang.to_string()),
            );
        }
        let cx = crate::domain::execution::ActionContext::from_arguments(
            &app,
            serde_json::Value::Object(args),
        );
        match action.execute(&cx).await {
            Ok(outcome) => {
                if let Some(batch_results) =
                    parse_translate_batch_payload(&outcome, non_empty.len())
                {
                    let mut results = lines.clone();
                    for ((idx, _), translated) in non_empty.iter().zip(batch_results) {
                        results[*idx] = translated;
                    }
                    tracing::info!(
                        count = n,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "translate_lines 完成（单次批量 tool）"
                    );
                    return Ok(results);
                }
                tracing::warn!("translate_lines: 批量 tool 返回结构异常，降级为单行并发");
            }
            Err(e) => {
                tracing::warn!(error = %e, "translate_lines: 批量 tool 失败，降级为单行并发");
            }
        }
    } else {
        tracing::warn!("translate_lines: translate_batch 未注册，降级为单行并发");
    }

    let mut handles = Vec::with_capacity(non_empty.len());
    for (idx, text) in non_empty {
        let app_clone = app.clone();
        let tl = target_lang.clone();
        let src_for_fallback = text.clone();
        handles.push(tokio::spawn(async move {
            let result = translate_text(app_clone, text, tl).await;
            match result {
                Ok(dst) => (idx, dst),
                Err(e) => {
                    tracing::warn!(line = idx, error = %e, "translate_lines: 单行翻译失败，降级到原文");
                    (idx, src_for_fallback)
                }
            }
        }));
    }

    let mut results = lines;
    for handle in handles {
        match handle.await {
            Ok((idx, dst)) => results[idx] = dst,
            Err(e) => tracing::warn!(error = %e, "translate_lines: 任务 join 失败"),
        }
    }
    tracing::info!(
        count = n,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "translate_lines 完成（单行并发降级）"
    );
    Ok(results)
}

/// 0.11.7：设置/清除标注模式（前端通知后端）。
#[tauri::command]
pub fn screenshot_set_annotation_mode(active: bool) {
    crate::infra::platform::screenshot::set_annotation_mode(active);
}

/// 隐藏截图覆盖窗（ESC / 失焦 / 选区过小时调）。
#[tauri::command]
pub fn hide_screenshot_overlay(app: tauri::AppHandle) {
    crate::infra::platform::window::hide_screenshot_overlay(&app);
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
    let chord_cfg = crate::app::config::get_chord_config(&pool).await;
    let disabled = crate::app::config::get_disabled_chord_actions(&pool).await;
    let language = crate::app::config::get_config(&pool).await.language;
    let stt_enabled = crate::app::stt_config::get_stt_config().enabled;
    let ai_enabled = crate::app::ai_config::get_ai_config().enabled;
    let Some(registry) = app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
    else {
        return Vec::new();
    };
    registry
        .list(&disabled, &chord_cfg.bindings, &language)
        .into_iter()
        .filter(|a| !(a["id"] == "voice_input" && !stt_enabled))
        .filter(|a| !(a["id"] == "chat" && !ai_enabled))
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
    let chord_cfg = crate::app::config::get_chord_config(&pool).await;
    let disabled = crate::app::config::get_disabled_chord_actions(&pool).await;
    let language = crate::app::config::get_config(&pool).await.language;
    let stt_enabled = crate::app::stt_config::get_stt_config().enabled;
    let ai_enabled = crate::app::ai_config::get_ai_config().enabled;
    let Some(registry) = app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
    else {
        return Vec::new();
    };
    registry
        .list_all(&disabled, &chord_cfg.bindings, &language)
        .into_iter()
        .filter(|a| !(a["id"] == "voice_input" && !stt_enabled))
        .filter(|a| !(a["id"] == "chat" && !ai_enabled))
        .collect()
}

/// 当前 Alt 键是否物理按下（0.8.5 §6.1）。前端轮询驱动 alt-active 状态——
/// WebView2 不转发 Alt 键自身的 keydown 到 JS，前端监听不可靠，改轮询物理态。
#[tauri::command]
pub fn is_alt_down() -> bool {
    crate::infra::platform::hotkey::is_alt_down()
}

/// 0.10.7：设置 Chord 独占模式。前端在「主窗 focused + Alt hold + chordEligible」
/// 满足时调 `set_chord_mode(true)`，Alt 松开 / 失焦 / 不再 eligible 时调
/// `set_chord_mode(false)`。
///
/// 后端读 chord 配置派生 tap 键集合（只含 semantic=tap，排除 hold 的 voice_input），
/// 传给 hotkey 模块。LL hook 在 chord mode 下吞掉这些键的 keydown，独占 chord 触发。
///
/// **非阻塞**：配置读取走 blocking（hotkey 全局状态是同步的，命令本身只需一次读）。
/// 命令在 Tauri 命令线程执行，不阻塞 hook 线程。
#[tauri::command]
pub async fn set_chord_mode(app: tauri::AppHandle, on: bool) -> Result<(), String> {
    if !on {
        crate::infra::platform::hotkey::set_chord_mode(false, std::collections::HashSet::new());
        return Ok(());
    }
    // 派生 tap 键集合
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let chord_cfg = crate::app::config::get_chord_config(&pool).await;
    let disabled = crate::app::config::get_disabled_chord_actions(&pool).await;
    let language = crate::app::config::get_config(&pool).await.language;
    let stt_enabled = crate::app::stt_config::get_stt_config().enabled;
    let Some(registry) = app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
    else {
        return Err("chord registry 未就绪".into());
    };
    let actions = registry.list(&disabled, &chord_cfg.bindings, &language);
    let mut tap_keys = std::collections::HashSet::new();
    for a in actions {
        // voice_input 在 STT 未启用时已被 list 过滤；此处再按 semantic=tap 收集
        if a["semantic"] == "tap" {
            if let Some(key) = a["key"].as_str() {
                tap_keys.insert(key.to_lowercase());
            }
        }
    }
    let _ = stt_enabled; // voice_input 是 hold 语义，不会被收进 tap_keys
    crate::infra::platform::hotkey::set_chord_mode(true, tap_keys);
    Ok(())
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
    let config = crate::app::config::get_config(&pool).await;
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
    if let Some(reg) = app.try_state::<std::sync::Arc<crate::domain::execution::ActionRegistry>>() {
        let disabled_vec: Vec<String> = disabled.iter().cloned().collect();
        bindings.extend(crate::domain::search::list_builtin_context_bindings(
            &disabled_vec,
            &reg,
            &lang,
        ));
    }

    bindings
}

/// 设置页-存储：获取四库统计信息（0.12.0 DB 四层拆分）。
///
/// 返回各库的行数 + 文件大小 + 路径，前端渲染分区展示。
#[tauri::command]
pub async fn get_storage_info(app: tauri::AppHandle) -> serde_json::Value {
    let pools = app.state::<crate::infra::data::DbPools>();

    // 历史库：history + clipboard_history 行数
    let history_count = crate::infra::data::history::count(&pools.history).await;
    let clipboard_stats = crate::infra::data::clipboard::get_stats(&pools.history).await;
    let clipboard_count = clipboard_stats
        .get("total")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    // AI 库：ai_tool_audit 行数
    let ai_audit_count = crate::infra::data::ai_audit::count(&pools.ai).await;

    // 缓存库：performance_metrics + icon_cache 行数
    // 新代码直接用 data 层真源（utils::perf 的 count 是 re-export，仅兼容旧调用点）
    let perf_count = crate::infra::data::perf::count(&pools.cache).await;
    let icon_cache_count = crate::infra::data::icon_cache::count(&pools.cache).await;

    // P2.7: 迁移失败标记（若有，前端存储面板显示警告）
    let migration_failed: Option<String> =
        sqlx::query_scalar("SELECT value FROM config WHERE key = 'migration_failed'")
            .fetch_optional(&pools.config)
            .await
            .ok()
            .flatten();

    // 文件大小
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    let data_dir = std::path::PathBuf::from(&appdata).join("blink");

    serde_json::json!({
        "databases": {
            "config": {
                "name": "配置库",
                "file": "blink_config.db",
                "size_bytes": file_size(&data_dir.join("blink_config.db")),
                "path": data_dir.join("blink_config.db").display().to_string(),
            },
            "history": {
                "name": "历史库",
                "file": "blink_history.db",
                "size_bytes": file_size(&data_dir.join("blink_history.db")),
                "path": data_dir.join("blink_history.db").display().to_string(),
                "history_count": history_count,
                "clipboard_count": clipboard_count,
            },
            "ai": {
                "name": "AI 库",
                "file": "blink_ai.db",
                "size_bytes": file_size(&data_dir.join("blink_ai.db")),
                "path": data_dir.join("blink_ai.db").display().to_string(),
                "audit_count": ai_audit_count,
            },
            "cache": {
                "name": "缓存库",
                "file": "blink_cache.db",
                "size_bytes": file_size(&data_dir.join("blink_cache.db")),
                "path": data_dir.join("blink_cache.db").display().to_string(),
                "perf_count": perf_count,
                "icon_cache_count": icon_cache_count,
            },
        },
        "data_dir": data_dir.display().to_string(),
        // P2.7: 迁移失败标记（None = 正常；Some(reason) = 旧库迁移失败，前端显示警告）
        "migration_failed": migration_failed,
        // 兼容旧前端字段
        "history_count": history_count,
        "db_path": data_dir.display().to_string(),
    })
}

/// 获取文件大小（字节），不存在返回 0。
fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// 清空 AI 审计日志（设置页-存储「清除 AI 调用历史」）。
#[tauri::command]
pub async fn clear_ai_audit(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().ai;
    crate::infra::data::ai_audit::clear_all(&pool).await;
    tracing::info!("AI 审计日志已清空");
    Ok(())
}

/// 设置页-存储：打开数据文件夹（0.12.0 §2.2.7）。
///
/// 调 `ShellExecuteW("explorer", %APPDATA%\blink)` 打开数据目录。
#[tauri::command]
pub fn open_data_folder() -> Result<(), String> {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    let data_dir = std::path::PathBuf::from(&appdata).join("blink");
    // 目录不存在时先创建，避免 explorer 打开“文档”等默认位置
    if !data_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&data_dir) {
            return Err(format!("创建数据目录失败: {e}"));
        }
    }
    std::process::Command::new("explorer.exe")
        .arg(&data_dir)
        .spawn()
        .map_err(|e| format!("打开文件夹失败: {e}"))?;
    Ok(())
}

/// 设置页-存储：清空缓存库（0.12.0 §2.2.7）。
///
/// 清空 performance_metrics + icon_cache 两表。缓存可重建，清空无风险。
#[tauri::command]
pub async fn clear_cache_db(app: tauri::AppHandle) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    // 清空 performance_metrics
    crate::infra::data::perf::clear_all(&pools.cache)
        .await
        .map_err(|e| format!("清空 performance_metrics 失败: {e}"))?;
    // 清空 icon_cache
    crate::infra::data::icon_cache::clear_all(&pools.cache).await;
    tracing::info!("缓存库已清空（performance_metrics + icon_cache）");
    Ok(())
}

/// 设置页-关于：应用元信息（版本/名称/描述/仓库）。
/// 版本从 Cargo.toml 编译期注入（`CARGO_PKG_*`），tauri.conf.json 版本单独在 bundle 层使用。
/// CI release workflow 会根据 git tag 自动同步两处版本；本地开发手动维护 Cargo.toml 即可。
#[tauri::command]
pub fn get_app_info() -> serde_json::Value {
    serde_json::json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "description": env!("CARGO_PKG_DESCRIPTION"),
        "license": env!("CARGO_PKG_LICENSE"),
        "repository": env!("CARGO_PKG_REPOSITORY"),
    })
}

/// 设置页-关于：检查 GitHub 最新 Release 版本。
///
/// 流程：请求 GitHub API `/repos/{owner/repo}/releases/latest` →
/// 取 `tag_name` 去掉 `v` 前缀 → semver 比较与当前版本。
///
/// 返回 JSON：
/// - 成功：`{ has_update, current_version, latest_version, release_url }`
/// - 网络失败：`{ has_update: false, current_version, error: "..." }`
///
/// **走全局代理**：如果用户配置了 `engine:_global_proxy`，检查更新请求也走代理。
/// 国内直连 `api.github.com` 极易超时，这是此前「检查更新无效」的根因。
#[tauri::command]
pub async fn check_update(app: tauri::AppHandle) -> serde_json::Value {
    let current = env!("CARGO_PKG_VERSION");
    let repo = env!("CARGO_PKG_REPOSITORY");
    // 从 "https://github.com/owner/repo" 提取 "owner/repo"
    let repo_path = repo
        .trim_start_matches("https://github.com/")
        .trim_start_matches("http://github.com/")
        .trim_end_matches('/');

    let api_url = format!("https://api.github.com/repos/{repo_path}/releases/latest");

    // 读取全局代理配置，与插件 HTTP 请求共用
    let proxy_url = {
        let pool = &app.state::<crate::infra::data::DbPools>().config;
        let cfg = crate::app::config::get_engine_config(&pool, "_global_proxy").await;
        cfg.and_then(|v| {
            let https = v
                .get("https")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty());
            let http = v
                .get("http")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty());
            https.or(http).map(|s| s.to_string())
        })
    };

    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("blink-updater");

    if let Some(ref url) = proxy_url {
        match reqwest::Proxy::all(url) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(e) => tracing::warn!(%e, proxy = %url, "check_update: 代理配置无效，回退直连"),
        }
    }

    let client = builder.build().unwrap_or_default();

    let resp_result = client.get(&api_url).send().await;
    let resp = match resp_result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%e, "check_update: 请求 GitHub API 失败");
            return serde_json::json!({
                "has_update": false,
                "current_version": current,
                "error": format!("网络请求失败: {e}"),
            });
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "check_update: GitHub API 返回非 2xx");
        return serde_json::json!({
            "has_update": false,
            "current_version": current,
            "error": format!("GitHub API 返回 {}", resp.status()),
        });
    }
    let body = match resp.json::<serde_json::Value>().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(%e, "check_update: 解析 JSON 失败");
            return serde_json::json!({
                "has_update": false,
                "current_version": current,
                "error": "响应解析失败".to_string(),
            });
        }
    };

    let tag = body["tag_name"].as_str().unwrap_or("");
    let latest = tag.trim_start_matches('v');
    let release_url = body["html_url"]
        .as_str()
        .unwrap_or(&format!("https://github.com/{repo_path}/releases/latest"))
        .to_string();

    let has_update = version_gt(latest, current);
    if has_update {
        tracing::info!(current, latest, "发现新版本");
    } else {
        tracing::debug!(current, latest, "已是最新版本");
    }

    serde_json::json!({
        "has_update": has_update,
        "current_version": current,
        "latest_version": latest,
        "release_url": release_url,
    })
}

/// 语义化版本比较：a > b 则返回 true。
///
/// 优先用 `semver` 库严格比较（支持 pre-release / build metadata），
/// 解析失败时 fallback 到简单数字比较（兼容非标准版本号如 `0.9`）。
fn version_gt(a: &str, b: &str) -> bool {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(va), Ok(vb)) => va > vb,
        _ => version_gt_fallback(a, b),
    }
}

/// Fallback：非标准版本号的简单数字比较，取前三段，缺失按 0 算。
fn version_gt_fallback(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> (u64, u64, u64) {
        let parts: Vec<u64> = s.split('.').filter_map(|p| p.parse().ok()).collect();
        (
            parts.first().copied().unwrap_or(0),
            parts.get(1).copied().unwrap_or(0),
            parts.get(2).copied().unwrap_or(0),
        )
    };
    parse(a) > parse(b)
}

/// 设置页-存储：清空历史记录。
#[tauri::command]
pub async fn clear_history(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::history::clear(&pool).await;
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

/// 调整 voice-overlay 窗口高度（G2 语音 mini overlay 自动撑高）。
///
/// 前端在文本更新后调用，传入期望的逻辑高度。宽度固定 300。
/// 若窗口底部超出显示器工作区，自动上移使其完整可见。
#[tauri::command]
pub async fn resize_voice_overlay(app: tauri::AppHandle, height: f64) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("voice-overlay") {
        let size = tauri::LogicalSize::new(260.0, height);
        win.set_size(size).map_err(|e| e.to_string())?;
        crate::infra::platform::window::clamp_to_work_area(&win);
    }
    Ok(())
}

// ── 配置相关命令 ────────────────────────────────────────────────────────────────

/// 获取完整配置。
#[tauri::command]
pub async fn get_config(app: tauri::AppHandle) -> crate::app::config::AppConfig {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::app::config::get_config(&pool).await
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

/// 泛型配置写入（0.8.6 P1-C 前端泛型化）。
///
/// 前端统一调用 `invoke('set_config', { key, value })`，后端按 key 路由到
/// 对应分片持久化 + 副作用（SearchService 热更新 / 平台 API / emit 事件）。
///
/// # 支持的 key
///
/// **AppConfig 分片**：`language` / `log_level` / `auto_start` / `hotkey` /
/// `tap_threshold` / `grace_period` / `general_config` / `autosuggest` /
/// `chord_toggles` / `clipboard_enabled` / `disabled_builtin_actions` /
/// `disabled_context_bindings` / `disabled_chord_actions` / `window_opacity`
///
/// **引擎配置**：`file_search` / `start_menu_config` / `calc_config` / `global_proxy` / `interpreter_paths`
///
/// **插件配置**：`plugin_config`
///
/// **Context 配置**：`context_config`
///
/// **截图配置**（0.11.10-b）：`screenshot_config` —— ScreenshotConfig 分片（prewarm_ocr 等）
#[tauri::command]
pub async fn set_config(
    app: tauri::AppHandle,
    key: String,
    value: serde_json::Value,
) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;

    match key.as_str() {
        // ── 单值字段（直接解析） ──────────────────────────────────────────
        "language" => {
            let language: String = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_language(&pool, language.clone()).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_language(language.clone());
            }
            // 托盘菜单是 Rust 侧静态构建的（不走前端 i18n），切语言后需主动重建。
            // on_menu_event 挂在 TrayIcon 上，set_menu 不影响 id 路由。
            crate::app::tray::rebuild_menu(&app, &language);
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(%language, "语言已更新");
        }
        "log_level" => {
            let level: String = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_log_level(&pool, level.clone()).await?;
            crate::infra::utils::logging::update_level(&level);
            tracing::info!(%level, "日志级别已切换");
        }
        "auto_start" => {
            let auto_start: bool = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_auto_start(&pool, auto_start).await?;
            use tauri_plugin_autostart::ManagerExt;
            let manager = app.autolaunch();
            if auto_start {
                manager.enable().map_err(|e| e.to_string())?;
            } else {
                manager.disable().map_err(|e| e.to_string())?;
            }
            tracing::info!(auto_start, "开机自启配置已更新");
        }
        "tap_threshold" => {
            let threshold: u64 = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_tap_threshold(&pool, threshold).await?;
            crate::infra::platform::hotkey::update_tap_threshold(threshold);
            tracing::debug!(threshold, "tap 阈值已更新");
        }
        "grace_period" => {
            let period: u64 = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_grace_period(&pool, period).await?;
            crate::infra::platform::window::update_grace_period(period);
            tracing::debug!(period, "grace period 已更新");
        }
        "clipboard_enabled" => {
            let enabled: bool = serde_json::from_value(value).map_err(|e| e.to_string())?;
            // 只读写 clipboard:config 分片，不走门面 get_config（避免 7 片全读全写）。
            // 0.9 删掉 AppConfig.clipboard 字段后此处不受影响。
            let mut clip_cfg = crate::app::config::ConfigStore::get::<
                crate::infra::data::clipboard::ClipboardConfig,
            >(&pool)
            .await;
            clip_cfg.enabled = enabled;
            crate::app::config::ConfigStore::set(&pool, &clip_cfg).await?;
            crate::infra::platform::clipboard::set_active(enabled);
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(enabled, "剪贴板监听开关已更新");
        }

        // ── 结构体字段（按 key 对应 serde 解析） ──────────────────────────
        "hotkey" => {
            let hotkey: crate::app::config::HotkeyConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_hotkey(&pool, hotkey.clone()).await?;
            crate::infra::platform::hotkey::update_config(hotkey.clone());
            tracing::info!(display = %hotkey.display, "全局热键已更新");
        }
        "general_config" => {
            let general: crate::app::config::GeneralConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            let max_results = general.max_results;
            crate::app::config::update_general_config(&pool, &general).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_max_results(max_results as usize);
            }
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(
                theme = %general.theme,
                search_history_enabled = general.search_history_enabled,
                search_history_days = general.search_history_days,
                max_results,
                page_size = general.page_size,
                "通用配置已更新"
            );
        }
        "autosuggest" => {
            let v: crate::app::config::AutosuggestUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_autosuggest_config(
                &pool,
                v.enabled,
                v.min_score,
                v.tab_key.clone(),
            )
            .await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_autosuggest_config(v.enabled, v.min_score);
            }
            tracing::info!(v.enabled, v.min_score, tab_key = %v.tab_key, "Autosuggest 配置已更新");
        }
        "chord_toggles" => {
            let v: crate::app::config::ChordTogglesUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_chord_toggles(&pool, v.chord_enabled, v.chord_hint_visible)
                .await?;
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(v.chord_enabled, v.chord_hint_visible, "Chord 开关已更新");
        }
        "chord_bindings" => {
            // 0.10.7：chord 键位绑定（设置页改键用）
            let bindings: crate::domain::chord::ChordBindings =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_chord_bindings(&pool, bindings.clone()).await?;
            let _ = app.emit("blink://config-changed", ());
            tracing::info!("Chord 键位绑定已更新");
        }
        "clipboard_config" => {
            // 0.10.7：剪贴板历史详细配置（retention_days / max_items / blacklist_keywords）
            let cfg: crate::infra::data::clipboard::ClipboardConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::ConfigStore::set(&pool, &cfg).await?;
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(
                enabled = cfg.enabled,
                max_items = cfg.max_items,
                retention_days = cfg.retention_days,
                "剪贴板配置已更新"
            );
        }

        // ── Disable 列表 ──────────────────────────────────────────────────
        "disabled_builtin_actions" => {
            let disabled: Vec<String> = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_disabled_builtin_actions(&pool, disabled.clone()).await?;
            let search_service =
                app.state::<std::sync::Arc<crate::domain::search::SearchService>>();
            search_service.update_disabled_builtin_actions(disabled.clone());
            tracing::info!(count = disabled.len(), ?disabled, "内置动作禁用列表已更新");
        }
        "disabled_context_bindings" => {
            let disabled: Vec<String> = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_disabled_context_bindings(&pool, disabled.clone()).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_disabled_context_bindings(disabled.clone());
            }
            tracing::info!(
                count = disabled.len(),
                ?disabled,
                "Context binding 禁用列表已更新"
            );
        }
        "disabled_chord_actions" => {
            let disabled: Vec<String> = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_disabled_chord_actions(&pool, disabled.clone()).await?;
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(
                count = disabled.len(),
                ?disabled,
                "Chord 动作禁用列表已更新"
            );
        }
        "window_opacity" => {
            let opacity: f64 = serde_json::from_value(value).map_err(|e| e.to_string())?;
            let opacity = opacity.clamp(0.2, 1.0); // 最低 20% 防止完全不可见
            let mut config = crate::app::config::get_config(&pool).await;
            config.window_opacity = opacity;
            crate::app::config::save_config(&pool, &config).await?;
            // 前端通过 blink://config-changed 事件自行读取并设置 CSS 变量
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(opacity, "主窗口透明度已更新");
        }

        // ── 引擎配置 ──────────────────────────────────────────────────────
        "file_search" => {
            let fs: crate::app::config::FileSearchConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_file_search(&pool, fs.clone()).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_engine_config(
                    "file",
                    crate::domain::search::EngineConfigUpdate::File(fs.clone()),
                )
                .await;
            }
            tracing::info!(enabled = fs.enabled, data_source = %fs.data_source, "文件搜索配置已更新");
        }
        "start_menu_config" => {
            let sm: crate::app::config::StartMenuConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_start_menu_config(&pool, &sm).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_engine_config(
                    "start_menu",
                    crate::domain::search::EngineConfigUpdate::StartMenu(sm.clone()),
                )
                .await;
            }
            tracing::info!(
                enabled = sm.enabled,
                scan_depth = sm.scan_depth,
                "应用搜索配置已更新"
            );
        }
        "calc_config" => {
            let cc: crate::app::config::CalcConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_calc_config(&pool, &cc).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_engine_config(
                    "calc",
                    crate::domain::search::EngineConfigUpdate::Calc(cc.clone()),
                )
                .await;
            }
            tracing::info!(enabled = cc.enabled, "计算器配置已更新");
        }
        "global_proxy" => {
            let v: crate::app::config::GlobalProxyUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            let config = serde_json::json!({ "http": v.http, "https": v.https });
            crate::app::config::set_engine_config(&pool, "_global_proxy", &config).await?;
            let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
            let has_http = !v.http.is_empty();
            let has_https = !v.https.is_empty();
            let proxy = if !has_http && !has_https {
                None
            } else {
                Some((v.http, v.https))
            };
            engine.update_global_proxy(proxy).await;
            tracing::info!(has_http, has_https, "全局代理配置已更新");
        }

        // ── 解释器路径配置 ────────────────────────────────────────────────
        "interpreter_paths" => {
            let json_str = serde_json::to_string(&value).map_err(|e| e.to_string())?;
            crate::infra::data::history::set_config(&pool, "interpreter_paths", &json_str)
                .await
                .map_err(|e| e.to_string())?;
            tracing::info!("解释器路径配置已更新");
        }

        // ── 插件配置 ──────────────────────────────────────────────────────
        "plugin_config" => {
            let v: crate::app::config::PluginConfigUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
            let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();
            let mut config = engine.get_config(&v.plugin_id).unwrap_or_default();
            config.enabled = v.enabled;
            config.settings = v.settings;
            let result = engine
                .update_config(&v.plugin_id, config, Some(&router))
                .await;
            match &result {
                Ok(_) => {
                    tracing::info!(plugin_id = %v.plugin_id, enabled = v.enabled, "插件配置已更新")
                }
                Err(err) => {
                    tracing::warn!(plugin_id = %v.plugin_id, error = %err, "插件配置更新失败")
                }
            }
            result?;
        }

        // ── Context 配置 ──────────────────────────────────────────────────
        "context_config" => {
            let ctx: crate::app::config::ContextConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::set_context_config(&pool, &ctx).await?;
            crate::infra::platform::selection::set_active(ctx.selection_enabled);
            crate::infra::platform::selection::set_sensitive_apps(ctx.sensitive_apps.clone());
            if let Some(mem) = app
                .try_state::<std::sync::Arc<std::sync::RwLock<crate::app::config::ContextConfig>>>()
            {
                *mem.write().unwrap() = ctx;
            }
            tracing::debug!("Context 配置已更新");
        }

        // ── AI 配置(0.9.1 Phase 3-6) ──────────────────────────────────────
        //
        // 完整 AIConfig 分片写入(第 7 分片,独立于 AppConfig 门面);写完
        // 通知 registry reload —— 骨架条 #7(切换零重启)在此触发。
        //
        // **注意**:AIConfig 结构里不含密钥,只含 `secret_ref` CM 别名。
        // 密钥独立走 `save_ai_secret / delete_ai_secret` 两个命令,永不进 SQLite。
        "ai_config" => {
            let ai: crate::app::ai_config::AIConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::ConfigStore::set(&pool, &ai).await?;

            // registry 热更新——空档降级 / factory 失败静默跳过 / 复用未变动实例
            if let Some(reg) =
                app.try_state::<std::sync::Arc<crate::domain::ai::AIProviderRegistry>>()
            {
                reg.reload(&ai);
            }
            // 对话 Agent 按需重建；memory 归 ChatService 所有，不随配置失效。
            if let Some(chat) =
                app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
            {
                chat.notify_config_changed();
            }

            // 0.12 §2.7: 同步更新 AIConfig 内存缓存（供 CloudSttEngine 等非 async 上下文读取）
            crate::app::ai_config::update_ai_cache(&ai);

            let _ = app.emit(
                "blink://config-changed",
                serde_json::json!({ "key": "ai_config" }),
            );
            tracing::info!(
                enabled = ai.enabled,
                providers = ai.providers.len(),
                tier_router = ai.tier_router.is_some(),
                tier_light = ai.tier_light.is_some(),
                tier_main = ai.tier_main.is_some(),
                direct_execute_safe_actions = ai.direct_execute_safe_actions,
                slo_hard_timeout_ms = ?ai.slo_hard_timeout_ms,
                "AI 配置已更新"
            );
        }

        // ── Screenshot 配置(0.11.10-b)───────────────────────────────────
        //
        // 截图 overlay 行为分片。目前只承载 prewarm_ocr;写完不需要热更新任何
        // 内存副本,前端每次 overlay 显示时按需读取(读路径:`get_config_section`)。
        "screenshot_config" => {
            let sc: crate::app::config::ScreenshotConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::ConfigStore::set(&pool, &sc).await?;
            tracing::info!(prewarm_ocr = sc.prewarm_ocr, "截图配置已更新");
        }

        _ => {
            return Err(format!("未知的配置 key: {key}"));
        }
    }

    Ok(())
}

// ── AI 密钥专用命令(0.9.1 Phase 6)─────────────────────────────────────────
//
// **为什么与 set_config 分开**:
// - `set_config` 走 `serde_json::Value`,任何字段都可能被 debug 打印/序列化
// - 密钥必须**只在** SecretString 生命周期内存活,IPC 参数拿到明文后立即
//   转 SecretString + 写 CM + 清零
// - 前端 IPC 参数是 `provider_id + secret`,后端**永不**回传/回显密钥
//
// **调用契约**:
// - 保存 Provider 前弹密钥输入框 → invoke("save_ai_secret", {providerId, secret})
// - 删除 Provider 前 invoke("delete_ai_secret", {providerId}) —— 之后再删 ai_config
//   里的 provider entry(否则 secret_ref 悬空)
// - 编辑 Key = 前端强制"清空重填",不允许"只改 base_url 保留旧 Key"(§5.2 铁则)

/// 保存 Provider API Key 到 Windows Credential Manager。
///
/// **参数**:
/// - `provider_id`:`ProviderEntry.id`(UUID,前端生成)
/// - `secret`:明文密钥——只在本 command 函数内活着,写完 CM 立即 SecretString drop
///
/// **失败**:
/// - `InvalidRef`:provider_id 含非法字符
/// - `Platform`:CM 写入失败(极少见,通常 headless session)
#[tauri::command]
pub async fn save_ai_secret(
    provider_id: String,
    secret: String,
    app: tauri::AppHandle,
) -> Result<(), String> {
    // 立即包进 SecretString——之后再也不用明文引用
    let secret_wrapped = crate::infra::platform::secret::SecretString::new(secret);
    crate::infra::platform::secret::save_secret(&provider_id, "key", &secret_wrapped)
        .map_err(|e| e.to_string())?;
    // Bump registry 内的密钥 epoch —— 让下次 reload invalidate 所有旧实例,
    // 保证"改密钥立即生效"(前端保存密钥后会紧接着调 set_config('ai_config') 触发 reload)。
    if let Some(reg) = app.try_state::<std::sync::Arc<crate::domain::ai::AIProviderRegistry>>() {
        reg.bump_secret_epoch();
    }
    // 日志绝不含 secret 内容
    tracing::info!(%provider_id, "AI Provider 密钥已保存到 Credential Manager");
    Ok(())
}

/// 从 Credential Manager 删除 Provider API Key。
///
/// **调用时机**:删除 Provider entry 前先调此,确保 CM 端幂等清理。
/// 未找到别名视为已删,静默返回 Ok(§5.2 铁则 3)。
///
/// **对称性**:与 `save_ai_secret` 一样在成功/幂等分支都 `bump_secret_epoch` ——
/// 让下次 reload 时任何仍引用此 pid 的旧 Arc 因 fingerprint 变化被强制丢弃。
/// 当前 UX 是"删 provider 顺带删密钥",紧随的 `set_config('ai_config')` 已经会
/// reload;这里 bump 一次是**未来 UX 铺路**(若加"清空密钥保留 provider"入口,
/// 光删 CM 不 bump 会让旧 Arc 继续带过期密钥用)。
#[tauri::command]
pub async fn delete_ai_secret(provider_id: String, app: tauri::AppHandle) -> Result<(), String> {
    let bump = || {
        if let Some(reg) = app.try_state::<std::sync::Arc<crate::domain::ai::AIProviderRegistry>>()
        {
            reg.bump_secret_epoch();
        }
    };
    match crate::infra::platform::secret::delete_secret(&provider_id, "key") {
        Ok(()) => {
            bump();
            tracing::info!(%provider_id, "AI Provider 密钥已从 CM 删除");
            Ok(())
        }
        Err(crate::infra::platform::secret::SecretError::NotFound(_)) => {
            // 幂等——CM 里没有此别名,视为已删;也 bump(池里若有引用此 pid 的
            // 旧 Arc,下次 reload 会因 fingerprint 变化重建)。
            bump();
            tracing::debug!(%provider_id, "AI Provider 密钥不在 CM 中,跳过删除");
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// 检查 Provider 是否已配 API Key(不返回明文,只返 true/false)。
///
/// 用于设置页初始化时判断 Provider 卡片显示"已配置"标记。
#[tauri::command]
pub async fn has_ai_secret(provider_id: String) -> Result<bool, String> {
    match crate::infra::platform::secret::load_secret(&provider_id, "key") {
        Ok(_) => Ok(true),
        Err(crate::infra::platform::secret::SecretError::NotFound(_)) => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

/// 获取 Provider 密钥的首尾掩码(如 `"sk-a••••cdef"`),供编辑 modal 占位展示。
///
/// 不返回明文——仅返回 `format_hint` 结果。密钥不存在返回 `None`。
#[tauri::command]
pub async fn get_ai_secret_hint(provider_id: String) -> Result<Option<String>, String> {
    match crate::infra::platform::secret::load_secret(&provider_id, "key") {
        Ok(secret) => Ok(Some(crate::infra::platform::secret::format_hint(
            secret.expose(),
        ))),
        Err(crate::infra::platform::secret::SecretError::NotFound(_)) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// 用 CM 里已存的密钥拉取可用模型列表(0.9.4)。
///
/// 获取供应商可用模型列表。
///
/// **密钥优先级**:api_key 非空 → 用 api_key;否则 provider_id 非空 → 从 CM 读。
///
/// **参数**:
/// - `kind`: 协议类型
/// - `base_url`: 供应商 base URL
/// - `api_key`: 明文密钥(新增供应商时前端传入);可空
/// - `provider_id`: 已保存供应商的 UUID,用于从 CM 读密钥;可空
///
/// **返回**:模型 id 列表;拉取失败返回错误。
#[tauri::command]
pub async fn fetch_ai_models(
    kind: String,
    base_url: Option<String>,
    api_key: Option<String>,
    provider_id: Option<String>,
) -> Result<Vec<String>, String> {
    use crate::app::ai_config::ProviderKind;

    // 密钥优先级:输入框 → CM
    let _cm_secret;
    let effective_key = if let Some(ref key) = api_key {
        if !key.trim().is_empty() {
            key.trim().to_string()
        } else if let Some(pid) = provider_id.as_deref() {
            match crate::infra::platform::secret::load_secret(pid, "key") {
                Ok(s) => {
                    _cm_secret = Some(s);
                    _cm_secret.as_ref().unwrap().expose().to_string()
                }
                Err(crate::infra::platform::secret::SecretError::NotFound(_)) => {
                    return Ok(Vec::new());
                }
                Err(e) => return Err(e.to_string()),
            }
        } else {
            return Ok(Vec::new());
        }
    } else if let Some(pid) = provider_id.as_deref() {
        match crate::infra::platform::secret::load_secret(pid, "key") {
            Ok(s) => {
                _cm_secret = Some(s);
                _cm_secret.as_ref().unwrap().expose().to_string()
            }
            Err(crate::infra::platform::secret::SecretError::NotFound(_)) => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.to_string()),
        }
    } else {
        return Ok(Vec::new());
    };

    let kind: ProviderKind =
        serde_json::from_str(&format!("\"{}\"", kind)).map_err(|_| format!("未知协议: {kind}"))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败: {e}"))?;

    let result = match kind {
        ProviderKind::OpenAICompatible => {
            let base = base_url.as_deref().unwrap_or("https://api.openai.com/v1");
            let base = base.trim_end_matches('/');
            let urls = if base.ends_with("/v1") {
                vec![
                    format!("{}/models", base),
                    format!("{}/models", base.trim_end_matches("/v1")),
                ]
            } else {
                vec![format!("{}/models", base), format!("{}/v1/models", base)]
            };
            fetch_openai_models(&client, &urls, &effective_key).await
        }
        ProviderKind::AnthropicMessages => {
            let base = base_url.as_deref().unwrap_or("https://api.anthropic.com");
            let url = format!("{}/v1/models?limit=100", base.trim_end_matches('/'));
            fetch_anthropic_models(&client, &url, &effective_key).await
        }
        ProviderKind::GeminiGenerateContent => {
            let base = base_url
                .as_deref()
                .unwrap_or("https://generativelanguage.googleapis.com");
            let url = format!(
                "{}/v1beta/models?key={}",
                base.trim_end_matches('/'),
                &effective_key
            );
            fetch_gemini_models(&client, &url).await
        }
        ProviderKind::OllamaHttp => {
            // 0.12 §2.3: ollama 模型列表走 /api/tags 端点（无需认证）
            let base = base_url.as_deref().unwrap_or("http://localhost:11434");
            let url = format!("{}/api/tags", base.trim_end_matches('/'));
            fetch_ollama_models(&client, &url).await
        }
    };
    // effective_key(String) 在这里出作用域
    result
}

async fn fetch_openai_models(
    client: &reqwest::Client,
    urls: &[String],
    api_key: &str,
) -> Result<Vec<String>, String> {
    let mut last_err = String::new();
    for url in urls {
        match client
            .get(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let models = resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| {
                        let arr = v
                            .get("data")
                            .and_then(|d| d.as_array())
                            .or_else(|| v.get("models").and_then(|m| m.as_array()))
                            .or_else(|| v.as_array());
                        arr.map(|a| {
                            a.iter()
                                .filter_map(|m| {
                                    m.get("id")
                                        .and_then(|id| id.as_str())
                                        .map(String::from)
                                        .or_else(|| {
                                            m.get("name").and_then(|n| n.as_str()).map(String::from)
                                        })
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .unwrap_or_default();
                if !models.is_empty() {
                    let mut sorted = models;
                    sorted.sort();
                    sorted.dedup();
                    return Ok(sorted);
                }
                last_err = "返回空列表".into();
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                last_err = format!("HTTP {status}");
                if status == 401 || status == 403 {
                    return Err(format!("认证失败(HTTP {status})"));
                }
            }
            Err(e) => {
                last_err = format!("{e}");
            }
        }
    }
    Err(format!("获取模型失败: {last_err}"))
}

async fn fetch_gemini_models(client: &reqwest::Client, url: &str) -> Result<Vec<String>, String> {
    match client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let models = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| {
                    v.get("models").and_then(|m| m.as_array()).map(|arr| {
                        arr.iter()
                            .filter_map(|m| {
                                m.get("name")
                                    .and_then(|n| n.as_str())
                                    .map(|s| s.replace("models/", ""))
                                    .filter(|n| n.to_lowercase().contains("gemini"))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default();
            if models.is_empty() {
                Err("返回空列表".to_string())
            } else {
                let mut sorted = models;
                sorted.sort();
                Ok(sorted)
            }
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                Err(format!("认证失败(HTTP {status})"))
            } else {
                Err(format!("获取模型失败(HTTP {status})"))
            }
        }
        Err(e) => Err(format!("获取模型失败: {e}")),
    }
}

/// 获取 ollama 本地模型列表（0.12 §2.3）。
///
/// ollama API: `GET /api/tags`（无需认证）
/// - Response: `{ "models": [{ "name": "llama3:latest", ... }] }`
async fn fetch_ollama_models(client: &reqwest::Client, url: &str) -> Result<Vec<String>, String> {
    match client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let models = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| {
                    v.get("models").and_then(|m| m.as_array()).map(|arr| {
                        arr.iter()
                            .filter_map(|m| {
                                m.get("name")
                                    .and_then(|n| n.as_str())
                                    .map(|s| s.to_string())
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default();
            if models.is_empty() {
                Err("ollama 返回空列表(可能未拉取模型,尝试 ollama pull llama3)".to_string())
            } else {
                let mut sorted = models;
                sorted.sort();
                Ok(sorted)
            }
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            Err(format!(
                "ollama 连接失败(HTTP {status}),确认 ollama serve 已启动"
            ))
        }
        Err(e) => Err(format!(
            "ollama 连接失败: {e},确认 ollama serve 已启动且地址正确"
        )),
    }
}

/// 获取 Anthropic 模型列表。
///
/// Anthropic API: `GET /v1/models`
/// - Header: `x-api-key: {key}`, `anthropic-version: 2023-06-01`
/// - Response: `{ "data": [{ "id": "model-id", ... }], "has_more": false }`
async fn fetch_anthropic_models(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    match client
        .get(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let models = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| {
                    v.get("data").and_then(|d| d.as_array()).map(|arr| {
                        arr.iter()
                            .filter_map(|m| {
                                m.get("id").and_then(|id| id.as_str()).map(String::from)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default();
            if models.is_empty() {
                Err("返回空列表".to_string())
            } else {
                let mut sorted = models;
                sorted.sort();
                sorted.dedup();
                Ok(sorted)
            }
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                Err(format!("认证失败(HTTP {status}),请检查 API Key"))
            } else {
                Err(format!("获取模型失败(HTTP {status})"))
            }
        }
        Err(e) => Err(format!("获取模型失败: {e}")),
    }
}

/// 获取当前 AI system prompt 信息（0.11.3 §3.8 token 监控）。
///
/// 构建与 AI lane 相同的 tools 列表 + system prompt，返回 token 数 / 工具数 / 预览。
/// 设置页 AI tab（高级）展示此信息，让用户感知 prompt 体积。
#[tauri::command]
pub async fn get_system_prompt_info(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    use crate::domain::ai::prompt::{build_prompt_infos, estimate_tokens, routing_system_prompt};
    use crate::domain::capability::CapabilityRegistry;
    use crate::domain::execution::group::{build_aggregated_tools, inject_plugin_settings};
    use crate::domain::plugin::PluginEngine;
    use std::sync::Arc;
    use tauri::Manager;

    let action_reg = app.state::<Arc<crate::domain::execution::ActionRegistry>>();
    let cap_reg = app.state::<Arc<CapabilityRegistry>>();
    let plugin_engine = app.state::<Arc<PluginEngine>>();

    // 构建 tools 列表（与 service.rs AI lane 同逻辑）
    let mut tools = build_aggregated_tools(&action_reg, &cap_reg);

    // 参数动态注入 + hints 收集
    let mut plugin_hints: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for ph in plugin_engine.all_plugins() {
        let manifest = ph.manifest();
        for td in &manifest.tools {
            let id = format!("{}:{}", manifest.id, td.name);
            if let Some(bindings) = &td.setting_bindings {
                if let Some(pos) = tools.iter().position(|s| s.name == id) {
                    let settings = plugin_engine.get_settings(&manifest.id);
                    tools[pos] =
                        inject_plugin_settings(tools[pos].clone(), settings.as_ref(), bindings);
                }
            }
            if let Some(hint) = &td.hint {
                plugin_hints.insert(id, hint.clone());
            }
        }
    }

    let tools_count = tools.len();
    let prompt_infos = build_prompt_infos(tools, &plugin_hints);
    // lang 不影响 prompt 内容（0.x 用中文），传 "zh"
    let prompt = routing_system_prompt(&prompt_infos, "zh");
    let tokens = estimate_tokens(&prompt);

    // 预览：前 200 字符（设置页展示用）
    let preview: String = prompt.chars().take(200).collect();

    Ok(serde_json::json!({
        "tokens": tokens,
        "tools_count": tools_count,
        "preview": preview,
        "threshold": 1500,
    }))
}

/// 测试 AI 供应商连通性(0.9.4)。
///
/// 用 `reqwest` 直接发一个最小请求验证 Key + URL 是否可用。
///
/// **参数**:
/// - `kind`: 协议类型
/// - `base_url`: 供应商 base URL
/// - `api_key`: 明文密钥(新增模式);编辑模式下可空
/// - `provider_id`: 可选;编辑模式下传入,从 CM 读已有密钥(api_key 为空时生效)
///
/// **密钥优先级**:api_key 非空 → 用 api_key;否则 provider_id 非空 → 从 CM 读
#[tauri::command]
pub async fn test_ai_provider(
    kind: String,
    base_url: Option<String>,
    api_key: String,
    provider_id: Option<String>,
) -> Result<String, String> {
    use crate::app::ai_config::ProviderKind;

    // 确定密钥来源:输入框优先,其次 CM
    let _cm_secret; // 持有 SecretString 生命周期
    let effective_key = if !api_key.trim().is_empty() {
        api_key.trim().to_string()
    } else if let Some(pid) = provider_id.as_deref() {
        match crate::infra::platform::secret::load_secret(pid, "key") {
            Ok(s) => {
                _cm_secret = Some(s);
                _cm_secret.as_ref().unwrap().expose().to_string()
            }
            Err(crate::infra::platform::secret::SecretError::NotFound(_)) => {
                return Err("未找到已保存的密钥，请填写 API Key".to_string());
            }
            Err(e) => return Err(e.to_string()),
        }
    } else {
        return Err("请填写 API Key".to_string());
    };

    let kind: ProviderKind =
        serde_json::from_str(&format!("\"{}\"", kind)).map_err(|_| format!("未知协议: {kind}"))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败: {e}"))?;

    let result = match kind {
        ProviderKind::OpenAICompatible => {
            let base = base_url.as_deref().unwrap_or("https://api.openai.com/v1");
            let base = base.trim_end_matches('/');
            // 尝试 /models 和 /v1/models 两个路径
            let urls = if base.ends_with("/v1") {
                vec![
                    format!("{}/models", base),
                    format!("{}/models", base.trim_end_matches("/v1")),
                ]
            } else {
                vec![format!("{}/models", base), format!("{}/v1/models", base)]
            };
            test_openai_models_endpoint(&client, &urls, &effective_key).await
        }
        ProviderKind::AnthropicMessages => {
            let base = base_url.as_deref().unwrap_or("https://api.anthropic.com");
            let base = base.trim_end_matches('/');
            // 优先用 models 端点(更轻量),失败再 fallback 到 messages 端点
            let models_url = format!("{}/v1/models?limit=1", base);
            match test_anthropic_models_endpoint(&client, &models_url, &effective_key).await {
                ok @ Ok(_) => ok,
                Err(_) => {
                    let messages_url = format!("{}/v1/messages", base);
                    test_anthropic_endpoint(&client, &messages_url, &effective_key).await
                }
            }
        }
        ProviderKind::GeminiGenerateContent => {
            let base = base_url
                .as_deref()
                .unwrap_or("https://generativelanguage.googleapis.com");
            let url = format!(
                "{}/v1beta/models?key={}",
                base.trim_end_matches('/'),
                &effective_key
            );
            test_gemini_endpoint(&client, &url).await
        }
        ProviderKind::OllamaHttp => {
            // 0.12 §2.3: ollama 连接测试走 /api/tags 端点（无需认证）
            let base = base_url.as_deref().unwrap_or("http://localhost:11434");
            let url = format!("{}/api/tags", base.trim_end_matches('/'));
            test_ollama_endpoint(&client, &url).await
        }
    };

    result
}

async fn test_openai_models_endpoint(
    client: &reqwest::Client,
    urls: &[String],
    api_key: &str,
) -> Result<String, String> {
    let mut last_err = String::new();
    for url in urls {
        match client
            .get(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let count = resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("data").and_then(|d| d.as_array().map(|a| a.len())))
                    .unwrap_or(0);
                return Ok(format!("连接成功,发现 {count} 个模型"));
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                last_err = format!("HTTP {status}");
                if status == 401 || status == 403 {
                    return Err(format!("认证失败(HTTP {status}),请检查 API Key"));
                }
            }
            Err(e) => {
                last_err = format!("{e}");
            }
        }
    }
    Err(format!("连接失败: {last_err}"))
}

/// 测试 Anthropic 模型列表端点连通性(更轻量,不消耗 token)。
async fn test_anthropic_models_endpoint(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<String, String> {
    match client
        .get(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let count = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v.get("data").and_then(|d| d.as_array().map(|a| a.len())))
                .unwrap_or(0);
            Ok(format!("连接成功,发现 {count} 个模型"))
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                Err(format!("认证失败(HTTP {status}),请检查 API Key"))
            } else {
                Err(format!("获取模型失败(HTTP {status})"))
            }
        }
        Err(e) => Err(format!("连接失败: {e}")),
    }
}

async fn test_anthropic_endpoint(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<String, String> {
    match client
        .post(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            Ok("连接成功,Anthropic API 可用".to_string())
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                Err(format!("认证失败(HTTP {status}),请检查 API Key"))
            } else {
                // 400 等其他状态码说明 Key 通了但请求格式有问题——也算连通
                Ok(format!("连接成功(HTTP {status}),Anthropic API 可达"))
            }
        }
        Err(e) => Err(format!("连接失败: {e}")),
    }
}

async fn test_gemini_endpoint(client: &reqwest::Client, url: &str) -> Result<String, String> {
    match client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let count = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v.get("models").and_then(|m| m.as_array().map(|a| a.len())))
                .unwrap_or(0);
            Ok(format!("连接成功,发现 {count} 个模型"))
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                Err(format!("认证失败(HTTP {status}),请检查 API Key"))
            } else {
                Err(format!("连接失败(HTTP {status})"))
            }
        }
        Err(e) => Err(format!("连接失败: {e}")),
    }
}

/// 测试 ollama 连接（0.12 §2.3）。
///
/// ollama 无需认证,只需检查 /api/tags 是否可达。
async fn test_ollama_endpoint(client: &reqwest::Client, url: &str) -> Result<String, String> {
    match client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let count = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v.get("models").and_then(|m| m.as_array().map(|a| a.len())))
                .unwrap_or(0);
            Ok(format!("连接成功,发现 {count} 个本地模型"))
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            Err(format!(
                "ollama 连接失败(HTTP {status}),确认 ollama serve 已启动"
            ))
        }
        Err(e) => Err(format!(
            "ollama 连接失败: {e},确认 ollama serve 已启动且地址正确"
        )),
    }
}

/// 打开当天日志文件（资源管理器中定位；文件不存在则打开文件夹）。
#[tauri::command]
pub fn open_log_file() -> Result<(), String> {
    let path = crate::infra::utils::logging::current_log_file();
    let arg = if path.exists() {
        format!("/select,{}", path.display())
    } else {
        // 当天尚无日志（如 error 级未产生），直接打开文件夹
        crate::infra::utils::logging::log_dir()
            .display()
            .to_string()
    };
    std::process::Command::new("explorer.exe")
        .arg(arg)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 打开日志文件夹。
#[tauri::command]
pub fn open_log_dir() -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .arg(crate::infra::utils::logging::log_dir())
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 获取日志路径信息（供设置页显示）。
#[tauri::command]
pub fn get_log_info() -> serde_json::Value {
    serde_json::json!({
        "dir": crate::infra::utils::logging::log_dir().to_string_lossy(),
        "current_file": crate::infra::utils::logging::current_log_file().to_string_lossy(),
    })
}

/// 恢复默认配置。
#[tauri::command]
pub async fn reset_config(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let config = crate::app::config::AppConfig::default();
    crate::app::config::save_config(&pool, &config).await
}

/// 获取应用搜索配置。
#[tauri::command]
pub async fn get_start_menu_config(app: tauri::AppHandle) -> crate::app::config::StartMenuConfig {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::app::config::get_start_menu_config(&pool).await
}

/// 获取计算器配置。
#[tauri::command]
pub async fn get_calc_config(app: tauri::AppHandle) -> crate::app::config::CalcConfig {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::app::config::get_calc_config(&pool).await
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

/// 获取所有已加载插件的信息（设置页用）。已含 enabled + settings（0.5.1）。
#[tauri::command]
pub async fn get_plugins(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    // 读当前语言,供 manifest 配置文案按 locale 取值(设置页中英双语)
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let lang = crate::app::config::get_config(&pool).await.language;
    engine.list_plugins(&lang)
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
    Ok(crate::app::config::get_engine_config(&pool, &engine_id)
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
    Ok(crate::app::config::get_context_config(&pool).await)
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

/// 将文本写入系统剪贴板（Windows API）。
/// 右键菜单独立 Popup 窗口中 navigator.clipboard 不可靠，改走后端。
#[tauri::command]
pub async fn copy_to_clipboard(text: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
        };

        // RAII guard: 确保 CloseClipboard 在所有路径上被调用
        struct ClipboardGuard;
        impl Drop for ClipboardGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseClipboard();
                }
            }
        }

        unsafe {
            if OpenClipboard(Some(HWND(std::ptr::null_mut()))).is_err() {
                return Err("打开剪贴板失败".into());
            }
            let _guard = ClipboardGuard;

            let _ = EmptyClipboard();

            // 分配全局内存（+1 for null terminator）
            let wchars: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let byte_size = wchars.len() * 2;
            let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_size)
                .map_err(|e| format!("GlobalAlloc 失败: {e}"))?;
            let ptr = GlobalLock(hmem) as *mut u16;
            if ptr.is_null() {
                return Err("GlobalLock 失败".into());
            }
            std::ptr::copy_nonoverlapping(wchars.as_ptr(), ptr, wchars.len());
            let _ = GlobalUnlock(hmem);

            // CF_UNICODETEXT = 13; SetClipboardData 要求 HANDLE 而非 HGLOBAL
            if SetClipboardData(13, Some(std::mem::transmute(hmem))).is_err() {
                return Err("SetClipboardData 失败".into());
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
    crate::infra::data::history::reset_weight(&pool, &lnk_path).await;
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
        tokio::task::spawn_blocking(|| crate::infra::platform::hotkey::record_hotkey_blocking())
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

// ── 右键菜单独立窗口（0.5.3+） ───────────────────────────────────────────────

/// 显示右键菜单独立窗口（突破主窗口边界裁剪）。
/// 复用已有窗口：首次创建，后续 hide → 更新数据 → show，避免重复创建 WebView2 的开销。
///
/// `width/height` 是菜单的 **CSS 像素**尺寸；光标物理坐标由后端 `GetCursorPos` 直接读取，
/// 不接受前端传入的 `screenX/Y`（WebView2 里那是 CSS 像素，高 DPI 屏会偏 1/3+）。
/// 定位/缩放/边界翻转全部走 `clamp_context_menu`。
#[tauri::command]
pub async fn show_context_menu(
    app: tauri::AppHandle,
    width: f64,
    height: f64,
    items: String,
) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    // 主题 resolve（auto → dark/light）
    let theme = {
        let pool = &app.state::<crate::infra::data::DbPools>().config;
        let raw = crate::app::config::get_config(&pool).await.theme;
        if raw == "auto" {
            let is_light = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
                .open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
                .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
                .map(|v| v == 1)
                .unwrap_or(false);
            if is_light {
                "light".to_string()
            } else {
                "dark".to_string()
            }
        } else {
            raw
        }
    };

    // 多屏感知定位：Win32 直接拿光标物理坐标 + 目标屏 DPI 缩放 + 智能翻转
    let (fx, fy, fw, fh) = crate::infra::platform::window::clamp_context_menu(width, height);

    // 复用已有窗口：resize → reposition → show → eval 渲染新数据 → force_topmost
    // ⚠️ 不能在隐藏态用 emit 传数据：WebView2 在 IsVisible=false 时会丢弃事件
    // （曾导致「窗口尺寸已撑开、内容却没更新」）。改用 eval（走 ExecuteScript 注入
    // 脚本到 webview 队列，show 之后必执行），比事件系统更可靠地更新菜单内容。
    if let Some(win) = app.get_webview_window("context-menu") {
        // ⚠️ 不能用 set_size + set_position：窗口在主屏预热、跨到 DPI 不同的屏时，
        // set_position 会触发 WM_DPICHANGED，tao 据此重设尺寸（不动位置），
        // 与刚排队的 set_size 竞态，导致多屏不同 DPI 下菜单尺寸/位置偏（Tauri #3610）。
        // 改用 SetWindowPos 一次原子设定位置+尺寸，绕开 tao 的 DPI 重设逻辑。
        //
        // 但即使走 SetWindowPos，跨 DPI 屏时 Windows 仍会给 hwnd 发 WM_DPICHANGED，
        // tao 的 wndproc 收到后会按建议 rect 再改一次尺寸——把我们刚设的物理尺寸推翻，
        // 症状是「切屏首次右键宽高错，第二次才对」。破法：show 之后再补一次
        // place_at_physical，让 WM_DPICHANGED 的抢跑跑完后再纠正一次。
        let hwnd_opt = win
            .hwnd()
            .ok()
            .map(|h| windows::Win32::Foundation::HWND(h.0 as _));
        if let Some(hwnd) = hwnd_opt {
            crate::infra::platform::window::place_at_physical(hwnd, fx, fy, fw, fh);
            // 撤销上次 hide_context_menu 设的 DWM Cloak，否则 show 后窗口仍不可见
            crate::infra::platform::window::apply_cloak(hwnd, false);
        } else {
            // hwnd 拿不到时的兜底（理论上不会到这）
            let _ = win.set_size(tauri::PhysicalSize::new(fw, fh));
            let _ = win.set_position(tauri::PhysicalPosition::new(fx, fy));
        }
        let _ = win.show();
        // 补一次：show 触发的 WM_DPICHANGED 若把尺寸改回去了，这里覆盖回来
        if let Some(hwnd) = hwnd_opt {
            crate::infra::platform::window::place_at_physical(hwnd, fx, fy, fw, fh);
        }
        let theme_js = serde_json::to_string(&theme).unwrap_or_else(|_| "\"dark\"".to_string());
        let js = format!(
            "window.__renderContextMenu && window.__renderContextMenu({items}, {theme})",
            items = items,
            theme = theme_js,
        );
        let _ = win.eval(&js);
        // Win32 直接设 TOPMOST，比 Tauri 的 set_always_on_top 更可靠
        if let Some(hwnd) = hwnd_opt {
            crate::infra::platform::window::force_topmost(hwnd);
        }
        tracing::trace!(fx, fy, fw, fh, items_len = items.len(), "右键菜单窗口复用");
        return Ok(());
    }

    // 首次创建：通过 URL 参数传递初始数据
    // ⚠️ builder 的 inner_size / position 是**逻辑像素**（tao 内部按 LogicalSize 处理），
    // 但 fw/fh/fx/fy 是物理像素——直接塞给 builder 会被 Tauri 按主屏 DPI 再放大一遍。
    // 这里传 CSS 尺寸（逻辑像素）让 builder 别炸，位置随便给个占位；build 完立刻
    // place_at_physical 强制矫正到目标物理坐标 + 尺寸，跟截图 overlay 同套路。
    let encoded_items = urlencoding::encode(&items).to_string();
    let url = format!("contextmenu-popup.html?items={encoded_items}&theme={theme}");
    tracing::debug!(fx, fy, fw, fh, "创建右键菜单窗口");
    let _win = WebviewWindowBuilder::new(&app, "context-menu", WebviewUrl::App(url.into()))
        .title("")
        .inner_size(width, height) // 逻辑像素占位，稍后 place_at_physical 覆盖
        .position(0.0, 0.0)
        .decorations(false)
        .transparent(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false) // 先隐藏建，place 后再 show，避免闪一下错位窗口
        .focused(false)
        .resizable(false)
        .build()
        .map_err(|e| format!("创建右键菜单窗口失败: {e}"))?;

    if let Ok(hwnd) = _win.hwnd() {
        let hwnd = windows::Win32::Foundation::HWND(hwnd.0 as _);
        crate::infra::platform::window::place_at_physical(hwnd, fx, fy, fw, fh);
        let _ = _win.show();
        // 首次创建同样补一次：show 若触发 WM_DPICHANGED 会撞乱刚设的尺寸
        crate::infra::platform::window::place_at_physical(hwnd, fx, fy, fw, fh);
        crate::infra::platform::window::force_topmost(hwnd);
    } else {
        // hwnd 拿不到的兜底路径
        let _ = _win.set_size(tauri::PhysicalSize::new(fw, fh));
        let _ = _win.set_position(tauri::PhysicalPosition::new(fx, fy));
        let _ = _win.show();
    }

    tracing::trace!(
        fx,
        fy,
        fw,
        fh,
        items_len = items.len(),
        "右键菜单窗口已创建"
    );
    Ok(())
}

/// 隐藏右键菜单窗口（hide 而非 close，保留窗口供下次复用）。
#[tauri::command]
pub async fn hide_context_menu(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("context-menu") {
        // DWM Cloak 先行：瞬间消失无 fade 动画，避免截图时拍到右键窗口残影
        if let Ok(hwnd) = win.hwnd() {
            crate::infra::platform::window::apply_cloak(
                windows::Win32::Foundation::HWND(hwnd.0 as _),
                true,
            );
        }
        let _ = win.hide();
        tracing::trace!("hide_context_menu: 已隐藏右键菜单窗口");
    }
    Ok(())
}

/// Popup 窗口菜单项被点击 → 通知主窗口执行动作。
/// action_id 是菜单项的唯一标识（JSON 数组索引）。
///
/// **顺序很重要**：先隐藏 Popup + 主窗口获焦，再 emit 事件。
/// 否则前端收到事件时 Popup 仍是前台窗口，`document.hasFocus() === false`，
/// `navigator.clipboard.readText()` 会被 Chromium 以「document 未获焦」为由拒绝，
/// `execCommand("paste")` 同样失效——症状就是「点粘贴，输入框仍空」（右键在
/// 主窗口边框时尤其容易复现，此时主窗口本就不是前台）。
#[tauri::command]
pub async fn context_menu_action(app: tauri::AppHandle, action_id: u32) -> Result<(), String> {
    // 1. 先隐藏 Popup 窗口，让主窗口有机会重回前台
    hide_context_menu(app.clone()).await?;
    // 2. 显式把主窗口置为前台并聚焦，保证 clipboard/execCommand 可用
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.show();
        let _ = main.set_focus();
    }
    // 3. 最后再通知前端执行动作
    app.emit("blink://context-menu-action", action_id)
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ─── 脚本解释器配置（Phase 0.6） ───────────────────────────────────────────

/// 探测系统中可用的脚本解释器状态。
///
/// 如果提供了 `python_path` 或 `node_path`，优先验证该路径（用户手动配置），
/// 无效时才回退到 PATH 扫描。
#[tauri::command]
pub async fn probe_interpreters(
    python_path: Option<String>,
    node_path: Option<String>,
) -> crate::domain::plugin::InterpretersStatus {
    tracing::debug!(?python_path, ?node_path, "探测脚本解释器状态");
    crate::domain::plugin::probe_interpreters(python_path.as_deref(), node_path.as_deref())
}

/// 获取已保存的解释器路径配置。
#[tauri::command]
pub async fn get_interpreter_paths(app: tauri::AppHandle) -> serde_json::Value {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::infra::data::history::get_config(&pool, "interpreter_paths")
        .await
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::json!({}))
}

// ─── 剪贴板历史（Phase 0.7.3）──────────────────────────────────────────────────

/// 获取最近的剪贴板历史。
#[tauri::command]
pub async fn get_clipboard_history(
    app: tauri::AppHandle,
    limit: Option<i64>,
) -> Vec<crate::infra::data::clipboard::ClipboardItem> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::query_recent(&pool, limit.unwrap_or(20)).await
}

/// 搜索剪贴板历史。
#[tauri::command]
pub async fn search_clipboard_history(
    app: tauri::AppHandle,
    query: String,
    limit: Option<i64>,
) -> Vec<crate::infra::data::clipboard::ClipboardItem> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::search(&pool, &query, limit.unwrap_or(20)).await
}

/// 记录剪贴板命中（用户选择粘贴某条历史）。
#[tauri::command]
pub async fn record_clipboard_hit(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::record_hit(&pool, &id).await;
    Ok(())
}

/// 删除指定剪贴板条目。
#[tauri::command]
pub async fn delete_clipboard_item(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::delete_item(&pool, &id).await;
    Ok(())
}

/// 清空所有剪贴板历史。
#[tauri::command]
pub async fn clear_clipboard_history(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::clear_all(&pool).await;
    Ok(())
}

/// 获取剪贴板统计信息。
#[tauri::command]
pub async fn get_clipboard_stats(app: tauri::AppHandle) -> serde_json::Value {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::get_stats(&pool).await
}

// ─── 性能统计（Phase 0.7.0）──────────────────────────────────────────────────

/// 获取性能统计概览（设置页 → 调试 Tab）。
#[tauri::command]
pub async fn get_perf_overview(app: tauri::AppHandle) -> serde_json::Value {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::utils::perf::get_overview(&pool).await
}

/// 查询指定指标的 P50/P90/P99。
#[tauri::command]
pub async fn get_perf_percentiles(
    app: tauri::AppHandle,
    category: String,
    name: String,
    limit: Option<i64>,
) -> serde_json::Value {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::utils::perf::query_percentiles(&pool, &category, &name, limit.unwrap_or(100))
        .await
}

/// 查询慢查询日志。
#[tauri::command]
pub async fn get_perf_slow_queries(
    app: tauri::AppHandle,
    category: String,
    threshold_ms: f64,
    limit: Option<i64>,
) -> Vec<crate::infra::utils::perf::PerformanceMetric> {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::utils::perf::query_slow(&pool, &category, threshold_ms, limit.unwrap_or(20)).await
}

/// 查询最近 N 条性能指标。
#[tauri::command]
pub async fn get_perf_recent(
    app: tauri::AppHandle,
    limit: Option<i64>,
) -> Vec<crate::infra::utils::perf::PerformanceMetric> {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::utils::perf::query_recent(&pool, limit.unwrap_or(100)).await
}

/// 导出性能报告（JSON 格式）。
/// 弹出保存文件对话框，用户选择路径后写入文件，返回保存的路径（取消时返回 null）。
#[tauri::command]
pub async fn export_perf_report(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    let report = crate::infra::utils::perf::export_report(&pool).await;

    // 弹出保存文件对话框
    let default_name = format!(
        "blink-perf-report-{}.json",
        chrono::Local::now().format("%Y-%m-%d")
    );

    let file_path = app
        .dialog()
        .file()
        .set_title("导出性能报告")
        .add_filter("JSON 文件", &["json"])
        .set_file_name(&default_name)
        .blocking_save_file()
        .and_then(|p| match p {
            tauri_plugin_dialog::FilePath::Path(path) => path.to_str().map(|s| s.to_string()),
            tauri_plugin_dialog::FilePath::Url(url) => Some(url.to_string()),
        });

    let Some(path) = file_path else {
        return Ok(None); // 用户取消了
    };

    // 写入文件
    let json_str = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    tokio::fs::write(&path, json_str)
        .await
        .map_err(|e| e.to_string())?;

    tracing::info!(path = %path, "性能报告已导出");
    Ok(Some(path))
}

/// 清除全部性能指标数据。
#[tauri::command]
pub async fn clear_perf_data(app: tauri::AppHandle) -> Result<u64, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::data::perf::clear_all(&pool).await
}

/// 在外部浏览器打开 URL。
#[tauri::command]
pub async fn open_url(url: String) -> Result<(), String> {
    tracing::debug!(%url, "open_url");

    // 使用 Windows ShellExecuteW 打开默认浏览器
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use windows::core::{PCWSTR, w};

        let url_wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(url_wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as i32 <= 32 {
            return Err(format!("打开 URL 失败，返回值: {}", result.0 as i32));
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // 非 Windows 平台使用 open crate（后续可添加）
        return Err("当前平台暂不支持打开 URL".to_string());
    }

    Ok(())
}

// ── 泛型配置命令（0.8.6 §8.1.3 ConfigStore）──────────────────────────────────

/// 泛型配置读取（0.8.6 §8.1.3）。
///
/// 前端 `invoke("get_config_section", { key: "app_config" })` → 返回该 key 的 JSON 值。
/// 不存在返回 `null`（前端自行 fallback 到默认值）。
///
/// **key 命名空间**：
/// - `app_config`：完整 AppConfig（兼容旧 key）
/// - `engine:{id}`：引擎配置（start_menu / calc / file_search）
/// - `plugin:{id}`：插件配置
/// - `context:config`：Context 层配置
///
/// 0.9 扩展：`ai.provider` / `ai.chat` 等直接加 key，零脚手架。
#[tauri::command]
pub async fn get_config_section(
    app: tauri::AppHandle,
    key: String,
) -> Result<serde_json::Value, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let json_str = crate::infra::data::history::get_config(&pool, &key).await;
    match json_str {
        Some(s) => serde_json::from_str(&s).map_err(|e| format!("配置解析失败: {e}")),
        None => Ok(serde_json::Value::Null),
    }
}

/// 泛型配置写入（0.8.6 §8.1.3）。
///
/// 前端 `invoke("set_config_section", { key: "app_config", value: {...} })` → 写入 SQLite。
/// 写入成功后 emit `blink://config-changed` 事件，前端各模块按需订阅。
///
/// **幂等性**：直接覆盖写，不需要先读后写。
#[tauri::command]
pub async fn set_config_section(
    app: tauri::AppHandle,
    key: String,
    value: serde_json::Value,
) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let json = serde_json::to_string(&value).map_err(|e| format!("序列化失败: {e}"))?;
    crate::infra::data::history::set_config(&pool, &key, &json)
        .await
        .map_err(|e| format!("配置写入失败: {e}"))?;

    // 广播配置变更事件（前端各模块按 key 订阅）
    if let Err(e) = app.emit("blink://config-changed", serde_json::json!({ "key": key })) {
        tracing::debug!(error = %e, "emit blink://config-changed failed");
    }

    Ok(())
}

// ── STT / 语音命令（0.10）─────────────────────────────────────────────────────

/// 读取 STT 配置。
#[tauri::command]
pub async fn get_stt_config(
    app: tauri::AppHandle,
) -> Result<crate::app::stt_config::SttConfig, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let config =
        crate::app::config::ConfigStore::get::<crate::app::stt_config::SttConfig>(&pool).await;
    Ok(config)
}

/// 保存 STT 配置。
///
/// `scope` 用于按区段打印日志，避免改本地配置时把云端字段也全部打印出来：
/// - `"global"`: 总开关 / 模式 / 流式 / 音频设备
/// - `"cloud"`: 云端供应商
/// - `"local"`: 本地引擎（模型 / 设备 / 热词 / ITN / VAD）
/// - `None`: 兼容旧调用，打印全量字段
#[tauri::command]
pub async fn set_stt_config(
    app: tauri::AppHandle,
    mut config: crate::app::stt_config::SttConfig,
    scope: Option<String>,
) -> Result<(), String> {
    // 0.12 §2.7 迁移回写：若老配置 cloud_provider 能在 AIConfig 找到匹配，
    // 回写 cloud 字段并清空 cloud_provider（兑现 effective_cloud 的 migration_needed 承诺，
    // 避免老配置永久停留临时态）。用户保存任意区段时触发。
    if config.cloud.is_none() && config.cloud_provider.is_some() {
        let ai_config = crate::app::ai_config::get_ai_config();
        if let Some((cloud, _)) = config.effective_cloud(&ai_config) {
            tracing::info!("STT 云端配置迁移回写: cloud_provider -> cloud");
            config.cloud = Some(cloud);
            config.cloud_provider = None;
        }
    }

    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::app::config::ConfigStore::set(&pool, &config)
        .await
        .map_err(|e| format!("保存 STT 配置失败: {e}"))?;
    // 更新内存缓存（供 STT 引擎同步读取）
    crate::app::stt_config::update_cache(&config);
    // 广播配置变更
    let _ = app.emit(
        "blink://config-changed",
        serde_json::json!({ "key": "stt:config" }),
    );

    match scope.as_deref() {
        Some("global") => {
            tracing::info!(
                scope = "global",
                enabled = config.enabled,
                mode = ?config.mode,
                streaming_mode = ?config.streaming_mode,
                audio_device_id = ?config.audio_device_id,
                "STT 配置已更新"
            );
        }
        Some("cloud") => {
            tracing::info!(
                scope = "cloud",
                cloud_provider = ?config.cloud_provider.as_ref().map(|p| (&p.kind, &p.model_id, &p.base_url)),
                "STT 配置已更新"
            );
        }
        Some("local") => {
            tracing::info!(
                scope = "local",
                local_model_id = ?config.local_model_id,
                funasr_model = %config.local_engine.funasr_model,
                device = %config.local_engine.device,
                auto_start_server = config.local_engine.auto_start_server,
                use_itn = config.local_engine.use_itn,
                hotwords_len = config.local_engine.hotwords.as_ref().map(|h| h.len()).unwrap_or(0),
                vad_silence_threshold = config.local_engine.vad.silence_threshold,
                vad_min_silence_ms = config.local_engine.vad.min_silence_ms,
                vad_min_sentence_ms = config.local_engine.vad.min_sentence_ms,
                "STT 配置已更新"
            );
        }
        _ => {
            // 兼容旧调用（无 scope）：打印全量
            tracing::info!(
                enabled = config.enabled,
                mode = ?config.mode,
                streaming_mode = ?config.streaming_mode,
                cloud_provider = ?config.cloud_provider.as_ref().map(|p| (&p.kind, &p.model_id, &p.base_url)),
                local_model_id = ?config.local_model_id,
                audio_device_id = ?config.audio_device_id,
                funasr_model = %config.local_engine.funasr_model,
                device = %config.local_engine.device,
                auto_start_server = config.local_engine.auto_start_server,
                use_itn = config.local_engine.use_itn,
                hotwords_len = config.local_engine.hotwords.as_ref().map(|h| h.len()).unwrap_or(0),
                vad_silence_threshold = config.local_engine.vad.silence_threshold,
                vad_min_silence_ms = config.local_engine.vad.min_silence_ms,
                vad_min_sentence_ms = config.local_engine.vad.min_sentence_ms,
                "STT 配置已更新"
            );
        }
    }
    Ok(())
}

/// 列出可用 STT 模型。
///
/// 新方案中模型由 FunASR 自动管理（首次使用时自动从 ModelScope 下载）。
/// 此接口返回模型元数据，供前端展示和选择。
#[tauri::command]
pub async fn list_stt_models() -> Result<Vec<serde_json::Value>, String> {
    let models = crate::domain::stt::model_registry();
    let config = crate::app::stt_config::get_stt_config();

    let result: Vec<serde_json::Value> = models
        .iter()
        .map(|m| {
            let is_selected = config.local_model_id.as_deref() == Some(m.id);
            serde_json::json!({
            "id": m.id,
            "display_name": m.display_name,
            "engine": m.engine,
            "params": m.params,
            "size_mb": m.size_mb,
            "languages": m.languages,
            "device": m.device,
                "description": m.description,
                "funasr_model_id": m.funasr_model_id,
                "is_selected": is_selected,
                // 兼容前端: 新方案中模型由 FunASR 自动管理,"已就绪"状态取决于服务是否运行
                "status": "managed_by_funasr",
            })
        })
        .collect();
    Ok(result)
}

/// 选择本地 STT 模型。
///
/// 新方案中模型由 FunASR 自动管理（首次启动 funasr-server 时自动下载）。
/// 此命令设置配置中的 `local_model_id` 和 `funasr_model` 并持久化到数据库，
/// 实际模型下载在 funasr-server 首次启动时由 FunASR 自动完成。
#[tauri::command]
pub async fn download_stt_model(app: tauri::AppHandle, model_id: String) -> Result<(), String> {
    let model =
        crate::domain::stt::find_model(&model_id).ok_or_else(|| format!("未知模型: {model_id}"))?;

    tracing::info!(
        model = %model_id,
        funasr_model = model.funasr_model_id,
        "选择 STT 模型（FunASR 自动管理下载）",
    );

    // 更新配置：设置选中的模型 + funasr_model 标识
    let mut config = crate::app::stt_config::get_stt_config();
    config.local_model_id = Some(model_id);
    config.local_engine.funasr_model = model.funasr_model_id.to_string();

    // 持久化到数据库（否则重启后丢失模型选择）
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::app::config::ConfigStore::set(&pool, &config)
        .await
        .map_err(|e| format!("保存 STT 配置失败: {e}"))?;

    // 更新内存缓存
    crate::app::stt_config::update_cache(&config);

    Ok(())
}

/// 取消选择 STT 模型。
///
/// 新方案中模型由 FunASR 管理，此命令仅清除配置中的选中状态。
#[tauri::command]
pub async fn delete_stt_model(model_id: String) -> Result<(), String> {
    tracing::info!(model = %model_id, "取消选择 STT 模型");
    let mut config = crate::app::stt_config::get_stt_config();
    if config.local_model_id.as_deref() == Some(model_id.as_str()) {
        config.local_model_id = None;
        crate::app::stt_config::update_cache(&config);
    }
    Ok(())
}

/// 取消语音录音(ESC 中断)。
#[tauri::command]
pub fn cancel_voice_recording(app: tauri::AppHandle) {
    if let Some(vs) = app.try_state::<std::sync::Arc<crate::app::voice::VoiceService>>() {
        vs.cancel_recording();
    }
}

/// 查询当前是否正在语音录音。
#[tauri::command]
pub fn is_voice_recording(app: tauri::AppHandle) -> bool {
    if let Some(vs) = app.try_state::<std::sync::Arc<crate::app::voice::VoiceService>>() {
        vs.is_recording()
    } else {
        false
    }
}

/// 音频测试活跃标志(全局,供 start/stop_audio_test 共享)。
static AUDIO_TEST_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 列出可用的音频输入设备。
#[tauri::command]
pub fn list_audio_devices() -> Vec<crate::infra::platform::audio::AudioDevice> {
    crate::infra::platform::audio::list_input_devices()
}

/// 测试音频设备:开始采集并发送音量级别事件。
/// 前端通过 `blink://audio-test-level` 事件接收音量级别 (0.0~1.0)。
#[tauri::command]
pub async fn start_audio_test(
    app: tauri::AppHandle,
    device_id: Option<String>,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;

    tracing::info!(?device_id, "音频测试: 开始");

    // 停止之前的测试（如果有）
    AUDIO_TEST_ACTIVE.store(false, Ordering::SeqCst);

    let mut capture = if let Some(id) = device_id {
        crate::infra::platform::audio::create_capture_with_device(id)
    } else {
        crate::infra::platform::audio::create_capture()
    };

    let format = crate::infra::platform::audio::AudioFormat::default();
    let mut rx = capture.start(format).map_err(|e| {
        tracing::error!(%e, "音频测试: 采集启动失败");
        format!("音频采集启动失败: {e}")
    })?;

    AUDIO_TEST_ACTIVE.store(true, Ordering::SeqCst);

    tracing::info!("音频测试: 采集已启动, 等待数据...");

    let app_clone = app.clone();
    tokio::spawn(async move {
        let mut chunk_count = 0u32;
        let mut max_level = 0.0f64;
        while let Some(chunk) = rx.recv().await {
            if !AUDIO_TEST_ACTIVE.load(Ordering::SeqCst) {
                break;
            }
            chunk_count += 1;
            // 计算 RMS 音量
            let level = if chunk.samples.is_empty() {
                0.0
            } else {
                let sum_sq: f64 = chunk
                    .samples
                    .iter()
                    .map(|s| (*s as f64) * (*s as f64))
                    .sum();
                let rms = (sum_sq / chunk.samples.len() as f64).sqrt();
                (rms * 3.0).min(1.0)
            };
            if level > max_level {
                max_level = level;
            }
            // 首个 chunk + 每 10 个 chunk 打一次日志，让用户知道数据在流动
            if chunk_count == 1 {
                tracing::info!(samples = chunk.samples.len(), "音频测试: 收到首个 chunk");
            } else if chunk_count % 10 == 0 {
                tracing::trace!(
                    chunk_count,
                    level = format!("{:.3}", level),
                    max_level = format!("{:.3}", max_level),
                    "音频测试: 数据流动中"
                );
            }
            let _ = app_clone.emit(
                "blink://audio-test-level",
                serde_json::json!({ "level": level }),
            );
        }
        // capture 的 Drop 会设置 capturing=false，capture 线程随即退出
        drop(capture);
        tracing::info!(
            chunk_count,
            max_level = format!("{:.3}", max_level),
            "音频测试: 已停止"
        );
    });

    Ok(())
}

/// 停止音频测试。
#[tauri::command]
pub fn stop_audio_test() {
    use std::sync::atomic::Ordering;
    AUDIO_TEST_ACTIVE.store(false, Ordering::SeqCst);
    tracing::info!("音频测试: 用户停止");
}

// ── Python 环境管理（uv 自管理）────────────────────────────────────────

/// 全局 funasr-server 子进程句柄。
static FUNASR_SERVER_CHILD: std::sync::Mutex<Option<tokio::process::Child>> =
    std::sync::Mutex::new(None);

/// funasr-server 日志环形缓冲区（最近 200 条）。
///
/// 服务可能在设置页打开前就自启动（auto_start_server），此时前端
/// `listen("blink://funasr-server-log")` 尚未注册，日志会丢失。
/// 缓冲区让设置页打开时通过 `get_funasr_log_history` 命令回补历史日志。
const FUNASR_LOG_BUFFER_CAP: usize = 200;
static FUNASR_LOG_BUFFER: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

/// 向日志缓冲区追加一行（带时间戳），同时 emit 到前端。
fn emit_funasr_log(app: &tauri::AppHandle, line: &str) {
    let ts = chrono::Local::now().format("%H:%M:%S").to_string();
    let entry = format!("[{}] {}", ts, line);
    {
        let mut buf = FUNASR_LOG_BUFFER.lock().unwrap();
        if buf.len() >= FUNASR_LOG_BUFFER_CAP {
            buf.pop_front();
        }
        buf.push_back(entry.clone());
    }
    let _ = app.emit(
        "blink://funasr-server-log",
        serde_json::json!({ "line": line }),
    );
}

/// 获取 funasr-server 历史日志（带时间戳）。
///
/// 设置页打开时调用此命令回补自启动期间产生的日志，
/// 避免用户打开设置页后看不到服务启动过程。
#[tauri::command]
pub fn get_funasr_log_history() -> Vec<String> {
    FUNASR_LOG_BUFFER.lock().unwrap().iter().cloned().collect()
}

/// 查询 Python 环境 + funasr-server 状态。
///
/// 返回 uv/venv/funasr 安装状态 + server 运行状态，供前端展示和诊断。
///
/// 异步执行：Python 子进程检测在 spawn_blocking 线程池中执行，不阻塞 UI 线程。
#[tauri::command]
pub async fn get_funasr_env() -> crate::domain::stt::funasr::FunasrEnv {
    let config = crate::app::stt_config::get_stt_config();
    crate::domain::stt::funasr::get_env_status_async(
        config.local_engine.server_port,
        config.local_engine.funasr_model.clone(),
    )
    .await
}

/// 一键安装 Python 环境（uv + venv + funasr）。
///
/// Blink 通过 uv 自动创建独立的 Python 3.12 虚拟环境并安装 funasr。
/// 用户无需手动安装 Python 或 pip 包。
///
/// 进度通过 `blink://python-env-progress` 事件通知前端：
/// - `{"stage": "uv", "status": "starting"}` — 检查/下载 uv
/// - `{"stage": "uv", "status": "done"}` — uv 就绪
/// - `{"stage": "venv", "status": "starting"}` — 创建 venv
/// - `{"stage": "venv", "status": "done"}` — venv 就绪
/// - `{"stage": "funasr", "status": "installing"}` — 安装 funasr
/// - `{"stage": "funasr", "status": "done"}` — funasr 安装完成
/// - `{"stage": "complete", "status": "ready"}` — 全部完成
/// - `{"stage": "error", "error": "..."}` — 出错
#[tauri::command]
pub async fn setup_python_env(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Emitter;

    // 进度回调：转发到前端 blink://python-env-progress
    let app_progress = app.clone();
    let on_progress: std::sync::Arc<dyn Fn(&str, &str) + Send + Sync> =
        std::sync::Arc::new(move |stage, status| {
            let _ = app_progress.emit(
                "blink://python-env-progress",
                serde_json::json!({ "stage": stage, "status": status }),
            );
        });

    // 日志回调：转发到前端 blink://funasr-server-log（含 uv 逐行安装进度）
    let app_log = app.clone();
    let on_log: std::sync::Arc<dyn Fn(&str) + Send + Sync> = std::sync::Arc::new(move |line| {
        emit_funasr_log(&app_log, line);
    });

    let device = crate::app::stt_config::get_stt_config().local_engine.device;
    crate::infra::platform::python::setup_with_progress(&device, on_progress, on_log).await
}

/// 启动 blink_stt_server 子进程。
///
/// 在后台异步启动 STT server，前端通过 `blink://funasr-server-status` 事件
/// 监听启动进度。模型首次下载可能需要较长时间。
#[tauri::command]
pub async fn start_funasr_server(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Emitter;

    // 从配置构建启动参数（含脚本释放 + 热词文件写入）
    let params = match crate::domain::stt::funasr::ServerStartParams::from_config() {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    let model = params.model.clone();
    let port = params.port;
    let device = params.device.clone();

    // CUDA 诊断：启动前确认 GPU 是否可用
    if device == "cuda" {
        match crate::infra::platform::python::detect_cuda() {
            Some(v) => {
                emit_funasr_log(
                    &app,
                    &format!("[Blink] ✅ 检测到 CUDA {v}，funasr-server 将使用 GPU 加速"),
                );
                tracing::info!(cuda = %v, "CUDA 检测成功，使用 GPU 加速");
            }
            None => {
                emit_funasr_log(
                    &app,
                    "[Blink] ⚠️ 配置为 CUDA 模式但未检测到 NVIDIA GPU，funasr-server 将回退到 CPU",
                );
                tracing::warn!("配置为 CUDA 但未检测到 GPU，将回退到 CPU");
            }
        }
    }

    // 检查 Python 环境是否就绪，未就绪则自动安装
    // setup_with_progress 内部会检测已安装 PyTorch 是否含 CUDA 支持，
    // 若 device==cuda 但 PyTorch 为 CPU 版，会自动重装 CUDA 版。
    let py_status = crate::infra::platform::python::check_status_async().await;
    if !py_status.env_ready || (device == "cuda" && !py_status.torch_cuda_available) {
        let need_cuda_reinstall =
            device == "cuda" && py_status.torch_installed && !py_status.torch_cuda_available;
        let _ = app.emit(
            "blink://funasr-server-status",
            serde_json::json!({ "stage": "setup_env", "message": "正在安装 Python 环境..." }),
        );
        if need_cuda_reinstall {
            emit_funasr_log(
                &app,
                "[Blink] ⚠️ 当前 PyTorch 为 CPU 版，正在重装 CUDA 版 PyTorch（可能需要数分钟）...",
            );
        }
        match crate::infra::platform::python::setup(&device).await {
            Ok(()) => {
                tracing::info!("Python 环境安装完成");
                // 安装后重新检查 CUDA 支持
                if device == "cuda" {
                    let cuda_ok = crate::infra::platform::python::check_torch_cuda();
                    if cuda_ok {
                        emit_funasr_log(&app, "[Blink] ✅ PyTorch CUDA 支持已就绪，GPU 加速可用");
                    } else {
                        emit_funasr_log(
                            &app,
                            "[Blink] ⚠️ PyTorch CUDA 支持不可用，将使用 CPU 推理",
                        );
                    }
                }
            }
            Err(e) => {
                let _ = app.emit(
                    "blink://funasr-server-status",
                    serde_json::json!({ "stage": "error", "error": format!("Python 环境安装失败: {e}") }),
                );
                return Err(format!(
                    "Python 环境安装失败: {e}
请在设置页手动点击「安装环境」按钮。"
                ));
            }
        }
    }

    let _ = app.emit(
        "blink://funasr-server-status",
        serde_json::json!({ "stage": "starting", "model": model, "port": port, "device": device }),
    );

    // ── 孤儿进程检测：FUNASR_SERVER_CHILD 为空但端口被占 ──
    // Blink 崩溃/异常退出后，上次的 funasr-server 子进程可能变成孤儿进程继续运行，
    // 占用监听端口。此时 child handle 丢失，无法通过正常途径管理。
    // 在启动新服务前先清理孤儿进程，避免端口冲突 + 日志无法捕获。
    //
    // MutexGuard 非 Send，必须在独立块中释放，不能跨 await 持有。
    let has_live_child = {
        let mut guard = FUNASR_SERVER_CHILD.lock().unwrap();
        guard
            .as_mut()
            .map(|c| c.try_wait().ok().flatten().is_none())
            .unwrap_or(false)
    };

    if !has_live_child && crate::domain::stt::funasr::is_server_ready(port) {
        // 端口被占但没有 Blink 管理的子进程 → 孤儿进程
        if let Some(pid) = crate::infra::platform::process::kill_process_by_port(port) {
            emit_funasr_log(
                &app,
                &format!("[Blink] ⚠️ 检测到孤儿进程 PID {pid} 占用端口 {port}，已自动清理"),
            );
            tracing::warn!(pid, port, "检测到孤儿 funasr-server 进程，已清理");
        } else {
            emit_funasr_log(
                &app,
                &format!("[Blink] ⚠️ 端口 {port} 被占用但无法定位进程，请手动检查任务管理器"),
            );
        }
        // 等端口释放
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // ── 防止重复启动：如果已有子进程在运行，直接返回 ──
    {
        let mut guard = FUNASR_SERVER_CHILD.lock().unwrap();
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(None) => {
                    // 子进程仍在运行，不重复启动
                    drop(guard);
                    let _ = app.emit(
                        "blink://funasr-server-status",
                        serde_json::json!({ "stage": "already_running", "port": port, "model": &model }),
                    );
                    tracing::info!("funasr-server 子进程已在运行，跳过重复启动");
                    return Ok(());
                }
                Ok(Some(_)) => {
                    // 子进程已退出，清理后继续
                    *guard = None;
                    tracing::info!("检测到旧的 funasr-server 子进程已退出，清理后重新启动");
                }
                Err(_) => {}
            }
        }
    }

    match crate::domain::stt::funasr::start_server(&params).await {
        Ok(Some((child, mut log_rx))) => {
            // 存储子进程句柄
            {
                let mut guard = FUNASR_SERVER_CHILD.lock().unwrap();
                *guard = Some(child);
            }

            // ── 转发 funasr-server 日志到前端 ──
            // 日志来自 start_server 内部的 stdout/stderr 读取 task，
            // 通过 unbounded channel 发送，这里转发为 Tauri 事件。
            // 同时写入全局缓冲区，供设置页打开时回补历史日志。
            let app_log = app.clone();
            tokio::spawn(async move {
                while let Some(line) = log_rx.recv().await {
                    emit_funasr_log(&app_log, &line);
                }
            });

            // ── 异步等待服务就绪（带子进程退出检测）──
            // 两阶段检查：先等 FastAPI HTTP 起来，再等模型加载完成。
            // 模型首次需从 ModelScope 下载（~234MB），可能需要数分钟。
            let app_clone = app.clone();
            let model_clone = model.clone();
            tokio::spawn(async move {
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(
                        crate::domain::stt::funasr::SERVER_STARTUP_TIMEOUT_SECS,
                    );

                // 是否已通知前端「模型加载中」（避免每轮轮询都发事件）
                let mut loading_notified = false;

                loop {
                    if std::time::Instant::now() > deadline {
                        let _ = app_clone.emit(
                            "blink://funasr-server-status",
                            serde_json::json!({
                                "stage": "error",
                                "error": format!(
                                    "funasr-server 在 {}s 内未就绪（端口 {}）",
                                    crate::domain::stt::funasr::SERVER_STARTUP_TIMEOUT_SECS,
                                    port
                                )
                            }),
                        );
                        tracing::error!(port, "funasr-server 启动超时");
                        // 清理子进程 + 标记停止（避免 SERVER_RUNNING 残留为 true）
                        let mut guard = FUNASR_SERVER_CHILD.lock().unwrap();
                        if let Some(child) = guard.as_mut() {
                            let _ = child.start_kill();
                        }
                        *guard = None;
                        drop(guard);
                        crate::domain::stt::funasr::mark_server_stopped();
                        return;
                    }

                    // 检查子进程是否已退出（崩溃 / 异常终止）
                    {
                        let mut guard = FUNASR_SERVER_CHILD.lock().unwrap();
                        if let Some(child) = guard.as_mut() {
                            match child.try_wait() {
                                Ok(Some(status)) => {
                                    // 子进程已退出
                                    *guard = None;
                                    drop(guard);
                                    crate::domain::stt::funasr::mark_server_stopped();
                                    let _ = app_clone.emit(
                                        "blink://funasr-server-status",
                                        serde_json::json!({
                                            "stage": "error",
                                            "error": format!("funasr-server 进程已退出: {status}")
                                        }),
                                    );
                                    tracing::error!(%status, port, "funasr-server 进程异常退出");
                                    return;
                                }
                                Ok(None) => {} // 仍在运行
                                Err(e) => {
                                    tracing::warn!(%e, "try_wait 失败");
                                }
                            }
                        } else {
                            // 子进程已被停止（用户点击停止按钮）
                            return;
                        }
                    }

                    // 检查模型加载状态（/health 端点的 model_status 字段）
                    let model_status = crate::domain::stt::funasr::check_model_loaded(port).await;
                    match model_status {
                        crate::domain::stt::funasr::ModelLoadStatus::Ready => {
                            let _ = app_clone.emit(
                                "blink://funasr-server-status",
                                serde_json::json!({ "stage": "ready", "port": port, "model": &model_clone }),
                            );
                            tracing::info!(port, "funasr-server 就绪（模型已加载）");
                            return;
                        }
                        crate::domain::stt::funasr::ModelLoadStatus::Error => {
                            let _ = app_clone.emit(
                                "blink://funasr-server-status",
                                serde_json::json!({
                                    "stage": "error",
                                    "error": "模型加载失败，请检查网络连接后重试，或查看日志排查原因"
                                }),
                            );
                            tracing::error!(port, "funasr-server 模型加载失败");
                            return;
                        }
                        crate::domain::stt::funasr::ModelLoadStatus::Loading
                        | crate::domain::stt::funasr::ModelLoadStatus::Idle => {
                            if !loading_notified {
                                let _ = app_clone.emit(
                                    "blink://funasr-server-status",
                                    serde_json::json!({ "stage": "loading_model", "port": port, "model": &model_clone }),
                                );
                                tracing::info!(port, "funasr-server HTTP 已就绪，模型加载中...");
                                loading_notified = true;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                        crate::domain::stt::funasr::ModelLoadStatus::Unreachable => {
                            // FastAPI 尚未启动，继续等待
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                    }
                }
            });

            Ok(())
        }
        Ok(None) => {
            // 端口已被占用但 FUNASR_SERVER_CHILD 为空——通常是孤儿进程
            // （start_funasr_server 开头已尝试清理，但可能清理失败或进程刚启动）
            // 此时无法捕获子进程 stdout/stderr，日志窗口不会有实时日志。
            emit_funasr_log(
                &app,
                &format!(
                    "[Blink] ⚠️ 端口 {port} 已被占用（可能是之前遗留的进程），无法捕获实时日志。建议先停止服务再重新启动。"
                ),
            );
            let app_clone = app.clone();
            let model_clone = model.clone();
            tokio::spawn(async move {
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(
                        crate::domain::stt::funasr::SERVER_STARTUP_TIMEOUT_SECS,
                    );
                let mut loading_notified = false;
                loop {
                    if std::time::Instant::now() > deadline {
                        let _ = app_clone.emit(
                            "blink://funasr-server-status",
                            serde_json::json!({
                                "stage": "error",
                                "error": format!(
                                    "funasr-server 模型在 {}s 内未加载完成（端口 {}）",
                                    crate::domain::stt::funasr::SERVER_STARTUP_TIMEOUT_SECS,
                                    port
                                )
                            }),
                        );
                        // 标记停止（Ok(None) 分支没有 child handle，只需标记状态）
                        crate::domain::stt::funasr::mark_server_stopped();
                        return;
                    }
                    let model_status = crate::domain::stt::funasr::check_model_loaded(port).await;
                    match model_status {
                        crate::domain::stt::funasr::ModelLoadStatus::Ready => {
                            let _ = app_clone.emit(
                                "blink://funasr-server-status",
                                serde_json::json!({ "stage": "ready", "port": port, "model": &model_clone }),
                            );
                            return;
                        }
                        crate::domain::stt::funasr::ModelLoadStatus::Error => {
                            let _ = app_clone.emit(
                                "blink://funasr-server-status",
                                serde_json::json!({
                                    "stage": "error",
                                    "error": "模型加载失败，请检查网络连接后重试"
                                }),
                            );
                            return;
                        }
                        crate::domain::stt::funasr::ModelLoadStatus::Loading
                        | crate::domain::stt::funasr::ModelLoadStatus::Idle => {
                            if !loading_notified {
                                let _ = app_clone.emit(
                                    "blink://funasr-server-status",
                                    serde_json::json!({ "stage": "loading_model", "port": port, "model": &model_clone }),
                                );
                                loading_notified = true;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                        crate::domain::stt::funasr::ModelLoadStatus::Unreachable => {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                    }
                }
            });
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// 停止 funasr-server 子进程。
///
/// 先 kill Blink 管理的子进程（通过 child handle），再检查端口是否仍被占。
/// 如果端口仍被占，说明存在孤儿进程（Blink 崩溃后遗留），通过 PID 清理。
#[tauri::command]
pub async fn stop_funasr_server() -> Result<(), String> {
    // 1. 先从 Mutex 中取出 child，避免跨 await 持有 MutexGuard（非 Send）
    let mut child_opt = FUNASR_SERVER_CHILD.lock().unwrap().take();
    if let Some(child) = child_opt.as_mut() {
        let _ = child.kill().await;
    }
    drop(child_opt);

    // 2. 检查端口是否仍被占（可能是孤儿进程）
    let port = crate::app::stt_config::get_stt_config()
        .local_engine
        .server_port;
    if crate::domain::stt::funasr::is_server_ready(port) {
        if let Some(pid) = crate::infra::platform::process::kill_process_by_port(port) {
            tracing::warn!(pid, port, "停止服务时检测到孤儿进程，已清理");
        }
        // 等端口释放
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    crate::domain::stt::funasr::mark_server_stopped();
    tracing::info!("funasr-server 已停止");
    Ok(())
}

/// Blink 退出时同步停止 funasr-server 子进程（避免孤儿进程）。
///
/// 使用 `start_kill()`（非 async）——发送 kill 信号后不等待进程退出，
/// 避免阻塞 app 退出。由 `main.rs` 的 `RunEvent::Exit` handler 调用。
pub fn shutdown_funasr_server_blocking() {
    let mut child_opt = FUNASR_SERVER_CHILD.lock().unwrap().take();
    if let Some(child) = child_opt.as_mut() {
        let _ = child.start_kill();
        crate::domain::stt::funasr::mark_server_stopped();
        tracing::info!("funasr-server 已在 Blink 退出时停止");
    }
}

/// STT 诊断：检查 FunASR 环境 + 服务状态 + 配置。
///
/// 返回详细诊断报告，帮助定位 "STT 不工作" 的具体原因：
/// 1. Python 是否安装及版本
/// 2. funasr 包是否安装及版本
/// 3. funasr-server 是否在运行（健康检查）
/// 4. 当前配置（模式、模型、端口）
/// 5. 如果服务就绪，下载示例音频调一次 HTTP API 验证识别效果
///
/// 所有诊断步骤同步输出到 tracing 日志，便于从日志文件排查问题。
#[tauri::command]
pub async fn diagnose_stt() -> Result<serde_json::Value, String> {
    let mut report = serde_json::json!({
        "funasr_env": {},
        "config": {},
        "models": [],
        "api_test": null,
    });

    let config = crate::app::stt_config::get_stt_config();
    let port = config.local_engine.server_port;

    tracing::info!("=== STT 诊断开始 ===");

    // ── FunASR 环境状态（异步，不阻塞 UI）──
    let env = crate::domain::stt::funasr::get_env_status_async(
        port,
        config.local_engine.funasr_model.clone(),
    )
    .await;

    let server_ready_tcp = crate::domain::stt::funasr::is_server_ready(port);
    let model_status = if server_ready_tcp {
        crate::domain::stt::funasr::check_model_loaded(port).await
    } else {
        crate::domain::stt::funasr::ModelLoadStatus::Unreachable
    };
    let server_ready = model_status == crate::domain::stt::funasr::ModelLoadStatus::Ready;
    let model_status_str = match model_status {
        crate::domain::stt::funasr::ModelLoadStatus::Ready => "ready",
        crate::domain::stt::funasr::ModelLoadStatus::Loading => "loading",
        crate::domain::stt::funasr::ModelLoadStatus::Idle => "idle",
        crate::domain::stt::funasr::ModelLoadStatus::Error => "error",
        crate::domain::stt::funasr::ModelLoadStatus::Unreachable => "unreachable",
    };

    // 同步诊断信息到 tracing 日志
    tracing::info!(
        available = env.uv_available,
        version = ?env.uv_version,
        "诊断: uv"
    );
    tracing::info!(
        exists = env.venv_exists,
        version = ?env.venv_python_version,
        "诊断: venv"
    );
    tracing::info!(
        installed = env.torch_installed,
        version = ?env.torch_version,
        "诊断: torch"
    );
    tracing::info!(
        installed = env.funasr_installed,
        version = ?env.funasr_version,
        "诊断: funasr"
    );
    tracing::info!(
        running = env.server_running,
        ready = server_ready,
        model_status = %model_status_str,
        port,
        "诊断: server"
    );

    report["funasr_env"] = serde_json::json!({
        "uv_available": env.uv_available,
        "uv_version": env.uv_version,
        "venv_exists": env.venv_exists,
        "venv_python_version": env.venv_python_version,
        "torch_installed": env.torch_installed,
        "torch_version": env.torch_version,
        "torch_cuda_available": env.torch_cuda_available,
        "funasr_installed": env.funasr_installed,
        "funasr_version": env.funasr_version,
        "env_ready": env.env_ready,
        "server_running": env.server_running,
        "server_port": env.server_port,
        "server_model": env.server_model,
        "server_ready": server_ready,
        "model_status": model_status_str,
    });

    // ── 配置状态 ──
    tracing::info!(
        mode = ?config.mode,
        model = %config.local_engine.funasr_model,
        device = %config.local_engine.device,
        streaming = config.streaming,
        "诊断: config"
    );

    report["config"] = serde_json::json!({
        "enabled": config.enabled,
        "mode": format!("{:?}", config.mode),
        "local_model_id": config.local_model_id,
        "funasr_model": config.local_engine.funasr_model,
        "server_port": config.local_engine.server_port,
        "device": config.local_engine.device,
        "streaming": config.streaming,
    });

    // ── 模型列表 ──
    let models = crate::domain::stt::model_registry();
    for model in models {
        report["models"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": model.id,
                "display_name": model.display_name,
                "funasr_model_id": model.funasr_model_id,
                "params": model.params,
                "size_mb": model.size_mb,
                "device": model.device,
                "is_selected": config.local_model_id.as_deref() == Some(model.id),
            }));
    }

    // ── API 测试：如果服务就绪，下载示例音频测试识别 ──
    if server_ready {
        tracing::info!("诊断: 开始 API 测试（下载示例音频）");
        // FunASR 官方中文示例音频（BAC009 数据集）
        let audio_url = "https://isv-data.oss-cn-hangzhou.aliyuncs.com/ics/MaaS/ASR/test_audio/BAC009S0764W0121.wav";

        match test_audio_via_server(audio_url, port).await {
            Ok(text) => {
                tracing::info!(%text, "诊断: API 测试成功");
                report["api_test"] = serde_json::json!({
                    "wav_written": true,
                    "result": {
                        "success": true,
                        "text": text,
                    },
                });
            }
            Err(e) => {
                tracing::warn!(%e, "诊断: API 测试失败");
                report["api_test"] = serde_json::json!({
                    "wav_written": true,
                    "result": {
                        "success": false,
                        "error": e,
                    },
                });
            }
        }
    } else {
        tracing::info!("诊断: API 测试跳过（服务未就绪）");
        report["api_test"] = serde_json::json!({
            "skipped": true,
            "reason": "funasr-server 未就绪",
        });
    }

    tracing::info!("=== STT 诊断完成 ===");
    tracing::info!(report = %report, "STT 诊断报告");
    Ok(report)
}

/// 下载示例音频并通过 funasr-server 测试识别。
///
/// 流程：
/// 1. HTTP 下载 WAV 音频
/// 2. 解析 WAV → f32 PCM 样本
/// 3. 分块喂入 LocalSttEngine（模拟 transcribe_chunk）
/// 4. 调用 finalize → POST 到 funasr-server
/// 5. 返回识别文本
async fn test_audio_via_server(audio_url: &str, port: u16) -> Result<String, String> {
    // 1. 下载音频
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

    let resp = client
        .get(audio_url)
        .send()
        .await
        .map_err(|e| format!("下载示例音频失败: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("下载音频 HTTP 失败: {}", resp.status()));
    }

    let wav_bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取音频字节失败: {e}"))?;

    tracing::info!(size = wav_bytes.len(), "诊断: 示例音频下载完成");

    // 2. 解析 WAV → f32 PCM 样本
    let samples = crate::domain::stt::wav::parse_wav_to_f32(&wav_bytes)?;
    let duration_ms = (samples.len() as f64 / 16000.0 * 1000.0) as u64;
    tracing::info!(samples = samples.len(), duration_ms, "诊断: WAV 解析完成");

    // 3. 创建引擎并分块喂入音频
    let engine = crate::domain::stt::local::LocalSttEngine::for_diagnostic(port);
    let chunk_size = 1600usize; // 100ms chunks
    for chunk in samples.chunks(chunk_size) {
        engine
            .transcribe_chunk(chunk)
            .await
            .map_err(|e| e.to_string())?;
    }

    // 4. 调用 finalize → POST 到 funasr-server
    tracing::info!("诊断: 调用 funasr-server 转录...");
    let result = engine.finalize().await.map_err(|e| e.to_string())?;

    Ok(result)
}

/// 云端 STT 连接测试：下载示例音频 → 发送到云端供应商 API → 返回识别文本。
///
/// 与 `diagnose_stt` 中的 `test_audio_via_server` 对称，
/// 区别是此命令发送到云端供应商而非本地 funasr-server。
#[tauri::command]
pub async fn test_cloud_stt() -> Result<serde_json::Value, String> {
    let config = crate::app::stt_config::get_stt_config();
    let ai_config = crate::app::ai_config::get_ai_config();

    // 0.12 §2.7: 复用 resolve_stt_endpoint，与 finalize 走同一配置解析路径
    // （支持新结构 cloud 字段 + 老配置自动迁移；旧实现只读 cloud_provider 导致新配置测试必失败）
    let endpoint = crate::domain::stt::cloud::resolve_stt_endpoint(&config, &ai_config)
        .map_err(|e| format!("云端 STT 配置解析失败: {e}"))?;

    let is_chat_asr = endpoint.uses_chat_completion_asr;
    let url = format!(
        "{}/{}",
        endpoint.base_url,
        if is_chat_asr {
            "chat/completions"
        } else {
            "audio/transcriptions"
        }
    );

    tracing::info!(
        url = %url,
        model = %endpoint.model_id,
        protocol = if is_chat_asr { "chat-completion" } else { "whisper" },
        "云端 STT 测试"
    );

    // 下载示例音频（复用与本地诊断相同的音频）
    let audio_url = "https://isv-data.oss-cn-hangzhou.aliyuncs.com/ics/MaaS/ASR/test_audio/BAC009S0764W0121.wav";
    let dl_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

    let resp = dl_client
        .get(audio_url)
        .send()
        .await
        .map_err(|e| format!("下载示例音频失败: {e}"))?;

    if !resp.status().is_success() {
        return Ok(serde_json::json!({
            "success": false,
            "error": format!("下载音频 HTTP {}", resp.status()),
        }));
    }

    let wav_bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取音频字节失败: {e}"))?;

    tracing::info!(size = wav_bytes.len(), "云端 STT 测试: 示例音频下载完成");

    // 发送到云端 API（复用 send_stt_request，与 finalize 同路径）
    let result = crate::domain::stt::cloud::send_stt_request(&endpoint, &wav_bytes).await;

    match result {
        Ok(text) => {
            tracing::info!(%text, "云端 STT 测试成功");
            Ok(serde_json::json!({
                "success": true,
                "text": text,
            }))
        }
        Err(e) => {
            let err_str = e.to_string();
            tracing::warn!(%err_str, "云端 STT 测试失败");

            // 根据错误内容给出更友好的提示
            let friendly = if err_str.contains("404") {
                format!(
                    "供应商未提供该接口（404）。\
                     请确认 {url} 存在。\
                     若使用 Mimo，请确认模型 ID 为 mimo-v2.5-asr；\
                     若使用其他供应商，请确认其支持音频转写端点。"
                )
            } else if err_str.contains("401") || err_str.contains("403") {
                "认证失败（401/403）。请检查 API Key 是否正确，以及是否有相应权限。".to_string()
            } else if err_str.contains("400") {
                format!(
                    "请求参数错误（400）。请检查模型 ID「{}」是否正确。原始错误: {err_str}",
                    endpoint.model_id
                )
            } else {
                err_str
            };

            Ok(serde_json::json!({
                "success": false,
                "error": friendly,
            }))
        }
    }
}

// ── 空间管理 ────────────────────────────────────────────────────────

/// 递归计算目录大小（字节）。
fn dir_size_bytes(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

/// 字节 → MB（保留两位小数）。
fn bytes_to_mb(bytes: u64) -> f64 {
    ((bytes as f64 / (1024.0 * 1024.0)) * 100.0).round() / 100.0
}

/// 获取 STT 相关空间占用信息。
///
/// 返回 uv 二进制、Python venv、ModelScope 模型缓存的大小。
#[tauri::command]
pub async fn get_stt_space_usage() -> serde_json::Value {
    let python_dir = dirs_next::data_dir()
        .unwrap_or_default()
        .join("blink")
        .join("python");

    let uv_dir = python_dir.join("uv");
    let venv_dir = python_dir.join("venv");

    // ModelScope 模型缓存：Blink 将其重定向到 python/models 目录（通过 MODELSCOPE_CACHE 环境变量）。
    // 旧版本可能仍在 ~/.cache/modelscope，也检查并显示。
    let models_dir = python_dir.join("models");
    let legacy_modelscope_cache =
        dirs_next::home_dir().map(|h| h.join(".cache").join("modelscope"));

    let mut items = Vec::new();
    let mut total_bytes: u64 = 0;

    // uv 二进制
    if uv_dir.exists() {
        let size = dir_size_bytes(&uv_dir);
        total_bytes += size;
        items.push(serde_json::json!({
            "label": "uv 二进制",
            "path": uv_dir.display().to_string(),
            "size_mb": bytes_to_mb(size),
        }));
    }

    // Python venv（含 torch + funasr）
    if venv_dir.exists() {
        let size = dir_size_bytes(&venv_dir);
        total_bytes += size;
        items.push(serde_json::json!({
            "label": "Python 虚拟环境 (venv + torch + funasr)",
            "path": venv_dir.display().to_string(),
            "size_mb": bytes_to_mb(size),
        }));
    }

    // ModelScope 模型缓存（Blink 自管理目录）
    if models_dir.exists() {
        let size = dir_size_bytes(&models_dir);
        total_bytes += size;
        items.push(serde_json::json!({
            "label": "FunASR 模型缓存",
            "path": models_dir.display().to_string(),
            "size_mb": bytes_to_mb(size),
        }));
    }

    // 旧版残留：~/.cache/modelscope（可能存在历史下载）
    if let Some(legacy_dir) = &legacy_modelscope_cache {
        if legacy_dir.exists() {
            let size = dir_size_bytes(legacy_dir);
            if size > 0 {
                total_bytes += size;
                items.push(serde_json::json!({
                    "label": "旧版模型缓存残留 (ModelScope 默认路径)",
                    "path": legacy_dir.display().to_string(),
                    "size_mb": bytes_to_mb(size),
                }));
            }
        }
    }

    serde_json::json!({
        "items": items,
        "total_mb": bytes_to_mb(total_bytes),
    })
}

/// 清理 STT Python 环境（删除 venv + uv）。
///
/// 会先停止 funasr-server（如果在运行），然后删除整个 python 目录。
/// 清理后需重新安装环境才能使用本地 STT。
#[tauri::command]
pub async fn cleanup_stt_space() -> Result<(), String> {
    // 先停止 funasr-server
    let mut child_opt = FUNASR_SERVER_CHILD.lock().unwrap().take();
    if let Some(child) = child_opt.as_mut() {
        let _ = child.kill().await;
        crate::domain::stt::funasr::mark_server_stopped();
    }
    drop(child_opt);

    let python_dir = dirs_next::data_dir()
        .unwrap_or_default()
        .join("blink")
        .join("python");

    let mut errors = Vec::new();

    // 删除 venv
    let venv_dir = python_dir.join("venv");
    if venv_dir.exists() {
        tracing::info!(path = %venv_dir.display(), "清理 venv");
        if let Err(e) = std::fs::remove_dir_all(&venv_dir) {
            errors.push(format!("删除 venv 失败: {e}"));
        }
    }

    // 删除 uv
    let uv_dir = python_dir.join("uv");
    if uv_dir.exists() {
        tracing::info!(path = %uv_dir.display(), "清理 uv");
        if let Err(e) = std::fs::remove_dir_all(&uv_dir) {
            errors.push(format!("删除 uv 失败: {e}"));
        }
    }

    // 删除模型缓存（Blink 自管理目录）
    let models_dir = python_dir.join("models");
    if models_dir.exists() {
        tracing::info!(path = %models_dir.display(), "清理模型缓存");
        if let Err(e) = std::fs::remove_dir_all(&models_dir) {
            errors.push(format!("删除模型缓存失败: {e}"));
        }
    }

    // 清理旧版残留：~/.cache/modelscope
    if let Some(legacy_dir) = dirs_next::home_dir().map(|h| h.join(".cache").join("modelscope")) {
        if legacy_dir.exists() {
            tracing::info!(path = %legacy_dir.display(), "清理旧版模型缓存残留");
            if let Err(e) = std::fs::remove_dir_all(&legacy_dir) {
                // 旧版残留清理失败不阻断（可能被其他程序占用）
                tracing::warn!(%e, "清理旧版模型缓存残留失败（不阻断）");
            }
        }
    }

    if errors.is_empty() {
        tracing::info!("STT 空间清理完成");
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// 打开 STT Python 环境所在文件夹（`%APPDATA%\blink\python\`）。
///
/// 方便用户查看 venv、uv、模型缓存等文件。目录不存在时自动创建。
#[tauri::command]
pub fn open_stt_folder() -> Result<(), String> {
    let python_dir = dirs_next::data_dir()
        .unwrap_or_default()
        .join("blink")
        .join("python");

    // 目录不存在时先创建，避免 explorer 打开"文档"等默认位置
    if !python_dir.exists() {
        std::fs::create_dir_all(&python_dir).map_err(|e| format!("创建目录失败: {e}"))?;
    }

    tracing::info!(path = %python_dir.display(), "打开 STT 文件夹");
    std::process::Command::new("explorer.exe")
        .arg(&python_dir)
        .spawn()
        .map_err(|e| format!("打开文件夹失败: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_translate_batch_payload, version_gt};
    use crate::domain::capability::ItemResult;
    use crate::domain::execution::ActionOutcome;

    #[test]
    fn translate_batch_payload_requires_matching_string_array() {
        let outcome = ActionOutcome::Items {
            items: vec![ItemResult {
                title: "批量翻译".into(),
                subtitle: None,
                payload: serde_json::json!({ "results": ["你好", "世界"] }),
                score: Some(1.0),
            }],
        };
        assert_eq!(
            parse_translate_batch_payload(&outcome, 2).unwrap(),
            vec!["你好".to_string(), "世界".to_string()]
        );
        assert!(parse_translate_batch_payload(&outcome, 1).is_none());

        let malformed = ActionOutcome::Items {
            items: vec![ItemResult {
                title: "批量翻译".into(),
                subtitle: None,
                payload: serde_json::json!({ "results": ["你好", 2] }),
                score: Some(1.0),
            }],
        };
        assert!(parse_translate_batch_payload(&malformed, 2).is_none());
    }

    #[test]
    fn version_gt_basic() {
        assert!(version_gt("0.9.0", "0.8.8"));
        assert!(version_gt("1.0.0", "0.99.99"));
        assert!(!version_gt("0.8.8", "0.8.8"));
        assert!(!version_gt("0.8.7", "0.8.8"));
    }

    #[test]
    fn version_gt_patch() {
        assert!(version_gt("0.8.9", "0.8.8"));
        assert!(!version_gt("0.8.8", "0.8.9"));
    }

    #[test]
    fn version_gt_malformed() {
        // 非标准版本号走 fallback，缺失部分按 0 算
        assert!(version_gt("0.9", "0.8.8"));
        assert!(!version_gt("0.8", "0.8.1"));
    }

    #[test]
    fn version_gt_semver_prerelease() {
        // semver: pre-release 版本低于同版本的 release
        assert!(!version_gt("1.0.0-rc.1", "1.0.0"));
        assert!(version_gt("1.0.0", "1.0.0-rc.1"));
        assert!(version_gt("1.0.0-rc.2", "1.0.0-rc.1"));
    }

    #[test]
    fn version_gt_0_10_over_0_9() {
        // 验证 0.10.x > 0.9.x（此前「检查更新无效」的版本号场景）
        assert!(version_gt("0.10.4", "0.9.8"));
        assert!(version_gt("0.10.0", "0.9.99"));
        assert!(!version_gt("0.9.8", "0.10.4"));
    }
}
