//! AI 配置分片（0.9.1 Phase 3）——第 7 个 KV 分片，key = `"app.ai"`。
//!
//! **架构对齐**（详见 phases/0.9-ai-layer.md §6）：
//! - 走 `impl ConfigKey` 融入现有分片体系（[[config-store-facade-pattern]]）
//! - **独立分片，不进 `AppConfig` 门面**——AI 是 opt-in 能力,不该跟主链路配置耦合
//! - 迁移无缝：老用户首次读拿到 `AIConfig::default()`,`enabled = false`,零副作用
//! - **secret_ref 不存 raw Key**——只存 CM 别名(如 `"blink/{uuid}/key"`),
//!   `provider_id` 作为 UUID 生成一次不变,`ProviderEntry` 序列化 IPC/SQLite 都安全
//!
//! **五条铁则**(§5.1 密钥安全):
//! 1. SQLite 只存 secret_ref,不存 raw Key
//! 2. 编辑 Key = 清空重填(前端强制)
//! 3. 删除 Provider = 立即 `CredDeleteW`(app 命令层调 secret::delete_secret)
//! 4. tracing/log/Debug 三通路不出现原文(SecretString 已守)
//! 5. serde 序列化 Provider 类型不带 secret 字段(**本文件类型都不含 SecretString**)
//!
//! **§3.6 未命中过滤四筛子字段**(默认关):`enabled` / `min_query_len` /
//! `require_whitespace` / `exclude_pure_numeric` / `respect_awareness_url_path`。
//! 0.9.1 只落配置项;真决策树留 0.9.2。
//!
//! **§5.3 总开关与首次配置耦合**:严格 opt-in——配完 Provider 不自动 flip
//! `enabled=true`,由前端 toast 引导。删除唯一 Provider 时自动置 false(应用层处理)。

use serde::{Deserialize, Serialize};

use super::config::ConfigKey;

/// AI 配置分片——第 7 个 KV。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AIConfig {
    // ── 总开关(§3.6 铁则 1:默认关) ─────────────────────────────────────────
    /// 主开关——**默认 false**。配完 Provider 不自动 flip(§5.3 严格 opt-in),
    /// 只能由用户显式打开。
    #[serde(default)]
    pub enabled: bool,

    /// 意图路由开关——即使 `enabled=true`,还要再打开此项才允许 AI 决策路由。
    /// 分两层是为了让"启用 AI 供应商但先只跑手动触发"这个中间态可用。
    #[serde(default)]
    pub allow_intent_routing: bool,

    // ── 未命中过滤四筛子(§3.6,阈值 0.9.2 spike 定) ─────────────────────────
    /// 最短 query 长度。`< min_query_len` 不走 AI,回退 fuzzy。
    #[serde(default = "default_min_query_len")]
    pub min_query_len: u8,

    /// CJK(中日韩)最短 query 长度。中文不需要空格分词,"翻译"=2 char 是完整意图。
    /// `< min_query_len_cjk` 且含 CJK 字符时不走 AI。默认 2。
    #[serde(default = "default_min_query_len_cjk")]
    pub min_query_len_cjk: u8,

    /// 必须包含至少一个空格(避免"打错一个字"就打 LLM)。
    #[serde(default = "default_true")]
    pub require_whitespace: bool,

    /// 排除纯数字/纯符号 query。
    #[serde(default = "default_true")]
    pub exclude_pure_numeric: bool,

    /// 尊重 Awareness 已判定的 URL/文件路径——命中直接 fallback。
    #[serde(default = "default_true")]
    pub respect_awareness_url_path: bool,

    // ── Provider 列表 ──────────────────────────────────────────────────────
    /// 用户配的 Provider 列表——顺序即 UI 展示顺序(前端可拖拽重排)。
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,

    // ── 三档指派 ───────────────────────────────────────────────────────────
    /// 路由档:意图分类 + 参数抽取。快、便宜、高频调。空 → 降级到 light。
    #[serde(default)]
    pub tier_router: Option<TierAssignment>,

    /// 轻量档:日常单轮任务。中等。空 → 降级到 main。
    #[serde(default)]
    pub tier_light: Option<TierAssignment>,

    /// 主档:多步推理 / Agent loop。最强、最贵。空 → resolve_tier 返回 None,
    /// SearchService fallback 常规 fuzzy(§6.4 兜底铁则)。
    #[serde(default)]
    pub tier_main: Option<TierAssignment>,

    // ── §3.4 危险动作白名单(默认关,即使 Safe 也需要 Tab) ─────────────────
    /// 允许 AI 直接执行 Safe 动作(§3.4)。**默认关**——0.9.2 起用户可打开。
    /// 关闭时任何 AI-routed 动作都需 Tab / Enter 确认。
    #[serde(default)]
    pub direct_execute_safe_actions: bool,

    // ── 流式输出 ─────────────────────────────────────────────────────────
    /// 流式输出开关——**默认 true**。开启后 AI 文本逐 chunk 推送到前端,
    /// 用户无需等待完整响应。关闭时 fallback 到非流式 complete(一次性返回)。
    #[serde(default = "default_true")]
    pub streaming: bool,

    // ── SLO 覆盖(§3.3 骨架层) ─────────────────────────────────────────────
    /// 单次路由调用硬超时(毫秒)。`None` → 用 default 20000ms。
    #[serde(default)]
    pub slo_hard_timeout_ms: Option<u32>,

    // ── 0.11.4 改进 2:结果回流 AI(Tool Chain + 三态配置) ────────────────
    /// AI 工具结果回流开关（§2.2.2 三态配置 D2）。
    ///
    /// - `Auto`（默认）: 本地模型开 + 云端模型关。0.11 阶段所有 provider 都是云端，
    ///   实际等同 `Off`；0.12 本地模型（Ollama / Mistral.rs）上线后 `Auto` 对它们自动开启。
    /// - `On`: 始终开启 Turn 2 回流（总结工具结果 / 链式调 safe tool）。
    /// - `Off`: 始终关闭（单轮直通，快，省 token）。
    ///
    /// 详见 `should_run_tool_feedback`。
    // ── 0.12.5 §5.4：对话配置 ──────────────────────────────────────────
    /// 对话窗口配置子结构（LLM 自动命名等）。
    #[serde(default)]
    pub chat_config: ChatConfig,

    #[serde(default)]
    pub ai_tool_result_feedback: ToolResultFeedback,
}

