//! Capability 能力协议层（0.9.7）。
//!
//! 一切"做一件事拿一个结果"的统一抽象——供 Action 编排 / AI tool_call /
//! （0.11）CLI 暴露 / MCP server 共享。
//!
//! **与 Action 的根本区别**（文档 §3.4）：
//! - Action 是「交互动作」——含 UI 副作用（弹窗/emit/copy），面向用户
//! - Capability 是「纯能力」——入参→出参，无 UI 假设，面向所有调用方
//!
//! **协议两层投影**（文档 §3.1）：
//! - 协议层（对外）：`invoke(Value) → Result<Value, Error>`，纯 JSON 进出。0.11 CLI/MCP 派生的输入。
//! - 进程内层（对内）：`invoke(args, ctx) → Result<CapabilityResult, CapabilityError>`。0.9.7 落地的形态。
//!
//! 两层是一个东西的两个投影：0.9.7 只实现进程内层，但签名设计保证协议层可零摩擦派生
//! （`args: Value` 已经是纯 JSON，`CapabilityResult` 已经 `Serialize`）。
//!
//! **超时/取消/SLO 三铁则**（文档 §3.5）：
//! - 硬超时：`InvokeContext.deadline`，对齐 `AIProvider::complete` 硬超时铁则
//! - 取消：drop future + seq 校验，复用 AI stream cancel 模式
//! - SLO：`MetricCategory::Capability`，`CapabilityRegistry::invoke` 包装层自动埋点

pub(crate) mod builtins; // Step 2 填真实能力（capture_screen 等）
mod error;
mod projection;
mod registry;
mod result;
mod schema;

pub use error::CapabilityError;
#[allow(unused_imports)]
pub use projection::{ActionDef, ActionKindDef, ProjectionRule, ResultShape, project};
pub use registry::CapabilityRegistry;
pub use result::{
    CapabilityResult, ItemAction, ItemResult, derive_title,
    rig_tool_result_to_text,
};
pub use schema::CapabilitySchema;

use serde_json::Value;

// ── inventory 收集（链接期自动注册）──────────────────────────────────────────

/// inventory 收集项——每个 Capability 文件 submit 一行，Registry 启动时自动收集。
///
/// `factory` 是零参函数指针（满足 inventory 的 `'static` 约束）。
/// 有状态能力的运行时依赖通过 `ctx.env` 获取——不在构造时注入。
pub struct CapabilityEntry {
    pub factory: fn() -> std::sync::Arc<dyn Capability>,
}

inventory::collect!(CapabilityEntry);

// ── Capability trait ─────────────────────────────────────────────────────────

/// 原子能力——一切"做一件事拿一个结果"的统一抽象（文档 §3.2）。
///
/// 四种调用方共享同一份 Capability：
/// - Action.execute() 编排它（如 ScreenshotAction 编排 capture_screen）
/// - AI tool 直接 tool_call 它（0.10 语音找文件 / Agent 窗口）
/// - （0.11）CLI 派生 `blink screenshot --display 1`
/// - （0.11）MCP server 暴露给外部 Agent
#[async_trait::async_trait]
pub trait Capability: Send + Sync {
    /// 唯一标识（如 `"capture_screen"`）。
    fn id(&self) -> &str;

    /// 能力自述——送 LLM 的 tool schema（纯 JSON Schema）。
    /// 被 `CapabilityRegistry::list()` → `build_capability_tools()` 消费。
    fn schema(&self) -> CapabilitySchema;

    /// 危险等级（复用 `execution::DangerClass`，保持单一安全枚举）。
    /// default `Safe`——危险动作（如 delete_file）显式 override。
    /// AI 入口通过 `requires_ai_confirmation()` 统一消费此字段。
    fn danger_class(&self) -> crate::domain::execution::DangerClass {
        crate::domain::execution::DangerClass::Safe
    }

    /// AI 调用前是否必须经过用户确认。
    ///
    /// 危险副作用与敏感数据读取共用同一条硬边界。所有 AI 入口必须调用本方法，
    /// 避免主窗口与对话窗口分别实现后产生策略漂移。
    fn requires_ai_confirmation(&self) -> bool {
        matches!(
            self.danger_class(),
            crate::domain::execution::DangerClass::Dangerous
        ) || self.schema().sensitive
    }

