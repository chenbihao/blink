//! ai 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use super::shared::{BuiltinToolSummary, ComposerBarSnapshot, McpServerSummary};
use crate::domain::event_names::EventNames;
use tauri::{Emitter, Manager};

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

// 0.17.6: trigger_ai / confirm_ai_action 命令已删除。
// 主窗口 AI 改走 ChatService（chat_prompt + confirm_chat_action），
// 旧的 SearchService AI 路径（PendingAiConfirmation / emit_ai_*）已整体移除。

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
/// 包装成 `ChatStreamEvent` 后定向 emit 到目标窗口（`blink://chat-stream`）。
///
/// 返回 `request_id`，前端据此过滤已中止请求的尾部 chunk。
/// 若已有 active request，返回错误。
///
/// 0.12.6：`group_id` 参数注入分组级系统提示词。
/// 0.17.6：`target_window`（默认 "chat"）+ `ephemeral`（默认 false）参数。
/// 主窗口 AI 传 `target_window="main"` + `ephemeral=true`，使用临时对话记忆。
#[tauri::command]
pub async fn chat_prompt(
    app: tauri::AppHandle,
    conversation_id: String,
    message: String,
    group_id: Option<String>,
    target_window: Option<String>,
    ephemeral: Option<bool>,
) -> Result<u64, String> {
    let chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .ok_or("ChatService 未注册")?;

    let target = target_window.unwrap_or_else(|| "chat".to_string());
    let kind = if ephemeral.unwrap_or(false) {
        crate::domain::ai::chat_service::ConversationKind::Ephemeral
    } else {
        crate::domain::ai::chat_service::ConversationKind::Persistent
    };

    // 0.17.6: 主窗口 AI 激活时设 watchdog 标志，防止失焦隐藏
    if target == "main" {
        crate::infra::platform::window::set_main_ai_active(true);
    }

    // 0.12.8: 查询系统提示词 + 调 prompt() 在前，持久化分组在后
    let pools = app.state::<crate::infra::data::DbPools>();

    let group_system_prompt = if let Some(ref gid) = group_id {
        crate::infra::data::conversations::get_group_system_prompt(&pools.ai, Some(gid))
            .await
            .unwrap_or(None)
    } else {
        crate::infra::data::conversations::get_effective_system_prompt(&pools.ai, &conversation_id)
            .await
            .unwrap_or(None)
    };

    let handle = chat
        .prompt(
            conversation_id.clone(),
            message,
            group_system_prompt,
            kind,
            target.clone(),
        )
        .await
        .map_err(|e| match e {
            crate::domain::ai::chat_service::ChatError::AlreadyActive(active) => {
                let win = active.target_window.unwrap_or_else(|| "chat".to_string());
                format!("AlreadyActive:{}", win)
            }
            other => other.to_string(),
        })?;

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

    // spawn 后台 task 消费 chunk 流并定向 emit 到目标窗口
    let app_clone = app.clone();
    let conv_id_clone = conv_id.clone();
    let target_win = handle.target_window.clone();
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
                tauri::EventTarget::window(&target_win),
                EventNames::CHAT_STREAM,
                &event,
            );
            if is_done {
                break;
            }
        }
        // 0.18.0: 不在 Done 瞬间清零 MAIN_WINDOW_AI_ACTIVE——用户可能还在看结果，
        // 看门狗会立即恢复失焦隐藏导致关窗。改为：
        // (1) exitAiMode 路径清零（前端 ESC / 切回搜索时调 clear_main_ai_active）
        // (2) 兜底定时器：Done 后 30s 清零，防标志长期滞留
        // 两者取先到者。hide_window 命令（ESC 隐藏窗口）也会清零。
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
                tauri::EventTarget::window(&target_win),
                EventNames::CHAT_STREAM,
                &event,
            );
        }
        // 0.18.0: 兜底定时器——Done 后延迟 30s 清零，防止用户既不退出 AI 模式也不关窗时标志长期滞留
        if target_win == "main" {
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                crate::infra::platform::window::set_main_ai_active(false);
                tracing::debug!("AI 标志兜底定时器清零（Done 后 30s）");
            });
        }
    });

    tracing::debug!(
        request_id,
        conversation_id = %conv_id,
        "chat_prompt: 后台 stream task 已启动"
    );
    Ok(request_id)
}

/// 0.18.0: 清除主窗口 AI 活跃标志（前端 exitAiMode 时调用）。
///
/// 配合 ai.rs stream task 的兜底定时器：
/// - exitAiMode 时前端调此命令立即清零（用户主动退出 AI 模式）
/// - 兜底定时器在 Done 后 30s 自动清零（用户既不退出也不关窗时）
/// 两者取先到者。hide_window 命令也会清零。
#[tauri::command]
pub fn clear_main_ai_active() {
    crate::infra::platform::window::set_main_ai_active(false);
    tracing::debug!("clear_main_ai_active: AI 标志已清零（exitAiMode 路径）");
}

/// 中止指定的对话请求（Phase 4）。
///
/// 返回 `true` = 已中止；`false` = request_id 不存在（已完成或已中止）。
#[tauri::command]
pub fn chat_abort(app: tauri::AppHandle, request_id: u64) -> bool {
    if let Some(chat) =
        app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
    {
        let aborted = chat.abort(request_id);
        // 0.17.6: abort 后清除 watchdog AI 标志（单活跃请求，abort 即意味着主窗口 AI 结束）
        if aborted {
            crate::infra::platform::window::set_main_ai_active(false);
        }
        aborted
    } else {
        false
    }
}

