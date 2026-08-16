//! 系统操作 Capability 集合（0.21.1）——从 Action 迁移为 Dangerous Capability。
//!
//! 每个系统操作保持独立 Capability（稳定 id、参数、确认和审计），
//! 禁止合并为 `system_operation { operation }`。
//!
//! - lock / shutdown / restart / sleep：Dangerous，AI 默认关闭，MCP 禁止，逐次确认
//! - clear_history：Dangerous，返回实际删除数
//! - exit_blink：Dangerous local-only，MCP 禁止

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

// ── Lock ────────────────────────────────────────────────────────────────────

pub struct LockWorkstation;

#[async_trait::async_trait]
impl Capability for LockWorkstation {
    fn id(&self) -> &str {
        "lock"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "lock".into(),
            description: "Lock the Windows workstation".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL_LOCAL,
            runtime_requirement: RuntimeRequirement::DESKTOP_SESSION,
            danger: DangerClass::Dangerous,
            sensitive: false,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::dangerous(true),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let _ = crate::infra::platform::lock::lock_workstation();
        Ok(CapabilityResult::Done {
            summary: "已锁定工作站".into(),
        })
    }
}

// ── Shutdown ────────────────────────────────────────────────────────────────

pub struct Shutdown;

#[async_trait::async_trait]
impl Capability for Shutdown {
    fn id(&self) -> &str {
        "shutdown"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "shutdown".into(),
            description: "Shut down the computer immediately".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL_LOCAL,
            runtime_requirement: RuntimeRequirement::DESKTOP_SESSION,
            danger: DangerClass::Dangerous,
            sensitive: false,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            // 不可记忆——每次都必须确认
            confirmation: ConfirmationPolicy::dangerous(false),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("shutdown.exe")
                .args(["/s", "/t", "0"])
                .spawn();
        }
        Ok(CapabilityResult::Done {
            summary: "关机指令已发送".into(),
        })
    }
}

// ── Restart ─────────────────────────────────────────────────────────────────

pub struct Restart;

#[async_trait::async_trait]
impl Capability for Restart {
    fn id(&self) -> &str {
        "restart"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "restart".into(),
            description: "Restart the computer immediately".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL_LOCAL,
            runtime_requirement: RuntimeRequirement::DESKTOP_SESSION,
            danger: DangerClass::Dangerous,
            sensitive: false,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::dangerous(false),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("shutdown.exe")
                .args(["/r", "/t", "0"])
                .spawn();
        }
        Ok(CapabilityResult::Done {
            summary: "重启指令已发送".into(),
        })
    }
}

// ── Sleep ───────────────────────────────────────────────────────────────────

pub struct Sleep;

#[async_trait::async_trait]
impl Capability for Sleep {
    fn id(&self) -> &str {
        "sleep"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "sleep".into(),
            description: "Put the computer to sleep".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL_LOCAL,
            runtime_requirement: RuntimeRequirement::DESKTOP_SESSION,
            danger: DangerClass::Dangerous,
            sensitive: false,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::dangerous(true),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("rundll32.exe")
                .args(["powrprof.dll,SetSuspendState", "0,1,0"])
                .spawn();
        }
        Ok(CapabilityResult::Done {
            summary: "睡眠指令已发送".into(),
        })
    }
}

// ── ClearHistory ────────────────────────────────────────────────────────────

pub struct ClearHistory;

#[async_trait::async_trait]
impl Capability for ClearHistory {
    fn id(&self) -> &str {
        "clear_history"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "clear_history".into(),
            description: "Clear all app launch history records (irreversible)".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL_LOCAL,
            runtime_requirement: RuntimeRequirement::MAIN_PROCESS,
            danger: DangerClass::Dangerous,
            sensitive: false,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::dangerous(true),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let pool = &ctx.env.db_pools().history;
        crate::infra::data::history::clear(pool).await;
        tracing::info!("搜索历史已清空");
        Ok(CapabilityResult::Done {
            summary: "搜索历史已清空".into(),
        })
    }
}

