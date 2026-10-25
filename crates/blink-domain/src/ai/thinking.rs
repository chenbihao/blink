//! 供应商 thinking 能力解析 / 意图编码 / 请求补丁（0.21.16 起，0.27.15 能力化）——
//! chat 对话窗口（`agent_provider`）与主窗口（`rig_provider`）共用。
//!
//! 0.27.15 三层结构：
//! 1. **能力解析** `resolve_thinking_capability`——按协议 + 方言 + 模型 id（+
//!    per-model `thinking_style` 逃生口）产出 [`ThinkingCapability`]，描述控件的
//!    诚实形态（等级/开关/不支持/未知）、合法档位、可否关闭、可否「开·默认」。
//!    目录真源是 rig 0.45 内置 catalog（models.dev + rig 手工复核，公共 API
//!    `ModelFacts::default().for_model(vendor, model)`）；未知网关不猜能力。
//! 2. **意图编码** [`ThinkingIntent`] → `thinking_request_patch`——把本轮显式
//!    意图编码成各协议 wire 字段。编码形状与 rig 0.45 typed options 的 provider
//!    映射对齐（DeepSeek `thinking.type`+`reasoning_effort`、Anthropic manual
//!    budget / adaptive `output_config.effort`、Gemini `thinkingConfig`、
//!    Ollama `think`），但保持自有 patch 层——需要表达 typed 枚举覆盖不了的
//!    组合（如 DeepSeek「开·默认强度」），且 raw `additional_params` 在 rig
//!    编码顺序中位于 typed options 之后会反超按钮意图，全 raw 路径让本轮
//!    意图的优先级由我们自己的归并保证。
//! 3. **私参归并** `merge_chat_parameters` + `strip_conflicting_thinking_params`——
//!    显式意图只覆盖其拥有的字段，剔除冲突私参、保留无关嵌套参数。

use rig_core::catalog::{ModelFacts, ModelSpec, ReasoningSupport};
use serde::Serialize;

use crate::ai::compat_dialect::{url_host, ResolvedCompatDialect};
use crate::config::ai_config::{ProviderKind, ThinkingStyle};

// ── 能力模型（0.27.15）─────────────────────────────────────────────────────

/// 思考控件形态——前端渲染与 `supports_effort` 判定的真源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingShape {
    /// 等级下拉——`levels` 为目录声明的合法档位（空表 = 不限档透传）。
    Levels,
    /// 简单开关——「开」= 默认强度/预算（`can_enable_default`）。
    Switch,
    /// 模型不支持思考参数——控件隐藏。
    Unsupported,
    /// 能力未知——保持「默认 omit + 显式覆盖」，按下拉/开关基形渲染并提示。
    Unknown,
}

/// 思考 wire 编码风格——patch 层内部分派（不进前端投影）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingEncoding {
    /// OpenAI Chat Completions `reasoning_effort`
    ReasoningEffort,
    /// DeepSeek `thinking.type`（开时可叠 `reasoning_effort`）
    DeepSeekThinking,
    /// Anthropic `thinking` budget / adaptive `output_config.effort`
    AnthropicThinking,
    /// Gemini `generationConfig.thinkingConfig`
    GeminiThinkingConfig,
    /// Ollama `think`（bool / 命名等级）
    OllamaThink,
    /// 不支持——任何显式意图本地拒绝
    None,
}

/// 思考能力快照——控件呈现、请求编码与本地校验的单一真源（0.27.15）。
///
/// 「未发送/跟随默认」不等于关闭：`FollowDefault` 意图 omit 全部思考字段，
/// 由服务端默认决定；`can_disable=false` 的模型不显示虚假「已关闭」。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ThinkingCapability {
    /// 控件形态
    pub shape: ThinkingShape,
    /// 合法档位（Levels 且目录已知时非空；空表 = 不限档透传）
    pub levels: Vec<String>,
    /// false = 始终开启（目录声明不可关闭），「关」不可用
    pub can_disable: bool,
    /// 「开·默认」可用（默认强度 / 默认预算），如 DeepSeek thinking.type=enabled
    pub can_enable_default: bool,
    /// 预算范围（min, max）——manual budget 型模型（Anthropic 部分档 / Gemini 2.5）
    pub budget: Option<(u32, u32)>,
    /// 目录声明的默认档（仅展示）
    pub default_effort: Option<String>,
    /// 能力是否来自降级启发（未知）——前端附提示
    pub unknown: bool,
    /// wire 编码风格（内部）
    #[serde(skip)]
    pub(crate) encoding: ThinkingEncoding,
    /// Anthropic adaptive thinking（等级档伴随 `thinking.type=adaptive`）
    #[serde(skip)]
    pub(crate) adaptive: bool,
    /// Anthropic 关闭词（`compat.thinking_off`，缺省 "disabled"）
    #[serde(skip)]
    pub(crate) thinking_off_word: Option<String>,
}

impl ThinkingCapability {
    /// 控件是否为下拉形态（等级/未知透传）——兼容 0.21.17 `supports_effort_levels` 语义。
    pub fn supports_effort(&self) -> bool {
        matches!(self.shape, ThinkingShape::Levels | ThinkingShape::Unknown)
            && self.encoding != ThinkingEncoding::None
    }

    /// 未知能力兜底（默认 omit + 显式覆盖透传）——provider 未配置等边界场景。
    pub fn unknown() -> Self {
        Self {
            shape: ThinkingShape::Unknown,
            levels: Vec::new(),
            can_disable: true,
            can_enable_default: false,
            budget: None,
            default_effort: None,
            unknown: true,
            encoding: ThinkingEncoding::ReasoningEffort,
            adaptive: false,
            thinking_off_word: None,
        }
    }
}

/// 本轮显式思考意图——由控件状态翻译（「默认」omit ≠ 关闭）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingIntent {
    /// 跟随服务默认——omit 全部思考字段（绝不因它出 400）
    FollowDefault,
    /// 显式关闭
    Disable,
    /// 开·默认强度/预算（无等级）
    EnableDefault,
    /// 指定等级
    Effort(String),
}

impl ThinkingIntent {
    /// 是否携带显式意图（决定是否剔除冲突私参）。
    pub fn is_explicit(&self) -> bool {
        !matches!(self, ThinkingIntent::FollowDefault)
    }

    /// 由请求参数翻译意图（chat 窗口载荷）。
    ///
    /// - 下拉型模型（`supports_dropdown`）：effort 线值 `None/""` → FollowDefault、
    ///   `"none"` → Disable、`"default_on"` → EnableDefault、其余 → Effort。
    /// - 开关型模型：`enabled` → EnableDefault / Disable。
    pub fn from_request(supports_dropdown: bool, effort: Option<&str>, enabled: bool) -> Self {
        if supports_dropdown {
            match effort.unwrap_or("") {
                "" => ThinkingIntent::FollowDefault,
                "none" => ThinkingIntent::Disable,
                "default_on" => ThinkingIntent::EnableDefault,
                level => ThinkingIntent::Effort(level.to_string()),
            }
        } else if enabled {
            ThinkingIntent::EnableDefault
        } else {
            ThinkingIntent::Disable
        }
    }

    /// 主窗口「默认关」策略翻译：per-model effort 优先，未配置时按能力降级——
    /// 开关型协议显式 Disable，等级型协议 FollowDefault（omit，避免给未知
    /// 网关发 `reasoning_effort:"none"` 出 400 的旧行为回归）。
    pub fn main_window(model_effort: Option<&str>, cap: &ThinkingCapability) -> Self {
        match model_effort.unwrap_or("") {
            "" => Self::main_window_default(cap),
            "none" => ThinkingIntent::Disable,
            "default_on" => ThinkingIntent::EnableDefault,
            level => ThinkingIntent::Effort(level.to_string()),
        }
    }

    fn main_window_default(cap: &ThinkingCapability) -> Self {
        match cap.encoding {
            ThinkingEncoding::ReasoningEffort | ThinkingEncoding::None => {
                ThinkingIntent::FollowDefault
            }
            _ => ThinkingIntent::Disable,
        }
    }
}

/// patch 本地拒绝（能力已知且意图越界）——可读错误，请求不发出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingPatchError(pub String);

impl std::fmt::Display for ThinkingPatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── 能力解析 ───────────────────────────────────────────────────────────────

/// rig 内置 catalog 的 vendor 名（`providers::*::PROVIDER_NAME`，models.dev key
/// 经 KEYS 表映射后的 rig vendor 名）。
mod vendor {
    pub const OPENAI: &str = "openai";
    pub const DEEPSEEK: &str = "deepseek";
    pub const MOONSHOT: &str = "moonshot";
    pub const ANTHROPIC: &str = "anthropic";
    pub const GEMINI: &str = "gcp.gemini";
    pub const OLLAMA: &str = "ollama";
}

/// OpenAI 官方端点 hostname——Generic 方言下唯一允许按 vendor=openai 查目录的
/// 场景（精确匹配，与 compat_dialect 官方端点表同哲学：不给未知网关套身份）。
const OPENAI_OFFICIAL_HOSTS: [&str; 1] = ["api.openai.com"];

