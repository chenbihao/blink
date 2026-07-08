//! 具体 Provider 工厂——按 `ProviderKind` 分派到 rig-core 实体或本地实现。
//!
//! ## 阶段与状态(0.9.1 Phase 5a)
//!
//! **本文件是"骨架 + 占位"**:
//! - `NoopFactory` 兜底占位——所有 build 请求都返 `NotConfigured`。
//!   用于 0.9.1 Phase 5a-6 中间态:AIConfig 已配置但 factory 不构造实例,
//!   `resolve_tier` 一律 NotConfigured,SearchService fallback 常规 fuzzy。
//! - `RigFactory` (0.9.1 Phase 5b) —— 接 rig `providers::openai/anthropic/...`,
//!   真跑 completion。**留 5b 落**,因为需要:
//!     - 密钥从 CM 读:`secret::load_secret(&entry.id, "key")`
//!     - reqwest Client 冷构造 + `.timeout()` 挂 §3.3 硬超时
//!     - rig `CompletionModel::completion_request` 调用
//!   现在没有前端设置页无法端到端验证,先留骨架。
//!
//! ## 为什么必须先落骨架
//!
//! §6.4 兜底铁则:AI 配置错误不能破坏主链路。`NoopFactory` 让老用户"AI 未配置"
//! 路径**运行时零冒烟**——即使 AppContext 持了 AIProviderRegistry,dispatch 也
//! 走 NotConfigured 兜底。这个铁则不能靠"用户没配就跳过 registry 构造"实现,
//! 因为 0.9.2 起 SearchService 需要一个稳定的 `ai_registry` 引用。

use std::sync::Arc;

use crate::app::ai_config::{ModelEntry, ProviderEntry};
use crate::domain::ai::provider::{AIError, AIProvider};
use crate::domain::ai::registry::ProviderFactory;

/// **占位 factory** —— 所有 `build` 请求返 `NotConfigured`。
///
/// **用途**:0.9.1 Phase 5a-6 中间态。老用户 AI 未配置 →
/// registry 无实例 → dispatch NotConfigured → SearchService fallback。
///
/// **Phase 5b 替换**:换成 `RigFactory` 接真 rig client。此 factory 保留作为
/// 单测 baseline("我没接 rig 时也能证明 registry 骨架正确")。
pub struct NoopFactory;

impl ProviderFactory for NoopFactory {
    fn build(
        &self,
        entry: &ProviderEntry,
        model: &ModelEntry,
    ) -> Result<Arc<dyn AIProvider>, AIError> {
        // 记 warn 便于开发期发现"你以为接了但没接"
        tracing::warn!(
            target: crate::infra::utils::perf::ai_slo::TARGET,
            provider_id = %entry.id,
            model_id = %model.id,
            kind = ?entry.kind,
            "NoopFactory 拒绝构造 provider——Phase 5b 起接 rig 才真跑"
        );
        Err(AIError::NotConfigured)
    }
}

/// 默认 factory 构造——Phase 5b 起返回 `RigFactory`,当前返回 `NoopFactory`。
///
/// **调用位置**:`main.rs::setup`。挂进 `AIProviderRegistry`。
#[allow(dead_code)] // 0.9.1 Phase 5a 定义,main.rs 起消费
pub fn default_factory() -> Arc<dyn ProviderFactory> {
    Arc::new(NoopFactory)
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ai_config::{ProviderKind};

    fn sample_entry() -> ProviderEntry {
        ProviderEntry {
            id: "p1".into(),
            display_name: "Test".into(),
            kind: ProviderKind::OpenAI,
            base_url: None,
            secret_ref: "blink/p1/key".into(),
            models: Vec::new(),
            created_at: 0,
        }
    }

    fn sample_model() -> ModelEntry {
        ModelEntry {
            id: "m1".into(),
            display_name: "M1".into(),
            context_window: None,
            input_price_per_million: None,
            output_price_per_million: None,
        }
    }

    #[test]
    fn noop_factory_always_returns_not_configured() {
        let f = NoopFactory;
        let result = f.build(&sample_entry(), &sample_model());
        assert!(matches!(result, Err(AIError::NotConfigured)));
    }

    #[test]
    fn default_factory_returns_noop_in_phase_5a() {
        // 0.9.1 Phase 5a:default 是 NoopFactory
        // 0.9.1 Phase 5b 起:替换为 RigFactory,该测试需同步改
        let f = default_factory();
        let result = f.build(&sample_entry(), &sample_model());
        assert!(
            matches!(result, Err(AIError::NotConfigured)),
            "Phase 5a default 必须仍是 NoopFactory"
        );
    }
}
