//! ai 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use tauri::{Emitter, Manager};
use crate::domain::event_names::EventNames;
use super::shared::{BuiltinToolSummary, ComposerBarSnapshot, McpServerSummary};

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

/// AI Capability 确认执行（command 名沿用旧 IPC 契约）。
///
/// 前端收到 `blink://ai-confirm-action` 事件后展示确认卡片,
/// 用户按 Enter 确认 → invoke 此 command → 后端执行能力。
///
/// **0.14 Capability-only**：只查 CapabilityRegistry，不提供 Action fallback。
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

    // 0.14.6 §2.2：从 state 获取 DomainEnv 桥接器
    let env_arc = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();

    // 0.14 Capability-only：确认卡片只允许执行 Capability。
    // command 名保留以兼容现有前端 IPC，但不再提供 ActionRegistry fallback。
    let search_service =
        app.state::<std::sync::Arc<crate::domain::search::SearchService>>();
    let seq = search_service
        .take_ai_confirmation(&action_name, &arguments)
        .ok_or_else(|| format!("没有匹配的待确认 Capability: {action_name}"))?;

    let cap_reg = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    if let Some(cap) = cap_reg.get(&action_name) {
        let ctx = crate::domain::capability::InvokeContext {
            env: env_arc.as_ref(),
            deadline: None,
        };
        match cap.invoke(arguments.clone(), &ctx).await {
            Ok(result) => {
                let summary = result.to_display_text();
                tracing::info!(%action_name, %summary, "confirm_ai_action: Capability 执行成功");
                write_confirm_audit(&app, &action_name, &arguments, &summary).await;
                if matches!(
                    &result,
                    crate::domain::capability::CapabilityResult::Done { .. }
                ) {
                    crate::infra::platform::window::hide(&app, "confirm_ai_action");
                } else {
                    search_service.emit_confirmed_capability_result(seq, &result);
                }
            }
            Err(e) => {
                tracing::error!(%action_name, error = %e, "confirm_ai_action: Capability 执行失败");
                return Err(e.to_string());
            }
        }
        return Ok(());
    }

    let msg = format!("未知 Capability id: {action_name}");
    tracing::warn!(%action_name, "confirm_ai_action: 未知 id");
    Err(msg)
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
///
/// 0.12.6：`group_id` 参数注入分组级系统提示词——设置对话所属分组后，
/// 查询分组系统提示词并传给 ChatService，影响 Agent 行为约束。
#[tauri::command]
pub async fn chat_prompt(
    app: tauri::AppHandle,
    conversation_id: String,
    message: String,
    group_id: Option<String>,
) -> Result<u64, String> {
    let chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .ok_or("ChatService 未注册")?;

    // 0.12.8: 查询系统提示词 + 调 prompt() 在前，持久化分组在后——
    // 避免 prompt 失败（如 AlreadyActive）时分组已写入 DB 的副作用先于校验问题。
    let pools = app.state::<crate::infra::data::DbPools>();

    // 按新 group_id 直接查系统提示词（Some → 查该分组；None → 查对话现有分组）
    let group_system_prompt = if let Some(ref gid) = group_id {
        crate::infra::data::conversations::get_group_system_prompt(&pools.ai, Some(gid))
            .await
            .unwrap_or(None)
    } else {
        // group_id = None：对话可能在已有分组中，查现有分组的提示词
        crate::infra::data::conversations::get_effective_system_prompt(
            &pools.ai,
            &conversation_id,
        )
        .await
        .unwrap_or(None)
    };

    let handle = chat
        .prompt(conversation_id.clone(), message, group_system_prompt)
        .await
        .map_err(|e| e.to_string())?;

    // prompt 成功后才持久化分组（副作用后置）
    if let Some(ref gid) = group_id {
        let pools2 = app.state::<crate::infra::data::DbPools>();
        if let Err(e) = crate::infra::data::conversations::set_conversation_group(
            &pools2.ai,
            &conversation_id,
            Some(gid),
        )
        .await
        {
            tracing::warn!(%conversation_id, %e, "chat_prompt: 持久化分组失败（不影响对话）");
        }
    }
    let request_id = handle.request_id;
    let conv_id = handle.conversation_id.clone();
    let mut chunks = handle.chunks;

    // spawn 后台 task 消费 chunk 流并定向 emit 到 chat 窗口
    let app_clone = app.clone();
    let conv_id_clone = conv_id.clone();
    tokio::spawn(async move {
        let mut done_sent = false;
        while let Some(chunk) = chunks.recv().await {
            let is_done = matches!(
                chunk,
                crate::domain::ai::agent_provider::ChatStreamChunk::Done { .. }
                    | crate::domain::ai::agent_provider::ChatStreamChunk::Error { .. }
                    | crate::domain::ai::agent_provider::ChatStreamChunk::MaxTurnsReached { .. }
            );
            if is_done {
                done_sent = true;
            }
            let event = crate::domain::ai::chat_service::ChatStreamEvent {
                request_id,
                conversation_id: conv_id_clone.clone(),
                chunk,
            };
            let _ = app_clone.emit_to(
                tauri::EventTarget::window("chat"),
                EventNames::CHAT_STREAM,
                &event,
            );
            if is_done {
                break;
            }
        }
        // 0.12.5：chunk 流意外结束（recv 返回 None）且未发送 Done/Error/MaxTurns
        // → 发送兜底 Done 事件，避免前端永远收不到结束事件而卡在流式模式
        if !done_sent {
            let event = crate::domain::ai::chat_service::ChatStreamEvent {
                request_id,
                conversation_id: conv_id_clone.clone(),
                chunk: crate::domain::ai::agent_provider::ChatStreamChunk::Done {
                    input_tokens: 0,
                    output_tokens: 0,
                    model_name: None,
                },
            };
            let _ = app_clone.emit_to(
                tauri::EventTarget::window("chat"),
                EventNames::CHAT_STREAM,
                &event,
            );
        }
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

/// 列出所有对话（按 last_active_at 倒序）。
///
/// 供 chat 侧边栏渲染对话列表。每条含 id / title / created_at / last_active_at / message_count。
#[tauri::command]
pub async fn list_chat_conversations(
    app: tauri::AppHandle,
) -> Result<Vec<crate::infra::data::conversations::Conversation>, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::list_conversations(&pools.ai).await
}

/// 删除指定对话（级联删除 messages）。
#[tauri::command]
pub async fn delete_chat_conversation(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<bool, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::delete_conversation(&pools.ai, &conversation_id)
        .await
}

/// 重命名对话。
#[tauri::command]
pub async fn rename_chat_conversation(
    app: tauri::AppHandle,
    conversation_id: String,
    title: String,
) -> Result<bool, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::rename_conversation(&pools.ai, &conversation_id, &title)
        .await
}

/// 查询对话的有效系统提示词（0.12.7 §6.5）。
///
/// 返回对话所属分组的 system_prompt（直属分组，非祖先继承）。
/// 无分组或分组无提示词时返回 None。
#[tauri::command]
pub async fn get_conversation_system_prompt(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<Option<String>, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::get_effective_system_prompt(&pools.ai, &conversation_id)
        .await
}

/// 加载对话的完整消息历史（0.12.3 Phase B）。
///
/// 从 DB 加载全量 messages（按 id 升序），反序列化 rig `Message`，
/// 提取 role + text + thinking，返回 `Vec<ChatMessageSnapshot>`。
/// 供前端切换对话时重建消息流。
#[tauri::command]
pub async fn get_chat_messages(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<Vec<ChatMessageSnapshot>, String> {
    use rig_core::completion::message::{AssistantContent, UserContent};
    use rig_core::completion::Message;

    let pools = app.state::<crate::infra::data::DbPools>();
    let rows = crate::infra::data::conversations::load_all_messages(&pools.ai, &conversation_id)
        .await?;

    let mut snapshots: Vec<ChatMessageSnapshot> = Vec::with_capacity(rows.len());
    for (role, content_json, created_at) in rows {
        let msg: Message = serde_json::from_str(&content_json)
            .map_err(|e| format!("反序列化消息失败: {e}"))?;

        match &msg {
            Message::User { content } => {
                // 检测 ToolResult 消息（rig 存为 User + ToolResult）
                let tool_result_text = content.iter().find_map(|c| match c {
                    UserContent::ToolResult(tr) => {
                        // 0.14.1: 复用 summarize_tool_result（含截断 + 图片占位）
                        let summary = crate::domain::ai::agent_provider::summarize_tool_result(tr);
                        if summary.is_empty() { None } else { Some(summary) }
                    }
                    _ => None,
                });

                if let Some(summary) = tool_result_text {
                    // summarize_tool_result 已处理截断（50000 字符 + 省略号）
                    // 附加到前一条 ToolCall 快照
                    if let Some(last_tool) = snapshots.iter_mut().rev().find(|s| s.tool_name.is_some() && s.tool_result.is_none()) {
                        last_tool.tool_result = Some(summary);
                    }
                    continue;
                }

                let text = content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                // 剥离 chat_service 注入的时间后缀（\n\n[当前时间：...]），不展示给用户
                let text = if let Some(idx) = text.rfind("\n\n[当前时间：") {
                    text[..idx].to_string()
                } else {
                    text
                };
                snapshots.push(ChatMessageSnapshot {
                    role,
                    text,
                    thinking: None,
                    tool_name: None,
                    tool_arguments: None,
                    tool_result: None,
                    created_at: Some(created_at),
                });
            }
            Message::Assistant { content, .. } => {
                let mut text = String::new();
                let mut thinking = String::new();
                let mut tool = None;
                let mut tool_args = None;
                for c in content.iter() {
                    match c {
                        AssistantContent::Text(t) => text.push_str(&t.text),
                        AssistantContent::Reasoning(r) => {
                            thinking.push_str(&r.display_text());
                        }
                        AssistantContent::ToolCall(tc) => {
                            tool = Some(tc.function.name.clone());
                            tool_args = Some(tc.function.arguments.to_string());
                        }
                        _ => {}
                    }
                }
                snapshots.push(ChatMessageSnapshot {
                    role,
                    text,
                    thinking: if thinking.is_empty() { None } else { Some(thinking) },
                    tool_name: tool,
                    tool_arguments: tool_args,
                    tool_result: None,
                    created_at: Some(created_at),
                });
            }
            Message::System { content } => {
                snapshots.push(ChatMessageSnapshot {
                    role,
                    text: content.clone(),
                    thinking: None,
                    tool_name: None,
                    tool_arguments: None,
                    tool_result: None,
                    created_at: Some(created_at),
                });
            }
        }
    }
    Ok(snapshots)
}

/// 异步生成对话标题（0.12.5 §5.3）。
///
/// 读取 `ChatConfig.auto_title` 开关 + `title_tier` 档位，调
/// `AIProvider::complete()`（非 Agent 路径，单轮补全）生成 6-10 字语义化标题。
///
/// 成功后更新 `conversations.title` 并 emit `blink://chat-title-updated` 事件，
/// 前端更新 header 标题 + 刷新侧边栏。失败静默降级（保持截断标题）。
#[tauri::command]
pub async fn generate_conversation_title(
    app: tauri::AppHandle,
    conversation_id: String,
    first_message: String,
) -> Result<(), String> {
    use crate::app::ai_config::Tier;
    use crate::domain::ai::message::{ChatMessage, CompletionRequest, Role};

    // 1. 读配置——auto_title 关闭则直接返回
    let registry = app
        .try_state::<std::sync::Arc<crate::domain::ai::registry::AIProviderRegistry>>()
        .ok_or("AIProviderRegistry 未注册")?;
    let config = registry.config_snapshot();
    let chat_cfg = &config.chat_config;
    if !chat_cfg.auto_title {
        return Ok(());
    }

    // 2. 解析 tier
    let tier = match chat_cfg.title_tier.as_str() {
        "main" => Tier::Main,
        "router" => Tier::Router,
        _ => Tier::Light,
    };
    let (provider, _actual_tier) = registry.resolve(tier).map_err(|e| {
        tracing::warn!(%conversation_id, "标题生成：provider 解析失败: {e}");
        e.to_string()
    })?;

    // 3. 构造精简 prompt
    let system_prompt = "请用 6-10 个字概括以下用户消息，作为对话标题。只输出标题文本，不要加引号或标点符号。";
    let user_content: String = first_message.chars().take(500).collect();

    let req = CompletionRequest {
        messages: vec![
            ChatMessage {
                role: Role::System,
                content: system_prompt.to_string(),
                tool_call_id: None,
            },
            ChatMessage {
                role: Role::User,
                content: user_content,
                tool_call_id: None,
            },
        ],
        tools: Vec::new(),
        max_tokens: Some(50),
        temperature: Some(0.0),
        timeout_ms: Some(10_000),
    };

    // 4. 调 LLM
    let resp = provider.complete(req).await.map_err(|e| {
        tracing::warn!(%conversation_id, "标题生成：LLM 调用失败: {e}");
        e.to_string()
    })?;

    // 5. 清理返回文本
    let title = resp.text
        .map(|t| {
            t.trim()
                .trim_matches('"')
                .trim_matches(['「', '」', '【', '】', '\'', '『', '』'])
                .trim()
                .to_string()
        })
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().take(30).collect::<String>());

    let Some(title) = title else {
        tracing::debug!(%conversation_id, "标题生成：LLM 返回空文本");
        return Ok(());
    };

    // 6. 更新 DB
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::rename_conversation(&pools.ai, &conversation_id, &title)
        .await?;

    // 7. emit 事件到 chat 窗口
    let _ = app.emit_to(
        tauri::EventTarget::window("chat"),
        EventNames::CHAT_TITLE_UPDATED,
        serde_json::json!({ "conversation_id": conversation_id, "title": title }),
    );

    tracing::info!(%conversation_id, %title, "对话标题已自动生成");
    Ok(())
}

/// 截断对话消息——保留前 `keep_count` 条，删除其余（0.12.5 §5.5）。
///
/// 用于消息编辑重发：用户编辑第 N 条消息后，前端调用此 command 截断后续消息，
/// 然后重新调 `chat_prompt` 重新生成 assistant 回复。
#[tauri::command]
pub async fn truncate_messages(
    app: tauri::AppHandle,
    conversation_id: String,
    keep_count: i64,
) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::truncate_messages(&pools.ai, &conversation_id, keep_count)
        .await?;
    tracing::info!(%conversation_id, keep_count, "truncate_messages: 消息已截断");
    Ok(())
}

