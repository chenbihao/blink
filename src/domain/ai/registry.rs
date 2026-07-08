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
//! **cache 策略**:key = `(provider_id, model_id)`。Provider 切换配置时,
//! 未变动的 (pid, mid) 保留旧实例(不重建 rig Client),减少冷构造抖动。
//!
//! **§6.4 兜底铁则**:AI 配置错误绝不破坏主链路——
//! - `resolve_tier` 返 `Err(NotConfigured)` → 上层 SearchService fallback 常规 fuzzy
//! - factory 构造失败 → 单个 provider skip + tracing::error,其他 provider 正常
//! - 全部构造失败 → registry 是空的,`resolve_tier` 一律 NotConfigured

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::app::ai_config::{AIConfig, ModelEntry, ProviderEntry, Tier};
use crate::domain::ai::provider::{AIError, AIProvider};

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

/// (provider_id, model_id) —— provider 缓存 key。
type CacheKey = (String, String);

/// Provider registry —— 运行时可热更新。
pub struct AIProviderRegistry {
    factory: Arc<dyn ProviderFactory>,
    /// 已构造的 provider 池——按 (provider_id, model_id) 索引
    providers: RwLock<HashMap<CacheKey, Arc<dyn AIProvider>>>,
    /// 当前 config 快照(轻量副本;修改 config 必须走 reload)
    config: RwLock<AIConfig>,
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
    /// - 老 (pid, mid) 仍在新 config 里 → 保留旧 Arc(不重建 rig Client)
    /// - 老 (pid, mid) 不再存在 → 从池里剔除(旧 Arc 引用计数归零时释放)
    /// - 新 (pid, mid) 未构造 → 调 factory 构造;失败 skip + warn
    #[allow(dead_code)]
    pub fn reload(&self, config: &AIConfig) {
        use std::collections::HashSet;

        // 1. 目标 key 集合(HashSet——O(1) 查,retain 不退化成 O(n²))
        let target_keys: HashSet<CacheKey> = config
            .providers
            .iter()
            .flat_map(|p| p.models.iter().map(move |m| (p.id.clone(), m.id.clone())))
            .collect();

        // 2. **读锁**算 diff——池里缺哪些 (pid, mid) 要构造。
        //    故意只拿读锁:factory.build 期间不阻塞 dispatch 的 resolve()。
        let to_build: Vec<(&ProviderEntry, &ModelEntry)> = {
            let pool = self.providers.read().expect("providers lock poisoned");
            config
                .providers
                .iter()
                .flat_map(|p| p.models.iter().map(move |m| (p, m)))
                .filter(|(p, m)| !pool.contains_key(&(p.id.clone(), m.id.clone())))
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
                    built.push(((provider.id.clone(), model.id.clone()), instance))
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
        let key = (provider_entry.id.clone(), model_entry.id.clone());

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
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ai_config::{ModelEntry, ProviderEntry, ProviderKind, TierAssignment};
    use crate::domain::ai::provider::tests::MockProvider;
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

    fn make_config(providers: Vec<(&str, Vec<&str>)>, tier_router: Option<(&str, &str)>) -> AIConfig {
        AIConfig {
            enabled: true,
            providers: providers
                .into_iter()
                .map(|(pid, models)| ProviderEntry {
                    id: pid.into(),
                    display_name: pid.into(),
                    kind: ProviderKind::OpenAI,
                    base_url: None,
                    secret_ref: format!("blink/{pid}/key"),
                    models: models
                        .into_iter()
                        .map(|mid| ModelEntry {
                            id: mid.into(),
                            display_name: mid.into(),
                            context_window: None,
                            input_price_per_million: None,
                            output_price_per_million: None,
                        })
                        .collect(),
                    created_at: 0,
                })
                .collect(),
            tier_router: tier_router.map(|(p, m)| TierAssignment {
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
        assert!(matches!(reg.resolve(Tier::Router), Err(AIError::NotConfigured)));
        assert!(matches!(reg.resolve(Tier::Light), Err(AIError::NotConfigured)));
        assert!(matches!(reg.resolve(Tier::Main), Err(AIError::NotConfigured)));
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
        let (p, actual) = reg.resolve(Tier::Router).unwrap();
        assert_eq!(p.model_id(), "mock-echo"); // MockProvider 内部 id,证明 dispatch 到实例
        assert_eq!(actual, Tier::Router);
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
        assert!(reg.resolve(Tier::Router).is_ok());
    }

    #[test]
    fn resolve_returns_not_configured_when_tier_points_to_failed_build() {
        // Tier 指向构造失败的 m1 → resolve 应返 NotConfigured
        let f = Arc::new(CountingFactory::fail_on("m1"));
        let cfg = make_config(vec![("p1", vec!["m1"])], Some(("p1", "m1")));
        let reg = AIProviderRegistry::from_config(f, &cfg);

        assert_eq!(reg.size(), 0);
        assert!(matches!(
            reg.resolve(Tier::Router),
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
        let old_p1_m1 = reg.resolve(Tier::Router).unwrap().0;

        // reload 到同样的 cfg——不应再调 factory
        reg.reload(&cfg1);
        assert_eq!(f.calls(), 2, "同 config reload 不该再 build");

        let new_p1_m1 = reg.resolve(Tier::Router).unwrap().0;
        assert!(Arc::ptr_eq(&old_p1_m1, &new_p1_m1), "reload 后 instance 应完全复用");
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
        let (p, _) = reg.resolve(Tier::Router).unwrap();
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
        let (_, actual) = reg.resolve(Tier::Router).unwrap();
        assert_eq!(actual, Tier::Main);
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
}