/// 解析思考能力（0.27.15 单一真源）。
///
/// - `style` 逃生口（0.21.22）保留：`Effort` 强制等级下拉（不限档透传）、
///   `Toggle` 强制开关（OpenAICompatible 走 `thinking.type`）。仅 OAICompatible 生效。
/// - 目录真源：rig 0.45 内置 catalog（`ModelFacts::default()` → `Catalog::builtin()`），
///   含快照后缀归一（`claude-sonnet-4-5-20250929` → `claude-sonnet-4-5`）。
/// - 目录 miss：DeepSeek/Moonshot 官方端点按 Unknown 开关降级；Gemini 按家族
///   启发（2.5 → 预算、3 → 等级、<2.5 → 不支持）；OpenAI 官方端点按 gpt-5+/
///   o-series 命名启发；未知网关一律 Unknown（现状透传语义）。
pub fn resolve_thinking_capability(
    kind: ProviderKind,
    compat: ResolvedCompatDialect,
    base_url: Option<&str>,
    model_id: &str,
    style: Option<ThinkingStyle>,
) -> ThinkingCapability {
    let facts = ModelFacts::default();
    let model = model_id.rsplit('/').next().unwrap_or(model_id);
    match kind {
        ProviderKind::OpenAICompatible => {
            // 逃生口优先（既有语义：用户声明覆盖启发）
            if let Some(ThinkingStyle::Effort) = style {
                return ThinkingCapability {
                    shape: ThinkingShape::Levels,
                    levels: Vec::new(),
                    can_disable: true,
                    can_enable_default: false,
                    budget: None,
                    default_effort: None,
                    unknown: false,
                    encoding: ThinkingEncoding::ReasoningEffort,
                    adaptive: false,
                    thinking_off_word: None,
                };
            }
            if let Some(ThinkingStyle::Toggle) = style {
                return ThinkingCapability {
                    shape: ThinkingShape::Switch,
                    levels: Vec::new(),
                    can_disable: true,
                    can_enable_default: true,
                    budget: None,
                    default_effort: None,
                    unknown: false,
                    encoding: ThinkingEncoding::DeepSeekThinking,
                    adaptive: false,
                    thinking_off_word: None,
                };
            }
            let deepseek_base = compat == ResolvedCompatDialect::DeepSeek
                || model
                    .to_ascii_lowercase()
                    .starts_with("deepseek");
            if deepseek_base {
                let spec = facts.for_model(vendor::DEEPSEEK, model);
                let mut cap = from_spec(spec, ThinkingEncoding::DeepSeekThinking);
                // DeepSeek thinking.type 开关与等级并存（V3.2+ 目录声明 toggle+effort）
                cap.can_enable_default = cap.can_disable || cap.unknown;
                return cap;
            }
            if compat == ResolvedCompatDialect::Moonshot {
                let spec = facts.for_model(vendor::MOONSHOT, model);
                // kimi-k3 → reasoning_effort 等级；k2.6 → thinking.type 开关
                let mut cap = from_spec(spec, ThinkingEncoding::ReasoningEffort);
                if cap.shape == ThinkingShape::Switch || cap.unknown {
                    cap.encoding = ThinkingEncoding::DeepSeekThinking;
                    cap.can_enable_default = true;
                }
                return cap;
            }
            // Generic：官方 OpenAI 端点可查目录（精确 hostname，不给未知网关套身份）
            let host = url_host(base_url);
            if host.as_deref().is_some_and(|h| OPENAI_OFFICIAL_HOSTS.contains(&h)) {
                let spec = facts.for_model(vendor::OPENAI, model);
                let mut cap = match spec {
                    Some(_) => from_spec(spec, ThinkingEncoding::ReasoningEffort),
                    // 官方端点的目录 miss：gpt-5+/o-series 命名启发（与 rig
                    // named_reasons 对齐），旧编号 GPT 不支持，其余未知
                    None => openai_named_capability(model),
                };
                cap.can_enable_default = false;
                return cap;
            }
            // 未知网关：能力未知——保持「默认 omit + 显式覆盖」（现状透传）
            ThinkingCapability::unknown()
        }
        ProviderKind::AnthropicMessages => {
            let spec = facts.for_model(vendor::ANTHROPIC, model);
            let mut cap = from_spec(spec, ThinkingEncoding::AnthropicThinking);
            // 「开·默认」= manual budget（开启必带预算，Anthropic 协议要求）
            if cap.shape != ThinkingShape::Unsupported {
                cap.can_enable_default = true;
            }
            cap
        }
        ProviderKind::GeminiGenerateContent => gemini_capability(&facts, model),
        ProviderKind::OllamaHttp => {
            let spec = facts.for_model(vendor::OLLAMA, model);
            let mut cap = from_spec(spec, ThinkingEncoding::OllamaThink);
            if cap.shape != ThinkingShape::Unsupported {
                // Ollama think:true 恒可用（目录声明 toggle 或等级）
                cap.can_enable_default = true;
            }
            cap
        }
    }
}

/// 把目录 `ReasoningSupport` 投影为能力（通用规则，编码风格由调用方指定）。
fn from_spec(spec: Option<&ModelSpec>, encoding: ThinkingEncoding) -> ThinkingCapability {
    let unknown = |default: Option<String>| ThinkingCapability {
        shape: ThinkingShape::Switch,
        levels: Vec::new(),
        can_disable: true,
        can_enable_default: false,
        budget: None,
        default_effort: default,
        unknown: true,
        encoding,
        adaptive: false,
        thinking_off_word: None,
    };
    let Some(spec) = spec else {
        return unknown(None);
    };
    let adaptive = spec.compat.adaptive_thinking;
    let thinking_off_word = spec.compat.thinking_off.clone();
    match &spec.reasoning {
        ReasoningSupport::None => ThinkingCapability {
            shape: ThinkingShape::Unsupported,
            levels: Vec::new(),
            can_disable: false,
            can_enable_default: false,
            budget: None,
            default_effort: None,
            unknown: false,
            encoding: ThinkingEncoding::None,
            adaptive,
            thinking_off_word,
        },
        ReasoningSupport::Unknown { default } => unknown(default.map(|e| e.as_str().to_string())),
        ReasoningSupport::Listed {
            levels,
            budget,
            can_disable,
            default,
        } => {
            let level_strings: Vec<String> =
                levels.iter().map(|e| e.as_str().to_string()).collect();
            let shape = if level_strings.is_empty() {
                ThinkingShape::Switch
            } else {
                ThinkingShape::Levels
            };
            ThinkingCapability {
                shape,
                levels: level_strings,
                // can_disable 未知（None）按可关闭处理——控件提供「关」，
                // 真实不支持时服务端 400 会经 T2/错误链路反馈
                can_disable: *can_disable,
                can_enable_default: false,
                budget: budget.as_ref().map(|r| (*r.start(), *r.end())),
                default_effort: default.map(|e| e.as_str().to_string()),
                unknown: false,
                encoding,
                adaptive,
                thinking_off_word,
            }
        },
        // non_exhaustive：未来目录变体按未知降级（不猜能力）
        _ => unknown(None),
    }
}

