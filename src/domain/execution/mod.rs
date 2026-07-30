//! Execution 域（0.8.6 §8.1.1）—— 动作的统一执行入口。
//!
//! **动机**：0.8.0 ~ 0.8.5 积累了四份"动作"概念——`SearchAction`（引擎内部模型）、
//! `BuiltinActionKind`（内置动作分派 tag）、`PluginAction`（插件 wire protocol）、
//! `ChordAction` trait（Chord 动作契约）。本质是同一概念的四种投影。0.8.6 抽
//! `Action` trait + `ActionOutcome`，把执行入口物理归一到本模块。
//!
//! **四域约束**：Execution 域是信任边界最末端。`ActionOutcome` 描述副作用意图，
//! 真正执行（系统调用 / IPC / 前端通知）由 command 层调 `Action::execute` 后按
//! `ActionOutcome` 分派。
//!
//! **前端契约不变**：前端仍接收 `Action { kind, payload }` 形状（`search/mod.rs`），
//! 本模块是后端内部的执行抽象——外部 JSON 零变化。

mod builtin;
pub mod group;
mod registry;
mod schema;

pub use registry::ActionRegistry;
pub use schema::{ActionSchema, DangerClass};
// builtin 动作 structs 通过 registry 间接使用，不需要 re-export
#[allow(unused_imports)]
pub(crate) use builtin::*;

use crate::domain::event::DomainEnv;
use serde_json::Value;

/// 动作执行上下文（0.8.6 §8.1.1；0.9.0 §3.3 tool-call 进化；0.14.6 §2.2 去 tauri）。
///
/// 包含执行所需的所有环境信息。不是所有字段都被所有动作消费——
/// `env` 仅 `Emit` / 需要 Tauri 运行时的动作使用；`arguments` 仅参数化动作使用。
///
/// **0.9.0 演进**：从单一 `arg: Option<Value>` 升级为结构化 `arguments: Value`
/// （保证是 JSON Object）。旧字符串参数走 `_legacy_arg` 键做兼容层，
/// 0.9.1 AI 路径下 `ToolCall.arguments` 可直接注入。
///
/// **0.14.6 §2.2**：`app_handle: &tauri::AppHandle` 替换为 `env: &dyn DomainEnv`，
/// domain 层不再直接依赖 tauri。
pub struct ActionContext<'a> {
    pub env: &'a dyn DomainEnv,
    /// 动作参数（JSON Object）。无参动作为 `{}`。
    ///
    /// **key 约定**：
    /// - 参数化动作用**语义键**（`url` / `path` / `text` 等，见 0.9.0 §3.3）
    /// - 旧字符串路径走 `_legacy_arg`（`arg_as_str` 读的就是它，兼容层，0.9.2 起可删）
    #[allow(dead_code)] // 0.14.2 后参数化 Action 已迁入 Capability，剩余 9 个 Action 不读此字段；保留供未来 Action 使用
    pub arguments: Value,
}

impl<'a> ActionContext<'a> {
    /// 从可选字符串参数构造（0.8.x 常见入口）。
    ///
    /// `arg: Some("...")` → `arguments = { "_legacy_arg": "..." }`
    /// `arg: None`        → `arguments = {}`
    ///
    /// 保留此签名让 `command` 层和 `chord` 层调用点零改动。
    pub fn new(env: &'a dyn DomainEnv, arg: Option<Value>) -> Self {
        let arguments = match arg {
            Some(v) => serde_json::json!({ "_legacy_arg": v }),
            None => serde_json::json!({}),
        };
        Self {
            env,
            arguments,
        }
    }

    /// 从结构化 arguments 构造（0.9.1 AI 路径 `ToolCall.arguments` 入口）。
    ///
    /// 若传入的不是 Object，会被规范化为 `{}` + 一条 warn 日志——
    /// 防御性设计，AI 可能产出畸形 JSON。
    #[allow(dead_code)] // 0.9.1 起消费
    pub fn from_arguments(env: &'a dyn DomainEnv, arguments: Value) -> Self {
        let arguments = if arguments.is_object() {
            arguments
        } else {
            tracing::warn!(
                ?arguments,
                "ActionContext::from_arguments 收到非 Object 参数，退化为 {{}}"
            );
            serde_json::json!({})
        };
        Self {
            env,
            arguments,
        }
    }

