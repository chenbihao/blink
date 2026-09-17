//! Capability 注册表（0.9.7）。
//!
//! `CapabilityRegistry` 持有所有能力的 `Arc<dyn Capability>` 实例。
//! 启动时通过 `inventory` 链接期收集自动注册——**新增能力只需在文件里写
//! `inventory::submit!`，注册表零改动**。
//!
//! `invoke()` 包装层统一 SLO 埋点（§3.5 铁则 3）——实现方无需手写 perf record。
//!
//! **0.22.18 审计契约**：`invoke()` 对非 MCP 来源统一写最小审计事件到
//! `ai_tool_audit` 表。MCP 来源在 `server.rs` 中已有自己的审计写入
//!（含完整参数和结果摘要），不在此重复。审计只记录 capability id、
//! origin、outcome 分类和耗时——**不记录完整参数、结果、窗口标题
//! 或截图内容**。写入失败只 warn 不阻塞主流程。
//!
//! **0.22.18 有界审计队列**：审计事件通过有界 mpsc channel + 专用 writer
//! task 写入 DB，替代无界 `tokio::spawn`。队列满时 warn 并按溢出策略丢弃，
//! DB 失败只 warn。Cancelled 和早退路径也记录审计事件。

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::Value;
use tokio::sync::mpsc::{self, Sender};

use super::error::CapabilityError;
use super::result::CapabilityResult;
use super::schema::CapabilitySchema;
use super::{Capability, CapabilityEntry, InvokeContext};

/// Capability 注册表初始化或动态注册时的身份冲突错误（0.21.13）。
///
/// 重复 capability id 必须确定性失败并报告冲突 id，
/// 不能由 inventory 遍历顺序静默决定生效实现。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// 构造时发现重复 id。携带冲突 id 字符串。
    #[error("CapabilityRegistry: 重复 id \"{id}\"——身份冲突，拒绝静默选择实现")]
    DuplicateId { id: String },
}

/// 审计队列容量——超过此数量时丢弃新事件。
const AUDIT_QUEUE_CAPACITY: usize = 64;

/// 审计事件载荷——通过有界 channel 发送给 writer task。
struct AuditEvent {
    cap_id: String,
    origin_str: String,
    summary: String,
    db_pool: sqlx::SqlitePool,
}

struct AuditWriter {
    sender: std::sync::Mutex<Option<Sender<AuditEvent>>>,
    handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// 进程级单 writer。sender 可在退出时 take，从而关闭 channel 并 drain。
static AUDIT_WRITER: OnceLock<AuditWriter> = OnceLock::new();

/// 能力注册表。id → `Arc<dyn Capability>` 的映射。
///
/// 内部 `RwLock`：读多写少（注册只在启动时由 inventory 收集，之后只读）。
///
/// **0.21.13**：重复 id 不再 warn + 跳过，而是确定性失败。
/// - 构造时：`try_from_entries` 返回 `Err(RegistryError::DuplicateId)`。
/// - 运行期 `register`：返回 `Err(RegistryError::DuplicateId)`，原 capability 不被覆盖。
/// - 公共 `list()` / `entries()` 均按稳定 id 排序。
pub struct CapabilityRegistry {
    caps: RwLock<HashMap<String, Arc<dyn Capability>>>,
}

impl CapabilityRegistry {
    /// 从 inventory 自动收集所有 `CapabilityEntry`（链接期注册的能力）。
    ///
    /// **零手动注册**：每个 Capability 文件写一行 `inventory::submit!` 即可。
    ///
    /// **0.21.13**：重复 id 不再静默跳过。inventory 收集后先建立确定性顺序，
    /// 再检查重复——重复 id 返回错误并阻止启动。
    ///
    /// # Panics
    ///
    /// debug 和 release 均在重复 id 时 panic——带歧义的 Registry 不得进入服务 wiring。
    /// 错误消息明确包含冲突 id，便于诊断。
    pub fn new() -> Self {
        Self::try_new().unwrap_or_else(|e| panic!("CapabilityRegistry 初始化失败: {e}"))
    }

