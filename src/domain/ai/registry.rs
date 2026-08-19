//! `AIProviderRegistry` —— 运行时 Provider 池 + 三档 dispatch。
//!
//! **职责**:
//! - 持有当前 `AIConfig::providers` 对应的 `Arc<dyn AIProvider>` 实例池
//! - 按 `Tier` dispatch → 找到对应 provider(空档降级见 `AIConfig::resolve_tier`)
//! - Provider 切换零重启(§4.4 骨架条 #7)——`reload(new_config)` 增量重建池
//!
//! **构造模型**:
//! Provider 的构造需要 rig-core `Client` 实体,不同 `ProviderKind` 走不同 rig
//! 模块。为了 Phase 5a 能先跑 dispatch loop 不引入 rig 网络代码,这里定义
//! `ProviderFactory` trait —— 5b 落 rig client 时提供 `RigProviderFactory` 实现,
//! 单测时用 `MockProviderFactory`。
//!
//! **cache 策略**:key = `(provider_id, model_id)` + fingerprint。Provider 切换
//! 配置时,未变动的 `(pid, mid, fingerprint)` 保留旧实例(不重建 rig Client);
//! **任一影响 rig client 的字段变动**(kind / base_url / 密钥版本)都 invalidate。
//!
//! **§6.4 兜底铁则**:AI 配置错误绝不破坏主链路——
//! - `resolve_tier` 返 `Err(NotConfigured)` → 上层 SearchService fallback 常规 fuzzy
//! - factory 构造失败 → 单个 provider skip + tracing::error,其他 provider 正常
//! - 全部构造失败 → registry 是空的,`resolve_tier` 一律 NotConfigured

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::domain::ai::provider::{AIError, AIProvider};
use crate::domain::config::ai_config::{AIConfig, ModelEntry, ProviderEntry, ProviderKind, Tier};

/// Provider 构造工厂——把 `ProviderEntry` + `ModelEntry` 变成 `Arc<dyn AIProvider>`。
///
/// **为什么用 trait 而不是自由函数**:
/// - Phase 5b 用 `RigProviderFactory` 接 rig-core;单测用 `MockProviderFactory`
/// - 未来 0.11 加本地模型(ollama / mistral.rs)时挂第二个 factory,不动 registry
#[allow(dead_code)] // 0.9.1 Phase 5a 定义,5b 起被 AppContext 消费
pub trait ProviderFactory: Send + Sync {
    /// 按 provider + model 构造 dyn 对象。
    ///
    /// **失败姿态**:返回 `AIError::NotConfigured / Provider(...)`,但**不能 panic**。
    /// registry 拿到 Err 会 skip 掉该 (pid, mid),不影响其他 provider。
    fn build(
        &self,
        entry: &ProviderEntry,
        model: &ModelEntry,
    ) -> Result<Arc<dyn AIProvider>, AIError>;
}

/// Provider 缓存 key —— `(pid, mid, fingerprint)`。
///
/// **fingerprint** 是"影响 rig client 构造的所有字段"的紧凑摘要:
/// `kind` + `base_url` + `secret_epoch`(密钥版本号,registry 实例内单调递增)。
/// 用户改任一字段都会让老 key 不再命中,强制重建实例——**这是"改了配置不生效"
/// 这类 bug 的根治方案**。
///
/// **不哈希 API key 本身**:密钥值绝不进 struct 字段。用一个 registry 内部 epoch,
/// 每次 `bump_secret_epoch` 后 bump,让所有 provider 的实例全部 invalidate——
/// 粒度粗但简单可靠,且**与用户改密钥的频率(极低)完美匹配**。
type CacheKey = (String, String, String);

