//! AI Provider、密钥、上下文与审计管理 commands。

use super::*;

/// 清空 AI 审计日志（设置页-存储「清除 AI 调用历史」）。
#[tauri::command]
pub async fn clear_ai_audit(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().ai;
    crate::infra::data::ai_audit::clear_all(pool).await;
    let _ = crate::infra::data::compact(pool).await; // VACUUM + WAL checkpoint 回收空间
    tracing::info!("AI 审计日志已清空");
    Ok(())
}

/// 清空全部对话历史（设置页-存储「清空对话」）。
///
/// 删除 conversations + messages + memory_fts，保留 conversation_groups（分组结构）。
/// 审计日志（ai_tool_audit）不受影响。
#[tauri::command]
pub async fn clear_all_conversations(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().ai;
    crate::infra::data::conversations::clear_all_conversations(pool).await?;
    let _ = crate::infra::data::compact(pool).await;
    tracing::info!("全部对话历史已清空（分组保留）");
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
    use crate::domain::capability::{build_capability_tools, inject_plugin_settings};
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
            if let Some(bindings) = &td.setting_bindings
                && let Some(pos) = tools.iter().position(|s| s.name == id)
            {
                let settings = plugin_engine.get_settings(&manifest.id);
                tools[pos] =
                    inject_plugin_settings(tools[pos].clone(), settings.as_ref(), bindings);
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
        "threshold": 5000,
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

    match kind {
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
    }
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
    Ok(chat.last_context_status_any())
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

    // 0.21.17: compute_context_status 需要 AgentProvider + ResolvedProviderEntries。
    // 从缓存获取 provider，从 resolve_current_entries 获取 resolved。
    let resolved = chat
        .resolve_current_entries(crate::domain::ai::chat_service::ConversationKind::Persistent)
        .map_err(|e| e.to_string())?;

    // 从缓存获取 AgentProvider（若缓存未命中则返回错误，提示用户先发一条消息）
    let provider = chat
        .cached_agent_ref()
        .ok_or("Agent 未构造，请先发送一条消息后再压缩")?;

    let status = chat
        .compute_context_status(&conversation_id, None, None, &provider, &resolved)
        .await;
    let _ = app.emit_to("chat", EventNames::CHAT_CONTEXT_STATUS, &status);
    Ok(status)
}

/// 清空所有 AI 权限记忆（设置页-AI对话能力「清除所有权限记忆」按钮，0.17.8）。
///
/// 只清持久化 DB 层（`ai_permission_memory` 表），不影响会话级 `HashSet`。
/// 用户主动撤销全部跨会话权限记忆。
#[tauri::command]
pub async fn clear_all_permission_memory(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    let pc = app.state::<std::sync::Arc<crate::domain::ai::tool_adapter::PendingConfirms>>();
    pc.clear_all_trusted_db().await;
    Ok(())
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
    let pure_chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .is_some_and(|chat| chat.is_pure_chat_mode());
    let context_status = if let Some(chat) =
        app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
    {
        chat.last_context_status_any()
    } else {
        None
    };

    // ── 中：内置工具（0.14 Capability-only）──
    let cap_registry = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();

    let builtin_tools: Vec<BuiltinToolSummary> = if pure_chat {
        Vec::new()
    } else {
        cap_registry
            .list()
            .into_iter()
            .map(|s| BuiltinToolSummary {
                name: s.name,
                description: s.description,
            })
            .collect()
    };

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
        let tool_names: Vec<String> = if pure_chat {
            Vec::new()
        } else if let Some(tools) = server_tools {
            tools
                .into_iter()
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
        estimated_tokens: context_status
            .as_ref()
            .map(|s| s.estimated_tokens)
            .unwrap_or(0),
        context_limit: context_status
            .as_ref()
            .map(|s| s.context_limit)
            .unwrap_or(0),
        usage_percent: context_status
            .as_ref()
            .map(|s| s.usage_percent)
            .unwrap_or(0),
        last_compressed: context_status
            .as_ref()
            .map(|s| s.last_compressed)
            .unwrap_or(false),
        last_compressed_count: context_status
            .as_ref()
            .map(|s| s.last_compressed_count)
            .unwrap_or(0),
        last_recall_count: context_status
            .as_ref()
            .map(|s| s.last_recall_count)
            .unwrap_or(0),
        preamble_tokens: context_status
            .as_ref()
            .map(|s| s.preamble_tokens)
            .unwrap_or(0),
        pending_message_tokens: context_status
            .as_ref()
            .map(|s| s.pending_message_tokens)
            .unwrap_or(0),
        // 0.21.17 扩展字段
        history_tokens: context_status
            .as_ref()
            .map(|s| s.history_tokens)
            .unwrap_or(0),
        tools_tokens: context_status.as_ref().map(|s| s.tools_tokens).unwrap_or(0),
        protocol_overhead_tokens: context_status
            .as_ref()
            .map(|s| s.protocol_overhead_tokens)
            .unwrap_or(0),
        multimodal_tokens: context_status
            .as_ref()
            .map(|s| s.multimodal_tokens)
            .unwrap_or(0),
        reserved_output_tokens: context_status
            .as_ref()
            .map(|s| s.reserved_output_tokens)
            .unwrap_or(0),
        safety_margin_tokens: context_status
            .as_ref()
            .map(|s| s.safety_margin_tokens)
            .unwrap_or(0),
        effective_input_limit: context_status
            .as_ref()
            .map(|s| s.effective_input_limit)
            .unwrap_or(0),
        remaining_tokens: context_status
            .as_ref()
            .map(|s| s.remaining_tokens)
            .unwrap_or(0),
        context_limit_source: context_status
            .as_ref()
            .map(|s| s.context_limit_source.clone())
            .unwrap_or_default(),
        confidence: context_status
            .as_ref()
            .map(|s| s.confidence.clone())
            .unwrap_or_default(),
        builtin_tools,
        mcp_servers,
        builtin_count,
        mcp_count,
        total_count: builtin_count + mcp_count,
    })
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

// ── 0.21.5: AI Capability Access IPC commands ───────────────────────────────

/// 获取 AI Capability 出口授权配置。
///
/// 返回 `AiCapabilityAccessConfig`（含 schema_version、profile、enabled_capabilities）。
#[tauri::command]
pub async fn get_ai_capability_access(
    app: tauri::AppHandle,
) -> Result<crate::domain::config::shards::AiCapabilityAccessConfig, String> {
    use tauri::Manager;
    let pools = app.state::<crate::infra::data::DbPools>();
    Ok(
        crate::domain::config::ai_capability_access::AiCapabilityAccessStore::load(&pools.config)
            .await,
    )
}

/// 设置单个 Capability 的 AI 启用状态。
///
/// 更新后广播 `blink://config-changed`，前端和 ChatService 据此刷新。
#[tauri::command]
pub async fn toggle_ai_capability(
    app: tauri::AppHandle,
    capability_id: String,
    enabled: bool,
) -> Result<crate::domain::config::shards::AiCapabilityAccessConfig, String> {
    use tauri::Manager;
    let pools = app.state::<crate::infra::data::DbPools>();
    let config =
        crate::domain::config::ai_capability_access::AiCapabilityAccessStore::toggle_capability(
            &pools.config,
            &capability_id,
            enabled,
        )
        .await?;

    // 广播配置变更（前端 + ChatService 订阅）
    broadcast_config_changed(&app);

    Ok(config)
}

/// 批量设置 Capability 的 AI 启用状态。
///
/// 用于设置页组级批量操作。更新后广播 `blink://config-changed`。
#[tauri::command]
pub async fn toggle_ai_capabilities(
    app: tauri::AppHandle,
    ops: Vec<(String, bool)>,
) -> Result<crate::domain::config::shards::AiCapabilityAccessConfig, String> {
    use tauri::Manager;
    let pools = app.state::<crate::infra::data::DbPools>();
    let config =
        crate::domain::config::ai_capability_access::AiCapabilityAccessStore::toggle_capabilities(
            &pools.config,
            &ops,
        )
        .await?;

    broadcast_config_changed(&app);

    Ok(config)
}

/// 重置 AI Capability allowlist 为推荐集合。
///
/// 重新生成推荐集合并覆盖当前配置。`profile` 保持为 `"recommended"`。
#[tauri::command]
pub async fn reset_ai_capability_access(
    app: tauri::AppHandle,
) -> Result<crate::domain::config::shards::AiCapabilityAccessConfig, String> {
    use tauri::Manager;
    let pools = app.state::<crate::infra::data::DbPools>();
    let cap_registry = app
        .state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>()
        .inner()
        .clone();
    let config =
        crate::domain::config::ai_capability_access::AiCapabilityAccessStore::reset_to_recommended(
            &pools.config,
            cap_registry.as_ref(),
        )
        .await?;

    broadcast_config_changed(&app);

    Ok(config)
}

/// 广播 `blink://config-changed` 事件。
///
/// ChatService 和前端设置页订阅此事件，触发 Agent 缓存失效和 UI 刷新。
fn broadcast_config_changed(app: &tauri::AppHandle) {
    use tauri::Emitter;
    if let Err(e) = app.emit(
        crate::domain::event_names::EventNames::CONFIG_CHANGED,
        serde_json::json!({}),
    ) {
        tracing::warn!(error = %e, "broadcast config-changed failed");
    }
}