    /// 纯能力执行：入参 → 出参。不碰 UI、不 emit、不弹窗。
    ///
    /// 运行时依赖通过 `ctx.env` 获取（满足 inventory 零参构造）。
    /// **硬超时铁则**（§3.5）：长耗时实现方必须在关键 await 点检查 `ctx.is_expired()`
    /// 或用 `tokio::time::timeout_at(ctx.deadline_or_far_future(), ...)` 包裹。
    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError>;
}

// ── InvokeContext ────────────────────────────────────────────────────────────

/// invoke 时的运行时上下文——运行时依赖通过 `env` 获取（满足 inventory 零参构造）。
///
/// 协议层（0.11 CLI/MCP）投影时，ctx 从环境变量/连接读取，不影响实现方。
///
/// **超时铁则**（§3.5 铁则 1，对齐 [0.9 AIProvider §3.3](../../ai/provider.rs)）：
/// `deadline` 为绝对截止时刻。Capability `invoke` 实现方**必须**在长耗时操作前
/// 检查 `is_expired()`，或用 `timeout_at(deadline_or_far_future())` 包裹。
/// 调用方负责传入合理 deadline（AI lane 从 `slo_hard_timeout_ms` 派生）。
///
/// **0.14.6 §2.2**：`app_handle: &tauri::AppHandle` 替换为最小化的
/// `env: &dyn CapabilityEnv`，domain 层不再直接依赖 tauri，且 Capability
/// 在类型层面拿不到事件、窗口或进程控制权限。
pub struct InvokeContext<'a> {
    /// 领域环境——能力通过它访问 managed state（如 `DbPools`、`SearchService`）。
    /// 满足 inventory 零参构造：config 不在构造时注入，在调用时通过 env 自取。
    pub env: &'a dyn crate::domain::event::CapabilityEnv,
    /// 绝对截止时刻。`None` = 无超时（仅本地同步编排，如 Alt+A 截图）。
    /// AI lane / 异步编排路径**必须**传 `Some`。
    pub deadline: Option<std::time::Instant>,
}

impl<'a> InvokeContext<'a> {
    /// 是否已超时。Capability 实现方在长耗时循环/网络调用前 poll 此方法。
    pub fn is_expired(&self) -> bool {
        self.deadline
            .map(|d| std::time::Instant::now() >= d)
            .unwrap_or(false)
    }

    /// 转 `tokio::time::Instant` 供 `timeout_at` 用。
    /// `None` 时返回远期 future（不超时）——让实现方无需分支处理。
    pub fn deadline_or_far_future(&self) -> tokio::time::Instant {
        self.deadline
            .map(tokio::time::Instant::from_std)
            .unwrap_or_else(|| tokio::time::Instant::now() + tokio::time::Duration::from_secs(3600))
    }
}

#[cfg(test)]
mod tests {
    // 直接测 is_expired / deadline_or_far_future 的纯逻辑——
    // 这两个方法只读 self.deadline，不碰 app_handle/config，故用镜像函数覆盖
    // （与 execution/mod.rs:216 同模式：避开造 AppHandle 的 UB）。

    fn is_expired(deadline: Option<std::time::Instant>) -> bool {
        deadline
            .map(|d| std::time::Instant::now() >= d)
            .unwrap_or(false)
    }

    fn resolve_to_tokio(deadline: Option<std::time::Instant>) -> tokio::time::Instant {
        deadline
            .map(tokio::time::Instant::from_std)
            .unwrap_or_else(|| tokio::time::Instant::now() + tokio::time::Duration::from_secs(3600))
    }

    #[test]
    fn deadline_none_is_never_expired() {
        assert!(!is_expired(None));
    }

    #[test]
    fn deadline_past_is_expired() {
        let past = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        assert!(is_expired(past));
    }

    #[test]
    fn deadline_future_not_expired() {
        let future = Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
        assert!(!is_expired(future));
    }

    #[test]
    fn none_deadline_resolves_to_far_future() {
        let resolved = resolve_to_tokio(None);
        assert!(resolved > tokio::time::Instant::now());
    }

    #[test]
    fn some_deadline_preserves_value() {
        let d = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let resolved = resolve_to_tokio(Some(d));
        // 转 tokio::Instant 后应代表同一时刻（容许微秒级偏差）
        let diff = resolved.saturating_duration_since(tokio::time::Instant::now());
        assert!(diff.as_secs() >= 29 && diff.as_secs() <= 31);
    }
}