/// OpenAI 官方端点目录 miss 的命名启发（对齐 rig `named_reasoning`）：
/// gpt-5+/o-series → 等级（low/medium/high）；旧编号 GPT → 不支持；其余未知。
fn openai_named_capability(model: &str) -> ThinkingCapability {
    let name = model.to_ascii_lowercase();
    let gpt5 = name.strip_prefix("gpt-").is_some_and(|rest| {
        rest.split(['.', '-'])
            .next()
            .and_then(|major| major.parse::<u32>().ok())
            .is_some_and(|major| major >= 5)
    });
    let o_series = name
        .strip_prefix('o')
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
    if gpt5 || o_series {
        // can_disable：o-series / gpt-5 本体 / -pro 不支持 effort none（rig named_can_disable）
        let pro = name.contains("-pro");
        let gpt5_plain = name == "gpt-5" || name.starts_with("gpt-5-");
        return ThinkingCapability {
            shape: ThinkingShape::Levels,
            levels: ["low", "medium", "high"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            can_disable: !(o_series || pro || gpt5_plain),
            can_enable_default: false,
            budget: None,
            default_effort: None,
            unknown: false,
            encoding: ThinkingEncoding::ReasoningEffort,
            adaptive: false,
            thinking_off_word: None,
        };
    }
    // 旧编号 GPT（gpt-4o/gpt-4.1 等）不推理；其余（微调名/部署名）未知
    let old_gpt = name.starts_with("gpt-");
    if old_gpt {
        ThinkingCapability {
            shape: ThinkingShape::Unsupported,
            levels: Vec::new(),
            can_disable: false,
            can_enable_default: false,
            budget: None,
            default_effort: None,
            unknown: false,
            encoding: ThinkingEncoding::None,
            adaptive: false,
            thinking_off_word: None,
        }
    } else {
        ThinkingCapability {
            shape: ThinkingShape::Unknown,
            levels: Vec::new(),
            can_disable: true,
            can_enable_default: false,
            budget: None,
            default_effort: None,
            unknown: true,
            encoding: ThinkingEncoding::ReasoningEffort,
            adaptive: false,
            thinking_off_word: None,
        }
    }
}

/// Gemini 能力：目录优先，miss 按家族启发（对齐 rig `named_thinking`——
/// 3.x → 等级（不可关）、2.5 → 预算、<2.5 → 不支持、其余未知开关）。
fn gemini_capability(facts: &ModelFacts, model: &str) -> ThinkingCapability {
    let name = model.to_ascii_lowercase();
    let name = name.strip_prefix("models/").unwrap_or(&name);
    let spec = facts.for_model(vendor::GEMINI, name);
    if let Some(spec) = spec {
        return from_spec(Some(spec), ThinkingEncoding::GeminiThinkingConfig);
    }
    // 家族启发（目录 miss：预览变体 / 自定义部署名）
    const LEVEL_FAMILIES: [(&str, &[&str]); 10] = [
        ("gemini-3.8-flash", &["low", "medium", "high"]),
        ("gemini-3.7-flash", &["low", "medium", "high"]),
        ("gemini-3.6-flash", &["minimal", "low", "medium", "high"]),
        ("gemini-3.5-flash-lite", &["minimal", "low", "medium", "high"]),
        ("gemini-3.5-flash", &["minimal", "low", "medium", "high"]),
        ("gemini-3.1-pro", &["low", "medium", "high"]),
        ("gemini-3.1-flash-lite", &["minimal", "low", "medium", "high"]),
        ("gemini-3-flash-preview", &["minimal", "low", "medium", "high"]),
        ("gemini-3-pro-preview", &["low", "high"]),
        ("gemini-3-pro", &["low", "high"]),
    ];
    if let Some((_, levels)) = LEVEL_FAMILIES.iter().find(|(id, _)| name.starts_with(id)) {
        return ThinkingCapability {
            shape: ThinkingShape::Levels,
            levels: levels.iter().map(|s| s.to_string()).collect(),
            can_disable: false,
            can_enable_default: false,
            budget: None,
            default_effort: None,
            unknown: false,
            encoding: ThinkingEncoding::GeminiThinkingConfig,
            adaptive: false,
            thinking_off_word: None,
        };
    }
    const BUDGET_FAMILIES: [(&str, u32, u32, bool); 3] = [
        ("gemini-2.5-flash-lite", 512, 24_576, true),
        ("gemini-2.5-flash", 0, 24_576, true),
        ("gemini-2.5-pro", 128, 32_768, false),
    ];
    if let Some((_, min, max, can_disable)) = BUDGET_FAMILIES
        .into_iter()
        .find(|(id, ..)| name.starts_with(id))
    {
        return ThinkingCapability {
            shape: ThinkingShape::Switch,
            levels: Vec::new(),
            can_disable,
            can_enable_default: true,
            budget: Some((min, max)),
            default_effort: None,
            unknown: false,
            encoding: ThinkingEncoding::GeminiThinkingConfig,
            adaptive: false,
            thinking_off_word: None,
        };
    }
    // <2.5 / 2.0 → 不支持；其余未知
    let major = name
        .strip_prefix("gemini-")
        .and_then(|rest| rest.split(['.', '-']).next())
        .and_then(|m| m.parse::<u32>().ok());
    match major {
        Some(major) if major < 2 || name.starts_with("gemini-2.0") => ThinkingCapability {
            shape: ThinkingShape::Unsupported,
            levels: Vec::new(),
            can_disable: false,
            can_enable_default: false,
            budget: None,
            default_effort: None,
            unknown: false,
            encoding: ThinkingEncoding::None,
            adaptive: false,
            thinking_off_word: None,
        },
        _ => ThinkingCapability {
            shape: ThinkingShape::Switch,
            levels: Vec::new(),
            can_disable: true,
            can_enable_default: true,
            budget: None,
            default_effort: None,
            unknown: true,
            encoding: ThinkingEncoding::GeminiThinkingConfig,
            adaptive: false,
            thinking_off_word: None,
        },
    }
}

// ── 意图编码（请求补丁）────────────────────────────────────────────────────

/// Anthropic 开启思考的默认 token 预算——须在 1024..max_tokens 之间。
/// Anthropic 未配 max_tokens 时由上层兜底 4096，2048 居中安全；用户可按模型调大。
pub(crate) const ANTHROPIC_THINKING_BUDGET: u64 = 2048;

/// Gemini 预算型模型「开·默认」预算——目录范围内取安全中值（2.5-pro min 128、
/// flash 系 min 0/512，2048 均在范围内且不与输出上限冲突）。
const GEMINI_DEFAULT_THINKING_BUDGET: i64 = 2048;

/// 供应商特定的 thinking 请求补丁（纯函数）——按能力 + 本轮意图编码 wire 字段。
///
/// 返回 `Ok(None)` = 不追加任何思考字段（FollowDefault / 无关编码）；
/// `Err` = 能力已知且意图越界（本地可读拒绝，请求不发出——避免白付 400 往返）。
///
/// 各协议 wire 形状（与 rig 0.45 typed options 的 provider 映射对齐）：
/// - OpenAI 风格（Generic / Moonshot k3）：`{"reasoning_effort": level}`，
///   关闭 = `"none"`；默认 omit。
/// - DeepSeek：`{"thinking":{"type":"enabled"/"disabled"}}`，等级档叠加
///   `{"reasoning_effort": low|high|max}`。
/// - Anthropic：关闭 `{"thinking":{"type":"disabled"}}`；开·默认 manual
///   `{"thinking":{"type":"enabled","budget_tokens":2048}}`；等级档 adaptive
///   `{"thinking":{"type":"adaptive"},"output_config":{"effort":…}}`（目录
///   adaptive_thinking 兼容位，无则仅 output_config）。
/// - Gemini：`{"generationConfig":{"thinkingConfig":{"thinkingBudget":N}}}` /
///   `{"thinkingLevel": level}`；关闭 = thinkingBudget 0（不可关模型本地拒绝）。
/// - Ollama：`{"think": true|false|level}`。
pub(crate) fn thinking_request_patch(
    cap: &ThinkingCapability,
    intent: &ThinkingIntent,
) -> Result<Option<serde_json::Value>, ThinkingPatchError> {
    if cap.encoding == ThinkingEncoding::None {
        return match intent {
            ThinkingIntent::FollowDefault => Ok(None),
            _ => Err(ThinkingPatchError(
                "该模型不支持思考参数，请在控件中恢复「默认」".into(),
            )),
        };
    }
    match cap.encoding {
        ThinkingEncoding::ReasoningEffort => match intent {
            ThinkingIntent::FollowDefault | ThinkingIntent::EnableDefault => Ok(None),
            ThinkingIntent::Disable => {
                if cap.can_disable {
                    Ok(Some(serde_json::json!({ "reasoning_effort": "none" })))
                } else {
                    Err(ThinkingPatchError(
                        "该模型不支持关闭思考（始终开启），请选择「默认」或一个档位".into(),
                    ))
                }
            }
            ThinkingIntent::Effort(level) => {
                if !cap.levels.is_empty() && !cap.levels.contains(level) {
                    return Err(ThinkingPatchError(format!(
                        "该模型支持的思考档位：{}（收到 \"{level}\"）",
                        cap.levels.join(" / ")
                    )));
                }
                Ok(Some(serde_json::json!({ "reasoning_effort": level })))
            }
        },
        ThinkingEncoding::DeepSeekThinking => match intent {
            ThinkingIntent::FollowDefault => Ok(None),
            ThinkingIntent::Disable => {
                if cap.can_disable {
                    Ok(Some(serde_json::json!({
                        "thinking": { "type": "disabled" }
                    })))
                } else {
                    Err(ThinkingPatchError(
                        "该模型不支持关闭思考（始终开启）".into(),
                    ))
                }
            }
            ThinkingIntent::EnableDefault => Ok(Some(
                serde_json::json!({ "thinking": { "type": "enabled" } }),
            )),
            ThinkingIntent::Effort(level) => {
                // 目录已知档位：开 + 等级叠加；未知（levels 空）保持旧行为——
                // 仅开关（等级对该模型无效）
                if !cap.levels.is_empty() {
                    if !cap.levels.contains(level) {
                        return Err(ThinkingPatchError(format!(
                            "该模型支持的思考档位：{}（收到 \"{level}\"）",
                            cap.levels.join(" / ")
                        )));
                    }
                    Ok(Some(serde_json::json!({
                        "thinking": { "type": "enabled" },
                        "reasoning_effort": level,
                    })))
                } else {
                    Ok(Some(
                        serde_json::json!({ "thinking": { "type": "enabled" } }),
                    ))
                }
            }
        },
        ThinkingEncoding::AnthropicThinking => match intent {
            ThinkingIntent::FollowDefault => Ok(None),
            ThinkingIntent::Disable => {
                if cap.can_disable {
                    let off = cap.thinking_off_word.as_deref().unwrap_or("disabled");
                    Ok(Some(serde_json::json!({
                        "thinking": { "type": off }
                    })))
                } else {
                    Err(ThinkingPatchError(
                        "该模型不支持关闭思考（始终开启），Anthropic 将按服务默认推理".into(),
                    ))
                }
            }
            ThinkingIntent::EnableDefault => Ok(Some(serde_json::json!({
                "thinking": {
                    "type": "enabled",
                    "budget_tokens": anthropic_default_budget(cap),
                }
            }))),
            ThinkingIntent::Effort(level) => {
                if cap.levels.is_empty() {
                    return Err(ThinkingPatchError(
                        "该模型不支持思考等级（预算型）——请使用开关或自定义 budget_tokens".into(),
                    ));
                }
                if !cap.levels.contains(level) {
                    return Err(ThinkingPatchError(format!(
                        "该模型支持的思考档位：{}（收到 \"{level}\"）",
                        cap.levels.join(" / ")
                    )));
                }
                if cap.adaptive {
                    Ok(Some(serde_json::json!({
                        "thinking": { "type": "adaptive" },
                        "output_config": { "effort": level },
                    })))
                } else {
                    Ok(Some(serde_json::json!({
                        "output_config": { "effort": level },
                    })))
                }
            }
        },
        ThinkingEncoding::GeminiThinkingConfig => match intent {
            ThinkingIntent::FollowDefault => Ok(None),
            ThinkingIntent::Disable => {
                if cap.can_disable {
                    Ok(Some(serde_json::json!({
                        "generationConfig": { "thinkingConfig": { "thinkingBudget": 0 } }
                    })))
                } else {
                    Err(ThinkingPatchError(
                        "该模型不支持关闭思考（始终开启），Gemini 将按服务默认推理".into(),
                    ))
                }
            }
            ThinkingIntent::EnableDefault => {
                // 预算型（2.5）：默认预算；等级型（3.x）：无独立「开」——跟随默认
                if cap.budget.is_some() {
                    Ok(Some(serde_json::json!({
                        "generationConfig": { "thinkingConfig": { "thinkingBudget": GEMINI_DEFAULT_THINKING_BUDGET } }
                    })))
                } else {
                    Ok(None)
                }
            }
            ThinkingIntent::Effort(level) => {
                if cap.levels.is_empty() {
                    return Err(ThinkingPatchError(
                        "该模型为预算型思考（thinkingBudget），不支持等级档位".into(),
                    ));
                }
                if !cap.levels.contains(level) {
                    return Err(ThinkingPatchError(format!(
                        "该模型支持的思考档位：{}（收到 \"{level}\"）",
                        cap.levels.join(" / ")
                    )));
                }
                Ok(Some(serde_json::json!({
                    "generationConfig": { "thinkingConfig": { "thinkingLevel": level } }
                })))
            }
        },
        ThinkingEncoding::OllamaThink => match intent {
            ThinkingIntent::FollowDefault => Ok(None),
            ThinkingIntent::Disable => Ok(Some(serde_json::json!({ "think": false }))),
            ThinkingIntent::EnableDefault => Ok(Some(serde_json::json!({ "think": true }))),
            ThinkingIntent::Effort(level) => {
                if !cap.levels.is_empty() && !cap.levels.contains(level) {
                    return Err(ThinkingPatchError(format!(
                        "该模型支持的思考档位：{}（收到 \"{level}\"）",
                        cap.levels.join(" / ")
                    )));
                }
                Ok(Some(serde_json::json!({ "think": level })))
            }
        },
        ThinkingEncoding::None => unreachable!("None encoding handled above"),
    }
}

/// Anthropic「开·默认」预算——目录范围（≥1024）与默认 2048 取交集。
fn anthropic_default_budget(cap: &ThinkingCapability) -> u64 {
    match cap.budget {
        Some((min, max)) => ANTHROPIC_THINKING_BUDGET.clamp(min as u64, max as u64),
        None => ANTHROPIC_THINKING_BUDGET,
    }
}

/// 该 provider 是否支持思考等级下拉（兼容 0.21.17 前端语义）。
///
/// 0.27.15 起走能力单一真源：`Levels`/`Unknown`（effort 编码）→ true。
pub fn thinking_supports_effort(
    kind: ProviderKind,
    compat: ResolvedCompatDialect,
    base_url: Option<&str>,
    model_id: &str,
    style: Option<ThinkingStyle>,
) -> bool {
    resolve_thinking_capability(kind, compat, base_url, model_id, style).supports_effort()
}

// ── 私参归并与校验 ─────────────────────────────────────────────────────────

/// 剔除与显式思考意图冲突的自定义参数（0.27.15）。
///
/// raw `additional_params` 在 rig 编码顺序中位于 typed options 之后会反超本轮
/// 按钮意图；全 raw 路径下同理——用户私参若含思考字段而本轮有显式意图，
/// 必须先剔除（仅剔除该编码拥有的键，保留无关嵌套字段）：
/// - OpenAI 风格：`reasoning_effort` / `thinking`
/// - Anthropic：`thinking` / `output_config.effort`（保留 output_config 其余键）
/// - Gemini：`generationConfig.thinkingConfig`（保留 generationConfig 其余键）
/// - Ollama：`think`
pub(super) fn strip_conflicting_thinking_params(
    encoding: ThinkingEncoding,
    params: &mut serde_json::Value,
) {
    let Some(map) = params.as_object_mut() else {
        return;
    };
    match encoding {
        ThinkingEncoding::ReasoningEffort | ThinkingEncoding::DeepSeekThinking => {
            map.remove("reasoning_effort");
            map.remove("thinking");
        }
        ThinkingEncoding::AnthropicThinking => {
            map.remove("thinking");
            if let Some(output) = map
                .get_mut("output_config")
                .and_then(|v| v.as_object_mut())
            {
                output.remove("effort");
                if output.is_empty() {
                    map.remove("output_config");
                }
            }
        }
        ThinkingEncoding::GeminiThinkingConfig => {
            if let Some(generation) = map
                .get_mut("generationConfig")
                .and_then(|v| v.as_object_mut())
            {
                generation.remove("thinkingConfig");
                if generation.is_empty() {
                    map.remove("generationConfig");
                }
            }
        }
        ThinkingEncoding::OllamaThink => {
            map.remove("think");
        }
        ThinkingEncoding::None => {}
    }
}

/// 按 ProviderKind 解析剔除所需的编码风格（主窗口 RigProvider 路径使用——
/// 该路径只有 kind，无完整能力快照）。
pub(super) fn strip_conflicting_thinking_params_for_kind(
    kind: ProviderKind,
    params: &mut serde_json::Value,
) {
    let encoding = match kind {
        ProviderKind::OpenAICompatible => ThinkingEncoding::ReasoningEffort,
        ProviderKind::AnthropicMessages => ThinkingEncoding::AnthropicThinking,
        ProviderKind::GeminiGenerateContent => ThinkingEncoding::GeminiThinkingConfig,
        ProviderKind::OllamaHttp => ThinkingEncoding::OllamaThink,
    };
    strip_conflicting_thinking_params(encoding, params);
}

/// 校验最终合并参数。手动思考预算至少 1024，且必须为回答留出输出空间。
pub(super) fn validate_thinking_budget(
    kind: ProviderKind,
    params: &mut Option<serde_json::Value>,
    max_tokens: Option<u64>,
) -> Result<(), super::AIError> {
    if kind != ProviderKind::AnthropicMessages {
        return Ok(());
    }
    let Some(params) = params.as_mut() else {
        return Ok(());
    };
    let max_tokens = match (
        max_tokens,
        params.get("max_tokens").and_then(|v| v.as_u64()),
    ) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => 4096,
    };
    if params["thinking"]["type"] != "enabled" {
        return Ok(());
    }
    let budget = params["thinking"]["budget_tokens"].as_u64().unwrap_or(0);
    if max_tokens <= 1024 || budget < 1024 {
        return Err(super::AIError::Provider(
            "Anthropic 深度思考要求 max_tokens 大于 1024，且 budget_tokens 至少为 1024".into(),
        ));
    }
    params["thinking"]["budget_tokens"] = budget.min(max_tokens - 1).into();
    Ok(())
}

