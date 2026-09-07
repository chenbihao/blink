//! `manage_window` Capability（0.22.14）。
//!
//! 基础窗口管理——AI 通过 `window_ref` 操作窗口状态。
//!
//! **支持操作**：
//! - `activate` — 激活窗口（bring to front + set focus）
//! - `minimize` — 最小化窗口
//! - `maximize` — 最大化窗口
//! - `restore` — 从最小化/最大化恢复到正常状态
//!
//! **安全模型**：
//! - 必须使用 `list_windows` 返回的 `window_ref`，**禁止使用裸 HWND**
//! - `window_ref` 在 `SurfacePort::manage_window_action` 中校验 HWND 有效性、PID 一致性、
//!   标题一致性、TTL 和 generation，过期或失效时返回 `StaleRef`，AI 应重新调用
//!   `list_windows`
//! - **禁止操作 Blink 自身窗口**——在执行任何窗口动作前检查 `is_blink`，
//!   返回 `SelfWindowForbidden` 结构化错误，确保零副作用
//!
//! **风险等级**：`Safe`——窗口状态操作是可逆的，不涉及数据丢失。
//! `activate` 可能影响用户焦点，但不造成不可逆后果。
//!
//! **sensitive=true**：窗口标题（隐含在 window_ref 校验中）属隐私敏感数据。
//!
//! **spawn_blocking**：Win32 `ShowWindow` / `SetForegroundWindow` 是同步 API，
//! 按 spec-backend §一"阻塞操作隔离"铁则，必须 `spawn_blocking`。
//!
//! **隐私**：日志中不记录完整窗口标题，只记录 action 和 process_name。
//!
//! **0.22.14 review**：重构为通过 `SurfacePort::manage_window_action` 调用，
//! 不再直接依赖 `infra::platform::window`。窗口操作后由 `SurfacePort` 实现
//! 核验 `IsIconic`/`IsZoomed` 状态。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
    SurfaceError,
};

/// `manage_window` — 通过 window_ref 管理窗口状态。
///
/// 入参：`{ "window_ref": "wref_...", "action": "activate" }`
/// 出参：`Done { summary: "已激活窗口 ({process_name})" }`
pub struct ManageWindow;

/// 窗口操作枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowAction {
    Activate,
    Minimize,
    Maximize,
    Restore,
}

impl WindowAction {
    fn parse(val: Option<&Value>) -> Result<Self, CapabilityError> {
        match val {
            None => Err(CapabilityError::InvalidArgs {
                detail: "manage_window: 缺少 action 参数".into(),
            }),
            Some(Value::String(s)) => match s.as_str() {
                "activate" => Ok(Self::Activate),
                "minimize" => Ok(Self::Minimize),
                "maximize" => Ok(Self::Maximize),
                "restore" => Ok(Self::Restore),
                other => Err(CapabilityError::InvalidArgs {
                    detail: format!(
                        "manage_window: 未知 action: {other}，允许: activate | minimize | maximize | restore"
                    ),
                }),
            },
            Some(other) => Err(CapabilityError::InvalidArgs {
                detail: format!("manage_window: action 应为字符串，实际值: {other}"),
            }),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Activate => "激活",
            Self::Minimize => "最小化",
            Self::Maximize => "最大化",
            Self::Restore => "恢复",
        }
    }

    fn as_action_str(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Minimize => "minimize",
            Self::Maximize => "maximize",
            Self::Restore => "restore",
        }
    }
}

/// 将 `SurfaceError` 映射到结构化 `CapabilityError`。
///
/// 穷尽匹配，确保 domain 层不泄漏 SurfaceError 内部细节。
fn map_surface_error(err: SurfaceError) -> CapabilityError {
    match err {
        SurfaceError::CreateFailed { detail } => CapabilityError::Internal { detail },
        SurfaceError::Unavailable { detail } => CapabilityError::StaleRef { detail },
        SurfaceError::ActivationFailed { detail } => CapabilityError::Internal { detail },
        SurfaceError::RestoreFailed { detail } => CapabilityError::Internal { detail },
        SurfaceError::WindowStateMismatch { expected, actual } => CapabilityError::Internal {
            detail: format!("窗口操作未生效：期望{expected}，实际{actual}"),
        },
    }
}

