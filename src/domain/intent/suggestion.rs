//! Suggestion 统一契约（0.8.3 §4.3 / §4.4）。
//!
//! **动机**：0.8.1 引入 `CompletionHint`（输入补全），0.8.2 让 Context 走召回，
//! 0.8.3 又要新增 Context Ghost。若各占 `SearchResponse` 一个字段，未来 0.9 AI
//! 意图判定还要再加一个 —— 每加一路信号就多一个字段 + 多一层前端优先级。
//!
//! **正解**：所有「待用户采纳的建议」抽成一个 `Suggestion { source, confidence, ... }`,
//! `RuleRouter::best_suggestion` 内部多源竞争产 top-1,契约只暴露一个字段。
//!
//! 与 `CompletionHint` 的关系：`CompletionHint` 保留为 0.8.1 输入补全的内部计算结果
//! （`compute_hint` 返回值），`best_suggestion` 内部把它包装成 `Suggestion { source: Keyword, ... }`
//! 再暴露。不再直接进 `SearchResponse`（0.8.3 §4.3 契约变更）。
//!
//! **P0 修订项对接**：`CompletionHint` 无 `score` 字段（0.8.3 §4.13 P0-2），
//! `best_suggestion` 需在构造 `Suggestion` 时**直接从 fuzzy 打分层拿分**，而不是
//! 从 `CompletionHint.score` 读取（它不存在）。见 `RuleRouter::best_suggestion` 实现。

use serde::Serialize;

use crate::infra::platform::context::AwarenessSource;

/// 建议来源。前端可按此分样式（Context 弱区分——更浅灰度或不同 icon）。
///
/// 0.9 预留 `Ai` 变体：AI 意图判定器成为另一个 Suggestion 生产者,走同一竞争路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SuggestionSource {
    /// 0.8.1 输入补全（首拼 fy → fanyi / 汉字 翻 → 翻译 / 部分拼音 fan → fanyi）。
    /// 非空 query 独占。
    Keyword,
    /// 0.8.3 环境感知（选中英文 → 翻译 / 剪贴板 URL → 打开链接）。
    /// 空 query 独占（非空 query 时 keyword 意图更强，Context 让位）。
    Context,
    // 0.9 预留：Ai —— AI function calling / 意图判定
}

/// Context 类 Suggestion 的取值来源（0.8.3 §4.9 UX 加强）——
/// 用户看到 Ghost 时能立刻知道「这个建议是基于我划的词 / 我剪贴板里的东西」。
///
/// 前端按此值查 i18n key（`suggestion.origin.selection` / `suggestion.origin.clipboard`）
/// 挂在 Ghost 尾部或 statusbar,弱视觉,不喧宾夺主。
///
/// Keyword 类 Suggestion 恒 None（输入补全无外部来源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SuggestionOrigin {
    /// 划词/UIA 抓取的选区文本（数据侧 `AwarenessSource::Selection`）
    Selection,
    /// 剪贴板文本（数据侧 `AwarenessSource::Clipboard`）
    Clipboard,
}

/// 零 cost 转换：数据侧 `AwarenessSource` → 前端契约 `SuggestionOrigin`（0.8.3 收尾）。
///
/// 一对一映射,让 `best_suggestion` 里 `Hit.origin.map(SuggestionOrigin::from)` 直接
/// 拿到前端契约值,不再有分支推断逻辑。**未来 Chord 加 `ChordSelection` 等变体时,
/// 本 `From` 决定映射策略**（可能仍映射到 Selection,或拆更细的 SuggestionOrigin 变体）。
impl From<AwarenessSource> for SuggestionOrigin {
    fn from(src: AwarenessSource) -> Self {
        match src {
            AwarenessSource::Selection => SuggestionOrigin::Selection,
            AwarenessSource::Clipboard => SuggestionOrigin::Clipboard,
        }
    }
}

/// 待用户采纳的建议。前端渲染为 Ghost text；用户按 Tab 时把 `replacement` 写回
/// 输入框，触发下一轮搜索。
///
/// 视觉分工与 0.8.1 保持一致：
/// - `display` 非空：overlay 渲染灰影（`→ fanyi` / `翻译 "the..."`）
/// - `display` 为空：overlay 不渲染字符,仅由 statusbar 提示"按 Tab"
///
/// 序列化契约：camelCase（与 `AppEntry` / 旧 `CompletionHint` 一致）。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    /// UI 显示的建议文本（"fanyi" / "翻译 hello world..."）。
    pub display: String,
    /// Tab 采纳时替换的完整 query（keyword + " " + arg / 或直接完整命令）。
    pub replacement: String,
    /// 来源。前端按此分样式（弱区分 Context vs Keyword）。
    pub source: SuggestionSource,
    /// 归一化置信度 `[0, 1]`。多路信号并存时选最高（0.8.3 阶段 Keyword/Context
    /// 因 query 空/非空互斥不会直接竞争，此字段为 0.9 AI 接入预留）。
    pub confidence: f64,
    /// 用户已输入部分的长度（字节，前端渲染灰色补全时对齐用）。
    /// Context Suggestion 恒为 0（空 query 场景无「已输入」）。
    pub prefix_len: usize,
    /// Context 类 Suggestion 的取值来源（划词 / 剪贴板）,供前端展示「来自划词」提示。
    /// Keyword 类恒 None；序列化时 `#[skip_serializing_if]` 省略字段减少前端 undefined 判定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<SuggestionOrigin>,
}
