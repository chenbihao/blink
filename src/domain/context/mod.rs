//! Domain 层 Context 辅助：**纯逻辑判定**，无平台调用。
//!
//! 与 `infra::platform::context`（采集实现，走 Win32/UIA）职责分离：
//! - `infra::platform::context::collect()` 采集 `ContextSnapshot`（副作用）
//! - `domain::context::probe`      判定文本类型（纯函数，可单测）
//!
//! 供 `BuiltinEngine`（0.8.0 §1.3 双路匹配）与 `intent::RuleRouter`（0.8.0 §1.2）共用，
//! 避免两处独立实现 URL / 文件路径判定。

pub mod probe;