/// 列出所有对话分组（按 sort_order 升序，含 parent_id 供前端构建树）。
#[tauri::command]
pub async fn list_conversation_groups(
    app: tauri::AppHandle,
) -> Result<Vec<crate::infra::data::conversations::ConversationGroup>, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    Ok(crate::infra::data::conversations::list_groups(&pools.ai).await)
}

/// 创建对话分组。
///
/// `parent_id` 为 None 表示顶层分组。`id` 由前端 `crypto.randomUUID()` 生成。
#[tauri::command]
pub async fn create_conversation_group(
    app: tauri::AppHandle,
    id: String,
    name: String,
    parent_id: Option<String>,
) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::create_group(&pools.ai, &id, &name, parent_id.as_deref())
        .await?;
    tracing::info!(%id, %name, "对话分组已创建");
    Ok(())
}

/// 重命名对话分组。
#[tauri::command]
pub async fn rename_conversation_group(
    app: tauri::AppHandle,
    group_id: String,
    name: String,
) -> Result<bool, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::rename_group(&pools.ai, &group_id, &name).await
}

/// 删除对话分组。
///
/// 组内对话移至默认（group_id = NULL），子分组 re-parent 到被删分组的父级。
#[tauri::command]
pub async fn delete_conversation_group(
    app: tauri::AppHandle,
    group_id: String,
) -> Result<bool, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::delete_group(&pools.ai, &group_id).await
}

