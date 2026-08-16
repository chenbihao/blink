//! `open_logs` 和 `open_data_dir` Capability（0.21.1）——从 Action 迁移。
//!
//! Safe GUI Capability，需 MAIN_PROCESS + DESKTOP_SESSION 运行时。
//! AI 推荐 allowlist 默认开启；MCP 代码级禁止（GUI 副作用）。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

// ── OpenLogs ────────────────────────────────────────────────────────────────

pub struct OpenLogs;

#[async_trait::async_trait]
impl Capability for OpenLogs {
    fn id(&self) -> &str {
        "open_logs"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "open_logs".into(),
            description: "Open the Blink log file with the default viewer".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL_LOCAL,
            runtime_requirement: RuntimeRequirement::MAIN_PROCESS
                | RuntimeRequirement::DESKTOP_SESSION,
            danger: DangerClass::Safe,
            sensitive: false,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::safe(),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let log_path = crate::infra::utils::logging::current_log_file();
        let log_dir = crate::infra::utils::logging::log_dir();

        if log_path.exists() {
            if let Err(e) = open::that(&log_path) {
                tracing::error!(error = %e, "打开日志文件失败，尝试打开目录");
                let _ = open::that(&log_dir);
            }
        } else {
            let _ = open::that(&log_dir);
        }
        Ok(CapabilityResult::Done {
            summary: "已打开日志文件".into(),
        })
    }
}

// ── OpenDataDir ─────────────────────────────────────────────────────────────

pub struct OpenDataDir;

#[async_trait::async_trait]
impl Capability for OpenDataDir {
    fn id(&self) -> &str {
        "open_data_dir"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "open_data_dir".into(),
            description: "Open the Blink data folder in the system file manager".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL_LOCAL,
            runtime_requirement: RuntimeRequirement::MAIN_PROCESS
                | RuntimeRequirement::DESKTOP_SESSION,
            danger: DangerClass::Safe,
            sensitive: false,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::safe(),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let dir = crate::infra::utils::paths::app_data_dir();
        if let Err(e) = open::that(&dir) {
            tracing::error!(error = %e, dir = %dir.display(), "打开数据目录失败");
        }
        Ok(CapabilityResult::Done {
            summary: "已打开数据目录".into(),
        })
    }
}

// ── inventory 注册 ──────────────────────────────────────────────────────────

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(OpenLogs) as Arc<dyn Capability>,
});
inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(OpenDataDir) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_logs_is_safe_ai_on() {
        let p = OpenLogs.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert_eq!(p.ai_default, AiDefault::On);
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
    }

    #[test]
    fn open_data_dir_is_safe_ai_on() {
        let p = OpenDataDir.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert_eq!(p.ai_default, AiDefault::On);
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
    }

    #[test]
    fn both_need_desktop_session() {
        let p = OpenLogs.policy();
        // runtime = MAIN_PROCESS | DESKTOP_SESSION，需要两者都满足
        let full = RuntimeRequirement::MAIN_PROCESS | RuntimeRequirement::DESKTOP_SESSION;
        assert!(p.runtime_requirement.is_satisfied_by(full));
        let p = OpenDataDir.policy();
        assert!(p.runtime_requirement.is_satisfied_by(full));
    }
}