impl Default for AIConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_intent_routing: false,
            min_query_len: default_min_query_len(),
            min_query_len_cjk: default_min_query_len_cjk(),
            require_whitespace: true,
            exclude_pure_numeric: true,
            respect_awareness_url_path: true,
            providers: Vec::new(),
            tier_router: None,
            tier_light: None,
            tier_main: None,
            direct_execute_safe_actions: false,
            streaming: true,
            slo_hard_timeout_ms: None,
            chat_config: ChatConfig::default(),
            ai_tool_result_feedback: ToolResultFeedback::default(),
        }
    }
}

impl ConfigKey for AIConfig {
    const KEY: &'static str = "app.ai";
}

// ── 内存缓存（供非 async 上下文读取）──────────────────────────────────────

use std::sync::{OnceLock, RwLock};

static AI_CONFIG_CACHE: OnceLock<RwLock<AIConfig>> = OnceLock::new();

/// 初始化 AIConfig 缓存（main.rs 启动时调用）。
pub fn init_ai_cache(config: AIConfig) {
    let _ = AI_CONFIG_CACHE.set(RwLock::new(config));
}

/// 更新 AIConfig 缓存（set_config 'ai_config' 命令调用后同步）。
pub fn update_ai_cache(config: &AIConfig) {
    if let Some(lock) = AI_CONFIG_CACHE.get() {
        if let Ok(mut guard) = lock.write() {
            *guard = config.clone();
        }
    }
}

/// 同步读取 AIConfig 缓存（供 STT 引擎等非 async 上下文使用）。
/// 若缓存未初始化，返回 default（AI 关闭、providers 为空）。
pub fn get_ai_config() -> AIConfig {
    AI_CONFIG_CACHE
        .get()
        .and_then(|lock| lock.read().ok().map(|g| g.clone()))
        .unwrap_or_default()
}

// ── 单个 Provider ────────────────────────────────────────────────────────

/// 一个 AI 供应商的配置项。
///
/// **`secret_ref` 是 CM 别名**——`SecretString` 不放这里,序列化到 SQLite / IPC
/// 都不带 raw Key。加载时按 `provider_id` 从 CM 拿。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// 内部标识——UUID,一旦生成不变。**不给用户看**。
    /// 用作 CM target name 的中段(`blink/{provider_id}/key`)。
    pub id: String,

    /// 用户自命名(如"我的 OpenAI 备用号")。
    pub display_name: String,

    /// 供应商种类——决定 rig 用哪个 `providers::*::Client`。
    pub kind: ProviderKind,

    /// 自定义 Base URL(可选)。`None` → 用 kind 对应 preset。
    /// OpenAI 兼容服务(deepseek/moonshot/一元 API 转发)用它。
    #[serde(default)]
    pub base_url: Option<String>,

    /// **CM 别名**,不是 raw Key。形如 `"blink/{id}/key"`。
    /// 由 `secret::build_target_name(&id, "key")` 生成,`SecretError::InvalidRef`
    /// 兜底非法输入。
    pub secret_ref: String,

    /// 该 Provider 下配的模型列表——用户可绑多个(GPT-4/mini/nano 等)。
    #[serde(default)]
    pub models: Vec<ModelEntry>,

    /// UTC 时间戳(秒),用于设置页展示"添加于"。
    pub created_at: i64,
}

/// 模型能力类型（0.12 §2.7 Provider 模型统一管理）。
///
/// 一个模型可同时具备多种能力（如某些 ollama 模型同时支持 chat + embedding）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelCapability {
    /// 对话/补全模型（含 tool use）——主窗口 complete + 对话窗口 agent loop。
    Chat,
    /// 嵌入模型——0.13 RAG 向量生产（ollama nomic-embed-text / OpenAI text-embedding-3 等）。
    Embedding,
    /// 语音转文字模型——**已废弃**，STT 配置已独立（见 SttConfig::cloud_provider）。
    /// 保留仅为反序列化兼容，前端不再展示此选项。
    Stt,
}

/// 供应商种类——**按协议分层,不按厂商**(0.9.2 第二步重构)。
///
/// **设计原则**:
/// - 用户脑子里的模型是"协议 + 端点",不是"厂商"
/// - 90%+ 平台走 OpenAI Chat Completions(硅基流动/DeepSeek/Moonshot/Groq/OpenRouter/xAI/…)
/// - 前端 modal 通过**预设下拉**兜底 base_url,用户不用查文档
///
/// **老配置迁移**(§5.3 静默):
/// - `"openai" / "deepseek" / "openai_compat"` → `OpenAICompatible`(base_url 缺失时按旧 kind 补 preset)
/// - `"anthropic"` → `AnthropicMessages`
/// - 通过 `#[serde(alias)]` 实现,零 toast、零 UI 感知
///
/// **序列化姿态**:每个变体显式 `rename` 输出稳定字符串。
///
/// **0.12 新增 `OllamaHttp`**:本地推理 provider,走 rig::providers::ollama。
/// 同时支持 chat 模型和 embedding 模型。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProviderKind {
    /// OpenAI Chat Completions 协议——涵盖 OpenAI 官方 / DeepSeek / 硅基流动 /
    /// Moonshot / Groq / OpenRouter / xAI / 自建代理等所有兼容 `/v1/chat/completions`
    /// 的平台。base_url 由用户填(前端预设下拉可一键填充)。
    ///
    /// **迁移别名**:老配置里 `"openai" / "deepseek" / "openai_compat"` 全部落到这里。
    #[serde(
        rename = "openai_compatible",
        alias = "openai",
        alias = "deepseek",
        alias = "openai_compat"
    )]
    OpenAICompatible,

    /// Anthropic Messages 协议——`/v1/messages`,仅 Claude 官方。
    #[serde(rename = "anthropic_messages", alias = "anthropic")]
    AnthropicMessages,

    /// Google Gemini GenerateContent 协议——`/v1beta/models/*:generateContent`,仅 Gemini。
    #[serde(rename = "gemini_generate_content")]
    GeminiGenerateContent,

    /// ollama HTTP API——本地推理,走 rig::providers::ollama。
    /// 同时支持 chat 模型和 embedding 模型。
    /// base_url 默认 `http://localhost:11434`,用户可改。
    /// **不需要 API Key**——ollama 是本地服务,secret_ref 可为空字符串。
    #[serde(rename = "ollama_http")]
    OllamaHttp,
}