/// 密钥版本号——每次 `bump_secret_epoch` 后 bump,让实例 invalidate。
///
/// **位于 registry 实例内**(而非全局 static):不同 registry 之间不串扰,测试
/// 可以并发跑。生产只有一个 registry 实例,行为等价。
///
/// 用 AtomicU64 是因为 bump 路径可能来自任何线程(save_ai_secret IPC 处理)。
///
/// **调用契约**:调用方 bump 后必须触发 `reload()`(通常通过
/// `set_config('ai_config')` 走一遍完整流程)。只 bump 不 reload,fingerprint
/// 里的 epoch 变化不会被观察到。
fn compute_provider_fingerprint(p: &ProviderEntry, secret_epoch: u64) -> String {
    // 简单拼接,不用 hasher——字段少、可读性高、诊断日志能直接看
    let kind = match p.kind {
        ProviderKind::OpenAICompatible => "oai",
        ProviderKind::AnthropicMessages => "anth",
        ProviderKind::GeminiGenerateContent => "gem",
        ProviderKind::OllamaHttp => "ollama",
    };
    let bu = p.base_url.as_deref().unwrap_or("");
    format!("{kind}|{bu}|e{secret_epoch}")
}

/// ChatService 构造 AgentProvider 所需的配置快照。
///
/// Provider / Model 均为 clone，调用方可在 registry 锁外完成较重的 rig Client 构造。
#[derive(Clone, Debug)]
pub struct ResolvedProviderEntries {
    pub provider: ProviderEntry,
    pub model: ModelEntry,
    pub cache_key: (String, String, String),
}

/// Provider registry —— 运行时可热更新。
pub struct AIProviderRegistry {
    factory: Arc<dyn ProviderFactory>,
    /// 已构造的 provider 池——按 (provider_id, model_id, fingerprint) 索引
    providers: RwLock<HashMap<CacheKey, Arc<dyn AIProvider>>>,
    /// 当前 config 快照(轻量副本;修改 config 必须走 reload)
    config: RwLock<AIConfig>,
    /// 密钥版本号——保存密钥后 bump 一次,让下次 reload 强制重建所有实例。
    /// 见 [`compute_provider_fingerprint`] 与 `bump_secret_epoch`。
    secret_epoch: AtomicU64,
}

impl AIProviderRegistry {
    /// 空 registry —— provider 池空,resolve_tier 一律返 NotConfigured。
    ///
    /// 用于 AI 未配置或 factory 全部失败的兜底状态。
    #[allow(dead_code)]
    pub fn new(factory: Arc<dyn ProviderFactory>) -> Self {
        Self {
            factory,
            providers: RwLock::new(HashMap::new()),
            config: RwLock::new(AIConfig::default()),
            secret_epoch: AtomicU64::new(0),
        }
    }

    /// 从 `AIConfig` 构造完整 registry——启动时用。
    ///
    /// 每个 provider × 每个 model 组合调 factory 尝试构造;失败的 skip 但 tracing::warn,
    /// 不 panic、不上抛。返回的 registry 至少是空 pool 状态(§6.4 兜底铁则)。
    #[allow(dead_code)]
    pub fn from_config(factory: Arc<dyn ProviderFactory>, config: &AIConfig) -> Self {
        let registry = Self::new(factory);
        registry.reload(config);
        registry
    }

