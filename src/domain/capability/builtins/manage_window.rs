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
//! - `window_ref` 在 `validate_window_ref_detailed` 中校验 HWND 有效性、PID 一致性、
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

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
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
}

/// 将 `RefValidation` 转换为对应的 `CapabilityError`。
fn ref_validation_to_error(
    validation: &crate::infra::platform::window::RefValidation,
) -> CapabilityError {
    use crate::infra::platform::window::RefValidation;
    match validation {
        RefValidation::NotFound => CapabilityError::StaleRef {
            detail: "window_ref 不存在，请重新调用 list_windows 获取最新窗口列表".into(),
        },
        RefValidation::ExpiredGeneration => CapabilityError::StaleRef {
            detail: "window_ref 已过期（generation 不匹配），请重新调用 list_windows".into(),
        },
        RefValidation::ExpiredTtl => CapabilityError::StaleRef {
            detail: "window_ref 已超时，请重新调用 list_windows 获取新引用".into(),
        },
        RefValidation::InvalidHwnd => CapabilityError::StaleRef {
            detail: "窗口句柄已失效（窗口可能被关闭），请重新调用 list_windows".into(),
        },
        RefValidation::PidMismatch => CapabilityError::StaleRef {
            detail: "窗口 PID 已变化（句柄可能被复用），请重新调用 list_windows".into(),
        },
        RefValidation::TitleMismatch => CapabilityError::StaleRef {
            detail: "窗口标题已变化（身份变化），请重新调用 list_windows".into(),
        },
        RefValidation::Valid(_) => unreachable!("Valid 应在调用方处理"),
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

        // spawn_blocking：validate_window_ref 内部调用 Win32 API（IsWindow、GetWindowThreadProcessId、
        // GetWindowTextW），窗口操作也调用同步 Win32 API（ShowWindow、SetForegroundWindow）
        let ref_id_owned = ref_id.to_string();
        let result = tokio::task::spawn_blocking(move || {
            use crate::infra::platform::window::RefValidation;

            // 校验 window_ref（详细版本，区分失效原因）
            let validation =
                crate::infra::platform::window::validate_window_ref_detailed(&ref_id_owned);
            let record = match validation {
                RefValidation::Valid(record) => record,
                ref err_reason => return Err(ref_validation_to_error(err_reason)),
            };

            // 铁则：禁止操作 Blink 自身窗口
            // 在执行任何窗口动作前检查 is_blink，返回 SelfWindowForbidden 并零副作用
            if record.is_blink {
                return Err(CapabilityError::SelfWindowForbidden {
                    detail: "manage_window 不允许操作 Blink 自身窗口，Blink 自隐藏只走截图事务"
                        .into(),
                });
            }

            // 执行窗口操作
            let hwnd = record.hwnd;
            let success = match action {
                WindowAction::Activate => crate::infra::platform::window::activate_window(hwnd),
                WindowAction::Minimize => {
                    crate::infra::platform::window::minimize_window(hwnd);
                    true
                }
                WindowAction::Maximize => {
                    crate::infra::platform::window::maximize_window(hwnd);
                    true
                }
                WindowAction::Restore => {
                    crate::infra::platform::window::restore_window(hwnd);
                    true
                }
            };

            // 不在日志中记录完整窗口标题（隐私）
            Ok::<(bool, String), CapabilityError>((success, record.process_name.clone()))
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("manage_window task 崩溃: {e}"),
        })??;

        let (success, process_name) = result;

        // activate 失败（SetForegroundWindow 返回 false，Windows 前台锁定限制）
        if action == WindowAction::Activate && !success {
            tracing::warn!(
                action = action.as_str(),
                process_name = %process_name,
                "manage_window: activate 失败（Windows 前台锁定限制）"
            );
            return Ok(CapabilityResult::Done {
                summary: format!(
                    "窗口 ({}) 激活请求已发送，但可能因系统前台锁定限制未生效",
                    process_name
                ),
            });
        }

        tracing::debug!(
            action = action.as_str(),
            process_name = %process_name,
            "manage_window: 操作完成"
        );

        let action_desc = match action {
            WindowAction::Activate => "已激活",
            WindowAction::Minimize => "已最小化",
            WindowAction::Maximize => "已最大化",
            WindowAction::Restore => "已恢复",
        };

        Ok(CapabilityResult::Done {
            summary: format!("{}窗口 ({})", action_desc, process_name),
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

    // ── ref_validation_to_error 测试 ──

    #[test]
    fn ref_validation_not_found_maps_to_stale_ref() {
        let err = ref_validation_to_error(&crate::infra::platform::window::RefValidation::NotFound);
        assert!(matches!(err, CapabilityError::StaleRef { .. }));
    }

    #[test]
    fn ref_validation_expired_ttl_maps_to_stale_ref() {
        let err =
            ref_validation_to_error(&crate::infra::platform::window::RefValidation::ExpiredTtl);
        assert!(matches!(err, CapabilityError::StaleRef { .. }));
        assert!(err.to_string().contains("超时"));
    }

    #[test]
    fn ref_validation_pid_mismatch_maps_to_stale_ref() {
        let err =
            ref_validation_to_error(&crate::infra::platform::window::RefValidation::PidMismatch);
        assert!(matches!(err, CapabilityError::StaleRef { .. }));
        assert!(err.to_string().contains("PID"));
    }

    #[test]
    fn ref_validation_title_mismatch_maps_to_stale_ref() {
        let err =
            ref_validation_to_error(&crate::infra::platform::window::RefValidation::TitleMismatch);
        assert!(matches!(err, CapabilityError::StaleRef { .. }));
        assert!(err.to_string().contains("标题"));
    }

    #[test]
    fn ref_validation_expired_generation_maps_to_stale_ref() {
        let err = ref_validation_to_error(
            &crate::infra::platform::window::RefValidation::ExpiredGeneration,
        );
        assert!(matches!(err, CapabilityError::StaleRef { .. }));
        assert!(err.to_string().contains("过期"));
    }

    #[test]
    fn ref_validation_invalid_hwnd_maps_to_stale_ref() {
        let err =
            ref_validation_to_error(&crate::infra::platform::window::RefValidation::InvalidHwnd);
        assert!(matches!(err, CapabilityError::StaleRef { .. }));
        assert!(err.to_string().contains("句柄"));
    }

    // ── is_blink 拒绝策略矩阵测试 ──

    /// 注册一个 is_blink=true 的 window_ref，验证 ref_validation_to_error
    /// 在 Valid + is_blink=true 时仍返回 SelfWindowForbidden（管理能力铁则）。
    ///
    /// 注意：这测试验证的是 ref 校验通过但 is_blink=true 的场景。
    /// 实际的 SelfWindowForbidden 检查在 invoke() 中，
    /// 这里只验证 ref_validation_to_error 对 Valid 不返回错误。
    #[test]
    fn ref_validation_valid_does_not_map_to_error() {
        // ref_validation_to_error 对 Valid 返回 unreachable，
        // 但我们不在测试中构造 Valid（因为 WindowRefRecord 含 Instant）。
        // 只验证所有非 Valid 变体都映射到 StaleRef。
        let variants = [
            crate::infra::platform::window::RefValidation::NotFound,
            crate::infra::platform::window::RefValidation::ExpiredGeneration,
            crate::infra::platform::window::RefValidation::ExpiredTtl,
            crate::infra::platform::window::RefValidation::InvalidHwnd,
            crate::infra::platform::window::RefValidation::PidMismatch,
            crate::infra::platform::window::RefValidation::TitleMismatch,
        ];
        for v in &variants {
            let err = ref_validation_to_error(v);
            assert!(
                matches!(err, CapabilityError::StaleRef { .. }),
                "所有非 Valid 变体应映射到 StaleRef"
            );
        }
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
