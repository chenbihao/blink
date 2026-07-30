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
///
/// **0.14.7 W3**：返回 `CommandError`（结构化错误协议）。
#[tauri::command]
pub async fn confirm_ai_action(
    app: tauri::AppHandle,
    action_name: String,
    arguments: serde_json::Value,
) -> Result<(), crate::app::command_error::CommandError> {
    tracing::debug!(%action_name, ?arguments, "confirm_ai_action: 用户确认 AI 动作");

    // 0.14.6 §2.2：从 state 获取 DomainEnv 桥接器
    let env_arc = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();

    // 0.14 Capability-only：确认卡片只允许执行 Capability。
    // command 名保留以兼容现有前端 IPC，但不再提供 ActionRegistry fallback。
    let search_service = app.state::<std::sync::Arc<crate::domain::search::SearchService>>();
    let seq = search_service
        .take_ai_confirmation(&action_name, &arguments)
        .ok_or_else(|| crate::app::command_error::CommandError::new(
            "not_found",
            &format!("没有匹配的待确认 Capability: {action_name}"),
            false,
        ))?;

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
                return Err(crate::app::command_error::CommandError::from(e));
            }
        }
        return Ok(());
    }

    tracing::warn!(%action_name, "confirm_ai_action: 未知 id");
    Err(crate::app::command_error::CommandError::new(
        "not_found",
        &format!("未知 Capability id: {action_name}"),
        false,
    ))
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
        crate::infra::data::conversations::get_effective_system_prompt(&pools.ai, &conversation_id)
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

mod conversations;
mod management;
pub use conversations::*;
pub use management::*;
