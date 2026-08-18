//! AI 对话、消息与分组 commands。

use super::*;

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
///
/// 0.17.8：同时清除该对话的会话级 trusted 记录（不影响持久化权限记忆）。
#[tauri::command]
pub async fn delete_chat_conversation(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<bool, String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    let result =
        crate::infra::data::conversations::delete_conversation(&pools.ai, &conversation_id).await;

    // 0.17.8: 清除该对话的会话级 trusted（不影响持久化 DB 记忆）
    if let Ok(true) = &result {
        use tauri::Manager;
        if let Some(pc) =
            app.try_state::<std::sync::Arc<crate::domain::ai::tool_adapter::PendingConfirms>>()
        {
            pc.clear_trust(&conversation_id).await;
        }
    }

    result
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
    use rig_core::completion::Message;
    use rig_core::completion::message::{AssistantContent, UserContent};

    let pools = app.state::<crate::infra::data::DbPools>();
    let rows =
        crate::infra::data::conversations::load_all_messages(&pools.ai, &conversation_id).await?;

    let mut snapshots: Vec<ChatMessageSnapshot> = Vec::with_capacity(rows.len());
    for (role, content_json, created_at) in rows {
        let msg: Message =
            serde_json::from_str(&content_json).map_err(|e| format!("反序列化消息失败: {e}"))?;

        match &msg {
            Message::User { content } => {
                // 检测 ToolResult 消息（rig 存为 User + ToolResult）
                let tool_result_text = content.iter().find_map(|c| match c {
                    UserContent::ToolResult(tr) => {
                        // 0.14.1: 复用 summarize_tool_result（含截断 + 图片占位）
                        let summary = crate::domain::ai::agent_provider::summarize_tool_result(tr);
                        if summary.is_empty() {
                            None
                        } else {
                            Some(summary)
                        }
                    }
                    _ => None,
                });

                if let Some(summary) = tool_result_text {
                    // summarize_tool_result 已处理截断（50000 字符 + 省略号）
                    // 附加到前一条 ToolCall 快照
                    if let Some(last_tool) = snapshots
                        .iter_mut()
                        .rev()
                        .find(|s| s.tool_name.is_some() && s.tool_result.is_none())
                    {
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
                    thinking: if thinking.is_empty() {
                        None
                    } else {
                        Some(thinking)
                    },
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
/// 读取 `ChatConfig.auto_title` 开关，按 `ChatConfig.title_model` 策略选模型
/// （默认超轻档），调 `AIProvider::complete()`（非 Agent 路径，单轮补全）生成
/// 6-10 字语义化标题。
///
/// 成功后更新 `conversations.title` 并 emit `blink://chat-title-updated` 事件，
/// 前端更新 header 标题 + 刷新侧边栏。失败静默降级（保持截断标题）。
#[tauri::command]
pub async fn generate_conversation_title(
    app: tauri::AppHandle,
    conversation_id: String,
    first_message: String,
) -> Result<(), String> {
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

    // 2. 按"对话窗口命名模型"策略选模型；默认超轻档，缺档时按档位链降级，
    //    自选模型不可用时同样回退超轻档。
    let provider = resolve_title_model(&registry, &chat_cfg.title_model).map_err(|e| {
        tracing::warn!(%conversation_id, "标题生成：provider 解析失败: {e}");
        e.to_string()
    })?;

    // 3. 构造精简 prompt
    let system_prompt =
        "请用 6-10 个字概括以下用户消息，作为对话标题。只输出标题文本，不要加引号或标点符号。";
    let user_content: String = first_message.chars().take(500).collect();

    let req = CompletionRequest {
        messages: vec![
            ChatMessage {
                role: Role::System,
                content: system_prompt.to_string(),
                tool_call_id: None,
                tool_name: None,
            },
            ChatMessage {
                role: Role::User,
                content: user_content,
                tool_call_id: None,
                tool_name: None,
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
    let title = resp
        .text
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

/// 按"对话窗口命名模型"策略解析 ready provider（LLM 自动命名用）。
///
/// - `ultra_light`（默认）/ `light` / `main` → 对应档位；空档由 `resolve_tier`
///   按"超轻 → 轻量 → 主"降级链自动向更高档降级
/// - `provider_id:model_id`（自选）→ 显式解析；不可用时回退超轻档
/// - 其余值 → 回退超轻档
fn resolve_title_model(
    registry: &crate::domain::ai::registry::AIProviderRegistry,
    policy: &str,
) -> Result<
    std::sync::Arc<dyn crate::domain::ai::provider::AIProvider>,
    crate::domain::ai::provider::AIError,
> {
    use crate::app::ai_config::Tier;

    // 自选模型：显式解析，失败回退超轻档
    if let Some((pid, mid)) = policy.split_once(':') {
        if !pid.is_empty() && !mid.is_empty() {
            return match registry.resolve_explicit(pid, mid) {
                Ok(p) => Ok(p),
                Err(crate::domain::ai::provider::AIError::NotConfigured) => {
                    tracing::warn!(
                        provider_id = pid,
                        model_id = mid,
                        "标题生成：自选命名模型不可用，回退超轻档"
                    );
                    registry.resolve(Tier::UltraLight).map(|(p, _)| p)
                }
                Err(other) => Err(other),
            };
        }
    }
    // 档位策略：默认超轻档
    let tier = match policy {
        "main" => Tier::Main,
        "light" => Tier::Light,
        _ => Tier::UltraLight,
    };
    registry.resolve(tier).map(|(p, _)| p)
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
    crate::infra::data::conversations::set_group_sort_order(&pools.ai, &group_id, sort_order).await
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
