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
    crate::infra::platform::secret::save_secret(&crate::infra::platform::secret::SecretRef::ai(&provider_id), &secret_wrapped)
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
    match crate::infra::platform::secret::delete_secret(&crate::infra::platform::secret::SecretRef::ai(&provider_id)) {
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
    match crate::infra::platform::secret::load_secret(&crate::infra::platform::secret::SecretRef::ai(&provider_id)) {
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
    match crate::infra::platform::secret::load_secret(&crate::infra::platform::secret::SecretRef::ai(&provider_id)) {
        Ok(secret) => Ok(Some(crate::infra::platform::secret::format_hint(
            secret.expose(),
        ))),
        Err(crate::infra::platform::secret::SecretError::NotFound(_)) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// 用 CM 里已存的密钥拉取可用模型列表(0.9.4)。
///
/// 0.21.21: 模型元数据——fetch_ai_models 返回富结构。
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ModelMeta {
    /// 模型 id（如 `gpt-5-nano`）。
    pub id: String,
    /// 上下文窗口大小（从 API 响应解析，可能为 None）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

impl Ord for ModelMeta {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl PartialOrd for ModelMeta {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

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
/// **返回**:模型元数据列表（含 id + context_window）;拉取失败返回错误。
#[tauri::command]
pub async fn fetch_ai_models(
    kind: String,
    base_url: Option<String>,
    api_key: Option<String>,
    provider_id: Option<String>,
) -> Result<Vec<ModelMeta>, String> {
    use crate::app::ai_config::ProviderKind;

    // 密钥优先级:输入框 → CM
    let _cm_secret;
    let effective_key = if let Some(ref key) = api_key {
        if !key.trim().is_empty() {
            key.trim().to_string()
        } else if let Some(pid) = provider_id.as_deref() {
            match crate::infra::platform::secret::load_secret(&crate::infra::platform::secret::SecretRef::ai(pid)) {
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
        match crate::infra::platform::secret::load_secret(&crate::infra::platform::secret::SecretRef::ai(pid)) {
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

    // 0.27.4：经 net 工厂（代理策略生效），模型列表拉取 15s 总超时
    let client = crate::infra::net::proxied_builder()
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

    // 0.21.21: API 未返回 context_window 时，用内置目录预填推荐值。
    // 铁则：只做推荐预填，绝不覆盖 API 返回的值或用户已配置的值。
    if let Ok(mut models) = result {
        for m in &mut models {
            if m.context_window.is_none() {
                m.context_window = crate::domain::ai::model_catalog::lookup_context_window(&m.id);
            }
        }
        return Ok(models);
    }
    result
}

/// 获取当前 AI system prompt 信息（0.11.3 §3.8 token 监控）。
///
/// 0.21.18: 展示口径改为「聊天 preamble（含 Skill 阶段 1 摘要）+ AI 授权的
/// Capability 工具（不含 MCP，下同）」。此前渲染的是旧意图路由器的工具清单
/// （运行时调用方已随 0.17.6 AI lane 移除，0.27.2 死代码已删）。
///
/// 0.21.22.1: tools 侧优先取缓存 Agent 的真实 tool 池快照（含 MCP 工具，
/// 与预算侧同源）；无缓存时退回 0.21.18 的估算口径。
///
/// 返回 JSON（保留旧 key 兼容前端，新增分项）：
/// - `tokens`: system_tokens + tools_tokens（前端展示用总量）
/// - `system_tokens`: preamble 估算 token
/// - `tools_tokens`: 工具定义估算 token
/// - `tools_count`: 工具数量
/// - `preview`: preamble 前 200 字符
/// - `threshold`: 5000（硬编码告警阈值）
#[tauri::command]
pub async fn get_system_prompt_info(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    use crate::domain::ai::prompt::{ToolSourceSummary, chat_system_prompt_with_skills};
    use crate::domain::ai::token_budget::{estimate_text_tokens, estimate_tools_tokens};
    use tauri::Manager;

    // 1. Skill 阶段 1 摘要快照（与对话启动态一致）
    let skill_summaries = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .map(|chat| chat.skill_summaries())
        .unwrap_or_default();

    // 2+3+4. 估算口径（兜底）——0.27.7 抽出为共用函数（含 allowlist 过滤）
    let estimated_infos = estimate_tool_infos(&app).await;

    // 0.21.22.1: 展示口径与预算侧同源——优先取当前缓存 Agent 的真实 tool 池快照
    // （`AgentProvider::tool_prompt_infos`，含 MCP 工具与 allowlist 实际过滤；
    // 缓存 key 含 mcp/allowlist epoch，命中即代表池未漂移）。无缓存（尚未发过
    // 消息）或纯对话模式（池为空）时退回上方估算口径。
    let live_pool = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .and_then(|chat| chat.cached_agent_ref())
        .filter(|provider| !provider.tool_prompt_infos().is_empty())
        .map(|provider| provider.tool_prompt_infos().to_vec());
    let (prompt_infos, uses_live_pool) = match live_pool {
        Some(infos) => (infos, true),
        None => (estimated_infos, false),
    };

    // 5. preamble：无分组提示、无触发 Skill，与对话启动态一致
    let preamble =
        chat_system_prompt_with_skills(None, &skill_summaries, &[], &ToolSourceSummary::default());

    // 6. 估算
    let system_tokens = estimate_text_tokens(&preamble);
    let tools_tokens = estimate_tools_tokens(&prompt_infos);
    let tools_count = prompt_infos.len();

    // 预览：前 200 字符（设置页展示用，按 char 迭代 UTF-8 安全）
    let preview: String = preamble.chars().take(200).collect();

    tracing::debug!(
        system_tokens,
        tools_tokens,
        tools_count,
        uses_live_pool,
        "get_system_prompt_info: 聊天 preamble + 工具 token（live=真实池快照，estimate=估算兜底）"
    );

    Ok(serde_json::json!({
        "tokens": system_tokens + tools_tokens,
        "system_tokens": system_tokens,
        "tools_tokens": tools_tokens,
        "tools_count": tools_count,
        "preview": preview,
        "threshold": 5000,
    }))
}

/// 0.27.7: 估算口径工具池（无缓存 Agent 时的兜底）。
///
/// 与对话启动态 AI lane 同逻辑：build_capability_tools → 插件参数动态注入 +
/// hints 收集 → AI allowlist 过滤（policy 级「允许 AI」过滤不复刻，估算略偏
/// 保守方向）。由 `get_system_prompt_info` 与 `get_slash_command_snapshot` 共用。
async fn estimate_tool_infos(
    app: &tauri::AppHandle,
) -> Vec<crate::domain::ai::prompt::ToolPromptInfo> {
    use crate::domain::ai::prompt::build_prompt_infos;
    use crate::domain::capability::{build_capability_tools, inject_plugin_settings};
    use crate::domain::plugin::PluginEngine;
    use tauri::Manager;

    let cap_reg = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    let plugin_engine = app.state::<std::sync::Arc<PluginEngine>>();
    let pools = app.state::<crate::infra::data::DbPools>();
    let allowlist =
        crate::domain::config::ai_capability_access::AiCapabilityAccessStore::enabled_set(
            &pools.config,
        )
        .await;

    let mut tools = build_capability_tools(&cap_reg);
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
    tools.retain(|t| allowlist.contains(&t.name));
    build_prompt_infos(tools, &plugin_hints)
}

/// 获取斜杠命令动态内容快照（0.27.7，/tools /mcp /skill 数据源，phase §5.7.4/8.3）。
///
/// 只读、无副作用：工具池优先取缓存 Agent 的真实池快照（live），无缓存时用
/// allowlist 估算兜底；**不调用 prepare_enabled，不因查看主动连接 MCP**——
/// 连接状态以 manager 当前 statuses/connected map 为准。pure_chat 模式工具池
/// 为空并注明原因（/mcp 仍可看连接状态，§5.7.5）。
#[tauri::command]
pub async fn get_slash_command_snapshot(
    app: tauri::AppHandle,
) -> Result<SlashCommandSnapshot, String> {
    use tauri::Manager;

    let chat = app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>();
    let pure_chat = chat.as_ref().is_some_and(|c| c.is_pure_chat_mode());

    // ── 工具池：live 优先，estimate 兜底（与 get_system_prompt_info 同源）──
    let (tool_infos, tools_source) = if pure_chat {
        (Vec::new(), "live")
    } else if let Some(infos) = chat
        .as_ref()
        .and_then(|c| c.cached_agent_ref())
        .filter(|p| !p.tool_prompt_infos().is_empty())
        .map(|p| p.tool_prompt_infos().to_vec())
    {
        (infos, "live")
    } else {
        (estimate_tool_infos(&app).await, "estimate")
    };
    let tools_empty_reason = if tool_infos.is_empty() && pure_chat {
        Some("pure_chat".to_string())
    } else {
        None
    };

    // MCP tool 来源对照（仅在线 server 的 tool 会出现在池中）
    let manager = app.state::<std::sync::Arc<crate::domain::mcp::McpClientManager>>();
    let mcp_source_map: std::collections::HashMap<String, String> = manager
        .get_tool_sources()
        .await
        .into_iter()
        .map(|s| (s.tool_name, s.server_name))
        .collect();

    let tools: Vec<SlashToolEntry> = tool_infos
        .into_iter()
        .map(|info| {
            let source = mcp_source_map
                .get(&info.name)
                .map(|server| format!("mcp:{server}"))
                .unwrap_or_else(|| "builtin".to_string());
            SlashToolEntry {
                name: info.name,
                description: info.description,
                source,
            }
        })
        .collect();

    // ── MCP 服务状态：disabled（配置关闭）> online（connected map 为真相源）
    //   > connecting > offline（含 reason）；只读，不触发连接 ──
    let pools = app.state::<crate::infra::data::DbPools>();
    let configs = crate::domain::mcp::McpServerConfigStore::load_all(&pools.config)
        .await
        .map_err(|e| e.to_string())?;
    let statuses = manager.get_statuses().await;

    let mut mcp_servers = Vec::with_capacity(configs.len());
    for config in configs {
        let transport = match &config.transport {
            crate::domain::mcp::config::McpTransport::Stdio => "stdio",
            crate::domain::mcp::config::McpTransport::Sse { .. } => "sse",
            crate::domain::mcp::config::McpTransport::Http { .. } => "http",
        };
        // connected map 是 online 的真相源（statuses 可能被 transient 探测覆盖）
        let server_tools = manager.get_server_tools(&config.name).await;
        let online = server_tools.is_some();
        let tool_names: Vec<String> = server_tools
            .map(|tools| {
                tools
                    .into_iter()
                    .filter(|t| !t.disabled)
                    .map(|t| t.name)
                    .collect()
            })
            .unwrap_or_default();
        let status = if !config.enabled {
            "disabled"
        } else if online {
            "online"
        } else if matches!(
            statuses.get(&config.name),
            Some(crate::domain::mcp::McpServerStatus::Connecting)
        ) {
            "connecting"
        } else {
            "offline"
        };
        mcp_servers.push(SlashMcpServer {
            name: config.name,
            transport: transport.to_string(),
            status: status.to_string(),
            tool_count: tool_names.len(),
            tool_names,
        });
    }

    // ── Skill 可用性：功能开关 + 扫描数量（不笼统隐藏管理入口，§5.7.5）──
    let (skills_enabled, skill_count) = match chat.as_ref() {
        Some(c) => (c.skill_registry().is_enabled(), c.skill_registry().count()),
        None => (false, 0),
    };

    tracing::debug!(
        tools = tools.len(),
        tools_source,
        mcp = mcp_servers.len(),
        pure_chat,
        "get_slash_command_snapshot: 斜杠命令快照已聚合"
    );

    Ok(SlashCommandSnapshot {
        mode: if pure_chat { "pure_chat" } else { "full" }.to_string(),
        tools,
        tools_empty_reason,
        tools_source: tools_source.to_string(),
        mcp_servers,
        skills_enabled,
        skill_count,
    })
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
        match crate::infra::platform::secret::load_secret(&crate::infra::platform::secret::SecretRef::ai(pid)) {
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

    // 0.27.4：经 net 工厂（代理策略生效），连接测试 10s 总超时
    let client = crate::infra::net::proxied_builder()
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
/// 返回上下文窗口状态。P0-2: 按 conversation_id 精确查询；0.21.18: 缓存 miss 时
/// 按需重算（历史对话切换 / 重启后首次查看），边界见
/// `ChatService::get_or_compute_context_status`。前端在聊天窗口加载与切换对话时调用。
#[tauri::command]
pub async fn get_context_window_status(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<Option<crate::domain::ai::chat_service::ContextWindowStatus>, String> {
    use tauri::Manager;
    let chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .ok_or("ChatService 未注册")?;
    Ok(chat.get_or_compute_context_status(&conversation_id).await)
}

/// 0.27.8: 手动压缩任务 command 结果（/compact 与 80% 告警按钮共用，phase §5.8.4）。
///
/// `status` 为预检投影：任务启动后经 `CHAT_COMPACT_JOB` 事件按
/// job_id/conversation_id 推送 running/succeeded/no_op/failed/cancelled。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CompactConversationOutcome {
    /// `started` / `busy`（AI 请求进行中）/ `in_flight`（已有摘要任务）/
    /// `no_content`（空会话）/ `noop` 由预检在任务内推送（此处 started 只表示受理）。
    pub status: &'static str,
    /// 任务 id（仅 started 携带；前端与事件对账识别重复触发）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u64>,
}

/// 手动压缩当前对话（0.27.8，phase §3.8/§5.8——/compact 与告警按钮共用执行逻辑）。
///
/// 替代旧 `compress_context_now`（0.21.23 异步化版本，已删）：
/// - 不再要求先发消息建立缓存 Agent——摘要 Agent 冷构造（无工具）
/// - 不受全局 summary_enabled 约束；成功后记录会话手动摘要启用状态
/// - 手动边界保留最近两轮完整 user turn；分段输入不超摘要模型预算
/// - 提交冻结高水位（CAS），编辑/截断/删除后的陈旧结果放弃
#[tauri::command]
pub async fn compact_conversation(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<CompactConversationOutcome, String> {
    use tauri::Manager;
    let chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .ok_or("ChatService 未注册")?;

    Ok(
        match chat
            .start_manual_compact(&conversation_id)
            .await
        {
            crate::domain::ai::chat_service::ManualCompactPrecheck::Started { job_id } => {
                CompactConversationOutcome { status: "started", job_id: Some(job_id) }
            }
            crate::domain::ai::chat_service::ManualCompactPrecheck::Busy => {
                CompactConversationOutcome { status: "busy", job_id: None }
            }
            crate::domain::ai::chat_service::ManualCompactPrecheck::InFlight => {
                CompactConversationOutcome { status: "in_flight", job_id: None }
            }
            crate::domain::ai::chat_service::ManualCompactPrecheck::NoContent => {
                CompactConversationOutcome { status: "no_content", job_id: None }
            }
            crate::domain::ai::chat_service::ManualCompactPrecheck::NoOp => {
                CompactConversationOutcome { status: "noop", job_id: None }
            }
        },
    )
}

/// 0.21.19: 获取对话的摘要内容（供前端渲染折叠分隔线）。
///
/// 返回所有摘要段合并后的文本（按 idx 升序拼接）。
/// 摘要关闭或无摘要时返回 None。
#[tauri::command]
pub async fn get_context_summary(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<Option<String>, String> {
    use tauri::Manager;
    let pool = &app.state::<crate::infra::data::DbPools>().ai;
    let summaries = crate::infra::data::conversations::load_summaries(pool, &conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    if summaries.is_empty() {
        return Ok(None);
    }
    let text = summaries
        .iter()
        .map(|s| s.content.as_str())
        .collect::<Vec<_>>()
        .join("\n---\n");
    Ok(Some(text))
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
/// P0-2: 按 conversation_id 精确查询上下文状态。
#[tauri::command]
pub async fn get_composer_bar_snapshot(
    app: tauri::AppHandle,
    conversation_id: String,
) -> Result<ComposerBarSnapshot, String> {
    use tauri::Manager;

    // ── 上：上下文容量（从 ChatService 缓存读）──
    let pure_chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .is_some_and(|chat| chat.is_pure_chat_mode());
    let context_status = if let Some(chat) =
        app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
    {
        // 0.21.18: 缓存 miss 时按需重算，历史对话 hover 也能显示容量
        chat.get_or_compute_context_status(&conversation_id).await
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

    // 无 ChatService / 状态未计算时用 Default 兜底（全零 + 空串，与旧 unwrap_or 行为一致）
    let cs = context_status.unwrap_or_default();

    // 0.21.23: 记忆健康度一览（压缩策略 / 摘要段数 / 最近一次摘要）——
    // 收口「知道有压缩 → 打开摘要 → 知道何时生效」链路的发现性问题
    let memory = if let Some(chat) =
        app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
    {
        chat.memory_health_summary(&conversation_id).await
    } else {
        Default::default()
    };

    Ok(ComposerBarSnapshot {
        estimated_tokens: cs.estimated_tokens,
        context_limit: cs.context_limit,
        usage_percent: cs.usage_percent,
        last_compressed: cs.last_compressed,
        last_compressed_count: cs.last_compressed_count,
        last_recall_count: cs.last_recall_count,
        preamble_tokens: cs.preamble_tokens,
        pending_message_tokens: cs.pending_message_tokens,
        // 0.21.17 扩展字段
        history_tokens: cs.history_tokens,
        tools_tokens: cs.tools_tokens,
        protocol_overhead_tokens: cs.protocol_overhead_tokens,
        multimodal_tokens: cs.multimodal_tokens,
        reserved_output_tokens: cs.reserved_output_tokens,
        safety_margin_tokens: cs.safety_margin_tokens,
        effective_input_limit: cs.effective_input_limit,
        remaining_tokens: cs.remaining_tokens,
        context_limit_source: cs.context_limit_source,
        confidence: cs.confidence,
        memory,
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
///
/// 0.21.21: 改走 `/api/show` POST 获取 context_length（`/api/tags` 无 context 字段）
async fn fetch_ollama_models(
    client: &reqwest::Client,
    url: &str,
) -> Result<Vec<ModelMeta>, String> {
    // Step 1: 获取模型列表
    let model_names = match client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| {
                v.get("models").and_then(|m| m.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default(),
        Ok(resp) => {
            let status = resp.status().as_u16();
            return Err(format!(
                "ollama 连接失败(HTTP {status}),确认 ollama serve 已启动"
            ));
        }
        Err(e) => {
            return Err(format!(
                "ollama 连接失败: {e},确认 ollama serve 已启动且地址正确"
            ));
        }
    };

    if model_names.is_empty() {
        return Err("ollama 返回空列表(可能未拉取模型,尝试 ollama pull llama3)".to_string());
    }

    // Step 2: 对每个模型走 /api/show 获取 context_length
    // 0.21.22.1: 并发拉取（本地 ollama，并发安全）——串行时 20 个模型约 1s+，
    // 设置页拉列表明显卡顿。失败静默降级 None，与原串行语义一致。
    let base_url = url.trim_end_matches("/api/tags");
    let show_url = format!("{base_url}/api/show");

    let fetched = futures::future::join_all(model_names.iter().map(|name| {
        let show_url = show_url.clone();
        async move {
            let context_window = fetch_ollama_context_length(client, &show_url, name)
                .await
                .unwrap_or(None);
            ModelMeta {
                id: name.clone(),
                context_window,
            }
        }
    }))
    .await;

    let mut models = Vec::with_capacity(fetched.len());
    models.extend(fetched);

    models.sort();
    Ok(models)
}

/// 0.21.21: 从 ollama /api/show 获取模型 context_length。
///
/// POST body: `{"model": "<id>"}`
/// Response `model_info` 中键形如 `*.context_length`，取 value。
async fn fetch_ollama_context_length(
    client: &reqwest::Client,
    show_url: &str,
    model_name: &str,
) -> Result<Option<u32>, String> {
    let resp = client
        .post(show_url)
        .header("Accept", "application/json")
        .json(&serde_json::json!({"model": model_name}))
        .send()
        .await
        .map_err(|e| format!("ollama /api/show 请求失败: {e}"))?;

    if !resp.status().is_success() {
        return Ok(None); // 静默失败
    }

    let body = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("ollama /api/show 响应解析失败: {e}"))?;

    // model_info 里的键形如 llama.context_length / general.context_length
    let model_info = body.get("model_info").or_else(|| body.get("parameters"));
    if let Some(info) = model_info.and_then(|v| v.as_object()) {
        for (key, value) in info {
            if key.ends_with(".context_length") || key.ends_with("context_length") {
                if let Some(n) = value.as_u64() {
                    return Ok(Some(n as u32));
                }
                // 有时是字符串
                if let Some(s) = value.as_str()
                    && let Ok(n) = s.parse::<u32>()
                {
                    return Ok(Some(n));
                }
            }
        }
    }

    Ok(None)
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
) -> Result<Vec<ModelMeta>, String> {
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
                                m.get("id").and_then(|id| id.as_str()).map(|id| {
                                    // 0.21.21: Anthropic context_window 字段
                                    let context_window = m
                                        .get("context_window")
                                        .and_then(|v| v.as_u64())
                                        .map(|v| v as u32);
                                    ModelMeta {
                                        id: id.to_string(),
                                        context_window,
                                    }
                                })
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
/// 0.27.10：内置模型目录查询——新建模型时的能力/窗口推荐预填。
/// 只用于**新建**条目预填；已有配置不被目录刷新改写（用户值优先，phase §3.4）。
#[tauri::command]
pub fn model_catalog_lookup(model_id: String) -> serde_json::Value {
    let normalized = model_id.rsplit('/').next().unwrap_or(&model_id).to_string();
    serde_json::json!({
        "context_window": crate::domain::ai::model_catalog::lookup_context_window(&normalized),
        "supports_vision": crate::domain::ai::model_catalog::lookup_supports_vision(&normalized),
    })
}

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