// ── ExitBlink ───────────────────────────────────────────────────────────────

pub struct ExitBlink;

#[async_trait::async_trait]
impl Capability for ExitBlink {
    fn id(&self) -> &str {
        "exit_blink"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "exit_blink".into(),
            description: "Exit the Blink launcher process".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            // local-only：仅本地入口可触发
            allowed_origins: OriginSet::LOCAL_SURFACE | OriginSet::LOCAL_COMMAND,
            runtime_requirement: RuntimeRequirement::GUI_SURFACE,
            danger: DangerClass::Dangerous,
            sensitive: false,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::dangerous(true),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let surface = ctx
            .runtime
            .surface
            .ok_or_else(|| CapabilityError::Unsupported {
                required: RuntimeRequirement::GUI_SURFACE.to_string(),
                actual: ctx.runtime.as_requirement().to_string(),
            })?;
        surface.hide_main_window("exit_blink");
        surface.exit_app();
        Ok(CapabilityResult::Done {
            summary: "Blink 已退出".into(),
        })
    }
}

// ── inventory 注册 ──────────────────────────────────────────────────────────

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(LockWorkstation) as Arc<dyn Capability>,
});
inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(Shutdown) as Arc<dyn Capability>,
});
inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(Restart) as Arc<dyn Capability>,
});
inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(Sleep) as Arc<dyn Capability>,
});
inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(ClearHistory) as Arc<dyn Capability>,
});
inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(ExitBlink) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_dangerous_ai_off_mcp_forbidden() {
        let p = LockWorkstation.policy();
        assert_eq!(p.danger, DangerClass::Dangerous);
        assert_eq!(p.ai_default, AiDefault::Off);
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
        assert!(p.confirmation.required);
        assert!(p.confirmation.rememberable); // lock 可记忆
    }

    #[test]
    fn shutdown_is_dangerous_not_rememberable() {
        let p = Shutdown.policy();
        assert_eq!(p.danger, DangerClass::Dangerous);
        assert!(!p.confirmation.rememberable); // 不可记忆
    }

    #[test]
    fn restart_is_dangerous_not_rememberable() {
        let p = Restart.policy();
        assert_eq!(p.danger, DangerClass::Dangerous);
        assert!(!p.confirmation.rememberable);
    }

    #[test]
    fn sleep_is_dangerous_rememberable() {
        let p = Sleep.policy();
        assert_eq!(p.danger, DangerClass::Dangerous);
        assert!(p.confirmation.rememberable);
    }

    #[test]
    fn clear_history_needs_main_process() {
        let p = ClearHistory.policy();
        assert_eq!(p.runtime_requirement, RuntimeRequirement::MAIN_PROCESS);
        assert_eq!(p.danger, DangerClass::Dangerous);
    }

    #[test]
    fn exit_blink_is_local_only() {
        let p = ExitBlink.policy();
        // 不允许 AI / CLI / MCP
        assert!(!p.allows_origin(crate::domain::capability::InvocationOrigin::LocalAi));
        assert!(!p.allows_origin(crate::domain::capability::InvocationOrigin::Cli));
        assert!(!p.allows_origin(crate::domain::capability::InvocationOrigin::Mcp));
        // 只允许 Surface / Command
        assert!(p.allows_origin(crate::domain::capability::InvocationOrigin::LocalSurface));
        assert!(p.allows_origin(crate::domain::capability::InvocationOrigin::LocalCommand));
    }

    #[test]
    fn all_system_caps_have_non_empty_schema_description() {
        for s in [
            LockWorkstation.schema(),
            Shutdown.schema(),
            Restart.schema(),
            Sleep.schema(),
            ClearHistory.schema(),
            ExitBlink.schema(),
        ] {
            assert!(!s.description.is_empty());
        }
    }
}