/// 对话请求显式意图优先于模型私参默认值，其余自定义参数保持透传。
///
/// 0.27.15：显式意图先剔除冲突私参（`strip_conflicting_thinking_params`）；
/// Gemini 预算与 Anthropic 预算同理——用户显式配置的同名字段在「开·默认」时
/// 优先于默认预算，再统一校验输出上限。
pub(super) fn merge_chat_parameters(
    encoding: ThinkingEncoding,
    custom: Option<&serde_json::Value>,
    patch: Option<serde_json::Value>,
    intent_explicit: bool,
) -> Option<serde_json::Value> {
    let mut params = custom.cloned().unwrap_or_else(|| serde_json::json!({}));
    // 预算型「开·默认」：用户显式配置的同名预算优先——strip 会剔除思考键，
    // 用户预算须先提取再回填（Anthropic budget_tokens / Gemini thinkingBudget）
    let user_anthropic_budget = patch
        .as_ref()
        .filter(|p| p.pointer("/thinking/budget_tokens").is_some())
        .and_then(|_| params.pointer("/thinking/budget_tokens").cloned());
    let user_gemini_budget = patch
        .as_ref()
        .filter(|p| p
            .pointer("/generationConfig/thinkingConfig/thinkingBudget")
            .is_some())
        .and_then(|_| {
            params
                .pointer("/generationConfig/thinkingConfig/thinkingBudget")
                .cloned()
        });
    if intent_explicit {
        strip_conflicting_thinking_params(encoding, &mut params);
    }
    if let Some(mut patch) = patch {
        if let Some(budget) = user_anthropic_budget
            && patch["thinking"]["type"] == "enabled"
        {
            patch["thinking"]["budget_tokens"] = budget;
        }
        if let Some(budget) = user_gemini_budget
            && let Some(value) = patch.pointer_mut("/generationConfig/thinkingConfig/thinkingBudget")
        {
            *value = budget;
        }
        merge_parameter_object(&mut params, patch);
        if params["thinking"]["type"] == "disabled" {
            if let Some(thinking) = params.get_mut("thinking").and_then(|v| v.as_object_mut()) {
                thinking.remove("budget_tokens");
            }
        }
    }
    (!params.as_object().is_none_or(|map| map.is_empty())).then_some(params)
}