/// 0.17.6a: 将主窗口临时对话提升为持久对话。
///
/// 流程：
/// 1. abort 当前请求（如有活跃）
/// 2. 从 `EphemeralConversationMemory` 导出当前对话全部消息
/// 3. 写入 `SqliteConversationMemory`（同一 conversation_id，INSERT OR IGNORE + 逐条 append）
/// 4. 清空 `EphemeralConversationMemory` 的该 conversation
/// 5. 打开对话窗口，加载该 conversation_id
///
/// 主窗口前端调用后自行 exitAiMode → SearchMode。
#[tauri::command]
pub async fn promote_ephemeral_conversation(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<(), String> {
    let chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .ok_or("ChatService 未注册")?;

    // 1. abort active request
    chat.abort_active();

    // 2. export ephemeral messages
    let messages = chat.export_ephemeral_messages(&conversation_id).await;

    if messages.is_empty() {
        tracing::warn!(%conversation_id, "promote_ephemeral_conversation: 临时对话无消息");
        return Err("临时对话无消息，无法提升".to_string());
    }

    // 3. write to persistent memory (SqliteConversationMemory)
    //    ConversationMemory::append 内部自动 create_conversation (INSERT OR IGNORE) + 逐条 append
    let persistent = chat.persistent_memory().clone();
    use rig_core::memory::ConversationMemory;
    persistent
        .append(&conversation_id, messages)
        .await
        .map_err(|e| format!("写入持久对话失败: {e}"))?;

    tracing::info!(
        %conversation_id,
        "promote_ephemeral_conversation: 临时对话已提升为持久对话"
    );

    // 4. clear ephemeral memory
    chat.remove_ephemeral_conversation(&conversation_id).await;

    // 5. open chat window with the conversation
    crate::infra::platform::window::show_chat_window(&app, None)
        .map_err(|e| format!("打开对话窗口失败: {e}"))?;

    // 6. emit chat-load-conversation event so chat window switches to this conversation
    use tauri::Emitter;
    let _ = app.emit_to(
        "chat",
        crate::domain::event_names::EventNames::CHAT_LOAD_CONVERSATION,
        &conversation_id,
    );

    Ok(())
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
    let main_pair = config
        .resolve_tier(Tier::Main)
        .map(|(p, m, _)| (p.id.clone(), m.id.clone()));
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
        // 0.16.0: 跳过禁用的 provider——不进入模型选择列表
        if !provider.enabled {
            continue;
        }
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

// ── 0.17.9: Ephemeral（主窗口 AI）独立模型选择 ──────────────────────────────

/// 列出主窗口 AI 可选的所有 Chat 能力模型（0.17.9）。
///
/// 与 `get_chat_models` 相同的模型列表，但标注 `is_selected` 基于 `ephemeral_selected`
/// （而非 Persistent 的 `selected`）。回落逻辑：ephemeral_selected 为 None 时标注 Light 档。
#[tauri::command]
pub fn get_ephemeral_models(app: tauri::AppHandle) -> Vec<ChatModelOption> {
    use crate::app::ai_config::{ModelCapability, Tier};

    let Some(registry) =
        app.try_state::<std::sync::Arc<crate::domain::ai::registry::AIProviderRegistry>>()
    else {
        return Vec::new();
    };
    let config = registry.config_snapshot();

    let main_pair = config
        .resolve_tier(Tier::Main)
        .map(|(p, m, _)| (p.id.clone(), m.id.clone()));
    let light_pair = config
        .resolve_tier(Tier::Light)
        .map(|(p, m, _)| (p.id.clone(), m.id.clone()));

    // 0.17.9: Ephemeral 的 selected——None 时回落 Light 档（而非 Main）
    let selected_pair = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .and_then(|chat| chat.current_ephemeral_selection())
        .map(|sel| (sel.provider_id, sel.model_id))
        .or_else(|| light_pair.clone());

    let mut options = Vec::new();
    for provider in &config.providers {
        if !provider.enabled {
            continue;
        }
        for model in &provider.models {
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

/// 设置主窗口 AI（Ephemeral 对话）的运行时选中模型（0.17.9）。
///
/// - `selection_id = None` 或空字符串：恢复 Light 档默认（Light 空则降级 Main）。
/// - `selection_id = Some("{provider_id}:{model_id}")`：切换到指定模型。
///
/// 返回 `true` = 切换成功；`false` = id 格式错误或 model 不存在/无 Chat 能力。
#[tauri::command]
pub async fn select_ephemeral_model(
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

    // None / 空字符串 = 恢复 Light 档
    let selection_id = match selection_id {
        None => {
            chat.select_ephemeral_model(None);
            return Ok(true);
        }
        Some(s) if s.trim().is_empty() => {
            chat.select_ephemeral_model(None);
            return Ok(true);
        }
        Some(s) => s,
    };

    let Some((provider_id, model_id)) = selection_id.split_once(':') else {
        return Ok(false);
    };
    if provider_id.is_empty() || model_id.is_empty() {
        return Ok(false);
    }

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
    chat.select_ephemeral_model(Some(selection));
    Ok(true)
}

// ── 辅助函数与类型（从 commands.rs 迁移）──

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

mod conversations;
mod management;
pub use conversations::*;
pub use management::*;
