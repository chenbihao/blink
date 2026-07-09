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

    // ── SLO 覆盖(§3.3 骨架层) ─────────────────────────────────────────────
    /// 单次路由调用硬超时(毫秒)。`None` → 用 default 2500ms。
    /// 用户可在设置页调,但不建议 > 3000(会破坏 fallback 平滑)。
    #[serde(default)]
    pub slo_hard_timeout_ms: Option<u32>,
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
            slo_hard_timeout_ms: None,
        }
    }
}

impl ConfigKey for AIConfig {
    const KEY: &'static str = "app.ai";
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProviderKind {
    /// OpenAI Chat Completions 协议——涵盖 OpenAI 官方 / DeepSeek / 硅基流动 /
    /// Moonshot / Groq / OpenRouter / xAI / 自建代理等所有兼容 `/v1/chat/completions`
    /// 的平台。base_url 由用户填(前端预设下拉可一键填充)。
    ///
    /// **迁移别名**:老配置里 `"openai" / "deepseek" / "openai_compat"` 全部落到这里。
    #[serde(rename = "openai_compatible", alias = "openai", alias = "deepseek", alias = "openai_compat")]
    OpenAICompatible,

    /// Anthropic Messages 协议——`/v1/messages`,仅 Claude 官方。
    #[serde(rename = "anthropic_messages", alias = "anthropic")]
    AnthropicMessages,

    /// Google Gemini GenerateContent 协议——`/v1beta/models/*:generateContent`,仅 Gemini。
    #[serde(rename = "gemini_generate_content")]
    GeminiGenerateContent,
}

/// 一个模型的元数据——纯记账,不驱动行为。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelEntry {
    /// 供应商 model id(如 `"gpt-5-nano" / "claude-opus-4-8"`)。
    pub id: String,

    /// 用户可读展示名(自定义,不影响调用)。
    pub display_name: String,

    /// 上下文窗口大小(可选)。前端提示"这个模型能吃多少 token"。
    #[serde(default)]
    pub context_window: Option<u32>,

    /// input 单价(美元 / 1M tokens),可选。
    #[serde(default)]
    pub input_price_per_million: Option<f32>,

    /// output 单价(美元 / 1M tokens),可选。
    #[serde(default)]
    pub output_price_per_million: Option<f32>,
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
                // 悬空引用——warn 后继续降级
                tracing::warn!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    requested = ?tier, actual = ?actual,
                    provider_id = %a.provider_id, model_id = %a.model_id,
                    "AI 档位引用悬空,降级到下一档"
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

    /// 查找 assignment 对应的 (Provider, Model);任一悬空返回 None。
    fn find_provider_model(&self, a: &TierAssignment) -> Option<(&ProviderEntry, &ModelEntry)> {
        let provider = self.providers.iter().find(|p| p.id == a.provider_id)?;
        let model = provider.models.iter().find(|m| m.id == a.model_id)?;
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
                context_window: Some(128_000),
                input_price_per_million: Some(0.1),
                output_price_per_million: Some(0.4),
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
        assert!(!s.to_lowercase().contains("api_key"), "序列化含 api_key: {s}");
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
        assert!(c.providers.is_empty());
    }

    #[test]
    fn config_key_uses_dedicated_kv_key() {
        // §6:第 7 分片,key = "app.ai"——不冲突已有分片
        assert_eq!(AIConfig::KEY, "app.ai");
    }

    #[test]
    fn provider_kind_serializes_stable_strings() {
        // 显式 rename 稳定输出(0.9.2 第二步:按协议 3 类)
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

        // 反序列化 roundtrip
        let k: ProviderKind = serde_json::from_str("\"openai_compatible\"").unwrap();
        assert_eq!(k, ProviderKind::OpenAICompatible);
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
            slo_hard_timeout_ms: Some(3000),
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
        assert_eq!(restored.slo_hard_timeout_ms, Some(3000));
    }
}