fn merge_parameter_object(target: &mut serde_json::Value, patch: serde_json::Value) {
    match (target, patch) {
        (serde_json::Value::Object(target), serde_json::Value::Object(patch)) => {
            for (key, value) in patch {
                merge_parameter_object(target.entry(key).or_insert(serde_json::Value::Null), value);
            }
        }
        (target, value) => *target = value,
    }
}

// ── T2: reasoning_effort 400 自愈（0.21.20）──────────────────────────────────

/// 从 API 400 错误消息中解析 `reasoning_effort` 的合法档位列表。
///
/// OpenAI 风格 400 错误体形如：
/// `Unsupported value: 'xhigh' is not a valid value for 'reasoning_effort'.
///  Supported values are: 'minimal', 'low', 'medium', 'high'.`
///
/// 大小写不敏感匹配 `reasoning_effort` 上下文 + `Supported values are:` 后的引号列表。
/// 解析不到 → None（非 OpenAI 风格错误，如 DeepSeek/Anthropic/Ollama 的 400 不匹配）。
pub(crate) fn parse_supported_reasoning_efforts(message: &str) -> Option<Vec<String>> {
    let lower = message.to_ascii_lowercase();
    // 必须同时出现 reasoning_effort 和 supported values
    if !lower.contains("reasoning_effort") || !lower.contains("supported values are") {
        return None;
    }
    // 截取 "Supported values are:" 之后的部分
    let after = lower.split("supported values are").nth(1)?;
    // 提取所有引号内的值——兼容 ' 和 " 引号
    let mut values = Vec::new();
    let mut in_quote = false;
    let mut current = String::new();
    for ch in after.chars() {
        if ch == '\'' || ch == '"' {
            if in_quote {
                if !current.is_empty() {
                    values.push(current.clone());
                    current.clear();
                }
                in_quote = false;
            } else {
                in_quote = true;
            }
        } else if in_quote {
            current.push(ch);
        }
    }
    if values.is_empty() {
        None
    } else {
        // 去重，保持顺序
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<String> = values
            .into_iter()
            .filter(|v| seen.insert(v.clone()))
            .collect();
        Some(deduped)
    }
}

/// 已知 reasoning_effort 档位排序——值越大思考越强。
///
/// `minimal` < `low` < `medium` < `high`。
/// `xhigh` / `max` 等视作高于 `high`（排序值为 4）。
/// 未知自定义值也视作高于 `high`（保守降级）。
fn effort_rank(effort: &str) -> u8 {
    match effort.to_ascii_lowercase().as_str() {
        "minimal" => 0,
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        // xhigh / max / 其他自定义值 → 高于 high
        _ => 4,
    }
}