    /// 增量热更新——从新 `AIConfig` 重建 provider 池,复用未变动的实例。
    ///
    /// **§4.4 骨架条 #7 落地**:切 provider 不重启进程,只重建 registry 内部池。
    /// - 老 (pid, mid, fp) 仍在新 config 里 → 保留旧 Arc(不重建 rig Client)
    /// - 老 (pid, mid, fp) 不再匹配 → 从池里剔除(**任一字段变动都 invalidate**)
    /// - 新 (pid, mid, fp) 未构造 → 调 factory 构造;失败 skip + warn
    ///
    /// **fingerprint 铁则**:cache key 的第三段是 `provider_fingerprint`,包含
    /// kind + base_url + secret_epoch。用户改 base_url / 换密钥 / 改协议后,新的
    /// fingerprint 与老实例不同 → 强制重建,保证"改配置立即生效"。
    #[allow(dead_code)]
    pub fn reload(&self, config: &AIConfig) {
        use std::collections::HashSet;

        // 快照当前 secret_epoch —— 同一次 reload 内共享,避免中途 bump 导致
        // "第 1 步与第 4 步 fingerprint 不一致"这种诡异 race。
        let epoch = self.secret_epoch.load(Ordering::SeqCst);

        // 1. 目标 key 集合(HashSet——O(1) 查,retain 不退化成 O(n²))
        //    0.16.0: 跳过 enabled=false 的 provider——不构造实例、不占池空间
        let fp_by_pid: HashMap<String, String> = config
            .providers
            .iter()
            .filter(|p| p.enabled)
            .map(|p| (p.id.clone(), compute_provider_fingerprint(p, epoch)))
            .collect();
        let target_keys: HashSet<CacheKey> = config
            .providers
            .iter()
            .filter(|p| p.enabled)
            .flat_map(|p| {
                let fp = fp_by_pid.get(&p.id).cloned().unwrap_or_default();
                p.models
                    .iter()
                    .map(move |m| (p.id.clone(), m.id.clone(), fp.clone()))
            })
            .collect();

        // 2. **读锁**算 diff——池里缺哪些 (pid, mid, fp) 要构造。
        //    故意只拿读锁:factory.build 期间不阻塞 dispatch 的 resolve()。
        let to_build: Vec<(&ProviderEntry, &ModelEntry)> = {
            let pool = self.providers.read().expect("providers lock poisoned");
            config
                .providers
                .iter()
                .filter(|p| p.enabled)
                .flat_map(|p| p.models.iter().map(move |m| (p, m)))
                .filter(|(p, m)| {
                    let fp = fp_by_pid.get(&p.id).cloned().unwrap_or_default();
                    !pool.contains_key(&(p.id.clone(), m.id.clone(), fp))
                })
                .collect()
        };

        // 3. **锁外**批量构造——factory.build 在 Phase 5b 会冷构造 rig Client
        //    (甚至验密钥走网),持锁做这事会阻塞所有 dispatch 与 set_config 响应。
        //    reload 由 set_config IPC 串行触发,reload-to-reload 不并发;即使并发,
        //    最坏只是多构造一次实例(第 4 步的 retain 会清掉多余条目),无害。
        let mut built: Vec<(CacheKey, Arc<dyn AIProvider>)> = Vec::new();
        let mut failed_count = 0usize;
        for (provider, model) in &to_build {
            match self.factory.build(provider, model) {
                Ok(instance) => {
                    let fp = fp_by_pid.get(&provider.id).cloned().unwrap_or_default();
                    built.push(((provider.id.clone(), model.id.clone(), fp), instance))
                }
                Err(e) => {
                    failed_count += 1;
                    tracing::warn!(
                        target: crate::infra::utils::perf::ai_slo::TARGET,
                        provider_id = %provider.id,
                        model_id = %model.id,
                        error = %e,
                        "Provider factory 构造失败,跳过"
                    );
                }
            }
        }

        // 4. **短时写锁**:剔除过时 key + 提交新实例 + 存 config 快照
        let new_count = built.len();
        let total = {
            let mut pool = self.providers.write().expect("providers lock poisoned");
            pool.retain(|key, _| target_keys.contains(key));
            for (key, instance) in built {
                pool.insert(key, instance);
            }
            pool.len()
        };
        *self.config.write().expect("config lock poisoned") = config.clone();

        tracing::info!(
            target: crate::infra::utils::perf::ai_slo::TARGET,
            total, new_count, failed_count,
            "AI Provider registry 已刷新"
        );
    }

    /// 按 tier dispatch —— 拿到实际的 `Arc<dyn AIProvider>`。
    ///
    /// 结合 `AIConfig::resolve_tier` 的空档降级 + provider 池悬空守卫:
    /// - resolve_tier 返 None → `AIError::NotConfigured`
    /// - resolve_tier 返 Some 但 pool 里没这个 (pid, mid) → 也是 NotConfigured
    ///   (通常是 factory 曾构造失败,或热更新中途)
    ///
    /// 返回的 tuple: `(provider, actual_tier)` —— actual_tier 用于 tracing SLO
    /// 埋点(观测降级路径)。
    #[allow(dead_code)]
    pub fn resolve(&self, tier: Tier) -> Result<(Arc<dyn AIProvider>, Tier), AIError> {
        let config = self.config.read().expect("config lock poisoned");
        let Some((provider_entry, model_entry, actual_tier)) = config.resolve_tier(tier) else {
            return Err(AIError::NotConfigured);
        };
        // cache key 三段:pid + mid + fingerprint。fingerprint 由当前 config 快照 +
        // registry 内 secret_epoch 决定,与 reload 时插入的 key 完全一致。
        let epoch = self.secret_epoch.load(Ordering::SeqCst);
        let fp = compute_provider_fingerprint(provider_entry, epoch);
        let key = (provider_entry.id.clone(), model_entry.id.clone(), fp);

        let pool = self.providers.read().expect("providers lock poisoned");
        let provider = pool.get(&key).cloned().ok_or_else(|| {
            // 池里没这个 key——通常是 factory 构造曾失败;记录一次以便排查
            tracing::warn!(
                target: crate::infra::utils::perf::ai_slo::TARGET,
                requested = ?tier, actual = ?actual_tier,
                provider_id = %provider_entry.id,
                model_id = %model_entry.id,
                "档位解析到 (pid, mid) 但池内无实例——通常是 factory 曾失败"
            );
            AIError::NotConfigured
        })?;

        Ok((provider, actual_tier))
    }

