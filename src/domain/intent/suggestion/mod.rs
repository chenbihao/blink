//! Suggestion 统一契约（0.8.3 §4.3 / §4.4；0.24.1 类型化重构）。
//!
//! **0.24.1 契约**：`SearchResponse.suggestion` 从 `Option<Suggestion>` 迁移为
//! `Option<SuggestionSet>`（带 revision 的双槽位集合）。单个 `Suggestion` 携带语义
//! `kind` 与类型化采纳动作 `action`——`replacement` 并入 `RouteQuery.query`，
//! `source` 字段退役（producer 身份仍由 `SuggestionProducer::source()` 表达，不上 wire）。
//!
//! **rank_score 与 confidence 分离**（0.24 §3.4）：规则排序值统一叫 `rank_score`；
//! `confidence` 保留给未来具备校准语义的模型输出——0.24 规则侧无生产者，字段不建，
//! 0.25 模型 Producer 落地时随 `ModelIntentProducer` 一起加。
//!
//! 0.8.3 动机存档：所有「待用户采纳的建议」共用一个 `SearchResponse` 字段，多源
//! 竞争产 top-1——每加一路信号不再多一个字段 + 多一层前端优先级分支。

pub mod context;
pub mod coordinator;
pub mod keyword;
pub mod producer;

use serde::Serialize;

use super::RankingHint;
use crate::infra::platform::context::AwarenessSource;

/// Producer 身份标识（0.24.1 起仅日志/调试用，不序列化上 wire）。
///
/// 0.24.1 前 `Suggestion.source` 携带此值供前端分支；现前端改按 `kind` 分支，
/// producer 身份只保留在 `SuggestionProducer::source()` 返回值里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionSource {
    /// 输入补全（首拼 fy → fanyi / 汉字 翻 → 翻译）。非空 query 独占。
    Keyword,
    /// 环境感知（选中英文 → 翻译）。空 query 独占。
    Context,
    /// AI Producer（0.24.2 从 SearchService 后置注入迁入；当前 AI 建议由
    /// SearchService 直接构造，不经 producer，故暂无构造点）。
    #[allow(dead_code)]
    Ai,
}

/// 建议的语义类别（0.24 §3.4 / §3.5）。
///
/// 前端按 kind 决定视觉通道：`Completion` → ghost 影子文字；
/// `Translate` / `AskAi` → 采纳提示（0.24.4 前暂走 statusbar，之后进 SuggestionBar）。
///
/// `OpenUrl` / `OpenPath` 是 0.25 变体（随 `InvokeCapability` 激活），0.24 不建死——
/// 打开类动作由结果列表承载（§3.5 决策：两步采纳劣于现状一步 Enter）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SuggestionKind {
    /// 输入补全：影子文字自然接在用户文本后（`fy` → `fanyi`）。
    Completion,
    /// 翻译建议（query/选区/剪贴板为非目标语言）。
    Translate,
    /// AI 处理建议（"按 Tab 问 AI" / 进 AI 模式）。
    AskAi,
}

/// 类型化采纳动作（0.24 §3.5）。
///
/// **0.24 采纳零执行能力**：只有"改 query 走一轮新搜索"与"进主窗口 AI 模式"两种
/// 无副作用动作，最坏情况（query 改错）立即可见可撤销——这是 §3.6 采纳协议
/// （前端乐观本地 + 单向遥测）成立的前提。`InvokeCapability` 变体留给 0.25，
/// 届时执行类采纳按 action 类型分流到同步 IPC 校验。
///
/// serde：externally tagged + camelCase → `{"routeQuery":{"query":"…"}}` /
/// `{"enterAiMode":{"prompt":"…"}}`。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SuggestionAction {
    /// 回写输入框 + 触发新一轮搜索（补全采纳与翻译采纳同用；
    /// 翻译即 query 变为"翻译 xxx"后命中确定路由）。
    RouteQuery {
        query: String,
    },
    /// 前端直进主窗口 AI 模式（0.17.6 起 ChatService ephemeral 对话）。
    EnterAiMode {
        prompt: String,
    },
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