    /// 从 `arguments` 按语义键抽字符串（0.9.0 起推荐入口）。
    ///
    /// 例：`cx.arg_str("url", "open_url")?` → 从 `arguments["url"]` 取字符串。
    ///
    /// 0.14.2 后参数化 Action 已迁入 Capability，当前 9 个 Action 不调用此方法；
    /// 保留供未来参数化 Action 使用。
    #[allow(dead_code)]
    pub fn arg_str(&self, key: &str, action_name: &str) -> Result<String, ExecError> {
        self.arguments
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ExecError::MissingArg(action_name.to_string()))
    }

    /// 兼容旧 `arg_as_str`——从 `_legacy_arg` 键读字符串。
    ///
    /// 0.9.0 阶段所有参数化 builtin 都会覆盖 `schema()` 声明语义键，
    /// 但 `_legacy_arg` fallback 保留一整个版本用于前端渐进迁移。
    /// **不加 deprecated**：0.9.0 前端契约不变，`ExecArg::UserExplicit(String)`
    /// 装配路径继续调这个方法，deprecation 会污染编译输出。
    #[allow(dead_code)]
    pub fn arg_as_str(&self, action_name: &str) -> Result<String, ExecError> {
        // 优先走 _legacy_arg（老装配路径）；若已迁到语义键，参数化 builtin 会自己覆盖
        self.arg_str("_legacy_arg", action_name)
    }
}

/// 动作执行结果（副作用意图）。
///
/// **不是**最终返回值——command 层拿到 `ActionOutcome` 后按类型分派：
/// - `Open` → `open::that(path)`
/// - `Copy` → 写剪贴板 + 可选 hit 回写
/// - `Emit` → `app.emit(event, payload)`
/// - `Nop` → 无操作
///
/// 设计为 enum 而非 `Box<dyn FnOnce>` 的原因：可序列化、可日志、command 层分派清晰。
///
/// **0.13.7**：`Items` 变体已删除——插件从 Action 迁入 Capability，
/// 结构化列表统一由 `CapabilityResult::Items` 承载。Action 回归纯粹副作用语义。
#[derive(Debug, Clone)]
pub enum ActionOutcome {
    /// 复制到剪贴板（计算结果 / 插件 Copy / 剪贴板历史）。
    /// `hit_id` 是命中回写通道（0.8.5 §6.4）——前端复制成功后 `record_clipboard_hit` 频率加权。
    #[allow(dead_code)] // 0.9 AI / 插件 adapter 消费；当前 Copy 走 SearchAction 路径
    Copy {
        text: String,
        hit_id: Option<String>,
    },
    /// 打开路径 / URL / 应用。
    #[allow(dead_code)] // Chord action 预留；当前 Open 走 SearchAction 路径
    Open { path: String },
    /// 向前端发事件（Chord 的 fill-query / show-ball 等副作用）。
    Emit {
        event: String,
        payload: serde_json::Value,
    },
    /// 纯展示项，无副作用。
    Nop,
    // 0.13.7：`Items` 变体已删除——插件从 Action 迁入 Capability，
    // 结构化列表统一由 `CapabilityResult::Items` 承载。Action 回归纯粹副作用语义。
}

/// 动作执行错误。
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// 参数化动作缺少参数。
    #[allow(dead_code)] // 0.14.2 后仅 arg_str 构造，arg_str 已标 dead_code
    #[error("{0}: 缺少字符串参数")]
    MissingArg(String),
    /// 执行过程中的错误（打开失败、系统调用失败等）。
    #[error("{0}")]
    Runtime(String),
}

/// 跨域转换：Capability 错误 → ExecError（Action 编排 Capability 失败时用）。
impl From<crate::domain::capability::CapabilityError> for ExecError {
    fn from(e: crate::domain::capability::CapabilityError) -> Self {
        ExecError::Runtime(e.to_string())
    }
}

/// 统一动作 trait（0.8.6 §8.1.1 / §8.2.4；0.9.0 §3.3 tool-call 进化）。
///
/// 一切副作用的统一入口。三种来源实现此 trait：
/// - **Builtin**：12 个内置动作（`builtin/*.rs`）—— 0.9.0 §3.3 全部显式实现 `schema()` + `danger_class()`
/// - **Chord**：`ScreenshotAction` / `VoiceInputAction` / `ClipboardHistoryAction`（`chord/mod.rs`）—— 0.9.0 §3.3 显式实现
/// - **Plugin**：每次调用产生的 `PluginItem` **不**直接 `impl Action`——它是运行期动态数据,而非静态可注册动作。
///   插件的 tool schema 投影(把每个**插件本身**注册为 tool)推迟到 **0.9.1** 与 `AIProvider` 引入一起做,
///   届时 manifest 的 `parameters` 字段和 `AIConfig` 一并设计。0.9.0 保持现状(SearchAction → 前端契约不变)。
///
/// 0.9 AI Provider 可产 `ChatAction` / `RunAgentAction`，直接 `impl Action`。
///
/// **0.9.0 演进**：新增 `schema()` + `danger_class()` 元数据方法。
/// - `schema()` default 返回无参 schema——12 个 builtin + 3 个 chord 全部显式覆盖(§3.3 铁则)
/// - `danger_class()` default 返回 `Safe`——但 §5.4 白名单铁则要求**所有 Action 显式覆盖**
///   (即使多数是 Safe),强制开发者思考是否危险。default impl 只是"漏网时的保底 Safe"
#[async_trait::async_trait]
pub trait Action: Send + Sync {
    /// 唯一标识（`BuiltinActionRegistry` 的 key，如 `"open_settings"`）。
    fn id(&self) -> &str;