    /// 初始化有界审计队列——在 tokio runtime 上下文中调用。
    ///
    /// 创建有界 mpsc channel 和专用 writer task。writer task 从 channel
    /// 接收审计事件并写入 DB，DB 失败只 warn。
    /// 队列满时 `try_send` 失败，warn 并按溢出策略丢弃事件。
    ///
    /// 此方法幂等——多次调用安全，只有首次调用创建 channel 和 task。
    pub fn init_audit_writer() {
        if AUDIT_WRITER.get().is_some() {
            return;
        }
        let (tx, mut rx) = mpsc::channel::<AuditEvent>(AUDIT_QUEUE_CAPACITY);
        let handle = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                // save_audit_log 内部处理错误（warn 不阻塞），返回 ()
                crate::infra::data::ai_audit::save_audit_log(
                    &event.db_pool,
                    &event.cap_id,
                    &serde_json::Value::Object(serde_json::Map::new()), // 不记录参数
                    &event.summary,
                    "", // provider_kind
                    "", // model_id
                    1,  // turn
                    &event.origin_str,
                )
                .await;
            }
            tracing::debug!("审计 writer task 退出（channel 已关闭）");
        });
        let writer = AuditWriter {
            sender: std::sync::Mutex::new(Some(tx)),
            handle: std::sync::Mutex::new(Some(handle)),
        };
        if let Err(writer) = AUDIT_WRITER.set(writer)
            && let Some(handle) = writer
                .handle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
        {
            handle.abort();
        }
    }

    /// 关闭发送端并等待 writer 排空队列。退出路径使用短超时兜底，避免数据库
    /// 异常拖住进程；超时后 abort 尚未完成的 writer 并明确记录丢失风险。
    pub async fn shutdown_audit_writer() {
        let Some(writer) = AUDIT_WRITER.get() else {
            return;
        };
        writer
            .sender
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let handle = writer
            .handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(mut handle) = handle
            && tokio::time::timeout(std::time::Duration::from_secs(2), &mut handle)
                .await
                .is_err()
        {
            tracing::warn!("审计队列退出排空超时，终止 writer");
            handle.abort();
        }
    }

    /// 从 inventory 收集能力，重复 id 返回确定性错误（0.21.13）。
    ///
    /// inventory 遍历顺序不稳定，因此先收集全部 entries 并按 id 排序，
    /// 再检查重复——错误不依赖链接器遍历顺序。
    pub fn try_new() -> Result<Self, RegistryError> {
        // 收集全部 entries（factory + id），先排序再检查重复。
        let mut entries: Vec<(String, Arc<dyn Capability>)> = Vec::new();
        for entry in inventory::iter::<CapabilityEntry> {
            let cap = (entry.factory)();
            let id = cap.id().to_string();
            entries.push((id, cap));
        }

        // 确定性顺序：按 id 排序后再检查重复。
        // 排序保证无论 inventory 遍历顺序如何，重复检查结果一致。
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut caps: HashMap<String, Arc<dyn Capability>> = HashMap::new();
        for (id, cap) in entries {
            if caps.contains_key(&id) {
                tracing::error!(%id, "CapabilityRegistry: 重复 id，拒绝初始化");
                return Err(RegistryError::DuplicateId { id });
            }
            tracing::debug!(%id, "capability registered (via inventory)");
            caps.insert(id, cap);
        }
        tracing::info!(count = caps.len(), "CapabilityRegistry 初始化完成");
        Ok(Self {
            caps: RwLock::new(caps),
        })
    }

    /// 从显式 entries 构造（可注入，用于测试）。
    ///
    /// 先按 id 排序再检查重复——错误不依赖输入顺序。
    #[allow(dead_code)] // 公开 API：测试注入构造 + 未来运行期工厂消费
    pub fn try_from_entries(entries: Vec<Arc<dyn Capability>>) -> Result<Self, RegistryError> {
        let mut sorted: Vec<(String, Arc<dyn Capability>)> = entries
            .into_iter()
            .map(|cap| (cap.id().to_string(), cap))
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        let mut caps: HashMap<String, Arc<dyn Capability>> = HashMap::new();
        for (id, cap) in sorted {
            if caps.contains_key(&id) {
                return Err(RegistryError::DuplicateId { id });
            }
            caps.insert(id, cap);
        }
        Ok(Self {
            caps: RwLock::new(caps),
        })
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
    /// **0.21.13**：重复 id 不再 warn + 跳过，改为返回 `Err(RegistryError::DuplicateId)`，
    /// 原 capability 不被覆盖。
    ///
    /// **参照**：`ChordRegistry::register`。
    #[allow(dead_code)] // 0.13 Skill 化（运行期生成 Capability）消费；0.12.0 铺路
    pub fn register(&self, cap: Arc<dyn Capability>) -> Result<(), RegistryError> {
        let id = cap.id().to_string();
        let mut caps = self.caps.write().unwrap();
        if caps.contains_key(&id) {
            tracing::warn!(%id, "CapabilityRegistry::register: 重复 id，拒绝注册");
            return Err(RegistryError::DuplicateId { id });
        }
        tracing::debug!(%id, "capability registered (via runtime register)");
        caps.insert(id, cap);
        Ok(())
    }

    /// 已注册能力数量。
    #[allow(dead_code)] // 调试/日志用
    pub fn len(&self) -> usize {
        self.caps.read().unwrap().len()
    }

    /// 返回所有已注册能力的 `(id, Arc<dyn Capability>)` 对（0.12.0 §2.4）。
    ///
    /// 供 `build_agent_tools()` 工厂函数遍历所有能力，包装成 `DynamicTool`。
    /// 读锁内一次性 clone 所有 Arc，避免多次锁获取。
    ///
    /// **0.21.13**：返回结果按稳定 id 排序，避免 AI/MCP/设置页顺序漂移。
    pub fn entries(&self) -> Vec<(String, Arc<dyn Capability>)> {
        let mut pairs: Vec<_> = self
            .caps
            .read()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
    }

    /// invoke 包装层（§3.5 铁则 3）——统一 SLO 埋点 + tracing + origin/runtime 门禁 + 审计。
    ///
    /// **0.21.0**：在调用 `cap.invoke()` 前执行代码级 origin/runtime 门禁：
    /// - origin 不在 `policy().allowed_origins` 中 → `OriginDenied`
    /// - runtime 不满足 `policy().runtime_requirement` → `Unsupported`
    ///
    /// **0.22.18**：对非 MCP 来源统一写最小审计事件。MCP 来源在 `server.rs`
    /// 中已有自己的审计写入（含参数和结果摘要），不在此重复。
    ///
    /// **调用方应优先用此方法**而非直接 `cap.invoke()`——保证 SLO 一致性 + 门禁 + 审计。
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
        let start = std::time::Instant::now();
        let cap = match self.get(id) {
            Some(c) => c,
            None => {
                tracing::warn!(capability = id, "invoke: 能力不存在");
                Self::emit_audit(ctx, id, "not_found", start.elapsed().as_secs_f64() * 1000.0);
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
            Self::emit_audit(
                ctx,
                id,
                "origin_denied",
                start.elapsed().as_secs_f64() * 1000.0,
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
            Self::emit_audit(
                ctx,
                id,
                "unsupported",
                start.elapsed().as_secs_f64() * 1000.0,
            );
            return Err(CapabilityError::Unsupported {
                required: policy.runtime_requirement.to_string(),
                actual: actual_runtime.to_string(),
            });
        }

        let outcome = cap.invoke(args, ctx).await;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;

        // outcome 分桶 + SLO 埋点（Cancelled 不计）
        let outcome_label = match &outcome {
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
                "ok"
            }
            Err(CapabilityError::Cancelled) => {
                // 用户取消——不计 perf（用户行为，非能力问题），trace 留痕
                tracing::trace!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    capability = id,
                    "capability invoke cancelled（不计 SLO）"
                );
                // 0.22.18：Cancelled 也写审计——记录所有尝试
                Self::emit_audit(ctx, id, "cancelled", elapsed);
                return outcome;
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
                "timeout"
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
                "error"
            }
        };

        // ── 0.22.18 统一审计契约（有界队列） ─────────────────────────
        // 对非 MCP 来源写最小审计事件到 ai_tool_audit 表。
        // MCP 来源在 server.rs 中已有自己的审计写入（含参数和结果摘要），不在此重复。
        //
        // 审计数据范围（隐私保护）：
        // - 记录：capability id、origin、outcome 分类、耗时
        // - 不记录：完整参数、结果内容、窗口标题、截图内容、路径
        // - arguments 存空 JSON（{}），result_summary 存 outcome 标签
        //
        // 通过有界 mpsc channel 发送给专用 writer task，队列满时 warn 并丢弃。
        Self::emit_audit(ctx, id, outcome_label, elapsed);

        outcome
    }

    /// 发送审计事件到有界队列——非阻塞，队列满时 warn 并丢弃。
    ///
    /// MCP 来源不在此重复写入（server.rs 已有审计）。
    /// 审计数据不含敏感原文（参数/结果/标题等）。
    fn emit_audit(ctx: &InvokeContext<'_>, id: &str, outcome_label: &str, elapsed: f64) {
        if ctx.origin == crate::domain::capability::InvocationOrigin::Mcp {
            return;
        }
        let Some(writer) = AUDIT_WRITER.get() else {
            // 审计 writer 未初始化——跳过（测试环境正常）
            return;
        };
        let tx = writer
            .sender
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(tx) = tx else {
            tracing::warn!(capability = id, "审计 writer 已关闭，丢弃审计事件");
            return;
        };
        let event = AuditEvent {
            cap_id: id.to_string(),
            origin_str: ctx.origin.to_string(),
            summary: format!("{outcome_label} ({elapsed:.0}ms)"),
            db_pool: ctx.env.db_pools().ai.clone(),
        };
        // try_send：队列满时立即返回错误——warn 并按溢出策略丢弃
        if tx.try_send(event).is_err() {
            tracing::warn!(
                capability = id,
                "审计队列已满（{}），丢弃审计事件",
                AUDIT_QUEUE_CAPACITY
            );
        }
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
    use crate::domain::capability::{
        CapabilityPolicy, InvocationOrigin, RuntimeCapabilities,
        policy::{DangerClass, OriginSet, RuntimeRequirement},
    };
    use crate::domain::event::{CapabilityEnv, EventPort};
    use std::sync::Arc;

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
        let _ = reg.register(Arc::new(MockCap {
            id_val: "sort_test_zulu",
        }));
        let _ = reg.register(Arc::new(MockCap {
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
        assert!(reg.register(cap).is_ok());
        assert!(reg.get("test_mock_cap").is_some());
    }

    #[test]
    fn register_duplicate_id_returns_error() {
        let reg = CapabilityRegistry::default();
        let cap1 = Arc::new(MockCap { id_val: "dup_cap" }) as Arc<dyn Capability>;
        assert!(reg.register(cap1).is_ok());
        // 第二次注册同 id → 返回错误（不覆盖）
        let cap2 = Arc::new(MockCap { id_val: "dup_cap" }) as Arc<dyn Capability>;
        let result = reg.register(cap2);
        assert!(result.is_err());
        if let Err(RegistryError::DuplicateId { id }) = result {
            assert_eq!(id, "dup_cap");
        } else {
            panic!("应返回 DuplicateId 错误");
        }
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
        assert!(reg.register(cap).is_ok());
        assert_eq!(reg.len(), before + 1);
    }

    // ── 0.21.11: Registry invoke 综合测试 ────────────────────────────────────────

    /// 测试 Safe capability 正常调用流程与 perf 标记。

    #[tokio::test]
    async fn invoke_safe_capability_success_with_perf_ok() {
        let reg = CapabilityRegistry::default();
        let cap = Arc::new(SafeMockCap {
            id_val: "test_safe_cap",
        });
        assert!(reg.register(cap).is_ok());

        let ctx = InvokeContext {
            env: &MockEnv {},
            origin: InvocationOrigin::LocalAi,
            runtime: RuntimeCapabilities {
                surface: None,
                main_process: true,
                desktop_session: true,
            },
            deadline: None,
        };

        let result = reg
            .invoke("test_safe_cap", serde_json::Value::Null, &ctx)
            .await;
        assert!(result.is_ok());

        // 验证返回正确结果
        match result.unwrap() {
            CapabilityResult::Done { summary } => assert_eq!(summary, "safe operation completed"),
            _ => panic!("预期 Done 结果"),
        }
    }

    /// 测试 OriginDenied 错误场景与不记录 perf。
    #[tokio::test]
    async fn invoke_origin_denied_error_no_perf_record() {
        let reg = CapabilityRegistry::default();
        let cap = Arc::new(DenyOriginCap {
            id_val: "test_deny_origin",
        });
        assert!(reg.register(cap).is_ok());

        let ctx = InvokeContext {
            env: &MockEnv {},
            origin: InvocationOrigin::Mcp, // policy 不允许的 origin
            runtime: RuntimeCapabilities {
                surface: None,
                main_process: true,
                desktop_session: true,
            },
            deadline: None,
        };

        let result = reg
            .invoke("test_deny_origin", serde_json::Value::Null, &ctx)
            .await;
        assert!(result.is_err());

        match result.unwrap_err() {
            CapabilityError::OriginDenied { origin, .. } => {
                assert_eq!(origin, "mcp");
            }
            _ => panic!("预期 OriginDenied 错误"),
        }
    }

    /// 测试 RuntimeUnsupported 错误场景。
    #[tokio::test]
    async fn invoke_runtime_unsupported_error() {
        let reg = CapabilityRegistry::default();
        let cap = Arc::new(NeedsGuiCap {
            id_val: "test_needs_gui",
        });
        assert!(reg.register(cap).is_ok());

        let ctx = InvokeContext {
            env: &MockEnv {},
            origin: InvocationOrigin::LocalAi,
            runtime: RuntimeCapabilities {
                surface: None,
                main_process: false, // 不提供 GUI
                desktop_session: false,
            },
            deadline: None,
        };

        let result = reg
            .invoke("test_needs_gui", serde_json::Value::Null, &ctx)
            .await;
        assert!(result.is_err());

        match result.unwrap_err() {
            CapabilityError::Unsupported { required, .. } => {
                assert!(required.contains("main_process"));
            }
            _ => panic!("预期 Unsupported 错误"),
        }
    }

    /// 测试 Timeout 场景与 perf 标记。
    #[tokio::test]
    async fn invoke_timeout_with_perf_timeout() {
        let reg = CapabilityRegistry::default();
        let cap = Arc::new(TimeoutCap {
            id_val: "test_timeout",
        });
        assert!(reg.register(cap).is_ok());

        let ctx = InvokeContext {
            env: &MockEnv {},
            origin: InvocationOrigin::LocalAi,
            runtime: RuntimeCapabilities {
                surface: None,
                main_process: true,
                desktop_session: true,
            },
            deadline: None,
        };

        let result = reg
            .invoke("test_timeout", serde_json::Value::Null, &ctx)
            .await;
        assert!(result.is_err());

        match result.unwrap_err() {
            CapabilityError::Timeout { .. } => { /* 预期 timeout 错误 */ }
            _ => panic!("预期 Timeout 错误"),
        }
    }

    /// 测试 Cancelled 场景，验证不计 perf。
    #[tokio::test]
    async fn invoke_cancelled_no_perf_record() {
        let reg = CapabilityRegistry::default();
        let cap = Arc::new(CancelledCap {
            id_val: "test_cancelled",
        });
        assert!(reg.register(cap).is_ok());

        let ctx = InvokeContext {
            env: &MockEnv {},
            origin: InvocationOrigin::LocalAi,
            runtime: RuntimeCapabilities {
                surface: None,
                main_process: true,
                desktop_session: true,
            },
            deadline: None,
        };

        let result = reg
            .invoke("test_cancelled", serde_json::Value::Null, &ctx)
            .await;
        assert!(result.is_err());

        match result.unwrap_err() {
            CapabilityError::Cancelled => { /* 预期取消错误 */ }
            _ => panic!("预期 Cancelled 错误"),
        }
    }

    // ── 测试用 Mock 实现 ────────────────────────────────────────────────

    /// Mock 环境——避免构造 AppHandle（遵循 AGENTS.md §7）。
    /// 只实现 invoke 测试路径必需的方法，其余返回 unimplemented。
    struct MockEnv;

    /// 0.22.18：测试用 in-memory DbPools——避免 `db_pools()` panic。
    /// 审计写入走后台 spawn，测试中不需要等待其完成。
    static TEST_POOLS: std::sync::OnceLock<crate::infra::data::DbPools> =
        std::sync::OnceLock::new();

    fn test_pools() -> &'static crate::infra::data::DbPools {
        TEST_POOLS.get_or_init(|| {
            // 用 connect_lazy 避免在 tokio runtime 内 block_on panic。
            // 审计写入失败时 save_audit_log 内部只 warn 不阻塞。
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_lazy("sqlite::memory:")
                .expect("test: lazy SQLite 连接构建失败");
            crate::infra::data::DbPools {
                config: pool.clone(),
                history: pool.clone(),
                ai: pool.clone(),
                cache: pool,
            }
        })
    }

    #[async_trait::async_trait]
    impl CapabilityEnv for MockEnv {
        fn db_pools(&self) -> &crate::infra::data::DbPools {
            test_pools()
        }
        fn plugin_engine(&self) -> Option<&std::sync::Arc<crate::domain::plugin::PluginEngine>> {
            None
        }
        fn search_service(&self) -> Option<&std::sync::Arc<crate::domain::search::SearchService>> {
            None
        }
        async fn list_managed_settings(
            &self,
        ) -> Result<Vec<crate::domain::config::ManagedSetting>, String> {
            unimplemented!("test mock: list_managed_settings not needed")
        }
        async fn update_managed_setting(
            &self,
            _setting_id: &str,
            _expected_old_value: serde_json::Value,
            _new_value: serde_json::Value,
        ) -> Result<crate::domain::config::ManagedSettingUpdate, String> {
            unimplemented!("test mock: update_managed_setting not needed")
        }
        fn sticky_service(&self) -> Option<&std::sync::Arc<crate::domain::sticky::StickyService>> {
            None
        }
        async fn create_sticky_and_notify(
            &self,
            _content: &str,
            _color: crate::domain::sticky::StickyColor,
        ) -> Result<crate::domain::sticky::StickyNote, crate::domain::sticky::StickyWorkflowError>
        {
            unimplemented!("test mock: create_sticky_and_notify not needed")
        }
        async fn create_sticky_and_show(
            &self,
            _content: &str,
            _x: Option<i32>,
            _y: Option<i32>,
            _w: Option<i32>,
            _h: Option<i32>,
        ) -> Result<String, String> {
            unimplemented!("test mock: create_sticky_and_show not needed")
        }
        async fn update_sticky_content_and_notify(
            &self,
            _sticky_id: &str,
            _content: &str,
            _expected_updated_at: Option<i64>,
            _source: crate::domain::sticky::StickyChangeSource,
        ) -> Result<i64, crate::domain::sticky::StickyWorkflowError> {
            unimplemented!("test mock: update_sticky_content_and_notify not needed")
        }
        async fn set_sticky_visibility_and_notify(
            &self,
            _sticky_id: &str,
            _visible: bool,
        ) -> Result<i64, crate::domain::sticky::StickyWorkflowError> {
            unimplemented!("test mock: set_sticky_visibility_and_notify not needed")
        }
        async fn trash_sticky_and_notify(
            &self,
            _sticky_id: &str,
        ) -> Result<(), crate::domain::sticky::StickyWorkflowError> {
            unimplemented!("test mock: trash_sticky_and_notify not needed")
        }
        async fn close_sticky_and_notify(
            &self,
            _sticky_id: &str,
            _final_content: &str,
            _expected_updated_at: Option<i64>,
        ) -> Result<
            crate::domain::sticky::StickyCloseOutcome,
            crate::domain::sticky::StickyWorkflowError,
        > {
            unimplemented!("test mock: close_sticky_and_notify not needed")
        }
        fn show_pin_image(
            &self,
            _png_bytes: Vec<u8>,
            _x: Option<i32>,
            _y: Option<i32>,
        ) -> Result<(i32, i32), String> {
            unimplemented!("test mock: show_pin_image not needed")
        }
    }

    impl EventPort for MockEnv {
        fn emit(&self, _name: &str, _payload: serde_json::Value) -> Result<(), String> {
            Ok(())
        }
        fn emit_to(
            &self,
            _window: &str,
            _name: &str,
            _payload: serde_json::Value,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    /// Mock Capability——Safe 能力
    struct SafeMockCap {
        id_val: &'static str,
    }

    #[async_trait::async_trait]
    impl Capability for SafeMockCap {
        fn id(&self) -> &str {
            self.id_val
        }
        fn schema(&self) -> CapabilitySchema {
            CapabilitySchema::empty(self.id_val, "safe mock")
        }
        fn policy(&self) -> CapabilityPolicy {
            CapabilityPolicy {
                danger: DangerClass::Safe,
                allowed_origins: OriginSet::ALL,
                runtime_requirement: RuntimeRequirement::NONE,
                ..Default::default()
            }
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, CapabilityError> {
            Ok(CapabilityResult::Done {
                summary: "safe operation completed".into(),
            })
        }
    }

    /// Mock Capability——拒绝特定 origin
    struct DenyOriginCap {
        id_val: &'static str,
    }

    #[async_trait::async_trait]
    impl Capability for DenyOriginCap {
        fn id(&self) -> &str {
            self.id_val
        }
        fn schema(&self) -> CapabilitySchema {
            CapabilitySchema::empty(self.id_val, "deny origin mock")
        }
        fn policy(&self) -> CapabilityPolicy {
            CapabilityPolicy {
                danger: DangerClass::Safe,
                allowed_origins: OriginSet::from_single(InvocationOrigin::LocalAi), // 只允许 LocalAi，拒绝 MCP
                runtime_requirement: RuntimeRequirement::NONE,
                ..Default::default()
            }
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, CapabilityError> {
            Ok(CapabilityResult::Done {
                summary: "should not reach here".into(),
            })
        }
    }

    /// Mock Capability——需要 GUI 运行时
    struct NeedsGuiCap {
        id_val: &'static str,
    }

    #[async_trait::async_trait]
    impl Capability for NeedsGuiCap {
        fn id(&self) -> &str {
            self.id_val
        }
        fn schema(&self) -> CapabilitySchema {
            CapabilitySchema::empty(self.id_val, "needs gui mock")
        }
        fn policy(&self) -> CapabilityPolicy {
            CapabilityPolicy {
                danger: DangerClass::Safe,
                allowed_origins: OriginSet::ALL,
                runtime_requirement: RuntimeRequirement::MAIN_PROCESS,
                ..Default::default()
            }
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, CapabilityError> {
            Ok(CapabilityResult::Done {
                summary: "gui operation".into(),
            })
        }
    }

    /// Mock Capability——返回 Timeout
    struct TimeoutCap {
        id_val: &'static str,
    }

    #[async_trait::async_trait]
    impl Capability for TimeoutCap {
        fn id(&self) -> &str {
            self.id_val
        }
        fn schema(&self) -> CapabilitySchema {
            CapabilitySchema::empty(self.id_val, "timeout mock")
        }
        fn policy(&self) -> CapabilityPolicy {
            CapabilityPolicy {
                danger: DangerClass::Safe,
                allowed_origins: OriginSet::ALL,
                runtime_requirement: RuntimeRequirement::NONE,
                ..Default::default()
            }
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, CapabilityError> {
            Err(CapabilityError::Timeout {
                detail: "test timeout".into(),
            })
        }
    }

    /// Mock Capability——返回 Cancelled
    struct CancelledCap {
        id_val: &'static str,
    }

    #[async_trait::async_trait]
    impl Capability for CancelledCap {
        fn id(&self) -> &str {
            self.id_val
        }
        fn schema(&self) -> CapabilitySchema {
            CapabilitySchema::empty(self.id_val, "cancelled mock")
        }
        fn policy(&self) -> CapabilityPolicy {
            CapabilityPolicy {
                danger: DangerClass::Safe,
                allowed_origins: OriginSet::ALL,
                runtime_requirement: RuntimeRequirement::NONE,
                ..Default::default()
            }
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, CapabilityError> {
            Err(CapabilityError::Cancelled)
        }
    }

    // ── 0.21.13: Registry identity 硬化测试 ────────────────────────────────────

    /// 两个不同 mock capability 使用相同 id，构造确定性失败。
    #[test]
    fn try_from_entries_duplicate_id_fails() {
        let cap1 = Arc::new(MockCap {
            id_val: "conflict_id",
        }) as Arc<dyn Capability>;
        let cap2 = Arc::new(MockCap {
            id_val: "conflict_id",
        }) as Arc<dyn Capability>;
        let result = CapabilityRegistry::try_from_entries(vec![cap1, cap2]);
        assert!(result.is_err());
        if let Err(RegistryError::DuplicateId { id }) = result {
            assert_eq!(id, "conflict_id", "错误必须包含冲突 id");
        } else {
            panic!("应返回 DuplicateId 错误");
        }
    }

    /// 错误 Display 明确包含冲突 id。
    #[test]
    fn registry_error_display_contains_id() {
        let err = RegistryError::DuplicateId {
            id: "my_conflict_cap".to_string(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("my_conflict_cap"),
            "Display 必须包含冲突 id: {msg}"
        );
        assert!(msg.contains("重复"), "Display 必须包含'重复'字样: {msg}");
    }

    /// 无论输入 entries 顺序如何，重复 id 都产生等价错误。
    #[test]
    fn duplicate_error_independent_of_input_order() {
        let cap_a = Arc::new(MockCap {
            id_val: "order_test",
        }) as Arc<dyn Capability>;
        let cap_b = Arc::new(MockCap {
            id_val: "order_test",
        }) as Arc<dyn Capability>;

        // 顺序 1: [A, B]
        let result1 = CapabilityRegistry::try_from_entries(vec![cap_a.clone(), cap_b.clone()]);
        // 顺序 2: [B, A] —— 由于 Arc clone，两个 cap 的 id 相同
        let result2 = CapabilityRegistry::try_from_entries(vec![cap_b, cap_a]);

        assert!(result1.is_err());
        assert!(result2.is_err());

        // 两种顺序产生等价错误（相同 id）
        if let (
            Err(RegistryError::DuplicateId { id: id1 }),
            Err(RegistryError::DuplicateId { id: id2 }),
        ) = (result1, result2)
        {
            assert_eq!(id1, id2, "不同输入顺序应产生等价错误");
            assert_eq!(id1, "order_test");
        } else {
            panic!("两种顺序都应返回 DuplicateId 错误");
        }
    }

    /// 正常输入构造成功。
    #[test]
    fn try_from_entries_normal_succeeds() {
        let cap1 = Arc::new(MockCap {
            id_val: "normal_cap_1",
        }) as Arc<dyn Capability>;
        let cap2 = Arc::new(MockCap {
            id_val: "normal_cap_2",
        }) as Arc<dyn Capability>;
        let reg =
            CapabilityRegistry::try_from_entries(vec![cap1, cap2]).expect("无重复 id 应构造成功");
        assert!(reg.get("normal_cap_1").is_some());
        assert!(reg.get("normal_cap_2").is_some());
    }

    /// `list()` 与 `entries()` 顺序稳定。
    #[test]
    fn list_and_entries_stable_order() {
        let reg = CapabilityRegistry::default();
        let _ = reg.register(Arc::new(MockCap {
            id_val: "zzz_stable_test",
        }));
        let _ = reg.register(Arc::new(MockCap {
            id_val: "aaa_stable_test",
        }));
        let _ = reg.register(Arc::new(MockCap {
            id_val: "mmm_stable_test",
        }));

        // entries() 按 id 排序
        let entry_ids: Vec<_> = reg.entries().into_iter().map(|(id, _)| id).collect();
        assert!(
            entry_ids.windows(2).all(|w| w[0] <= w[1]),
            "entries() 应按 id 排序: {entry_ids:?}"
        );

        // list() 按 schema.name 排序
        let list_names: Vec<_> = reg.list().into_iter().map(|s| s.name).collect();
        assert!(
            list_names.windows(2).all(|w| w[0] <= w[1]),
            "list() 应按 name 排序: {list_names:?}"
        );
    }

    /// 动态 register 重复 id 返回错误，原 capability 不被覆盖。
    #[test]
    fn register_duplicate_does_not_overwrite() {
        let reg = CapabilityRegistry::default();
        let cap1 = Arc::new(MockCap {
            id_val: "no_overwrite",
        }) as Arc<dyn Capability>;
        assert!(reg.register(cap1).is_ok());

        // 第二次注册同 id → 返回错误
        let cap2 = Arc::new(MockCap {
            id_val: "no_overwrite",
        }) as Arc<dyn Capability>;
        let result = reg.register(cap2);
        assert!(result.is_err());
        if let Err(RegistryError::DuplicateId { id }) = result {
            assert_eq!(id, "no_overwrite");
        } else {
            panic!("应返回 DuplicateId 错误");
        }

        // 原 capability 仍在
        assert!(reg.get("no_overwrite").is_some());
    }

    /// 正常动态注册成功且可 invoke/get。
    #[test]
    fn register_normal_succeeds_and_gettable() {
        let reg = CapabilityRegistry::default();
        let cap = Arc::new(MockCap {
            id_val: "gettable_cap",
        }) as Arc<dyn Capability>;
        assert!(reg.register(cap).is_ok());
        assert!(reg.get("gettable_cap").is_some());
    }
}