/// 从支持的档位列表中选择 fallback 档位。
///
/// 策略：选「不高于 attempted 的最高支持档」。
/// - 若 attempted 是已知档位（minimal/low/medium/high），选 ≤ 其排名的最高支持档；
/// - 若 attempted 是未知自定义值（rank=4），取支持列表中排名最高的档；
/// - 若支持列表为空 → None。
/// - 若找不到不高于 attempted 的档（attempted 太低）→ 取支持列表最低档。
pub(crate) fn pick_fallback_effort(attempted: &str, supported: &[String]) -> Option<String> {
    if supported.is_empty() {
        return None;
    }
    let attempted_rank = effort_rank(attempted);
    // 尝试选不高于 attempted 的最高支持档
    let mut best: Option<(u8, &str)> = None;
    for s in supported {
        let rank = effort_rank(s);
        if rank <= attempted_rank {
            match best {
                None => best = Some((rank, s)),
                Some((br, _)) if rank > br => best = Some((rank, s)),
                _ => {}
            }
        }
    }
    match best {
        Some((_, s)) => Some(s.to_string()),
        // 找不到不高于 attempted 的 → 取支持列表最低档
        None => supported
            .iter()
            .min_by_key(|s| effort_rank(s))
            .map(|s| s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 能力解析 ────────────────────────────────────────────────────────────

    fn cap(
        kind: ProviderKind,
        compat: ResolvedCompatDialect,
        base_url: Option<&str>,
        model: &str,
    ) -> ThinkingCapability {
        resolve_thinking_capability(kind, compat, base_url, model, None)
    }

    /// rig 内置目录的真实条目能被解析（vendor 字符串与公共 API 钉住）。
    #[test]
    fn catalog_rows_resolve_via_public_api() {
        // Anthropic：sonnet-4-5 预算型（budget ≥1024，可关）
        let sonnet = cap(
            ProviderKind::AnthropicMessages,
            ResolvedCompatDialect::Generic,
            None,
            "claude-sonnet-4-5",
        );
        assert_eq!(sonnet.shape, ThinkingShape::Switch);
        assert!(sonnet.can_disable);
        assert!(sonnet.can_enable_default);
        assert_eq!(sonnet.budget, Some((1024, 64_000)));
        // 快照后缀归一
        let dated = cap(
            ProviderKind::AnthropicMessages,
            ResolvedCompatDialect::Generic,
            None,
            "claude-sonnet-4-5-20250929",
        );
        assert_eq!(dated.shape, sonnet.shape);

        // Anthropic：opus-4-6 等级 + 预算 + 可关
        let opus = cap(
            ProviderKind::AnthropicMessages,
            ResolvedCompatDialect::Generic,
            None,
            "claude-opus-4-6",
        );
        assert_eq!(opus.shape, ThinkingShape::Levels);
        assert!(!opus.levels.is_empty());
        assert!(opus.can_disable);
        assert!(opus.budget.is_some());

        // DeepSeek：官方方言 → toggle + effort(low/high/max)
        let ds = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::DeepSeek,
            Some("https://api.deepseek.com/v1"),
            "deepseek-v4-pro",
        );
        assert_eq!(ds.shape, ThinkingShape::Levels);
        assert_eq!(ds.levels, ["low", "high", "max"]);
        assert!(ds.can_disable);
        assert!(ds.can_enable_default);

        // Gemini：2.5-pro 预算型不可关；3-pro 等级型
        let g25 = cap(
            ProviderKind::GeminiGenerateContent,
            ResolvedCompatDialect::Generic,
            None,
            "gemini-2.5-pro",
        );
        assert_eq!(g25.shape, ThinkingShape::Switch);
        assert!(!g25.can_disable, "2.5-pro 目录无 toggle——不可关闭");
        assert_eq!(g25.budget, Some((128, 32_768)));
        let g3 = cap(
            ProviderKind::GeminiGenerateContent,
            ResolvedCompatDialect::Generic,
            None,
            "gemini-3-pro-preview",
        );
        assert_eq!(g3.shape, ThinkingShape::Levels);
        assert!(!g3.can_disable);

        // Ollama：目录命中
        let ol = cap(
            ProviderKind::OllamaHttp,
            ResolvedCompatDialect::Generic,
            None,
            "qwen3:32b",
        );
        assert_ne!(ol.shape, ThinkingShape::Unsupported);

        // OpenAI 官方端点：gpt-5.4 系 → 等级
        let oa = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
        );
        assert_eq!(oa.shape, ThinkingShape::Levels);
        assert!(!oa.unknown);
    }

    /// 未知网关（Generic 方言 + 非 OpenAI 官方 host）→ Unknown，不猜能力。
    #[test]
    fn unknown_gateway_stays_unknown() {
        let c = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://my-vllm.example.com/v1"),
            "Qwen3-32B",
        );
        assert_eq!(c.shape, ThinkingShape::Unknown);
        assert!(c.unknown);
        assert!(c.levels.is_empty());
        assert!(c.supports_effort(), "未知网关保留下拉透传（现状）");
    }

    /// OpenAI 官方 hostname 精确匹配；子串/端口变体不算官方。
    #[test]
    fn openai_official_host_exact_match_only() {
        let official = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com/v1"),
            "totally-custom-model",
        );
        // 官方端点目录 miss：命名启发对非 gpt/o 名字 → 未知（能力诚实）
        assert_eq!(official.shape, ThinkingShape::Unknown);
        let known = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
        );
        assert!(!known.unknown, "官方端点 + 目录命中 → 能力已知");
        let fake = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com.evil.com/v1"),
            "gpt-5.4-mini",
        );
        assert!(fake.unknown, "api.openai.com.evil.com 不是官方——未知网关");
    }

    /// DeepSeek 模型名经第三方代理（Generic 方言）保留既有启发。
    #[test]
    fn deepseek_model_via_proxy_resolves_deepseek() {
        let c = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://my-proxy.example.com/v1"),
            "deepseek-v4-flash",
        );
        assert_eq!(c.encoding, ThinkingEncoding::DeepSeekThinking);
        assert!(c.can_enable_default);
    }

    /// 目录 miss 的 DeepSeek（官方端点 + 未知模型名）→ Unknown 开关降级。
    #[test]
    fn deepseek_catalog_miss_degrades_to_switch() {
        let c = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::DeepSeek,
            Some("https://api.deepseek.com/v1"),
            "custom-finetune",
        );
        assert_eq!(c.shape, ThinkingShape::Switch);
        assert!(c.unknown);
        assert!(c.can_enable_default, "thinking.type 开关仍可用");
    }

    /// Moonshot：k3 等级（reasoning_effort）；k2.6 开关（thinking.type）。
    #[test]
    fn moonshot_dialect_split_by_model() {
        let k3 = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Moonshot,
            Some("https://api.moonshot.cn/v1"),
            "kimi-k3",
        );
        assert_eq!(k3.shape, ThinkingShape::Levels);
        assert_eq!(k3.encoding, ThinkingEncoding::ReasoningEffort);
        let k26 = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Moonshot,
            Some("https://api.moonshot.cn/v1"),
            "kimi-k2.6",
        );
        assert_eq!(k26.encoding, ThinkingEncoding::DeepSeekThinking);
        assert!(k26.can_enable_default);
    }

    /// style 逃生口：Effort 强制等级（不限档）；Toggle 强制 thinking.type 开关。
    #[test]
    fn style_override_forces_shape() {
        let effort = resolve_thinking_capability(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com/v1"),
            "o3",
            Some(ThinkingStyle::Effort),
        );
        assert_eq!(effort.shape, ThinkingShape::Levels);
        assert!(effort.levels.is_empty(), "强制 Effort 不限档（透传）");
        assert!(effort
            .supports_effort());

        let toggle = resolve_thinking_capability(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://my-gw.example.com/v1"),
            "some-model",
            Some(ThinkingStyle::Toggle),
        );
        assert_eq!(toggle.shape, ThinkingShape::Switch);
        assert_eq!(toggle.encoding, ThinkingEncoding::DeepSeekThinking);
    }

    /// OpenAI 官方端点目录 miss 命名启发：o3 → 等级且不可关；gpt-4o → 不支持。
    #[test]
    fn openai_named_heuristics() {
        let o3 = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com/v1"),
            "o3-mini",
        );
        assert_eq!(o3.shape, ThinkingShape::Levels);
        assert!(!o3.can_disable, "o-series 不支持 effort none");
        let gpt4o = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com/v1"),
            "gpt-4o",
        );
        assert_eq!(gpt4o.shape, ThinkingShape::Unsupported);
    }

    /// Gemini 家族启发：2.5-flash 预算可关；gemini-2.0 不支持。
    #[test]
    fn gemini_family_heuristics() {
        let flash = cap(
            ProviderKind::GeminiGenerateContent,
            ResolvedCompatDialect::Generic,
            None,
            "gemini-2.5-flash-preview-09-2025",
        );
        assert_eq!(flash.shape, ThinkingShape::Switch);
        assert!(flash.can_disable);
        assert_eq!(flash.budget, Some((0, 24_576)));
        let none = cap(
            ProviderKind::GeminiGenerateContent,
            ResolvedCompatDialect::Generic,
            None,
            "gemini-2.0-flash",
        );
        assert_eq!(none.shape, ThinkingShape::Unsupported);
    }

    // ── 意图编码 ────────────────────────────────────────────────────────────

    fn patch(
        cap: &ThinkingCapability,
        intent: ThinkingIntent,
    ) -> Result<Option<serde_json::Value>, ThinkingPatchError> {
        thinking_request_patch(cap, &intent)
    }

    /// DeepSeek：开关 + 等级组合（V3.2+ 目录声明 low/high/max）。
    #[test]
    fn deepseek_switch_and_effort_combo() {
        let c = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::DeepSeek,
            Some("https://api.deepseek.com/v1"),
            "deepseek-v4-flash",
        );
        assert_eq!(
            patch(&c, ThinkingIntent::Disable).unwrap(),
            Some(serde_json::json!({ "thinking": { "type": "disabled" } }))
        );
        assert_eq!(
            patch(&c, ThinkingIntent::EnableDefault).unwrap(),
            Some(serde_json::json!({ "thinking": { "type": "enabled" } }))
        );
        assert_eq!(patch(&c, ThinkingIntent::FollowDefault).unwrap(), None);
        assert_eq!(
            patch(&c, ThinkingIntent::Effort("high".into())).unwrap(),
            Some(serde_json::json!({
                "thinking": { "type": "enabled" },
                "reasoning_effort": "high",
            }))
        );
        // 越界等级 → 本地可读拒绝
        assert!(patch(&c, ThinkingIntent::Effort("medium".into())).is_err());
    }

    /// DeepSeek 目录 miss（等级未知）：等级意图退化为开关（旧行为——等级无效）。
    #[test]
    fn deepseek_unknown_effort_degrades_to_switch() {
        let c = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::DeepSeek,
            Some("https://api.deepseek.com/v1"),
            "old-custom-model",
        );
        assert_eq!(
            patch(&c, ThinkingIntent::Effort("high".into())).unwrap(),
            Some(serde_json::json!({ "thinking": { "type": "enabled" } }))
        );
    }

    /// OpenAI 风格：默认/显式等级/关闭/自定义透传。
    #[test]
    fn reasoning_effort_encoding() {
        let unknown_gw = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://my-gw.example.com/v1"),
            "some-model",
        );
        // 未知网关：默认 omit、任意档位/自定义透传（现状）
        assert_eq!(patch(&unknown_gw, ThinkingIntent::FollowDefault).unwrap(), None);
        assert_eq!(
            patch(&unknown_gw, ThinkingIntent::Effort("xhigh".into())).unwrap(),
            Some(serde_json::json!({ "reasoning_effort": "xhigh" }))
        );
        assert_eq!(
            patch(&unknown_gw, ThinkingIntent::Effort("20-50-80".into())).unwrap(),
            Some(serde_json::json!({ "reasoning_effort": "20-50-80" }))
        );
        assert_eq!(
            patch(&unknown_gw, ThinkingIntent::Disable).unwrap(),
            Some(serde_json::json!({ "reasoning_effort": "none" }))
        );

        // 官方端点已知档位（gpt-5.4 目录含 xhigh）：越界拒绝
        let official = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
        );
        assert!(official.levels.contains(&"xhigh".to_string()));
        assert!(patch(&official, ThinkingIntent::Effort("20-50-80".into())).is_err());
        assert!(patch(&official, ThinkingIntent::Effort("xhigh".into())).is_ok());
    }

    /// o-series（不可关）：Disable 本地拒绝，不白付 400。
    #[test]
    fn cannot_disable_model_rejects_off() {
        let o3 = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com/v1"),
            "o3-mini",
        );
        assert!(patch(&o3, ThinkingIntent::Disable).is_err());
        let g25 = cap(
            ProviderKind::GeminiGenerateContent,
            ResolvedCompatDialect::Generic,
            None,
            "gemini-2.5-pro",
        );
        assert!(patch(&g25, ThinkingIntent::Disable).is_err());
    }

    /// Anthropic：manual budget 默认 / adaptive 等级 / disabled。
    #[test]
    fn anthropic_encoding_matrix() {
        let sonnet = cap(
            ProviderKind::AnthropicMessages,
            ResolvedCompatDialect::Generic,
            None,
            "claude-sonnet-4-5",
        );
        assert_eq!(
            patch(&sonnet, ThinkingIntent::Disable).unwrap(),
            Some(serde_json::json!({ "thinking": { "type": "disabled" } }))
        );
        assert_eq!(
            patch(&sonnet, ThinkingIntent::EnableDefault).unwrap(),
            Some(serde_json::json!({
                "thinking": { "type": "enabled", "budget_tokens": 2048 }
            }))
        );
        // 预算型模型不支持等级
        assert!(patch(&sonnet, ThinkingIntent::Effort("high".into())).is_err());

        let opus = cap(
            ProviderKind::AnthropicMessages,
            ResolvedCompatDialect::Generic,
            None,
            "claude-opus-4-6",
        );
        let adaptive = patch(&opus, ThinkingIntent::Effort("high".into())).unwrap();
        // opus-4-6 目录 adaptive_thinking=true → thinking.type=adaptive + output_config
        assert_eq!(
            adaptive,
            Some(serde_json::json!({
                "thinking": { "type": "adaptive" },
                "output_config": { "effort": "high" },
            }))
        );
    }

    /// Gemini：等级 thinkingLevel / 预算 thinkingBudget / 关闭 budget 0。
    #[test]
    fn gemini_encoding_matrix() {
        let g3 = cap(
            ProviderKind::GeminiGenerateContent,
            ResolvedCompatDialect::Generic,
            None,
            "gemini-3-pro-preview",
        );
        assert_eq!(
            patch(&g3, ThinkingIntent::Effort("high".into())).unwrap(),
            Some(serde_json::json!({
                "generationConfig": { "thinkingConfig": { "thinkingLevel": "high" } }
            }))
        );
        // 3 代不可关
        assert!(patch(&g3, ThinkingIntent::Disable).is_err());

        let flash = cap(
            ProviderKind::GeminiGenerateContent,
            ResolvedCompatDialect::Generic,
            None,
            "gemini-2.5-flash",
        );
        assert_eq!(
            patch(&flash, ThinkingIntent::Disable).unwrap(),
            Some(serde_json::json!({
                "generationConfig": { "thinkingConfig": { "thinkingBudget": 0 } }
            }))
        );
        assert_eq!(
            patch(&flash, ThinkingIntent::EnableDefault).unwrap(),
            Some(serde_json::json!({
                "generationConfig": { "thinkingConfig": { "thinkingBudget": 2048 } }
            }))
        );
    }

    /// Ollama：bool / 命名等级。
    #[test]
    fn ollama_encoding() {
        let qwen = cap(
            ProviderKind::OllamaHttp,
            ResolvedCompatDialect::Generic,
            None,
            "qwen3:32b",
        );
        for enabled in [true, false] {
            let intent = if enabled {
                ThinkingIntent::EnableDefault
            } else {
                ThinkingIntent::Disable
            };
            let p = patch(&qwen, intent).unwrap();
            assert_eq!(p, Some(serde_json::json!({ "think": enabled })));
        }
        // 本地未知模型（目录 miss）→ 开关仍可用
        let custom = cap(
            ProviderKind::OllamaHttp,
            ResolvedCompatDialect::Generic,
            None,
            "my-finetune:latest",
        );
        assert_eq!(
            patch(&custom, ThinkingIntent::EnableDefault).unwrap(),
            Some(serde_json::json!({ "think": true }))
        );
    }

    /// Unsupported：显式意图全部本地拒绝；FollowDefault omit。
    #[test]
    fn unsupported_rejects_explicit_intents() {
        let gpt4o = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com/v1"),
            "gpt-4o",
        );
        assert_eq!(gpt4o.shape, ThinkingShape::Unsupported);
        assert_eq!(patch(&gpt4o, ThinkingIntent::FollowDefault).unwrap(), None);
        assert!(patch(&gpt4o, ThinkingIntent::Disable).is_err());
        assert!(patch(&gpt4o, ThinkingIntent::Effort("high".into())).is_err());
    }

    /// 意图翻译：下拉线值 / 开关布尔 / 主窗口默认策略。
    #[test]
    fn intent_translation() {
        use ThinkingIntent::*;
        assert_eq!(ThinkingIntent::from_request(true, None, true), FollowDefault);
        assert_eq!(ThinkingIntent::from_request(true, Some(""), false), FollowDefault);
        assert_eq!(ThinkingIntent::from_request(true, Some("none"), true), Disable);
        assert_eq!(
            ThinkingIntent::from_request(true, Some("default_on"), true),
            EnableDefault
        );
        assert_eq!(
            ThinkingIntent::from_request(true, Some("high"), true),
            Effort("high".into())
        );
        assert_eq!(ThinkingIntent::from_request(false, None, true), EnableDefault);
        assert_eq!(ThinkingIntent::from_request(false, Some("high"), false), Disable);

        // 主窗口默认策略：等级型 omit；开关型显式关
        let ds = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::DeepSeek,
            Some("https://api.deepseek.com/v1"),
            "deepseek-v4-flash",
        );
        assert_eq!(ThinkingIntent::main_window(None, &ds), Disable);
        assert_eq!(
            ThinkingIntent::main_window(Some("high"), &ds),
            Effort("high".into())
        );
        let gw = cap(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://my-gw.example.com/v1"),
            "some-model",
        );
        assert_eq!(ThinkingIntent::main_window(None, &gw), FollowDefault);
    }

    /// supports_effort：下拉形态判定（兼容旧语义）。
    #[test]
    fn supports_effort_scope() {
        // 未知网关 → 下拉透传
        assert!(thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://gw.example.com/v1"),
            "any-model",
            None,
        ));
        // OpenAI 官方 → 下拉
        assert!(thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            None,
        ));
        // gpt-4o 不支持 → 开关都无（Unsupported）
        assert!(!thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            ResolvedCompatDialect::Generic,
            Some("https://api.openai.com/v1"),
            "gpt-4o",
            None,
        ));
        // Anthropic 预算型 → 开关（false）
        assert!(!thinking_supports_effort(
            ProviderKind::AnthropicMessages,
            ResolvedCompatDialect::Generic,
            None,
            "claude-sonnet-4-5",
            None,
        ));
        // Ollama → 开关
        assert!(!thinking_supports_effort(
            ProviderKind::OllamaHttp,
            ResolvedCompatDialect::Generic,
            None,
            "qwen3:32b",
            None,
        ));
    }

    /// ModelEntry serde: 老配置缺 thinking_style 字段 → None（零迁移）。
    #[test]
    fn model_entry_serde_legacy_without_thinking_style() {
        use crate::config::ai_config::ModelEntry;
        let legacy = serde_json::json!({
            "id": "test-model",
            "display_name": "Test",
            "enabled": true,
            "capabilities": ["chat"]
        });
        let entry: ModelEntry = serde_json::from_value(legacy).expect("反序列化应成功");
        assert_eq!(entry.thinking_style, None);
    }

    /// ModelEntry serde: thinking_style 各值正确解析。
    #[test]
    fn model_entry_serde_with_thinking_style() {
        use crate::config::ai_config::{ModelEntry, ThinkingStyle};
        let with_effort = serde_json::json!({
            "id": "m1", "display_name": "M1", "enabled": true,
            "capabilities": ["chat"], "thinking_style": "effort"
        });
        let entry: ModelEntry = serde_json::from_value(with_effort).expect("反序列化应成功");
        assert_eq!(entry.thinking_style, Some(ThinkingStyle::Effort));

        let with_toggle = serde_json::json!({
            "id": "m2", "display_name": "M2", "enabled": true,
            "capabilities": ["chat"], "thinking_style": "toggle"
        });
        let entry: ModelEntry = serde_json::from_value(with_toggle).expect("反序列化应成功");
        assert_eq!(entry.thinking_style, Some(ThinkingStyle::Toggle));

        let with_auto = serde_json::json!({
            "id": "m3", "display_name": "M3", "enabled": true,
            "capabilities": ["chat"], "thinking_style": "auto"
        });
        let entry: ModelEntry = serde_json::from_value(with_auto).expect("反序列化应成功");
        assert_eq!(entry.thinking_style, Some(ThinkingStyle::Auto));
    }

    // ── T2: parse_supported_reasoning_efforts / pick_fallback_effort ──────────

    /// 标准 OpenAI 400 错误体——解析出 4 个合法档位。
    #[test]
    fn parse_standard_openai_400() {
        let msg = "Unsupported value: 'xhigh' is not a valid value for 'reasoning_effort'. Supported values are: 'minimal', 'low', 'medium', 'high'.";
        let result = parse_supported_reasoning_efforts(msg);
        assert_eq!(
            result,
            Some(vec![
                "minimal".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
            ])
        );
    }

    /// 大小写不敏感——消息全大写也能解析。
    #[test]
    fn parse_case_insensitive() {
        let msg = "UNSUPPORTED VALUE: 'XHIGH' IS NOT A VALID VALUE FOR 'REASONING_EFFORT'. SUPPORTED VALUES ARE: 'MINIMAL', 'LOW', 'MEDIUM', 'HIGH'.";
        let result = parse_supported_reasoning_efforts(msg);
        assert_eq!(
            result,
            Some(vec![
                "minimal".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
            ])
        );
    }

    /// 无 Supported values 列表 → None。
    #[test]
    fn parse_no_supported_values_returns_none() {
        let msg = "Invalid reasoning_effort value";
        assert_eq!(parse_supported_reasoning_efforts(msg), None);
    }

    /// 非 reasoning_effort 相关的 400 → None。
    #[test]
    fn parse_non_reasoning_effort_400_returns_none() {
        let msg = "Unsupported value: 'foo' is not a valid value for 'temperature'. Supported values are: '0', '1', '2'.";
        assert_eq!(parse_supported_reasoning_efforts(msg), None);
    }

    /// DeepSeek thinking.type 的 400 不含 supported values → None。
    #[test]
    fn parse_deepseek_400_returns_none() {
        let msg = "Invalid thinking type: enabled";
        assert_eq!(parse_supported_reasoning_efforts(msg), None);
    }

    /// 双引号也能解析。
    #[test]
    fn parse_double_quotes() {
        let msg = "Unsupported value: \"xhigh\" is not a valid value for \"reasoning_effort\". Supported values are: \"minimal\", \"low\", \"medium\", \"high\".";
        let result = parse_supported_reasoning_efforts(msg);
        assert_eq!(
            result,
            Some(vec![
                "minimal".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
            ])
        );
    }

    /// 去重——重复档位只保留一份。
    #[test]
    fn parse_dedup_values() {
        let msg = "Unsupported value for 'reasoning_effort'. Supported values are: 'low', 'low', 'high', 'high'.";
        let result = parse_supported_reasoning_efforts(msg);
        assert_eq!(result, Some(vec!["low".to_string(), "high".to_string(),]));
    }

    /// pick_fallback_effort: attempted=xhigh（未知高值）→ 选支持列表中最高档 high。
    #[test]
    fn fallback_xhigh_picks_high() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("xhigh", &supported),
            Some("high".to_string())
        );
    }

    /// pick_fallback_effort: attempted=high → 选 high（恰好匹配）。
    #[test]
    fn fallback_high_picks_high() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("high", &supported),
            Some("high".to_string())
        );
    }

    /// pick_fallback_effort: attempted=medium → 选 medium（不选 high，因 high > medium）。
    #[test]
    fn fallback_medium_picks_medium() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("medium", &supported),
            Some("medium".to_string())
        );
    }

    /// pick_fallback_effort: attempted=low → 选 low。
    #[test]
    fn fallback_low_picks_low() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("low", &supported),
            Some("low".to_string())
        );
    }

    /// pick_fallback_effort: attempted=minimal → 选 minimal（恰好最低）。
    #[test]
    fn fallback_minimal_picks_minimal() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("minimal", &supported),
            Some("minimal".to_string())
        );
    }

    /// pick_fallback_effort: 支持列表不含 minimal（如老 o1 只收 low/medium/high），
    /// attempted=minimal → 取支持列表最低档 low。
    #[test]
    fn fallback_minimal_with_no_minimal_picks_low() {
        let supported = vec!["low".to_string(), "medium".to_string(), "high".to_string()];
        assert_eq!(
            pick_fallback_effort("minimal", &supported),
            Some("low".to_string())
        );
    }

    /// pick_fallback_effort: 自定义值 20-50-80 → 取支持列表最高档。
    #[test]
    fn fallback_custom_value_picks_highest() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("20-50-80", &supported),
            Some("high".to_string())
        );
    }

    /// pick_fallback_effort: 空支持列表 → None。
    #[test]
    fn fallback_empty_supported_returns_none() {
        assert_eq!(pick_fallback_effort("high", &[]), None);
    }

    /// pick_fallback_effort: 支持列表只有 high，attempted=low → 取 high（兜底取最低档=唯一档）。
    #[test]
    fn fallback_only_high_attempted_low() {
        let supported = vec!["high".to_string()];
        assert_eq!(
            pick_fallback_effort("low", &supported),
            Some("high".to_string())
        );
    }
}