impl ProviderKind {
    /// 是否为本地推理 provider（0.11.4 改进 2 §2.2.2 三态配置用）。
    ///
    /// 0.12 起 OllamaHttp 返回 true——`ToolResultFeedback::Auto` 对本地模型自动开启 Turn 2 回流。
    ///
    /// **穷尽性保证**：match 不加通配符，0.12 新增变体时编译器强制开发者思考是否本地。
    pub fn is_local(self) -> bool {
        match self {
            ProviderKind::OllamaHttp => true,
            ProviderKind::OpenAICompatible
            | ProviderKind::AnthropicMessages
            | ProviderKind::GeminiGenerateContent => false,
        }
    }

    /// 获取 serde 序列化后的字符串（与 serde rename 对齐）。
    ///
    /// 用于审计日志存储 `provider_kind` 字段——保证与 JSON 序列化一致。
    pub fn as_serde_str(self) -> &'static str {
        match self {
            ProviderKind::OpenAICompatible => "openai_compatible",
            ProviderKind::AnthropicMessages => "anthropic_messages",
            ProviderKind::GeminiGenerateContent => "gemini_generate_content",
            ProviderKind::OllamaHttp => "ollama_http",
        }
    }

    /// 是否需要 API Key（0.12 新增）。
    ///
    /// 本地 provider（如 ollama）不需要密钥,前端 modal 可隐藏 key 输入框。
    pub fn requires_secret(self) -> bool {
        !self.is_local()
    }
}

/// AI 工具结果回流开关（0.11.4 改进 2 §2.2.2 三态配置 D2）。
///
/// 控制 `handle_ai_tool_calls` 在 Turn 1 执行工具后是否进入 Turn 2 回流：
/// - `Auto`（默认）：本地模型开 + 云端模型关——本地零成本适合 Turn 2，云端省 token
/// - `On`：始终开启（用户显式想要 AI 总结工具结果 / 链式调 safe tool）
/// - `Off`：始终关闭（单轮直通，最快）
///
/// 序列化为小写字符串，与前端 select option 对齐。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolResultFeedback {
    /// 本地模型开 + 云端模型关。
    ///
    /// 0.11 阶段所有 provider 都是云端，实际等同 `Off`；
    /// 0.12 本地模型上线后对它们自动开启。
    Auto,
    /// 始终开启 Turn 2 回流。
    ///
    /// 0.11.6 起为默认值——"打开应用"等 tool chain 闭环场景需要 Turn 2 才能自动执行，
    /// 默认开启保证核心体验。用户嫌慢/费 token 可手动改 Auto 或 Off。
    #[default]
    On,
    /// 始终关闭 Turn 2 回流（单轮直通）。
    Off,
}

impl ToolResultFeedback {
    /// 根据当前 provider kind 判断是否实际运行 Turn 2 回流。
    ///
    /// - `On` → 始终 true
    /// - `Off` → 始终 false
    /// - `Auto` → `provider_kind.is_local()`（本地开 / 云端关）
    pub fn should_run(self, provider_kind: ProviderKind) -> bool {
        match self {
            ToolResultFeedback::On => true,
            ToolResultFeedback::Off => false,
            ToolResultFeedback::Auto => provider_kind.is_local(),
        }
    }
}

// ── 对话配置（0.12.5 §5.4）─────────────────────────────────────────────────

/// 对话窗口配置子结构（0.12.5 §5.4）。
///
/// 存于 AIConfig 第 7 分片的 `chat_config` 子字段。
/// 老配置缺此字段时 serde default 填充——零迁移成本。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatConfig {
    /// LLM 自动命名开关——开启后新对话首条消息发送后异步调 LLM 生成标题。
    /// 默认关闭——opt-in 设计，用户可能不想消耗 token 生成标题。
    #[serde(default)]
    pub auto_title: bool,

    /// 命名模型档位——LLM 命名使用的模型档位。
    /// "main" / "light" / "router"，默认 "light"（日常命名不需要强模型）。
    #[serde(default = "default_title_tier")]
    pub title_tier: String,

    /// 记忆策略配置（0.13.1 §3.7）。
    ///
    /// 控制对话历史窗口模式（固定条数 / token-aware）及压缩参数。
    /// `context_limit` 字段 `#[serde(skip)]`，运行时从 `ModelEntry.context_window` 注入，
    /// 不持久化——serde 反序列化时自动为 None，`apply_config` 时保留运行时值。
    #[serde(default)]
    pub memory_config: crate::domain::ai::memory::MemoryConfig,

    /// Skill 配置（0.13.3）。
    ///
    /// 控制是否启用 Skill 约定式发现及哪些来源目录被扫描。
    #[serde(default)]
    pub skill_config: SkillConfig,
}

/// `ChatConfig` 的 Default 实现——`title_tier` 走 `default_title_tier()` 而非空字符串。
/// serde 在整个 `chat_config` 字段缺失时调 `Default::default()`，此时必须拿到 "light"。
impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            auto_title: false,
            title_tier: default_title_tier(),
            memory_config: crate::domain::ai::memory::MemoryConfig::default(),
            skill_config: SkillConfig::default(),
        }
    }
}

/// `ChatConfig.title_tier` 的 serde 默认值——"light"。
fn default_title_tier() -> String {
    "light".to_string()
}

// ── SkillConfig（0.13.3）──────────────────────────────────────────────────────

