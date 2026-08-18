//! `create_sticky` Capability（0.19.3）。
//!
//! 创建桌面便签并显示窗口 → `Done`。
//!
//! **背景**：便签创建原先只在 chord action（Alt+V）和 command 层可用，
//! AI 看不到。本 cap 补上"AI 创建便签"的执行入口，是"帮我把这件事钉个便签"
//! "读完剪贴板内容帮我记一下"等场景的核心依赖。
//!
//! **DangerClass::Safe + sensitive=true**（§3.4）：
//! - Safe——便签创建是可逆的（用户能关/删），不标 Dangerous，与 `open_url` 同级
//! - sensitive——写便签内容属用户隐私数据，AI 调用前需用户确认
//!
//! **位置参数**：`x`/`y`/`w`/`h` 均可选（物理像素），None 时居中到当前前台窗口
//! 所在显示器。AI 可通过 `list_windows` 获取窗口位置后计算目标坐标，实现
//! "把便签钉在某窗口旁"的定位场景。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

/// `create_sticky` — 创建便签并显示桌面窗口。
///
/// 入参：`{ content?: String, x?: int, y?: int, w?: int, h?: int }`。
/// `content` 缺省/空白时创建空白便签（Alt+S 空输入框仍唤起空白便签）。
/// 出参：`Done { summary: "已创建便签 {id}" }`。
pub struct CreateSticky;

/// 解析 `content` 参数：缺省 / `null` / 空或纯空白 → 空白便签；类型错误 → `InvalidArgs`。
fn parse_content(args: &Value) -> Result<String, CapabilityError> {
    match args.get("content") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.trim().to_string()),
        Some(_) => Err(CapabilityError::InvalidArgs {
            detail: "create_sticky: content 参数类型错误，应为字符串".into(),
        }),
    }
}

#[async_trait::async_trait]
impl Capability for CreateSticky {
    fn id(&self) -> &str {
        "create_sticky"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "create_sticky".into(),
            description: "创建一个桌面便签并显示窗口。可指定位置(x/y)和尺寸(w/h)，均为物理像素坐标；不指定则居中显示。content 可省略或为空，此时创建空白便签。返回创建的便签id。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "便签正文内容，省略或为空时创建空白便签"
                    },
                    "x": {
                        "type": "integer",
                        "description": "窗口左上角 x 坐标（物理像素），不指定则居中"
                    },
                    "y": {
                        "type": "integer",
                        "description": "窗口左上角 y 坐标（物理像素），不指定则居中"
                    },
                    "w": {
                        "type": "integer",
                        "description": "窗口宽度（物理像素），不指定则用默认值 280"
                    },
                    "h": {
                        "type": "integer",
                        "description": "窗口高度（物理像素），不指定则用默认值 320"
                    }
                },
                "required": []
            }),
            sensitive: true, // 写便签内容属隐私
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_AND_CLI,
            runtime_requirement: RuntimeRequirement::GUI_SURFACE,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::Forbidden,
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
                detail: "create_sticky 截止时刻已过".into(),
            });
        }

        let content = parse_content(&args)?;

        let x = args.get("x").and_then(Value::as_i64).map(|v| v as i32);
        let y = args.get("y").and_then(Value::as_i64).map(|v| v as i32);
        let w = args.get("w").and_then(Value::as_i64).map(|v| v as i32);
        let h = args.get("h").and_then(Value::as_i64).map(|v| v as i32);

        let sticky_id = ctx
            .env
            .create_sticky_and_show(&content, x, y, w, h)
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: format!("创建便签失败: {e}"),
            })?;

        tracing::info!(sticky_id = %sticky_id, "create_sticky: 便签已创建并显示");

        Ok(CapabilityResult::Done {
            summary: format!("已创建便签 {sticky_id}"),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(CreateSticky) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_create_sticky() {
        assert_eq!(CreateSticky.id(), "create_sticky");
    }

    #[test]
    fn schema_has_content_parameter() {
        let s = CreateSticky.schema();
        assert_eq!(s.name, "create_sticky");
        assert_eq!(s.parameters["properties"]["content"]["type"], "string");
        // content 可省略（空白便签合法，Alt+S 空输入框仍唤起）
        let required = s.parameters["required"].as_array().unwrap();
        assert!(!required.iter().any(|v| v == "content"));
    }

    #[test]
    fn parse_content_blank_is_allowed() {
        assert_eq!(parse_content(&json!({})).unwrap(), "");
        assert_eq!(parse_content(&json!({"content": null})).unwrap(), "");
        assert_eq!(parse_content(&json!({"content": ""})).unwrap(), "");
        assert_eq!(parse_content(&json!({"content": "   "})).unwrap(), "");
    }

    #[test]
    fn parse_content_trims_and_keeps_text() {
        assert_eq!(parse_content(&json!({"content": "  hello  "})).unwrap(), "hello");
    }

    #[test]
    fn parse_content_rejects_wrong_type() {
        let e = parse_content(&json!({"content": 42})).unwrap_err();
        assert!(matches!(e, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn schema_has_optional_position_params() {
        let s = CreateSticky.schema();
        assert_eq!(s.parameters["properties"]["x"]["type"], "integer");
        assert_eq!(s.parameters["properties"]["y"]["type"], "integer");
        assert_eq!(s.parameters["properties"]["w"]["type"], "integer");
        assert_eq!(s.parameters["properties"]["h"]["type"], "integer");
        // x/y/w/h 不在 required 中
        let required = s.parameters["required"].as_array().unwrap();
        assert!(!required.iter().any(|v| v == "x"));
        assert!(!required.iter().any(|v| v == "y"));
        assert!(!required.iter().any(|v| v == "w"));
        assert!(!required.iter().any(|v| v == "h"));
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = CreateSticky.schema();
        assert!(s.sensitive, "create_sticky 必须 sensitive=true");
    }

    #[test]
    fn danger_class_is_safe() {
        use crate::domain::capability::policy::DangerClass;
        assert_eq!(CreateSticky.danger_class(), DangerClass::Safe);
    }

    #[test]
    fn schema_description_mentions_sticky() {
        let s = CreateSticky.schema();
        assert!(
            s.description.contains("便签"),
            "schema description 应提及便签"
        );
    }
}