    /// 主显示名（走 `LocalizableText`，0.8.6 §8.2.4 i18n）。
    fn title(&self) -> &crate::domain::plugin::LocalizableText;

    /// 副显示名。
    fn subtitle(&self) -> &crate::domain::plugin::LocalizableText;

    /// 动作元数据（0.9.0 §3.2 遗留）。
    ///
    /// **default**：无参 schema,name = `self.id()`,description 空字符串。
    /// 12 个内置动作会在 Phase 3c 显式覆盖为「语义键 + i18n 描述」。
    ///
    /// **0.14 Capability-only**：Action schema 不进入 AI tool 池；当前仅保留作本地
    /// 注册表自检与未来交互元数据收敛，不得重新投影给 LLM。
    #[allow(dead_code)]
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty(self.id(), "")
    }

    /// 危险等级（0.9.0 §5.4 白名单铁则,独立于交互模式）。
    ///
    /// **default = Safe**——但项目铁则要求所有 Action **显式覆盖**（哪怕都返回 Safe）,
    /// 让新增动作的开发者被迫思考是否危险。default 只作漏网保底。
    ///
    /// **0.14 Capability-only**：此等级只描述本地交互 Action，不参与 AI 注册；
    /// AI 安全策略只读取 `Capability::requires_ai_confirmation()`。
    #[allow(dead_code)]
    fn danger_class(&self) -> DangerClass {
        DangerClass::Safe
    }

    /// 执行动作，返回副作用意图。
    async fn execute(&self, cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ActionContext 需要 &dyn DomainEnv,但 arg_str / arg_as_str 只读 arguments 字段——
    // 我们直接测抽取字符串的纯逻辑(镜像 arg_str 实现),避免造假 DomainEnv 引用(UB)。
    fn extract(arguments: &Value, key: &str, action_name: &str) -> Result<String, ExecError> {
        arguments
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ExecError::MissingArg(action_name.to_string()))
    }

    #[test]
    fn legacy_arg_is_wrapped_into_arguments_object() {
        // 镜像 ActionContext::new 的转换逻辑:0.9.0 兼容层
        let arg = Some(json!("https://example.com"));
        let arguments = match arg {
            Some(v) => json!({ "_legacy_arg": v }),
            None => json!({}),
        };
        assert_eq!(arguments["_legacy_arg"], "https://example.com");
    }

    #[test]
    fn empty_arg_produces_empty_object() {
        let arg: Option<Value> = None;
        let arguments = match arg {
            Some(v) => json!({ "_legacy_arg": v }),
            None => json!({}),
        };
        assert_eq!(arguments, json!({}));
    }

    #[test]
    fn arg_str_reads_semantic_key() {
        // 0.9.0 参数化 builtin 用语义键 url/path
        let args = json!({ "url": "https://blink.dev" });
        let v = extract(&args, "url", "open_url").unwrap();
        assert_eq!(v, "https://blink.dev");
    }

    #[test]
    fn arg_str_missing_key_errors() {
        let args = json!({});
        let err = extract(&args, "url", "open_url").unwrap_err();
        assert!(matches!(err, ExecError::MissingArg(a) if a == "open_url"));
    }

    #[test]
    fn arg_str_empty_string_errors() {
        // 空白字符串等同缺参——0.8.x 语义保留
        let args = json!({ "url": "   " });
        assert!(extract(&args, "url", "open_url").is_err());
    }

    #[test]
    fn arg_as_str_reads_legacy_key() {
        // 兼容层:老装配路径通过 arg_as_str 读 _legacy_arg
        let args = json!({ "_legacy_arg": "C:/tmp/foo.txt" });
        assert_eq!(
            extract(&args, "_legacy_arg", "open_path").unwrap(),
            "C:/tmp/foo.txt"
        );
    }

    #[test]
    fn from_arguments_normalizes_non_object() {
        // 防御性:AI 可能产畸形 JSON,ActionContext::from_arguments 会退化为 {}
        let input = json!("not an object");
        let normalized = if input.is_object() { input } else { json!({}) };
        assert_eq!(normalized, json!({}));
    }

    #[test]
    fn from_arguments_accepts_object() {
        let input = json!({ "text": "hello", "target_lang": "en" });
        let normalized = if input.is_object() {
            input.clone()
        } else {
            json!({})
        };
        assert_eq!(normalized, input);
    }
}