#[cfg(test)]
mod request_parameter_tests {
    use super::*;
    use serde_json::json;

    /// 显式意图剔除冲突私参、保留无关嵌套字段；FollowDefault 全保留。
    #[test]
    fn explicit_intent_strips_conflicting_custom_params() {
        // OpenAI 风格：effort 意图剔除 reasoning_effort/thinking，保留 top_p/extra
        let custom = json!({
            "reasoning_effort": "low",
            "thinking": {"type": "enabled"},
            "top_p": 0.8,
            "extra": {"nested": true},
        });
        let merged = merge_chat_parameters(
            ThinkingEncoding::ReasoningEffort,
            Some(&custom),
            Some(json!({"reasoning_effort": "high"})),
            true,
        )
        .unwrap();
        assert_eq!(merged["reasoning_effort"], "high");
        assert_eq!(merged["top_p"], 0.8);
        assert_eq!(merged["extra"], custom["extra"]);
        assert!(merged.get("thinking").is_none(), "显式 effort 意图剔除 thinking 私参");

        // FollowDefault：私参原样透传（现状语义）
        let untouched = merge_chat_parameters(
            ThinkingEncoding::ReasoningEffort,
            Some(&custom),
            None,
            false,
        )
        .unwrap();
        assert_eq!(untouched, custom);

        // Anthropic：剔除 thinking + output_config.effort，保留 output_config 其余键
        let anthropic_custom = json!({
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "output_config": {"effort": "low", "max_output": 1},
            "top_k": 5,
        });
        let merged = merge_chat_parameters(
            ThinkingEncoding::AnthropicThinking,
            Some(&anthropic_custom),
            Some(json!({"thinking": {"type": "disabled"}})),
            true,
        )
        .unwrap();
        assert_eq!(merged["thinking"], json!({"type": "disabled"}));
        assert_eq!(merged["output_config"], json!({"max_output": 1}));
        assert_eq!(merged["top_k"], 5);

        // Gemini：剔除 generationConfig.thinkingConfig，保留 generationConfig 其余键
        let gemini_custom = json!({
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 512}, "temperature": 0.5},
            "safety": "block",
        });
        let merged = merge_chat_parameters(
            ThinkingEncoding::GeminiThinkingConfig,
            Some(&gemini_custom),
            Some(json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 2048}}})),
            true,
        )
        .unwrap();
        // 用户静态预算（512）优先于「开·默认」默认预算（2048）——开关无预算输入，
        // 静态配置是唯一预算入口（0.21.16 语义保留）
        assert_eq!(
            merged["generationConfig"],
            json!({"temperature": 0.5, "thinkingConfig": {"thinkingBudget": 512}})
        );
        assert_eq!(merged["safety"], "block");

        // Ollama：剔除 think
        let ollama_custom = json!({"think": true, "num_ctx": 8192});
        let merged = merge_chat_parameters(
            ThinkingEncoding::OllamaThink,
            Some(&ollama_custom),
            Some(json!({"think": false})),
            true,
        )
        .unwrap();
        assert_eq!(merged["think"], false);
        assert_eq!(merged["num_ctx"], 8192);
    }

    /// 用户显式预算优先于「开·默认」默认预算（Anthropic / Gemini）。
    #[test]
    fn user_budget_wins_over_default_enable() {
        let custom = json!({"thinking": {"type": "enabled", "budget_tokens": 4096, "note": "x"}});
        let merged = merge_chat_parameters(
            ThinkingEncoding::AnthropicThinking,
            Some(&custom),
            Some(json!({"thinking": {"type": "enabled", "budget_tokens": 2048}})),
            true,
        )
        .unwrap();
        assert_eq!(merged["thinking"]["budget_tokens"], 4096);

        let gemini_custom =
            json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 512}}});
        let merged = merge_chat_parameters(
            ThinkingEncoding::GeminiThinkingConfig,
            Some(&gemini_custom),
            Some(json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 2048}}})),
            true,
        )
        .unwrap();
        assert_eq!(
            merged["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            512
        );
    }

    /// 对话参数归并：请求控制只覆盖相关字段（沿用既有语义回归）。
    #[test]
    fn chat_parameters_preserve_custom_fields_and_override_only_request_controls() {
        let custom = json!({"thinking":{"type":"enabled","budget_tokens":4096},"reasoning_effort":"low","top_p":0.8,"extra":{"nested":true}});
        let merged = merge_chat_parameters(
            ThinkingEncoding::ReasoningEffort,
            Some(&custom),
            Some(json!({"reasoning_effort":"high"})),
            true,
        ).unwrap();
        assert_eq!(merged["top_p"], 0.8);
        assert_eq!(merged["extra"], custom["extra"]);
        assert!(
            merge_chat_parameters(ThinkingEncoding::ReasoningEffort, None, None, false).is_none()
        );
        let custom = json!({"thinking":{"type":"enabled","budget_tokens":4096,"display":"omitted"}});
        let disabled = merge_chat_parameters(
            ThinkingEncoding::AnthropicThinking,
            Some(&custom),
            Some(json!({"thinking":{"type":"disabled"}})),
            true,
        ).unwrap();
        assert!(disabled["thinking"].get("budget_tokens").is_none());
    }

    /// Anthropic manual 预算随最终输出上限校验/收紧（沿用既有语义回归）。
    #[test]
    fn anthropic_manual_budget_follows_final_output_limit() {
        for (max, expected) in [(1025, 1024), (2048, 2047), (4096, 2048)] {
            let mut params = Some(json!({"thinking":{"type":"enabled","budget_tokens":2048}}));
            validate_thinking_budget(ProviderKind::AnthropicMessages, &mut params, Some(max))
                .unwrap();
            assert_eq!(params.unwrap()["thinking"]["budget_tokens"], expected);
        }
        let mut params = Some(json!({"thinking":{"type":"enabled","budget_tokens":2048}}));
        assert!(
            validate_thinking_budget(ProviderKind::AnthropicMessages, &mut params, Some(1024))
                .is_err()
        );
        let mut custom_limit =
            Some(json!({"max_tokens":1024,"thinking":{"type":"enabled","budget_tokens":2048}}));
        assert!(
            validate_thinking_budget(
                ProviderKind::AnthropicMessages,
                &mut custom_limit,
                Some(4096)
            )
            .is_err()
        );
        let mut off = Some(json!({"thinking":{"type":"disabled"}}));
        assert!(
            validate_thinking_budget(ProviderKind::AnthropicMessages, &mut off, Some(600)).is_ok()
        );
    }
}