/// 待用户采纳的建议。前端按 `kind` 分视觉通道（ghost 影子 / 采纳提示）；
/// 用户 Tab / Shift+Tab / 点击时执行 `action`（0.24 §3.5 / §3.6）。
///
/// - `display` 非空：补全场景 overlay 渲染灰影（`→ fanyi`）
/// - `display` 为空：overlay 不渲染字符,仅由 statusbar 提示"按 Tab"
///
/// 序列化契约：camelCase（与 `AppEntry` 一致）。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    /// 语义 slug（`completion-keyword` / `translate-selection` / `ai-query` 等）。
    /// 采纳遥测与日志用，前端不解释、仅回传（§3.6）。
    pub id: String,
    /// 语义类别——决定前端视觉通道与 Coordinator 按 Kind 去重（§3.4）。
    pub kind: SuggestionKind,
    /// 类型化采纳动作（replacement 的继任者）。
    pub action: SuggestionAction,
    /// 规则排序值。0.24.1 沿用旧 confidence 数值（排序行为不变），
    /// 0.24.2 按 §3.4 rank 表归一（如补全 `0.70 + 0.28 × fuzzy`）。
    pub rank_score: f64,
    /// UI 显示的建议文本（"fanyi" / `翻译 "the..."`）。
    pub display: String,
    /// 用户已输入部分的长度（字节，前端渲染灰色补全时对齐用）。
    /// Context 类 Suggestion 恒为 0（空 query 场景无「已输入」）。
    pub prefix_len: usize,
    /// Context 类 Suggestion 的取值来源（划词 / 剪贴板）,供前端展示「来自划词」提示。
    /// Keyword 类恒 None；序列化时 `#[skip_serializing_if]` 省略字段减少前端 undefined 判定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<SuggestionOrigin>,
    /// **0.8.6 deprecated**：RankingHint 由 Coordinator 独立返回，不再挂在 Suggestion 上。
    /// 0.24.1 保留（producer → coordinator 的过渡通道），0.24.2 随 `CoordinateInput`
    /// 重构改由 producer 返回值独立携带后移除。
    #[deprecated(
        since = "0.8.6",
        note = "RankingHint 由 SuggestionCoordinator 独立返回，不再挂在 Suggestion 上"
    )]
    #[serde(skip_serializing)]
    pub ranking_hint: Option<RankingHint>,
}

/// 一轮搜索产出的建议集合（0.24 §3.2 双槽位）。
///
/// 只暴露 primary / secondary 两个可见槽位，不展示第三条及更多候选；
/// **键随槽走不随视觉区走**——Tab 恒采纳 primary、Shift+Tab 恒采纳 secondary。
/// 0.24.1 secondary 恒 None（coordinator 仍是 top-1 语义），
/// 0.24.2 起 Coordinator 按 Kind 去重 + 分层排序选取双槽。
///
/// `revision` = 本次 search seq（后端原样回填）：前端与本地 seq 比对，
/// 不等则拒绝采纳（§3.6 过期防护，本地零 IPC）。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionSet {
    pub revision: u64,
    /// 主建议槽（Tab）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<Suggestion>,
    /// 次建议槽（Shift+Tab）。0.24.1 恒 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary: Option<Suggestion>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(deprecated)]
    fn sample(kind: SuggestionKind, action: SuggestionAction) -> Suggestion {
        Suggestion {
            id: "completion-keyword".to_string(),
            kind,
            action,
            rank_score: 0.9,
            display: "fanyi".to_string(),
            prefix_len: 2,
            origin: Some(SuggestionOrigin::Selection),
            ranking_hint: None,
        }
    }

    /// wire 形状：camelCase 字段名 + externally tagged action + 空槽/内部通道不上 wire。
    #[test]
    fn suggestion_set_wire_shape() {
        let set = SuggestionSet {
            revision: 42,
            primary: Some(sample(
                SuggestionKind::Completion,
                SuggestionAction::RouteQuery {
                    query: "fanyi".to_string(),
                },
            )),
            secondary: None,
        };
        let v = serde_json::to_value(&set).unwrap();
        assert_eq!(v["revision"], 42);
        assert_eq!(v["primary"]["id"], "completion-keyword");
        assert_eq!(v["primary"]["kind"], "completion");
        assert_eq!(v["primary"]["rankScore"], 0.9);
        assert_eq!(v["primary"]["action"]["routeQuery"]["query"], "fanyi");
        assert_eq!(v["primary"]["origin"], "selection");
        assert!(
            v.get("secondary").is_none(),
            "空槽 skip_serializing_if 应省略"
        );
        assert!(
            v["primary"].get("rankingHint").is_none(),
            "ranking_hint 内部通道不序列化"
        );
    }

    #[test]
    fn enter_ai_mode_wire_shape() {
        let set = SuggestionSet {
            revision: 7,
            primary: Some(sample(
                SuggestionKind::AskAi,
                SuggestionAction::EnterAiMode {
                    prompt: "hello".to_string(),
                },
            )),
            secondary: None,
        };
        let v = serde_json::to_value(&set).unwrap();
        assert_eq!(v["primary"]["kind"], "askAi");
        assert_eq!(v["primary"]["action"]["enterAiMode"]["prompt"], "hello");
    }
}
