//! SuggestionProducer trait（0.8.6 §8.1.2）。
//!
//! 一切 Suggestion 的统一生产入口。三种来源实现此 trait：
//! - `KeywordProducer`：输入补全（首拼/拼音/汉字 → keyword）
//! - `ContextProducer`：环境感知（选区/剪贴板 → 翻译）
//! - `AiProducer`：AskAi 候选（0.24.2 从 SearchService 后置注入迁入）
//!
//! **Producer 纯度铁则**（0.24 §3.7）：produce 只读 query + snapshot，
//! 路由/能力可用性判定不进 Producer，由 Coordinator 的 eligibility 承接。

use blink_infra::platform::context::AwarenessSnapshot;

use super::{Suggestion, SuggestionSource};

/// Suggestion 生产者 trait（0.8.6 §8.1.2）。
///
/// 每个 producer 独立产出候选 Suggestion 列表，由 `SuggestionCoordinator` 做竞争仲裁。
/// `produce` 是纯同步函数（0.8.6 阶段无 IO），0.9 AI 异步化时再扩展。
///
/// `source()` 由 coordinator 在收集候选时调用，把 producer 身份盖章到
/// `Suggestion.source`（内部字段）——可观测性日志（filter/impression/adoption）
/// 据此记 producer。
pub trait SuggestionProducer: Send + Sync {
    /// 此 producer 的来源标识（coordinator 盖章 + 日志用）。
    fn source(&self) -> SuggestionSource;

    /// 产出候选 Suggestion 列表。
    ///
    /// - `query`：用户当前输入（**未 trim**——保留末尾空格语义信号）
    /// - `snapshot`：环境快照（选区/剪贴板/前台应用）
    ///
    /// 返回空 Vec 表示此 producer 无命中。
    fn produce(&self, query: &str, snapshot: &AwarenessSnapshot) -> Vec<Suggestion>;
}
