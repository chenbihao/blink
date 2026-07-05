//! SuggestionProducer trait（0.8.6 §8.1.2）。
//!
//! 一切 Suggestion 的统一生产入口。三种来源实现此 trait：
//! - `KeywordProducer`：输入补全（首拼/拼音/汉字 → keyword）
//! - `ContextProducer`：环境感知（选区/剪贴板 → 翻译/打开链接）
//! - (0.9) `AIProducer`：AI 意图判定

use crate::infra::platform::context::AwarenessSnapshot;

use super::{Suggestion, SuggestionSource};

/// Suggestion 生产者 trait（0.8.6 §8.1.2）。
///
/// 每个 producer 独立产出候选 Suggestion 列表，由 `SuggestionArbiter` 做竞争仲裁。
/// `produce` 是纯同步函数（0.8.6 阶段无 IO），0.9 AI 异步化时再扩展。
pub trait SuggestionProducer: Send + Sync {
    /// 此 producer 的来源标识（调试 / 日志用）。
    #[allow(dead_code)]
    fn source(&self) -> SuggestionSource;

    /// 产出候选 Suggestion 列表。
    ///
    /// - `query`：用户当前输入（**未 trim**——保留末尾空格语义信号）
    /// - `snapshot`：环境快照（选区/剪贴板/前台应用）
    ///
    /// 返回空 Vec 表示此 producer 无命中。
    fn produce(&self, query: &str, snapshot: &AwarenessSnapshot) -> Vec<Suggestion>;
}