/// 更新分组的系统提示词。`prompt` 为 None 时清除。
#[tauri::command]
pub async fn update_conversation_group_system_prompt(
    app: tauri::AppHandle,
    group_id: String,
    prompt: Option<String>,
) -> Result<bool, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::update_group_system_prompt(
        &pools.ai,
        &group_id,
        prompt.as_deref(),
    )
    .await
}

/// 移动对话到指定分组。`group_id` 为 None 移至默认组。
#[tauri::command]
pub async fn move_conversation_to_group(
    app: tauri::AppHandle,
    conversation_id: String,
    group_id: Option<String>,
) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::set_conversation_group(
        &pools.ai,
        &conversation_id,
        group_id.as_deref(),
    )
    .await
}

/// 设置分组排序权重（拖拽排序用）。
#[tauri::command]
pub async fn set_group_sort_order(
    app: tauri::AppHandle,
    group_id: String,
    sort_order: i64,
) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::set_group_sort_order(&pools.ai, &group_id, sort_order)
        .await
}

/// 设置分组折叠状态。
#[tauri::command]
pub async fn set_group_expanded(
    app: tauri::AppHandle,
    group_id: String,
    expanded: bool,
) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::conversations::set_group_expanded(&pools.ai, &group_id, expanded).await
}

/// 清空 AI 审计日志（设置页-存储「清除 AI 调用历史」）。
#[tauri::command]
pub async fn clear_ai_audit(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().ai;
    crate::infra::data::ai_audit::clear_all(&pool).await;
    tracing::info!("AI 审计日志已清空");
    Ok(())
}

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

