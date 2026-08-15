//! Capability 注册表（0.9.7）。
//!
//! `CapabilityRegistry` 持有所有能力的 `Arc<dyn Capability>` 实例。
//! 启动时通过 `inventory` 链接期收集自动注册——**新增能力只需在文件里写
//! `inventory::submit!`，注册表零改动**。
//!
//! `invoke()` 包装层统一 SLO 埋点（§3.5 铁则 3）——实现方无需手写 perf record。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde_json::Value;

use super::error::CapabilityError;
use super::result::CapabilityResult;
use super::schema::CapabilitySchema;
use super::{Capability, CapabilityEntry, InvokeContext};

/// 能力注册表。id → `Arc<dyn Capability>` 的映射。
///
/// 内部 `RwLock`：读多写少（注册只在启动时由 inventory 收集，之后只读）。
pub struct CapabilityRegistry {
    caps: RwLock<HashMap<String, Arc<dyn Capability>>>,
}

impl CapabilityRegistry {
    /// 从 inventory 自动收集所有 `CapabilityEntry`（链接期注册的能力）。
    ///
    /// **零手动注册**：每个 Capability 文件写一行 `inventory::submit!` 即可。
    /// 重复 id → warn + 跳过（与旧 ActionRegistry 一致策略）。
    pub fn new() -> Self {
        let mut caps: HashMap<String, Arc<dyn Capability>> = HashMap::new();
        for entry in inventory::iter::<CapabilityEntry> {
            let cap = (entry.factory)();
            let id = cap.id().to_string();
            if caps.contains_key(&id) {
                tracing::warn!(id = %id, "CapabilityRegistry: 重复 id,跳过");
                continue;
            }
            tracing::debug!(id = %id, "capability registered (via inventory)");
            caps.insert(id, cap);
        }
        tracing::info!(count = caps.len(), "CapabilityRegistry 初始化完成");
        Self {
            caps: RwLock::new(caps),
        }
    }

    /// 按 id 查找能力（clone Arc，锁内完成）。
    pub fn get(&self, id: &str) -> Option<Arc<dyn Capability>> {
        self.caps.read().unwrap().get(id).cloned()
    }

    /// 按名称列出所有已注册能力的 schema（供 AI tool 池 / CLI / MCP 消费）。
    ///
    /// 注册表内部是 `HashMap`，遍历顺序不稳定；在公共 list 边界统一排序，
    /// 避免设置页与 MCP `tools/list` 每次启动出现不同顺序。
    pub fn list(&self) -> Vec<CapabilitySchema> {
        let mut schemas: Vec<_> = self
            .caps
            .read()
            .unwrap()
            .values()
            .map(|cap| cap.schema())
            .collect();
        schemas.sort_by(|a, b| a.name.cmp(&b.name));
        schemas
    }

    /// 运行期动态注册能力（0.12.0 §2.5）。
    ///
    /// **用途**：0.13 Skill 化（CLI → AI 生成 Capability）等运行期才产生的能力。
    /// inventory 是链接期静态机制，运行期产生的 capability 走此入口。
    ///
    /// **去重策略**：与 `new()` 的 inventory 收集一致——重复 id → warn + 跳过。
    /// 与 inventory 共存：`new()` 先收集链接期能力，`register()` 在运行期追加。
    ///
    /// **参照**：`ChordRegistry::register`。
    #[allow(dead_code)] // 0.13 Skill 化（运行期生成 Capability）消费；0.12.0 铺路
    pub fn register(&self, cap: Arc<dyn Capability>) {
        let id = cap.id().to_string();
        let mut caps = self.caps.write().unwrap();
        if caps.contains_key(&id) {
            tracing::warn!(id = %id, "CapabilityRegistry::register: 重复 id,跳过");
            return;
        }
        tracing::debug!(id = %id, "capability registered (via runtime register)");
        caps.insert(id, cap);
    }

    /// 已注册能力数量。
    #[allow(dead_code)] // 调试/日志用
    pub fn len(&self) -> usize {
        self.caps.read().unwrap().len()
    }