    /// 按 (provider_id, model_id) 显式解析到 ready provider——对话窗口命名模型自选。
    ///
    /// 与 `resolve(tier)` 一样返回可用的 `Arc<dyn AIProvider>`；不做档位降级，
    /// provider/model 不存在或已禁用则返回 `NotConfigured`。
    pub fn resolve_explicit(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<Arc<dyn AIProvider>, AIError> {
        let config = self.config.read().expect("config lock poisoned");
        let provider_entry = config
            .providers
            .iter()
            .find(|p| p.id == provider_id && p.enabled)
            .ok_or(AIError::NotConfigured)?;
        let model_entry = provider_entry
            .models
            .iter()
            .find(|m| m.id == model_id && m.enabled)
            .ok_or(AIError::NotConfigured)?;
        let epoch = self.secret_epoch.load(Ordering::SeqCst);
        let fp = compute_provider_fingerprint(provider_entry, epoch);
        let key = (provider_entry.id.clone(), model_entry.id.clone(), fp);

        let pool = self.providers.read().expect("providers lock poisoned");
        pool.get(&key).cloned().ok_or(AIError::NotConfigured)
    }

    /// 当前池内 provider 数量——诊断/测试用。
    #[allow(dead_code)]
    pub fn size(&self) -> usize {
        self.providers.read().map(|p| p.len()).unwrap_or(0)
    }

    /// 当前 config 快照——诊断/测试用。
    #[allow(dead_code)]
    pub fn config_snapshot(&self) -> AIConfig {
        self.config.read().expect("config lock poisoned").clone()
    }

    /// 解析 chat 使用的 Main 档 Provider + Model，并生成与主窗口 registry 一致的缓存 key。
    ///
    /// clone 后立即释放 config 读锁，ChatService 可在锁外构造 rig Agent。
    pub(crate) fn resolve_entries(&self, tier: Tier) -> Result<ResolvedProviderEntries, AIError> {
        let config = self.config.read().expect("config lock poisoned");
        let Some((provider, model, _actual_tier)) = config.resolve_tier(tier) else {
            return Err(AIError::NotConfigured);
        };
        let fingerprint =
            compute_provider_fingerprint(provider, self.secret_epoch.load(Ordering::SeqCst));
        Ok(ResolvedProviderEntries {
            provider: provider.clone(),
            model: model.clone(),
            cache_key: (provider.id.clone(), model.id.clone(), fingerprint),
        })
    }

    /// 按 (provider_id, model_id) 显式解析——供 ChatService 运行时模型选择器(0.12.2 §4.4)。
    ///
    /// 与 `resolve_entries(tier)` 共用 cache_key 计算,保证「selected 模型」与
    /// 「tier 默认模型」命中同一缓存实例(若恰好相同)。
    ///
    /// 不做 tier 降级;provider/model 不存在或 model 已禁用则返回 `NotConfigured`。
    /// 不校验 `ModelCapability::Chat`——调用方(commands 层)在写入 selected 前已校验。
    pub(crate) fn resolve_explicit_entries(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<ResolvedProviderEntries, AIError> {
        let config = self.config.read().expect("config lock poisoned");
        let provider = config
            .providers
            .iter()
            .find(|p| p.id == provider_id && p.enabled)
            .ok_or(AIError::NotConfigured)?;
        let model = provider
            .models
            .iter()
            .find(|m| m.id == model_id && m.enabled)
            .ok_or(AIError::NotConfigured)?;
        let fingerprint =
            compute_provider_fingerprint(provider, self.secret_epoch.load(Ordering::SeqCst));
        Ok(ResolvedProviderEntries {
            provider: provider.clone(),
            model: model.clone(),
            cache_key: (provider.id.clone(), model.id.clone(), fingerprint),
        })
    }

    /// 校验 (provider_id, model_id) 是否存在且 model enabled(供 commands 层写入 selected 前校验)。
    ///
    /// 返回 `(provider_display_name, model_display_name)`,display_name 为空时回落 id。
    pub(crate) fn validate_model_exists(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Option<(String, String)> {
        let config = self.config.read().expect("config lock poisoned");
        let provider = config
            .providers
            .iter()
            .find(|p| p.id == provider_id && p.enabled)?;
        let model = provider
            .models
            .iter()
            .find(|m| m.id == model_id && m.enabled)?;
        let model_name = if model.display_name.is_empty() {
            model.id.clone()
        } else {
            model.display_name.clone()
        };
        Some((provider.display_name.clone(), model_name))
    }

    /// Bump 密钥版本号——`save_ai_secret` 成功后调,让下次 reload 时**所有**实例
    /// invalidate 并按新密钥重建。
    ///
    /// **调用契约**:bump 后必须紧跟一次 `reload()`,否则 fingerprint 变化不会
    /// 被观察到(现有实例仍旧,resolve 返回它们时也仍是旧密钥)。
    /// commands.rs 的 `save_ai_secret` 只 bump——reload 由前端紧接着的
    /// `set_config('ai_config')` IPC 触发。
    pub fn bump_secret_epoch(&self) {
        self.secret_epoch.fetch_add(1, Ordering::SeqCst);
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ai::provider::tests::MockProvider;
    use crate::domain::config::ai_config::{
        ModelCapability, ModelEntry, ProviderEntry, ProviderKind, TierAssignment,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 计数每次 build 被调多少次——验证热更新的"复用未变动实例"能力。
    struct CountingFactory {
        build_calls: AtomicUsize,
        /// 匹配这个 model_id 的 build 会失败——验证 factory 失败不影响其他 provider
        fail_for_model: Option<String>,
    }

    impl CountingFactory {
        fn new() -> Self {
            Self {
                build_calls: AtomicUsize::new(0),
                fail_for_model: None,
            }
        }
        fn fail_on(model_id: &str) -> Self {
            Self {
                build_calls: AtomicUsize::new(0),
                fail_for_model: Some(model_id.to_string()),
            }
        }
        fn calls(&self) -> usize {
            self.build_calls.load(Ordering::SeqCst)
        }
    }

    impl ProviderFactory for CountingFactory {
        fn build(
            &self,
            _entry: &ProviderEntry,
            model: &ModelEntry,
        ) -> Result<Arc<dyn AIProvider>, AIError> {
            self.build_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_for_model.as_deref() == Some(&model.id) {
                return Err(AIError::Provider(format!("mock fail for {}", model.id)));
            }
            Ok(Arc::new(MockProvider::echo_tool_call(
                "open_url",
                serde_json::json!({ "url": "https://example.com" }),
            )))
        }
    }

    fn make_config(
        providers: Vec<(&str, Vec<&str>)>,
        tier_ultra_light: Option<(&str, &str)>,
    ) -> AIConfig {
        AIConfig {
            enabled: true,
            providers: providers
                .into_iter()
                .map(|(pid, models)| ProviderEntry {
                    id: pid.into(),
                    display_name: pid.into(),
                    kind: ProviderKind::OpenAICompatible,
                    base_url: None,
                    secret_ref: format!("blink/{pid}/key"),
                    models: models
                        .into_iter()
                        .map(|mid| ModelEntry {
                            id: mid.into(),
                            display_name: mid.into(),
                            enabled: true,
                            context_window: None,
                            input_price_per_million: None,
                            output_price_per_million: None,
                            temperature: None,
                            max_tokens: None,
                            custom_parameters: Vec::new(),
                            reasoning_effort: None,
                            capabilities: vec![ModelCapability::Chat],
                        })
                        .collect(),
                    enabled: true,
                    created_at: 0,
                })
                .collect(),
            tier_ultra_light: tier_ultra_light.map(|(p, m)| TierAssignment {
                provider_id: p.into(),
                model_id: m.into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn empty_registry_returns_not_configured() {
        let f = Arc::new(CountingFactory::new());
        let reg = AIProviderRegistry::new(f);
        assert!(matches!(
            reg.resolve(Tier::UltraLight),
            Err(AIError::NotConfigured)
        ));
        assert!(matches!(
            reg.resolve(Tier::Light),
            Err(AIError::NotConfigured)
        ));
        assert!(matches!(
            reg.resolve(Tier::Main),
            Err(AIError::NotConfigured)
        ));
        assert_eq!(reg.size(), 0);
    }

    #[test]
    fn from_config_builds_pool_and_resolves() {
        let f = Arc::new(CountingFactory::new());
        let cfg = make_config(vec![("p1", vec!["m1", "m2"])], Some(("p1", "m1")));
        let reg = AIProviderRegistry::from_config(f.clone(), &cfg);

        // 池里 2 个 (p1, m1) + (p1, m2)
        assert_eq!(reg.size(), 2);
        assert_eq!(f.calls(), 2);

        // dispatch Router → (p1, m1)
        let (p, actual) = reg.resolve(Tier::UltraLight).unwrap();
        assert_eq!(p.model_id(), "mock-echo"); // MockProvider 内部 id,证明 dispatch 到实例
        assert_eq!(actual, Tier::UltraLight);
    }

    #[test]
    fn factory_failure_skips_provider_without_panic() {
        // p1/m1 会失败,p1/m2 成功——registry 应包含 p1/m2 一个 provider
        let f = Arc::new(CountingFactory::fail_on("m1"));
        let cfg = make_config(vec![("p1", vec!["m1", "m2"])], Some(("p1", "m2")));
        let reg = AIProviderRegistry::from_config(f.clone(), &cfg);

        assert_eq!(reg.size(), 1, "只有 m2 应构造成功");
        assert_eq!(f.calls(), 2, "两个 model 都被尝试构造");

        // Router 指向 m2 → 成功
        assert!(reg.resolve(Tier::UltraLight).is_ok());
    }

    #[test]
    fn resolve_returns_not_configured_when_tier_points_to_failed_build() {
        // Tier 指向构造失败的 m1 → resolve 应返 NotConfigured
        let f = Arc::new(CountingFactory::fail_on("m1"));
        let cfg = make_config(vec![("p1", vec!["m1"])], Some(("p1", "m1")));
        let reg = AIProviderRegistry::from_config(f, &cfg);

        assert_eq!(reg.size(), 0);
        assert!(matches!(
            reg.resolve(Tier::UltraLight),
            Err(AIError::NotConfigured)
        ));
    }

    #[test]
    fn reload_reuses_unchanged_instances() {
        let f = Arc::new(CountingFactory::new());
        let cfg1 = make_config(vec![("p1", vec!["m1", "m2"])], Some(("p1", "m1")));
        let reg = AIProviderRegistry::from_config(f.clone(), &cfg1);
        assert_eq!(f.calls(), 2);

        // 保存旧实例的引用,证明"没被重建"
        let old_p1_m1 = reg.resolve(Tier::UltraLight).unwrap().0;

        // reload 到同样的 cfg——不应再调 factory
        reg.reload(&cfg1);
        assert_eq!(f.calls(), 2, "同 config reload 不该再 build");

        let new_p1_m1 = reg.resolve(Tier::UltraLight).unwrap().0;
        assert!(
            Arc::ptr_eq(&old_p1_m1, &new_p1_m1),
            "reload 后 instance 应完全复用"
        );
    }

    #[test]
    fn reload_rebuilds_when_base_url_changes() {
        // fingerprint 覆盖了 base_url:改 base_url 后必须重建实例。
        // **这是"改了配置不生效"这类 bug 的回归守卫**。
        let f = Arc::new(CountingFactory::new());
        let cfg1 = make_config(vec![("p1", vec!["m1"])], Some(("p1", "m1")));
        let reg = AIProviderRegistry::from_config(f.clone(), &cfg1);
        assert_eq!(f.calls(), 1);
        let old = reg.resolve(Tier::UltraLight).unwrap().0;

        // 改 base_url
        let mut cfg2 = cfg1.clone();
        cfg2.providers[0].base_url = Some("https://api.new.com/v1".into());
        reg.reload(&cfg2);
        assert_eq!(f.calls(), 2, "base_url 变了必须重建");

        let new = reg.resolve(Tier::UltraLight).unwrap().0;
        assert!(!Arc::ptr_eq(&old, &new), "新实例不该等于旧实例");
    }

    #[test]
    fn reload_rebuilds_when_kind_changes() {
        // fingerprint 覆盖了 kind:改协议后必须重建实例。
        let f = Arc::new(CountingFactory::new());
        let cfg1 = make_config(vec![("p1", vec!["m1"])], Some(("p1", "m1")));
        let reg = AIProviderRegistry::from_config(f.clone(), &cfg1);
        let old = reg.resolve(Tier::UltraLight).unwrap().0;

        let mut cfg2 = cfg1.clone();
        cfg2.providers[0].kind = ProviderKind::AnthropicMessages;
        reg.reload(&cfg2);
        assert_eq!(f.calls(), 2);
        let new = reg.resolve(Tier::UltraLight).unwrap().0;
        assert!(!Arc::ptr_eq(&old, &new));
    }

    #[test]
    fn bump_secret_epoch_forces_rebuild_on_next_reload() {
        // bump_secret_epoch 后 fingerprint 变化 → reload 强制重建所有实例。
        // **这是"改了 API Key 不生效"这类 bug 的回归守卫**。
        let f = Arc::new(CountingFactory::new());
        let cfg = make_config(vec![("p1", vec!["m1"])], Some(("p1", "m1")));
        let reg = AIProviderRegistry::from_config(f.clone(), &cfg);
        let old = reg.resolve(Tier::UltraLight).unwrap().0;

        // 用户改密钥场景:save_ai_secret bump epoch → set_config 触发 reload
        reg.bump_secret_epoch();
        reg.reload(&cfg);
        assert_eq!(f.calls(), 2, "epoch bump 后必须重建实例");
        let new = reg.resolve(Tier::UltraLight).unwrap().0;
        assert!(!Arc::ptr_eq(&old, &new));
    }

    #[test]
    fn reload_adds_new_removes_stale() {
        let f = Arc::new(CountingFactory::new());
        // 初始:p1 有 m1
        let cfg1 = make_config(vec![("p1", vec!["m1"])], Some(("p1", "m1")));
        let reg = AIProviderRegistry::from_config(f.clone(), &cfg1);
        assert_eq!(reg.size(), 1);
        assert_eq!(f.calls(), 1);

        // 换 config:p1 只有 m2 (旧 m1 应移除,新 m2 应构造)
        let cfg2 = make_config(vec![("p1", vec!["m2"])], Some(("p1", "m2")));
        reg.reload(&cfg2);
        assert_eq!(reg.size(), 1, "换 model 后仍 1 个");
        assert_eq!(f.calls(), 2, "只对 m2 调一次 build");

        // 验证 tier 指向新 m2 生效
        let (p, _) = reg.resolve(Tier::UltraLight).unwrap();
        assert_eq!(p.model_id(), "mock-echo");
    }

    #[test]
    fn resolve_reports_actual_tier_after_degrade() {
        let f = Arc::new(CountingFactory::new());
        // 只配了 tier_main
        let mut cfg = make_config(vec![("p1", vec!["opus"])], None);
        cfg.tier_main = Some(TierAssignment {
            provider_id: "p1".into(),
            model_id: "opus".into(),
        });
        let reg = AIProviderRegistry::from_config(f, &cfg);

        // 请求 Router → 降级到 Main
        let (_, actual) = reg.resolve(Tier::UltraLight).unwrap();
        assert_eq!(actual, Tier::Main);
    }

    #[test]
    fn resolve_entries_uses_same_fingerprint_as_provider_pool() {
        let f = Arc::new(CountingFactory::new());
        let cfg = make_config(vec![("p1", vec!["m1"])], Some(("p1", "m1")));
        let reg = AIProviderRegistry::from_config(f, &cfg);

        let resolved = reg.resolve_entries(Tier::UltraLight).unwrap();
        assert_eq!(resolved.provider.id, "p1");
        assert_eq!(resolved.model.id, "m1");
        assert_eq!(resolved.cache_key.0, "p1");
        assert_eq!(resolved.cache_key.1, "m1");
        assert!(resolved.cache_key.2.ends_with("|e0"));

        reg.bump_secret_epoch();
        let after_secret_change = reg.resolve_entries(Tier::UltraLight).unwrap();
        assert_ne!(resolved.cache_key, after_secret_change.cache_key);
    }

    #[test]
    fn config_snapshot_matches_reload() {
        let f = Arc::new(CountingFactory::new());
        let reg = AIProviderRegistry::new(f);

        let cfg = make_config(vec![("p1", vec!["m1"])], Some(("p1", "m1")));
        reg.reload(&cfg);

        let snap = reg.config_snapshot();
        assert_eq!(snap.providers.len(), 1);
        assert_eq!(snap.providers[0].id, "p1");
    }

    // ── 0.12.2 §4.4 模型选择器：resolve_explicit_entries / validate_model_exists ──

    #[test]
    fn resolve_explicit_entries_returns_when_exists() {
        let f = Arc::new(CountingFactory::new());
        let reg = AIProviderRegistry::new(f);
        // tier_ultra_light 指向 m1,便于对比 cache_key
        reg.reload(&make_config(
            vec![("p1", vec!["m1", "m2"])],
            Some(("p1", "m1")),
        ));

        let entries = reg.resolve_explicit_entries("p1", "m2").unwrap();
        assert_eq!(entries.provider.id, "p1");
        assert_eq!(entries.model.id, "m2");
        // 显式解析的 cache_key 与 tier 解析的 cache_key 第三段(fingerprint)应一致,
        // 因为同一 provider 的 fingerprint 不变;前两段按各自 (pid, mid) 不同。
        let tier_entries = reg.resolve_entries(Tier::UltraLight).unwrap();
        assert_eq!(
            entries.cache_key.0, tier_entries.cache_key.0,
            "provider id 应同"
        );
        assert_eq!(
            entries.cache_key.2, tier_entries.cache_key.2,
            "fingerprint 应同"
        );
        assert_eq!(tier_entries.cache_key.1, "m1");
        assert_eq!(entries.cache_key.1, "m2");
    }

    #[test]
    fn resolve_explicit_entries_rejects_missing() {
        let f = Arc::new(CountingFactory::new());
        let reg = AIProviderRegistry::new(f);
        reg.reload(&make_config(vec![("p1", vec!["m1"])], None));

        // provider 不存在
        assert!(matches!(
            reg.resolve_explicit_entries("p_missing", "m1"),
            Err(AIError::NotConfigured)
        ));
        // model 不存在
        assert!(matches!(
            reg.resolve_explicit_entries("p1", "m_missing"),
            Err(AIError::NotConfigured)
        ));
    }

    #[test]
    fn resolve_explicit_entries_rejects_disabled_model() {
        let f = Arc::new(CountingFactory::new());
        let reg = AIProviderRegistry::new(f);
        let mut cfg = make_config(vec![("p1", vec!["m1"])], None);
        cfg.providers[0].models[0].enabled = false; // 禁用
        reg.reload(&cfg);

        assert!(matches!(
            reg.resolve_explicit_entries("p1", "m1"),
            Err(AIError::NotConfigured)
        ));
    }

    #[test]
    fn validate_model_exists_returns_display_names() {
        let f = Arc::new(CountingFactory::new());
        let reg = AIProviderRegistry::new(f);
        reg.reload(&make_config(vec![("p1", vec!["m1"])], None));

        let names = reg.validate_model_exists("p1", "m1").unwrap();
        assert_eq!(names, ("p1".to_string(), "m1".to_string()));
        // 不存在返回 None
        assert!(reg.validate_model_exists("p1", "m_missing").is_none());
        assert!(reg.validate_model_exists("p_missing", "m1").is_none());
    }
}