#[async_trait::async_trait]
impl Capability for ManageWindow {
    fn id(&self) -> &str {
        "manage_window"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "manage_window".into(),
            description: "通过 window_ref 管理窗口状态。支持 activate（激活窗口）、minimize（最小化）、maximize（最大化）、restore（从最小化/最大化恢复）。window_ref 从 list_windows 获取，过期后需重新调用 list_windows。禁止操作 Blink 自身窗口。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "window_ref": {
                        "type": "string",
                        "description": "窗口引用（从 list_windows 获取）"
                    },
                    "action": {
                        "type": "string",
                        "enum": ["activate", "minimize", "maximize", "restore"],
                        "description": "窗口操作类型"
                    }
                },
                "required": ["window_ref", "action"]
            }),
            sensitive: true,
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::DESKTOP_SESSION,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::DefaultOff,
            confirmation: ConfirmationPolicy::sensitive(),
        }
    }

    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 铁则 1 前置检查
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "manage_window 截止时刻已过".into(),
            });
        }

        let ref_id = args
            .get("window_ref")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "manage_window: 缺少 window_ref 参数".into(),
            })?;

        let action = WindowAction::parse(args.get("action"))?;

        // 通过 SurfacePort 执行——domain 层不直接接触 infra
        let surface = ctx
            .runtime
            .surface
            .ok_or_else(|| CapabilityError::Unsupported {
                required: "surface (GUI runtime)".into(),
                actual: ctx.runtime.as_requirement().to_string(),
            })?;

        // 先校验 window_ref 是否属于 Blink 自身窗口（零副作用检查）
        let hwnd = surface
            .validate_window_ref(ref_id)
            .map_err(map_surface_error)?;

        if surface.is_blink_hwnd(hwnd) {
            return Err(CapabilityError::SelfWindowForbidden {
                detail: "manage_window 不允许操作 Blink 自身窗口，Blink 自隐藏只走截图事务".into(),
            });
        }

        // 执行窗口操作（SurfacePort 实现内部包含 spawn_blocking + 状态核验）
        let result = surface
            .manage_window_action(ref_id, action.as_action_str())
            .await
            .map_err(map_surface_error)?;

        // activate 失败（SetForegroundWindow 返回 false，Windows 前台锁定限制）
        if action == WindowAction::Activate && !result.success {
            tracing::warn!(
                action = action.as_str(),
                process_name = %result.process_name,
                "manage_window: activate 失败（Windows 前台锁定限制）"
            );
            return Ok(CapabilityResult::Done {
                summary: format!(
                    "窗口 ({}) 激活请求已发送，但可能因系统前台锁定限制未生效",
                    result.process_name
                ),
            });
        }

        tracing::debug!(
            action = action.as_str(),
            process_name = %result.process_name,
            "manage_window: 操作完成"
        );

        let action_desc = match action {
            WindowAction::Activate => "已激活",
            WindowAction::Minimize => "已最小化",
            WindowAction::Maximize => "已最大化",
            WindowAction::Restore => "已恢复",
        };

        Ok(CapabilityResult::Done {
            summary: format!("{}窗口 ({})", action_desc, result.process_name),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(ManageWindow) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_manage_window() {
        assert_eq!(ManageWindow.id(), "manage_window");
    }

    #[test]
    fn schema_has_window_ref_and_action() {
        let s = ManageWindow.schema();
        assert_eq!(s.name, "manage_window");
        let props = &s.parameters["properties"];
        assert_eq!(props["window_ref"]["type"], "string");
        assert_eq!(props["action"]["type"], "string");
        let actions = props["action"]["enum"].as_array().unwrap();
        assert_eq!(actions.len(), 4);
        assert!(actions.contains(&json!("activate")));
        assert!(actions.contains(&json!("minimize")));
        assert!(actions.contains(&json!("maximize")));
        assert!(actions.contains(&json!("restore")));
    }

    #[test]
    fn schema_required_has_both_params() {
        let s = ManageWindow.schema();
        let required = s.parameters["required"].as_array().unwrap();
        assert_eq!(required.len(), 2);
        assert!(required.contains(&json!("window_ref")));
        assert!(required.contains(&json!("action")));
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = ManageWindow.schema();
        assert!(s.sensitive, "manage_window 必须 sensitive=true");
    }

    #[test]
    fn schema_description_mentions_window_ref() {
        let s = ManageWindow.schema();
        assert!(
            s.description.contains("window_ref"),
            "schema description 应提及 window_ref"
        );
    }

    #[test]
    fn schema_description_mentions_blink_forbidden() {
        let s = ManageWindow.schema();
        assert!(
            s.description.contains("Blink"),
            "schema description 应提及禁止操作 Blink 窗口"
        );
    }

    #[test]
    fn schema_no_hwnd_param() {
        // 铁则：公开 schema 中不暴露 hwnd 参数
        let s = ManageWindow.schema();
        let props = &s.parameters["properties"];
        assert!(
            props.get("hwnd").is_none(),
            "manage_window schema 不应暴露 hwnd 参数"
        );
    }

    // ── WindowAction 解析测试 ──

    #[test]
    fn parse_action_activate() {
        assert_eq!(
            WindowAction::parse(Some(&json!("activate"))).unwrap(),
            WindowAction::Activate
        );
    }

    #[test]
    fn parse_action_minimize() {
        assert_eq!(
            WindowAction::parse(Some(&json!("minimize"))).unwrap(),
            WindowAction::Minimize
        );
    }

    #[test]
    fn parse_action_maximize() {
        assert_eq!(
            WindowAction::parse(Some(&json!("maximize"))).unwrap(),
            WindowAction::Maximize
        );
    }

    #[test]
    fn parse_action_restore() {
        assert_eq!(
            WindowAction::parse(Some(&json!("restore"))).unwrap(),
            WindowAction::Restore
        );
    }

    #[test]
    fn parse_action_missing() {
        let err = WindowAction::parse(None).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn parse_action_invalid_string() {
        let err = WindowAction::parse(Some(&json!("close"))).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
        assert!(err.to_string().contains("close"));
    }

    #[test]
    fn parse_action_invalid_type() {
        let err = WindowAction::parse(Some(&json!(42))).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn action_as_str_correct() {
        assert_eq!(WindowAction::Activate.as_str(), "激活");
        assert_eq!(WindowAction::Minimize.as_str(), "最小化");
        assert_eq!(WindowAction::Maximize.as_str(), "最大化");
        assert_eq!(WindowAction::Restore.as_str(), "恢复");
    }

    // ── map_surface_error 测试 ──

    #[test]
    fn map_surface_error_unavailable_maps_to_stale_ref() {
        let err = map_surface_error(SurfaceError::Unavailable {
            detail: "test".into(),
        });
        assert!(matches!(err, CapabilityError::StaleRef { .. }));
    }

    #[test]
    fn map_surface_error_state_mismatch_maps_to_internal() {
        let err = map_surface_error(SurfaceError::WindowStateMismatch {
            expected: "minimized".into(),
            actual: "not minimized".into(),
        });
        assert!(matches!(err, CapabilityError::Internal { .. }));
        assert!(err.to_string().contains("minimized"));
    }

    // ── policy 一致性测试 ──

    #[test]
    fn policy_allowed_origins_all() {
        let p = ManageWindow.policy();
        // manage_window 对所有来源开放（但需要 window_ref 校验）
        assert!(
            p.allowed_origins
                .contains(crate::domain::capability::policy::InvocationOrigin::LocalAi),
            "manage_window 应允许 LocalAi"
        );
        assert!(
            p.allowed_origins
                .contains(crate::domain::capability::policy::InvocationOrigin::Mcp),
            "manage_window 应允许 Mcp"
        );
    }

    #[test]
    fn policy_does_not_require_gui_surface() {
        // manage_window 不依赖 SurfacePort——窗口操作直接走 infra Win32 原语
        // 只需 DESKTOP_SESSION 即可满足（不需要 GUI_SURFACE）
        let p = ManageWindow.policy();
        assert!(
            p.runtime_requirement
                .is_satisfied_by(RuntimeRequirement::DESKTOP_SESSION),
            "manage_window 只需 DESKTOP_SESSION，不需要 GUI_SURFACE"
        );
    }
}