/// 获取当前 AI system prompt 信息（0.11.3 §3.8 token 监控）。
///
/// 构建与 AI lane 相同的 tools 列表 + system prompt，返回 token 数 / 工具数 / 预览。
/// 设置页 AI tab（高级）展示此信息，让用户感知 prompt 体积。
#[tauri::command]
pub async fn get_system_prompt_info(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    use crate::domain::ai::prompt::{build_prompt_infos, estimate_tokens, routing_system_prompt};
    use crate::domain::capability::CapabilityRegistry;
    use crate::domain::execution::group::{build_capability_tools, inject_plugin_settings};
    use crate::domain::plugin::PluginEngine;
    use std::sync::Arc;
    use tauri::Manager;

    let cap_reg = app.state::<Arc<CapabilityRegistry>>();
    let plugin_engine = app.state::<Arc<PluginEngine>>();

    // 构建 tools 列表（与 service.rs AI lane 同逻辑）
    let mut tools = build_capability_tools(&cap_reg);

    // 参数动态注入 + hints 收集
    let mut plugin_hints: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for ph in plugin_engine.all_plugins() {
        let manifest = ph.manifest();
        for td in &manifest.tools {
            let id = crate::domain::plugin::plugin_tool_id(&manifest.id, &td.name);
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

/// 获取当前上下文窗口状态（0.13.6）。
///
/// 返回上次 `compute_context_status()` 计算的缓存结果。若从未计算过则返回 null。
/// 前端在聊天窗口加载时调用此 command 获取初始状态。
#[tauri::command]
pub async fn get_context_window_status(
    app: tauri::AppHandle,
) -> Result<Option<crate::domain::ai::chat_service::ContextWindowStatus>, String> {
    use tauri::Manager;
    let chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .ok_or("ChatService 未注册")?;
    Ok(chat.last_context_status())
}

/// 强制压缩当前对话的上下文窗口（0.13.6）。
///
/// 调用 `memory.load_with_stats()` 走一遍 token_aware_truncate 流程，
/// 然后返回更新后的上下文窗口状态。
#[tauri::command]
pub async fn compress_context_now(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<crate::domain::ai::chat_service::ContextWindowStatus, String> {
    use tauri::Manager;
    let chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .ok_or("ChatService 未注册")?;
    let status = chat.compute_context_status(&conversation_id, None, None).await;
    let _ = app.emit_to("chat", EventNames::CHAT_CONTEXT_STATUS, &status);
    Ok(status)
}

/// 获取 composer bar 悬浮预览快照（一次 IPC 聚合上下文 + 内置 tool + MCP 服务）。
///
/// 供前端 composer bar hover popup 使用——避免前端发 4 个 IPC 请求拼装。
#[tauri::command]
pub async fn get_composer_bar_snapshot(
    app: tauri::AppHandle,
) -> Result<ComposerBarSnapshot, String> {
    use tauri::Manager;

    // ── 上：上下文容量（从 ChatService 缓存读）──
    let (estimated_tokens, context_limit, usage_percent, last_compressed, last_compressed_count, last_recall_count, preamble_tokens, pending_message_tokens) =
        if let Some(chat) = app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>() {
            let cs = chat.last_context_status();
            if let Some(s) = cs {
                (s.estimated_tokens, s.context_limit, s.usage_percent, s.last_compressed, s.last_compressed_count, s.last_recall_count, s.preamble_tokens, s.pending_message_tokens)
            } else {
                (0, 0, 0, false, 0, 0, 0, 0)
            }
        } else {
            (0, 0, 0, false, 0, 0, 0, 0)
        };

    // ── 中：内置工具（0.14 Capability-only）──
    let cap_registry = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();

    let builtin_tools: Vec<BuiltinToolSummary> = cap_registry
        .list()
        .into_iter()
        .map(|s| BuiltinToolSummary {
            name: s.name,
            description: s.description,
        })
        .collect();

    let builtin_count = builtin_tools.len();

    // ── 下：MCP 服务（含 online/offline + tool 列表）──
    let pools = app.state::<crate::infra::data::DbPools>();
    let configs = crate::domain::mcp::McpServerConfigStore::load_all(&pools.config)
        .await
        .map_err(|e| e.to_string())?;

    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();

    let mut mcp_servers: Vec<McpServerSummary> = Vec::new();
    let mut mcp_count = 0;

    for config in configs {
        let transport = match &config.transport {
            crate::domain::mcp::config::McpTransport::Stdio => "stdio",
            crate::domain::mcp::config::McpTransport::Sse { .. } => "sse",
            crate::domain::mcp::config::McpTransport::Http { .. } => "http",
        };

        // 用 connected map 作为 online 的真相源——statuses 可能被 test_connection
        // 的 transient 探测覆盖成 Offline，但持久连接仍在 connected map 中。
        let server_tools = manager.get_server_tools(&config.name).await;
        let online = server_tools.is_some();
        let tool_names: Vec<String> = if let Some(tools) = server_tools {
            tools.into_iter()
                .filter(|t| !t.disabled)
                .map(|t| t.name)
                .collect()
        } else {
            Vec::new()
        };
        let tool_count = tool_names.len();

        mcp_count += tool_count;
        mcp_servers.push(McpServerSummary {
            name: config.name,
            transport: transport.to_string(),
            online,
            tool_count,
            tool_names,
        });
    }

    Ok(ComposerBarSnapshot {
        estimated_tokens,
        context_limit,
        usage_percent,
        last_compressed,
        last_compressed_count,
        last_recall_count,
        preamble_tokens,
        pending_message_tokens,
        builtin_tools,
        mcp_servers,
        builtin_count,
        mcp_count,
        total_count: builtin_count + mcp_count,
    })
}


// ── 辅助函数与类型（从 commands.rs 迁移）──

/// 写用户确认执行的审计日志（0.14.4 从 confirm_ai_action 抽出共用）。
async fn write_confirm_audit(
    app: &tauri::AppHandle,
    action_name: &str,
    arguments: &serde_json::Value,
    summary: &str,
) {
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
    let audit_summary = format!("用户确认执行: {summary}");
    crate::infra::data::ai_audit::save_audit_log(
        pool,
        action_name,
        arguments,
        &audit_summary,
        &provider_kind_str,
        &model_id_str,
        0,
        "internal",
    )
    .await;
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ChatModelOption {
    pub id: String,
    pub provider_name: String,
    pub model_name: String,
    pub is_main: bool,
    pub is_light: bool,
    pub is_selected: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ChatMessageSnapshot {
    pub role: String,
    pub text: String,
    pub thinking: Option<String>,
    /// assistant 消息包含 ToolCall 时的工具名（前端渲染为工具卡片）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// 0.12.7 §6.6：工具调用参数 JSON 字符串（前端折叠展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_arguments: Option<String>,
    /// 工具执行结果摘要（从 ToolResult 消息提取，附加到前一条 ToolCall 快照）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<String>,
    /// 0.12.7 §6.4：消息创建时间戳（Unix 秒），前端据此插入时间分隔符
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
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


/// 辅助：从 ChatService 获取 skill 数量（不暴露 SkillRegistry 细节）。
pub(crate) fn cs_count_skills(
    chat: &std::sync::Arc<crate::domain::ai::chat_service::ChatService>,
) -> usize {
    chat.skill_registry().count()
}