/// Skill 配置——控制 Skill 约定式发现与来源开关。
///
/// 存于 `ChatConfig.skill_config` 子字段，随 AIConfig 第 7 分片持久化。
/// 老配置缺此字段时 serde default 填充——零迁移成本。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillConfig {
    /// Skill 功能总开关。默认 true——Skill 是增强性能力，无安全风险，默认开启。
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 启用 Blink 自身目录 `%APPDATA%\blink\skills\`。
    #[serde(default = "default_true")]
    pub source_blink: bool,

    /// 启用 Claude Code 目录 `~/.claude/skills/`。
    #[serde(default = "default_true")]
    pub source_claude: bool,

    /// 启用 ZCode 目录 `~/.zcode/skills/`。默认关——ZCode 用户较少。
    #[serde(default)]
    pub source_zcode: bool,
}

impl Default for SkillConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            source_blink: true,
            source_claude: true,
            source_zcode: false,
        }
    }
}

impl SkillConfig {
    /// 返回当前启用的来源列表（按 SkillSource 优先级排序）。
    pub fn enabled_sources(&self) -> Vec<crate::domain::ai::skill::SkillSource> {
        use crate::domain::ai::skill::SkillSource;
        let mut sources = Vec::new();
        if self.source_blink {
            sources.push(SkillSource::Blink);
        }
        if self.source_claude {
            sources.push(SkillSource::Claude);
        }
        if self.source_zcode {
            sources.push(SkillSource::Zcode);
        }
        sources
    }
}

/// 一个模型的元数据 + 调用参数默认值。
///
/// **0.9.4 Step 1** 起 `temperature / max_tokens / custom_parameters` 三个字段进入,
/// 变成"调用参数**默认值**"载体——请求方(SearchService 路由档等)不指定时 fallback 到这里。
/// 优先级见 `RigProvider::complete`(rig_provider.rs)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelEntry {
    /// 供应商 model id(如 `"gpt-5-nano" / "claude-opus-4-8"`)。
    pub id: String,

    /// 用户可读展示名(自定义,不影响调用)。
    pub display_name: String,

    /// 是否启用。默认 true——老配置缺字段时 serde 填充,零迁移成本。
    /// 前端模型表格的启用开关控制此字段。
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 上下文窗口大小(可选)。前端提示"这个模型能吃多少 token"。
    #[serde(default)]
    pub context_window: Option<u32>,

    /// input 单价(美元 / 1M tokens),可选。
    #[serde(default)]
    pub input_price_per_million: Option<f32>,

    /// output 单价(美元 / 1M tokens),可选。
    #[serde(default)]
    pub output_price_per_million: Option<f32>,

    // ── 0.9.4 Step 1:调用参数默认值 ──────────────────────────────────────
    /// 采样温度默认值。`CompletionRequest.temperature` 为 `None` 时 fallback 此值;
    /// 请求方显式指定(如路由档 `temperature=0.0`)时**优先级高于此字段**——保证路由确定性。
    #[serde(default)]
    pub temperature: Option<f32>,

    /// 输出 token 上限默认值。`CompletionRequest.max_tokens=None` 时 fallback。
    #[serde(default)]
    pub max_tokens: Option<u32>,

    /// 自定义参数——透传到 rig `additional_params`(见 rig-core `CompletionRequest`)。
    /// 常见用途:`top_p` / `reasoning_effort` / 各家扩展 flag。
    /// 前端 value 输入自动推断类型(number/bool/json/string)。
    #[serde(default)]
    pub custom_parameters: Vec<CustomParam>,

/// 模型能力列表（0.12 §2.7）。
///
/// 一个模型可同时具备多种能力（如某些 ollama 模型同时支持 chat + embedding）。
///
/// **默认 `[Chat]`**——老配置缺字段时 serde 填充,零迁移成本。
    #[serde(default = "default_chat_capability")]
    pub capabilities: Vec<ModelCapability>,
}

/// 自定义参数键值对——序列化到 rig `additional_params` JSON。
///
/// **value 用 `serde_json::Value`**:string/number/bool/array/object 全能装。
/// 前端输入 `"0.9"` 会推断成 number,`"true"` 推断成 bool,`{...}` 推断成 object,
/// 其余 fallback string。推断逻辑在前端(`settings.js` 提交时 JSON.parse 尝试)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomParam {
    pub key: String,
    pub value: serde_json::Value,
}

/// 三档指派——引用 provider + model。
///
/// 因为是 `String` id 引用,可能悬空(用户删了对应 model 后没同步)。
/// `AIConfig::resolve_tier` 负责空档降级 + 悬空引用回退(§6.2)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TierAssignment {
    pub provider_id: String,
    pub model_id: String,
}

// ── Tier 概念(运行时选路) ───────────────────────────────────────────────

/// 三档标识——`resolve_tier` 消费。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // 0.9.1 Phase 4 起被 AIProvider dispatch 消费
pub enum Tier {
    /// 路由档:意图分类 + 参数抽取
    Router,
    /// 轻量档:单轮任务
    Light,
    /// 主档:多步推理
    Main,
}

// ── AIConfig 行为(空档降级 + 兜底) ──────────────────────────────────────

impl AIConfig {
    /// 按档位选出 (Provider, Model),空档自动降级到下一档。
    ///
    /// - `Router` 空 → `Light` → `Main` → None
    /// - `Light` 空 → `Main` → None
    /// - `Main` 空 → None(SearchService fallback 常规 fuzzy)
    ///
    /// 悬空引用(assignment 指向已删的 provider/model)同样触发降级并 warn。
    ///
    /// **§6.4 UX 铁则**:降级路径必须 tracing warn,让"悄悄烧贵模型"变可见。
    /// 消费方(SearchService)拿到降级信号后再决定要不要 emit 前端 event。
    ///
    /// 返回 `(&ProviderEntry, &ModelEntry, actual_tier)`——`actual_tier` 是实际
    /// 落到哪一档(用户请求 Router 但落到 Light,`actual_tier = Light`)。
    #[allow(dead_code)] // 0.9.1 Phase 5 起 AppContext AIProvider dispatch 消费
    pub fn resolve_tier(&self, tier: Tier) -> Option<(&ProviderEntry, &ModelEntry, Tier)> {
        let chain: &[(Tier, &Option<TierAssignment>)] = match tier {
            Tier::Router => &[
                (Tier::Router, &self.tier_router),
                (Tier::Light, &self.tier_light),
                (Tier::Main, &self.tier_main),
            ],
            Tier::Light => &[
                (Tier::Light, &self.tier_light),
                (Tier::Main, &self.tier_main),
            ],
            Tier::Main => &[(Tier::Main, &self.tier_main)],
        };

        for (idx, (actual, assignment)) in chain.iter().enumerate() {
            let Some(a) = assignment else { continue };
            let Some(pair) = self.find_provider_model(a) else {
                // 悬空引用 或 model 被禁用——warn 后继续降级
                tracing::warn!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    requested = ?tier, actual = ?actual,
                    provider_id = %a.provider_id, model_id = %a.model_id,
                    "AI 档位引用不可用(悬空或已禁用),降级到下一档"
                );
                continue;
            };
            if idx > 0 {
                tracing::warn!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    requested = ?tier, degraded_to = ?actual,
                    "AI 档位降级",
                );
            }
            return Some((pair.0, pair.1, *actual));
        }
        None
    }

    /// 查找 assignment 对应的 (Provider, Model);任一悬空或 model 被禁用返回 None。
    ///
    /// **0.9.4:enabled=false 视同悬空**——用户在前端关掉 model 开关的语义就是
    /// "从可选池里剔除"。resolve_tier 上层看到 None 会 warn + 降级到下一档。
    fn find_provider_model(&self, a: &TierAssignment) -> Option<(&ProviderEntry, &ModelEntry)> {
        let provider = self.providers.iter().find(|p| p.id == a.provider_id)?;
        let model = provider
            .models
            .iter()
            .find(|m| m.id == a.model_id && m.enabled)?;
        Some((provider, model))
    }

    /// 检测 provider_id 是否被任一档引用——删除 Provider 前问一下。
    ///
    /// 用于 §6.4 UX:删 Provider 前弹"此 Provider 是 Router 档的引用,删除后回退"。
    #[allow(dead_code)] // 0.9.1 Phase 6 前端删 Provider 时消费
    pub fn tiers_referencing(&self, provider_id: &str) -> Vec<Tier> {
        let mut hits = Vec::new();
        if self
            .tier_router
            .as_ref()
            .is_some_and(|a| a.provider_id == provider_id)
        {
            hits.push(Tier::Router);
        }
        if self
            .tier_light
            .as_ref()
            .is_some_and(|a| a.provider_id == provider_id)
        {
            hits.push(Tier::Light);
        }
        if self
            .tier_main
            .as_ref()
            .is_some_and(|a| a.provider_id == provider_id)
        {
            hits.push(Tier::Main);
        }
        hits
    }
}

// ── serde 默认值函数 ─────────────────────────────────────────────────────

fn default_min_query_len() -> u8 {
    4
}

fn default_min_query_len_cjk() -> u8 {
    2
}

fn default_true() -> bool {
    true
}

/// `ModelEntry.capabilities` 的 serde 默认值——`[Chat]`。
fn default_chat_capability() -> Vec<ModelCapability> {
    vec![ModelCapability::Chat]
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_provider(id: &str, model_id: &str) -> ProviderEntry {
        ProviderEntry {
            id: id.to_string(),
            display_name: format!("Test {id}"),
            kind: ProviderKind::OpenAICompatible,
            base_url: None,
            secret_ref: format!("blink/{id}/key"),
            models: vec![ModelEntry {
                id: model_id.to_string(),
                display_name: model_id.to_string(),
                enabled: true,
                context_window: Some(128_000),
                input_price_per_million: Some(0.1),
                output_price_per_million: Some(0.4),
                temperature: None,
                max_tokens: None,
                custom_parameters: Vec::new(),
                capabilities: vec![ModelCapability::Chat],
            }],
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn default_is_disabled() {
        let c = AIConfig::default();
        assert!(!c.enabled, "默认必须 opt-in（§3.6 铁则 1）");
        assert!(!c.allow_intent_routing);
        assert!(!c.direct_execute_safe_actions);
        assert!(c.streaming, "流式默认开启");
        assert_eq!(c.min_query_len, 4);
        assert!(c.require_whitespace);
        assert!(c.exclude_pure_numeric);
        assert!(c.respect_awareness_url_path);
        assert!(c.providers.is_empty());
        assert!(c.tier_router.is_none());
        assert!(c.tier_light.is_none());
        assert!(c.tier_main.is_none());
    }

    #[test]
    fn provider_serializes_without_secret() {
        // AIConfig 类型链上完全没有 SecretString——序列化字节里绝不能出现"api_key"
        let c = AIConfig {
            providers: vec![sample_provider("p1", "gpt-4")],
            ..Default::default()
        };
        let s = serde_json::to_string(&c).unwrap();

        // secret_ref 是别名,可以出现
        assert!(s.contains("blink/p1/key"));
        // 但绝不能出现"api_key" / "sk-" / "secret"（正文里的字段名）
        assert!(
            !s.to_lowercase().contains("api_key"),
            "序列化含 api_key: {s}"
        );
        assert!(!s.contains("sk-"), "序列化含 sk- 前缀: {s}");
    }

    #[test]
    fn resolve_tier_returns_none_when_all_empty() {
        let c = AIConfig::default();
        assert!(c.resolve_tier(Tier::Router).is_none());
        assert!(c.resolve_tier(Tier::Light).is_none());
        assert!(c.resolve_tier(Tier::Main).is_none());
    }

    #[test]
    fn resolve_tier_router_degrades_to_light() {
        // 只配了 light,请求 router → 降级到 light
        let c = AIConfig {
            providers: vec![sample_provider("p1", "gpt-4")],
            tier_light: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "gpt-4".to_string(),
            }),
            ..Default::default()
        };
        let (p, m, actual) = c.resolve_tier(Tier::Router).unwrap();
        assert_eq!(p.id, "p1");
        assert_eq!(m.id, "gpt-4");
        assert_eq!(actual, Tier::Light, "router 空 → 实际落到 light");
    }

    #[test]
    fn resolve_tier_light_degrades_to_main() {
        let c = AIConfig {
            providers: vec![sample_provider("p1", "opus")],
            tier_main: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "opus".to_string(),
            }),
            ..Default::default()
        };
        let (_p, _m, actual) = c.resolve_tier(Tier::Light).unwrap();
        assert_eq!(actual, Tier::Main);
    }

    #[test]
    fn resolve_tier_returns_exact_when_configured() {
        // Router 直接配好 → 不降级
        let c = AIConfig {
            providers: vec![sample_provider("p1", "nano")],
            tier_router: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "nano".to_string(),
            }),
            ..Default::default()
        };
        let (_p, _m, actual) = c.resolve_tier(Tier::Router).unwrap();
        assert_eq!(actual, Tier::Router, "已配置不该降级");
    }

    #[test]
    fn resolve_tier_dangling_reference_falls_through() {
        // Router 指向不存在的 provider → 悬空降级
        let c = AIConfig {
            providers: vec![sample_provider("p1", "gpt-4")],
            tier_router: Some(TierAssignment {
                provider_id: "ghost".to_string(), // 不存在
                model_id: "gpt-4".to_string(),
            }),
            tier_light: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "gpt-4".to_string(),
            }),
            ..Default::default()
        };
        let (_p, _m, actual) = c.resolve_tier(Tier::Router).unwrap();
        assert_eq!(actual, Tier::Light, "悬空 provider 应降级");
    }

    #[test]
    fn resolve_tier_dangling_model_id_falls_through() {
        // Router provider 对但 model_id 悬空 → 也算悬空降级
        let c = AIConfig {
            providers: vec![sample_provider("p1", "gpt-4")],
            tier_router: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "ghost-model".to_string(),
            }),
            tier_main: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "gpt-4".to_string(),
            }),
            ..Default::default()
        };
        let (_p, _m, actual) = c.resolve_tier(Tier::Router).unwrap();
        assert_eq!(actual, Tier::Main);
    }

    #[test]
    fn resolve_tier_disabled_model_falls_through_like_dangling() {
        // 0.9.4:enabled=false 语义等同悬空——resolve_tier 应降级到下一档
        let mut provider = sample_provider("p1", "gpt-4");
        provider.models[0].enabled = false; // 用户在前端关掉了这个 model
        // 再加一个启用的 model 用作降级目标(在 tier_main 指过来)
        provider.models.push(ModelEntry {
            id: "gpt-3.5".to_string(),
            display_name: "GPT-3.5".to_string(),
            enabled: true,
            context_window: None,
            input_price_per_million: None,
            output_price_per_million: None,
            temperature: None,
            max_tokens: None,
            custom_parameters: Vec::new(),
            capabilities: vec![ModelCapability::Chat],
        });
        let c = AIConfig {
            providers: vec![provider],
            tier_router: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "gpt-4".to_string(), // 已禁用
            }),
            tier_main: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "gpt-3.5".to_string(), // 启用
            }),
            ..Default::default()
        };
        let (_p, m, actual) = c.resolve_tier(Tier::Router).unwrap();
        assert_eq!(actual, Tier::Main, "禁用 model 应触发降级到 Main");
        assert_eq!(m.id, "gpt-3.5");
    }

    #[test]
    fn resolve_tier_returns_none_when_all_tiers_disabled() {
        // 全档指向的 model 都禁用 → 全域降级失败,返回 None
        let mut provider = sample_provider("p1", "gpt-4");
        provider.models[0].enabled = false;
        let c = AIConfig {
            providers: vec![provider],
            tier_main: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "gpt-4".to_string(),
            }),
            ..Default::default()
        };
        assert!(
            c.resolve_tier(Tier::Main).is_none(),
            "全禁用时应返回 None,SearchService fallback fuzzy"
        );
    }

    #[test]
    fn tiers_referencing_returns_all_hits() {
        // 一个 provider 被 Router + Light 引用
        let c = AIConfig {
            providers: vec![sample_provider("p1", "gpt-4")],
            tier_router: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "gpt-4".to_string(),
            }),
            tier_light: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "gpt-4".to_string(),
            }),
            ..Default::default()
        };
        let hits = c.tiers_referencing("p1");
        assert_eq!(hits, vec![Tier::Router, Tier::Light]);
        assert!(c.tiers_referencing("ghost").is_empty());
    }

    #[test]
    fn deserialize_from_partial_json_fills_defaults() {
        // 老用户 config 只有旧字段——serde default 填补新字段(向后兼容)
        let json = r#"{"enabled": false}"#;
        let c: AIConfig = serde_json::from_str(json).unwrap();
        assert!(!c.enabled);
        assert_eq!(c.min_query_len, 4);
        assert!(c.require_whitespace);
        assert!(c.streaming, "老配置缺 streaming 字段应默认 true");
        assert!(c.providers.is_empty());
    }

    #[test]
    fn model_entry_backward_compat_without_call_params() {
        // 0.9.4 Step 1:老 config 的 model 没有 temperature/max_tokens/custom_parameters
        // 三个新字段,反序列化零错误,值全落 None / 空 vec。
        let json = r#"{
            "id": "gpt-5-nano",
            "display_name": "GPT-5 Nano",
            "enabled": true
        }"#;
        let m: ModelEntry = serde_json::from_str(json).unwrap();
        assert_eq!(m.id, "gpt-5-nano");
        assert!(m.enabled);
        assert!(m.temperature.is_none());
        assert!(m.max_tokens.is_none());
        assert!(m.custom_parameters.is_empty());
        // 0.12: 老配置缺 capabilities 字段 → serde default 填 [Chat]
        assert_eq!(m.capabilities, vec![ModelCapability::Chat]);
    }

    #[test]
    fn model_entry_call_params_serialize_roundtrip() {
        // 三新字段全填 + custom_parameters 混合 number / bool / string:roundtrip 稳定。
        let m = ModelEntry {
            id: "gpt-5-mini".into(),
            display_name: "M".into(),
            enabled: true,
            context_window: None,
            input_price_per_million: None,
            output_price_per_million: None,
            temperature: Some(0.7),
            max_tokens: Some(4096),
            custom_parameters: vec![
                CustomParam {
                    key: "top_p".into(),
                    value: serde_json::json!(0.9),
                },
                CustomParam {
                    key: "web_search".into(),
                    value: serde_json::json!(true),
                },
                CustomParam {
                    key: "extra_body".into(),
                    value: serde_json::json!("raw-string"),
                },
            ],
            capabilities: vec![ModelCapability::Chat, ModelCapability::Embedding],
        };
        let s = serde_json::to_string(&m).unwrap();
        let m2: ModelEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn config_key_uses_dedicated_kv_key() {
        // §6:第 7 分片,key = "app.ai"——不冲突已有分片
        assert_eq!(AIConfig::KEY, "app.ai");
    }

    #[test]
    fn provider_kind_serializes_stable_strings() {
        // 显式 rename 稳定输出(0.9.2 第二步:按协议 4 类)
        assert_eq!(
            serde_json::to_string(&ProviderKind::OpenAICompatible).unwrap(),
            "\"openai_compatible\""
        );
        assert_eq!(
            serde_json::to_string(&ProviderKind::AnthropicMessages).unwrap(),
            "\"anthropic_messages\""
        );
        assert_eq!(
            serde_json::to_string(&ProviderKind::GeminiGenerateContent).unwrap(),
            "\"gemini_generate_content\""
        );
        // 0.12: OllamaHttp
        assert_eq!(
            serde_json::to_string(&ProviderKind::OllamaHttp).unwrap(),
            "\"ollama_http\""
        );

        // 反序列化 roundtrip
        let k: ProviderKind = serde_json::from_str("\"openai_compatible\"").unwrap();
        assert_eq!(k, ProviderKind::OpenAICompatible);
        let k: ProviderKind = serde_json::from_str("\"ollama_http\"").unwrap();
        assert_eq!(k, ProviderKind::OllamaHttp);
    }

    #[test]
    fn provider_kind_legacy_aliases_migrate_silently() {
        // 老配置迁移铁则:所有旧值都能反序列化到新 kind,零错误、零 toast
        let cases = [
            ("\"openai\"", ProviderKind::OpenAICompatible),
            ("\"deepseek\"", ProviderKind::OpenAICompatible),
            ("\"openai_compat\"", ProviderKind::OpenAICompatible),
            ("\"anthropic\"", ProviderKind::AnthropicMessages),
        ];
        for (input, expected) in cases {
            let k: ProviderKind = serde_json::from_str(input)
                .unwrap_or_else(|e| panic!("老 kind {input} 迁移失败: {e}"));
            assert_eq!(k, expected, "{input} 应迁移到 {expected:?}");
        }
    }

    #[test]
    fn round_trip_through_json_preserves_all_fields() {
        let original = AIConfig {
            enabled: true,
            allow_intent_routing: true,
            min_query_len: 8,
            min_query_len_cjk: 3,
            require_whitespace: false,
            exclude_pure_numeric: false,
            respect_awareness_url_path: true,
            providers: vec![sample_provider("p1", "gpt-4")],
            tier_router: Some(TierAssignment {
                provider_id: "p1".to_string(),
                model_id: "gpt-4".to_string(),
            }),
            tier_light: None,
            tier_main: None,
            direct_execute_safe_actions: true,
        streaming: false,
        slo_hard_timeout_ms: Some(3000),
        chat_config: ChatConfig {
            auto_title: true,
            title_tier: "router".to_string(),
            memory_config: crate::domain::ai::memory::MemoryConfig::default(),
            skill_config: SkillConfig::default(),
        },
        ai_tool_result_feedback: ToolResultFeedback::On,
    };
        let s = serde_json::to_string(&original).unwrap();
        let restored: AIConfig = serde_json::from_str(&s).unwrap();

        assert_eq!(restored.enabled, original.enabled);
        assert_eq!(restored.allow_intent_routing, original.allow_intent_routing);
        assert_eq!(restored.min_query_len, original.min_query_len);
        assert_eq!(restored.min_query_len_cjk, original.min_query_len_cjk);
        assert_eq!(restored.providers.len(), 1);
        assert_eq!(restored.providers[0].id, "p1");
        assert_eq!(restored.tier_router, original.tier_router);
        assert_eq!(
            restored.direct_execute_safe_actions,
            original.direct_execute_safe_actions
        );
        assert_eq!(restored.streaming, original.streaming);
        assert_eq!(restored.slo_hard_timeout_ms, Some(3000));
        assert_eq!(
            restored.ai_tool_result_feedback,
            ToolResultFeedback::On,
            "0.11.4: 回流开关 round-trip"
        );
        // 0.12.5: ChatConfig round-trip
        assert!(restored.chat_config.auto_title);
        assert_eq!(restored.chat_config.title_tier, "router");
    }

    // ── 0.11.4 改进 2:ToolResultFeedback 三态配置 ────────────────────────

    #[test]
    fn tool_result_feedback_default_is_on() {
        // 0.11.6: 默认值从 Auto 改为 On，保证 tool chain 闭环默认可用
        assert_eq!(ToolResultFeedback::default(), ToolResultFeedback::On);
        assert_eq!(
            AIConfig::default().ai_tool_result_feedback,
            ToolResultFeedback::On
        );
    }

    #[test]
    fn tool_result_feedback_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&ToolResultFeedback::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&ToolResultFeedback::On).unwrap(),
            "\"on\""
        );
        assert_eq!(
            serde_json::to_string(&ToolResultFeedback::Off).unwrap(),
            "\"off\""
        );
    }

    #[test]
    fn tool_result_feedback_deserializes_lowercase() {
        assert_eq!(
            serde_json::from_str::<ToolResultFeedback>("\"auto\"").unwrap(),
            ToolResultFeedback::Auto
        );
        assert_eq!(
            serde_json::from_str::<ToolResultFeedback>("\"on\"").unwrap(),
            ToolResultFeedback::On
        );
        assert_eq!(
            serde_json::from_str::<ToolResultFeedback>("\"off\"").unwrap(),
            ToolResultFeedback::Off
        );
    }

    #[test]
    fn tool_result_feedback_legacy_config_defaults_to_on() {
        // 老配置没有 ai_tool_result_feedback 字段 → serde default 填 On（0.11.6 改）
        let json = r#"{"enabled": false}"#;
        let c: AIConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.ai_tool_result_feedback, ToolResultFeedback::On);
    }

    // ── 0.12.5 §5.4: ChatConfig 向后兼容 ──────────────────────────────

    #[test]
    fn chat_config_defaults_when_missing() {
        // 老配置没有 chat_config 字段 → serde default 填充
        let json = r#"{"enabled": false}"#;
        let c: AIConfig = serde_json::from_str(json).unwrap();
        assert!(!c.chat_config.auto_title, "auto_title 默认 false");
        assert_eq!(c.chat_config.title_tier, "light", "title_tier 默认 light");
        // 0.13.1: memory_config 默认 TokenAware 模式
        assert_eq!(
            c.chat_config.memory_config.mode,
            crate::domain::ai::memory::WindowMode::TokenAware,
            "memory_config.mode 默认 TokenAware"
        );
    }

    // ── 0.13.1: MemoryConfig 向后兼容 ───────────────────────────────

    #[test]
    fn memory_config_roundtrip() {
        let original = AIConfig {
            enabled: true,
            chat_config: ChatConfig {
                auto_title: false,
                title_tier: "light".to_string(),
                memory_config: crate::domain::ai::memory::MemoryConfig {
                    mode: crate::domain::ai::memory::WindowMode::FixedCount,
                    window_size: 30,
                    context_limit: None,
                    trigger_ratio: 0.85,
                    compress_ratio: 0.65,
                    recall_enabled: false,
                    recall_top_k: 5,
                },
                skill_config: SkillConfig::default(),
            },
            ..Default::default()
        };
        let s = serde_json::to_string(&original).unwrap();
        let restored: AIConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(restored.chat_config.memory_config.mode, crate::domain::ai::memory::WindowMode::FixedCount);
        assert_eq!(restored.chat_config.memory_config.window_size, 30);
        assert!((restored.chat_config.memory_config.trigger_ratio - 0.85).abs() < 0.001);
        assert!((restored.chat_config.memory_config.compress_ratio - 0.65).abs() < 0.001);
        // context_limit 不持久化（#[serde(skip)]）
        assert_eq!(restored.chat_config.memory_config.context_limit, None);
    }

    #[test]
    fn provider_kind_is_local_returns_false_for_cloud_true_for_local() {
        // 云端 provider
        assert!(!ProviderKind::OpenAICompatible.is_local());
        assert!(!ProviderKind::AnthropicMessages.is_local());
        assert!(!ProviderKind::GeminiGenerateContent.is_local());
        // 0.12: 本地 provider
        assert!(ProviderKind::OllamaHttp.is_local());
    }

    #[test]
    fn provider_kind_as_serde_str_matches_serde_rename() {
        // 与 serde rename 对齐
        assert_eq!(
            ProviderKind::OpenAICompatible.as_serde_str(),
            "openai_compatible"
        );
        assert_eq!(
            ProviderKind::AnthropicMessages.as_serde_str(),
            "anthropic_messages"
        );
        assert_eq!(
            ProviderKind::GeminiGenerateContent.as_serde_str(),
            "gemini_generate_content"
        );
        // 0.12
        assert_eq!(ProviderKind::OllamaHttp.as_serde_str(), "ollama_http");
    }

    #[test]
    fn should_run_tool_feedback_on_always_true() {
        // On → 无论云端本地都开
        assert!(ToolResultFeedback::On.should_run(ProviderKind::OpenAICompatible));
        assert!(ToolResultFeedback::On.should_run(ProviderKind::AnthropicMessages));
    }

    #[test]
    fn should_run_tool_feedback_off_always_false() {
        // Off → 无论云端本地都关
        assert!(!ToolResultFeedback::Off.should_run(ProviderKind::OpenAICompatible));
        assert!(!ToolResultFeedback::Off.should_run(ProviderKind::AnthropicMessages));
    }

    #[test]
    fn should_run_tool_feedback_auto_follows_provider_locality() {
        // Auto → 云端关
        assert!(!ToolResultFeedback::Auto.should_run(ProviderKind::OpenAICompatible));
        assert!(!ToolResultFeedback::Auto.should_run(ProviderKind::AnthropicMessages));
        assert!(!ToolResultFeedback::Auto.should_run(ProviderKind::GeminiGenerateContent));
        // 0.12: Auto → 本地开
        assert!(ToolResultFeedback::Auto.should_run(ProviderKind::OllamaHttp));
    }

    // ── 0.12 §2.7: ModelCapability ────────────────────────────────────────

    #[test]
    fn model_capability_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&ModelCapability::Chat).unwrap(),
            "\"chat\""
        );
        assert_eq!(
            serde_json::to_string(&ModelCapability::Embedding).unwrap(),
            "\"embedding\""
        );
        assert_eq!(
            serde_json::to_string(&ModelCapability::Stt).unwrap(),
            "\"stt\""
        );
    }

    #[test]
    fn model_entry_default_capabilities_is_chat() {
        // 老配置缺 capabilities → serde default 填 [Chat]
        let json = r#"{"id":"m","display_name":"M","enabled":true}"#;
        let m: ModelEntry = serde_json::from_str(json).unwrap();
        assert_eq!(m.capabilities, vec![ModelCapability::Chat]);
    }

    #[test]
    fn model_entry_multi_capabilities_roundtrip() {
        let m = ModelEntry {
            id: "nomic-embed-text".into(),
            display_name: "Nomic Embed".into(),
            enabled: true,
            context_window: None,
            input_price_per_million: None,
            output_price_per_million: None,
            temperature: None,
            max_tokens: None,
            custom_parameters: Vec::new(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::Embedding],
        };
        let s = serde_json::to_string(&m).unwrap();
        let m2: ModelEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(m, m2);
        assert_eq!(m2.capabilities.len(), 2);
    }

    #[test]
    fn provider_kind_requires_secret() {
        // 云端 provider 需要密钥
        assert!(ProviderKind::OpenAICompatible.requires_secret());
        assert!(ProviderKind::AnthropicMessages.requires_secret());
        assert!(ProviderKind::GeminiGenerateContent.requires_secret());
        // 本地 provider 不需要
        assert!(!ProviderKind::OllamaHttp.requires_secret());
    }
}