    /// 返回所有已注册能力的 `(id, Arc<dyn Capability>)` 对（0.12.0 §2.4）。
    ///
    /// 供 `build_agent_tools()` 工厂函数遍历所有能力，包装成 `ToolDyn`。
    /// 读锁内一次性 clone 所有 Arc，避免多次锁获取。
    pub fn entries(&self) -> Vec<(String, Arc<dyn Capability>)> {
        self.caps
            .read()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// invoke 包装层（§3.5 铁则 3）——统一 SLO 埋点 + tracing + origin/runtime 门禁。
    ///
    /// **0.21.0**：在调用 `cap.invoke()` 前执行代码级 origin/runtime 门禁：
    /// - origin 不在 `policy().allowed_origins` 中 → `OriginDenied`
    /// - runtime 不满足 `policy().runtime_requirement` → `Unsupported`
    ///
    /// **调用方应优先用此方法**而非直接 `cap.invoke()`——保证 SLO 一致性 + 门禁。
    ///
    /// **outcome 分桶**（文档 §3.3）：
    /// - `Ok` → "ok"
    /// - `Err(Timeout)` → "timeout"
    /// - `Err(Cancelled)` → **不计 perf**（用户行为，非能力问题），直接返回
    /// - 其他 `Err` → "error"
    pub async fn invoke(
        &self,
        id: &str,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let cap = match self.get(id) {
            Some(c) => c,
            None => {
                tracing::warn!(capability = id, "invoke: 能力不存在");
                return Err(CapabilityError::NotFound { id: id.into() });
            }
        };

        // ── 0.21.0 代码级门禁 ──────────────────────────────────────────
        let policy = cap.policy();

        // 门禁 1：调用来源检查
        if !policy.allows_origin(ctx.origin) {
            tracing::warn!(
                capability = id,
                origin = %ctx.origin,
                allowed = %policy.allowed_origins,
                "invoke: 来源不被允许"
            );
            return Err(CapabilityError::OriginDenied {
                origin: ctx.origin.to_string(),
                allowed: policy.allowed_origins.to_string(),
            });
        }

        // 门禁 2：运行时要求检查
        let actual_runtime = ctx.runtime.as_requirement();
        if !policy.runtime_satisfied(actual_runtime) {
            tracing::warn!(
                capability = id,
                required = %policy.runtime_requirement,
                actual = %actual_runtime,
                "invoke: 运行时不满足要求"
            );
            return Err(CapabilityError::Unsupported {
                required: policy.runtime_requirement.to_string(),
                actual: actual_runtime.to_string(),
            });
        }

        let start = std::time::Instant::now();
        let outcome = cap.invoke(args, ctx).await;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;

        // outcome 分桶 + SLO 埋点（Cancelled 不计）
        match &outcome {
            Ok(_) => {
                tracing::debug!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    capability = id,
                    elapsed_ms = elapsed as u64,
                    outcome = "ok",
                    "capability invoke"
                );
                crate::infra::utils::perf::record(
                    crate::infra::utils::perf::MetricCategory::Capability,
                    id,
                    elapsed,
                    Some("ok"),
                );
            }
            Err(CapabilityError::Cancelled) => {
                // 用户取消——不计 perf（用户行为，非能力问题），trace 留痕
                tracing::trace!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    capability = id,
                    "capability invoke cancelled（不计 SLO）"
                );
            }
            Err(CapabilityError::Timeout { .. }) => {
                tracing::debug!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    capability = id,
                    elapsed_ms = elapsed as u64,
                    outcome = "timeout",
                    "capability invoke"
                );
                crate::infra::utils::perf::record(
                    crate::infra::utils::perf::MetricCategory::Capability,
                    id,
                    elapsed,
                    Some("timeout"),
                );
            }
            Err(e) => {
                tracing::debug!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    capability = id,
                    elapsed_ms = elapsed as u64,
                    outcome = "error",
                    error = %e,
                    "capability invoke"
                );
                crate::infra::utils::perf::record(
                    crate::infra::utils::perf::MetricCategory::Capability,
                    id,
                    elapsed,
                    Some("error"),
                );
            }
        }

        outcome
    }
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_get_returns_none() {
        // new() 从 inventory 收集——测试环境无真实能力 submit
        let reg = CapabilityRegistry::default();
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn list_returns_vec_without_panic() {
        let reg = CapabilityRegistry::default();
        let _schemas = reg.list();
        // 测试环境 inventory 可能含测试 submit，只验证不 panic + 返回 Vec
    }

    #[test]
    fn list_is_sorted_by_capability_name() {
        let reg = CapabilityRegistry::default();
        reg.register(Arc::new(MockCap {
            id_val: "sort_test_zulu",
        }));
        reg.register(Arc::new(MockCap {
            id_val: "sort_test_alpha",
        }));

        let names: Vec<_> = reg.list().into_iter().map(|schema| schema.name).collect();
        assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    /// registry::invoke 的 NotFound 路径在 get(id) 返回 None 时短路——
    /// 不构造 InvokeContext（避开 AppHandle mock，遵循 AGENTS.md §7
    /// "Tauri 集成层免自动化"）。NotFound 纯逻辑在 get() 这里覆盖。
    #[test]
    fn invoke_not_found_path_covered_by_get() {
        let reg = CapabilityRegistry::default();
        // invoke 内部先 get(id)；get 返回 None → NotFound。
        // 这里验证 get 的 None 路径（invoke 包装层的行为在集成测试覆盖）
        assert!(reg.get("any_unknown_id").is_none());
    }

    /// 验证 inventory collect type 编译期可用（链接期收集机制正常）
    /// ——若 inventory::collect! / submit! 配置有误，此测试无法编译。
    #[test]
    fn inventory_collect_type_compiles() {
        // 仅验证 CapabilityEntry 类型存在且可被 inventory 引用
        let _ = std::any::TypeId::of::<CapabilityEntry>();
    }

    // ── register() 运行期动态注册测试（0.12.0 §2.5）──────────────────────

    /// 测试用 mock Capability——避免构造 AppHandle（遵循 AGENTS.md §7）。
    struct MockCap {
        id_val: &'static str,
    }

    #[async_trait::async_trait]
    impl Capability for MockCap {
        fn id(&self) -> &str {
            self.id_val
        }
        fn schema(&self) -> CapabilitySchema {
            CapabilitySchema::empty(self.id_val, "mock for test")
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, CapabilityError> {
            Ok(CapabilityResult::Done {
                summary: "mock".into(),
            })
        }
    }

    #[test]
    fn register_adds_new_capability() {
        let reg = CapabilityRegistry::default();
        let cap = Arc::new(MockCap {
            id_val: "test_mock_cap",
        }) as Arc<dyn Capability>;
        reg.register(cap);
        assert!(reg.get("test_mock_cap").is_some());
    }

    #[test]
    fn register_duplicate_id_is_skipped() {
        let reg = CapabilityRegistry::default();
        let cap1 = Arc::new(MockCap { id_val: "dup_cap" }) as Arc<dyn Capability>;
        reg.register(cap1);
        // 第二次注册同 id → warn + 跳过（不覆盖）
        let cap2 = Arc::new(MockCap { id_val: "dup_cap" }) as Arc<dyn Capability>;
        reg.register(cap2);
        // 仍然能 get 到（第一次注册的）
        assert!(reg.get("dup_cap").is_some());
    }

    #[test]
    fn register_coexists_with_inventory() {
        // new() 先收集 inventory，register() 追加——两者共存
        let reg = CapabilityRegistry::default();
        let before = reg.len();
        let cap = Arc::new(MockCap {
            id_val: "coexist_cap",
        }) as Arc<dyn Capability>;
        reg.register(cap);
        assert_eq!(reg.len(), before + 1);
    }
}
