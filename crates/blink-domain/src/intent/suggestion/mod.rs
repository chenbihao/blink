//! Suggestion 统一契约（0.8.3 §4.3 / §4.4；0.24.1 类型化重构；0.24.2 评分收敛）。
//!
//! **0.24.1 契约**：`SearchResponse.suggestion` 从 `Option<Suggestion>` 迁移为
//! `Option<SuggestionSet>`（带 revision 的双槽位集合）。单个 `Suggestion` 携带语义
//! `kind` 与类型化采纳动作 `action`——`replacement` 并入 `RouteQuery.query`，
//! wire 不再有 `source` 字段（producer 身份由 `Suggestion.source` 内部携带，
//! coordinator 盖章，仅供日志）。
//!
//! **0.24.2 契约**：`SuggestionCoordinator` 按 §3.4 执行 eligibility → 分层 →
//! 按 Kind 去重 → 排序 → 双槽选取（secondary 过 `secondary_min_rank` 门槛）；
//! 编排层输入收敛为 `RouteSummary` / `SuggestionAvailability`（§3.7）。
//!
//! **rank_score 与 confidence 分离**（0.24 §3.4）：规则排序值统一叫 `rank_score`；
//! `confidence` 保留给未来具备校准语义的模型输出——0.24 规则侧无生产者，字段不建，
//! 0.25 模型 Producer 落地时随 `ModelIntentProducer` 一起加。
//!
//! 0.8.3 动机存档：所有「待用户采纳的建议」共用一个 `SearchResponse` 字段，多源
//! 竞争产 top-1——每加一路信号不再多一个字段 + 多一层前端优先级分支。

pub mod ai;
pub mod context;
pub mod coordinator;
pub mod fatigue;
pub mod keyword;
pub mod producer;

use std::hash::{Hash, Hasher};

use serde::Serialize;

use super::{RankingHint, Route};
use blink_infra::platform::context::AwarenessSource;

/// Producer 身份标识（0.24.1 起不序列化上 wire）。
///
/// 由 `SuggestionProducer::source()` 声明，`SuggestionCoordinator` 在收集候选时
/// 盖章到 `Suggestion.source`（内部字段，`#[serde(skip)]`）——filter/impression/
/// adoption 日志据此记 producer（§6.1"采纳遥测含 producer"）。前端不消费。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionSource {
    /// 输入补全（首拼 fy → fanyi / 汉字 翻 → 翻译）。非空 query 独占。
    Keyword,
    /// 环境感知（选中英文 → 翻译）。空 query 独占。
    Context,
    /// AI Producer（0.24.2 从 SearchService 后置注入迁入；ai-trigger 候选由
    /// Coordinator 直接构造，同样盖 Ai）。
    Ai,
}

/// 建议的语义类别（0.24 §3.4 / §3.5；0.24.8 打开类激活）。
///
/// 前端按 kind 决定视觉通道：`Completion` → ghost 影子文字；
/// `Translate` / `AskAi` / 打开类 → 采纳提示（SuggestionBar）。
///
/// 打开类三 Kind（0.24.8 激活，原 0.25 候选提前）：剪贴板/输入为 URL 或文件路径时
/// 的打开动作建议，采纳动作为 `InvokeCapability`。环境感知的打开呈现从 result lane
/// （BuiltinEngine context 召回）移交到建议槽——result 只保留 keyword 命中路径，
/// 「智能提示统一走 Tab/Shift+Tab 采纳」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SuggestionKind {
    /// 输入补全：影子文字自然接在用户文本后（`fy` → `fanyi`）。
    Completion,
    /// 翻译建议（query/选区/剪贴板为非目标语言）。
    Translate,
    /// AI 处理建议（"按 Tab 问 AI" / 进 AI 模式）。
    AskAi,
    /// 打开 URL（剪贴板/输入为 URL）。rank 0.95（§3.4 rank 表预留值）。
    OpenUrl,
    /// 打开路径（剪贴板/输入为文件路径）。rank 0.93。
    OpenPath,
    /// 资源管理器定位（剪贴板/输入为文件路径）。rank 0.88（仅与 OpenPath 同现，
    /// 场景内无翻译竞争，高于 AI 兜底 0.80）。
    RevealInExplorer,
}

/// 类型化采纳动作（0.24 §3.5）。
///
/// **0.24 采纳零执行能力**：只有"改 query 走一轮新搜索"与"进主窗口 AI 模式"两种
/// 无副作用动作，最坏情况（query 改错）立即可见可撤销——这是 §3.6 采纳协议
/// （前端乐观本地 + 单向遥测）成立的前提。
///
/// **0.24.8 契约修订**：新增 `InvokeCapability`（打开类建议），采纳侧复用
/// `run_builtin_action` 同步 IPC——CapabilityRegistry 的 origin/runtime/policy
/// 门禁全量生效，与 result Enter 路径同一执行边界；revision 过期防护不变。
///
/// serde：externally tagged + camelCase → `{"routeQuery":{"query":"…"}}` /
/// `{"enterAiMode":{"prompt":"…"}}` / `{"invokeCapability":{"capabilityId":"…","args":{…}}}`。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SuggestionAction {
    /// 回写输入框 + 触发新一轮搜索（补全采纳与翻译采纳同用；
    /// 翻译即 query 变为"翻译 xxx"后命中确定路由）。
    RouteQuery { query: String },
    /// 前端直进主窗口 AI 模式（0.17.6 起 ChatService ephemeral 对话）。
    EnterAiMode { prompt: String },
    /// 调用 Capability 执行（0.24.8，打开类建议）。`args` 为目标 Capability
    /// schema 接收的最终 JSON object（如 `{"url":"https://…"}`），由 producer
    /// 构造——与 BuiltinEngine `ParamSource::extract` 的参数形状约定一致。
    /// （enum 级 rename_all 只覆盖变体名，变体字段需自带 camelCase。）
    #[serde(rename_all = "camelCase")]
    InvokeCapability {
        capability_id: String,
        args: serde_json::Value,
    },
}

/// Context 类 Suggestion 的取值来源（0.8.3 §4.9 UX 加强）——
/// 用户看到 Ghost 时能立刻知道「这个建议是基于我划的词 / 我剪贴板里的东西」。
///
/// 前端按此值查 i18n key（`suggestion.origin.selection` / `suggestion.origin.clipboard`）
/// 挂在 Ghost 尾部或 statusbar,弱视觉,不喧宾夺主。
///
/// Keyword 类 Suggestion 恒 None（输入补全无外部来源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
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

/// 会话内文本指纹（0.24 §3.8 降频计数键成分）。
///
/// `DefaultHasher`（SipHash）非跨运行稳定——指纹只活在进程内（降频计数不落盘），
/// 与"日志不记原文"的隐私口径一致：指纹不可逆推原文，仅用于区分"剪贴板换内容了"。
pub(crate) fn text_fingerprint(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// 路由摘要（0.24 §3.7）——Coordinator eligibility 的编排层输入。
///
/// **编排顺序即防环**：SearchService 先算 Route、后算 Suggestion，本类型是编排层
/// 传值不是域依赖——Routing 依旧对 Awareness 无知，Suggestion 域只读路由结果摘要。
///
/// - `Open`：空 Mixed（无任何规则命中）——query 派生的翻译/AI 兜底候选允许参选。
/// - `Routed`：已有确定路由或非空候选（Takeover / EngineTakeover / 非空 Mixed）——
///   query 派生的 Translate/AskAi 候选全部排除（"已明确命中的路由不得被翻译抢占"）。
/// - `AiTrigger`：`"ai "` 前缀显式触发——Suggestion 恒为 ai-trigger AskAi（强信号独占）。
#[derive(Debug, Clone)]
pub enum RouteSummary {
    Open,
    Routed,
    AiTrigger { arg: String },
}

impl From<&Route> for RouteSummary {
    fn from(route: &Route) -> Self {
        match route {
            Route::Mixed { candidates } if candidates.is_empty() => RouteSummary::Open,
            Route::Mixed { .. } => RouteSummary::Routed,
            Route::Takeover { .. } | Route::EngineTakeover { .. } => RouteSummary::Routed,
            Route::AiTrigger { arg } => RouteSummary::AiTrigger {
                arg: arg.to_string(),
            },
        }
    }
}

/// 能力可用性表（0.24 §3.7）——由编排层（SearchService）汇出传入 Coordinator。
///
/// 翻译插件绑定/黑名单检查留在 producer 路径（规则表状态，`match_context_hits` 已有），
/// 这里只承载服务层才知道的可用性：AI Provider 是否配置并启用。
#[derive(Debug, Clone, Copy)]
pub struct SuggestionAvailability {
    /// AI registry 已注入且 `AIConfig.enabled`——AskAi 候选与 ai-trigger 的总闸。
    pub ai_available: bool,
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
    /// 候选来源文本的会话内指纹（0.24 §3.8 降频计数键成分，`text_fingerprint`）。
    /// query 派生候选 = hash(query)；awareness 派生 = hash(选区/剪贴板文本)。
    /// 不上 wire（`#[serde(skip)]`），不落盘。
    #[serde(skip)]
    pub fingerprint: u64,
    /// 生产该候选的 Producer 身份（coordinator 收集时按 `producer.source()` 盖章，
    /// 可观测性日志用）。不上 wire（`#[serde(skip)]`）。
    #[serde(skip)]
    pub source: SuggestionSource,
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
/// 0.24.2 起 Coordinator 按 Kind 去重 + 分层排序选取双槽（secondary 需过
/// `secondary_min_rank` 门槛）。
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
    /// 次建议槽（Shift+Tab）。需过 `secondary_min_rank` 门槛（0.24 §3.4）。
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
            fingerprint: 0,
            source: SuggestionSource::Keyword,
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
            secondary: Some(sample(
                SuggestionKind::Translate,
                SuggestionAction::RouteQuery {
                    query: "翻译 hello".to_string(),
                },
            )),
        };
        let v = serde_json::to_value(&set).unwrap();
        assert_eq!(v["revision"], 42);
        assert_eq!(v["primary"]["id"], "completion-keyword");
        assert_eq!(v["primary"]["kind"], "completion");
        assert_eq!(v["primary"]["rankScore"], 0.9);
        assert_eq!(v["primary"]["action"]["routeQuery"]["query"], "fanyi");
        assert_eq!(v["primary"]["origin"], "selection");
        assert_eq!(v["secondary"]["kind"], "translate");
        assert!(
            v["primary"].get("rankingHint").is_none(),
            "ranking_hint 内部通道不序列化"
        );
        assert!(
            v["primary"].get("fingerprint").is_none(),
            "fingerprint 内部字段不序列化"
        );
        assert!(
            v["primary"].get("source").is_none(),
            "source 内部字段不序列化"
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
        assert!(
            v.get("secondary").is_none(),
            "空槽 skip_serializing_if 应省略"
        );
    }

    /// 0.24.8：InvokeCapability wire 形状（externally tagged camelCase，
    /// capability_id → capabilityId，args 原样透传 JSON object）。
    #[test]
    #[allow(deprecated)]
    fn invoke_capability_wire_shape() {
        let set = SuggestionSet {
            revision: 9,
            primary: Some(Suggestion {
                id: "open-url-clipboard".to_string(),
                kind: SuggestionKind::OpenUrl,
                action: SuggestionAction::InvokeCapability {
                    capability_id: "open_url".to_string(),
                    args: serde_json::json!({"url": "https://example.com"}),
                },
                rank_score: 0.95,
                display: "https://example.com".to_string(),
                prefix_len: 0,
                origin: Some(SuggestionOrigin::Clipboard),
                fingerprint: 0,
                source: SuggestionSource::Context,
                ranking_hint: None,
            }),
            secondary: Some(Suggestion {
                id: "reveal-clipboard".to_string(),
                kind: SuggestionKind::RevealInExplorer,
                action: SuggestionAction::InvokeCapability {
                    capability_id: "reveal_in_explorer".to_string(),
                    args: serde_json::json!({"path": "C:\\tmp\\a.txt"}),
                },
                rank_score: 0.88,
                display: "C:\\tmp\\a.txt".to_string(),
                prefix_len: 0,
                origin: Some(SuggestionOrigin::Clipboard),
                fingerprint: 0,
                source: SuggestionSource::Context,
                ranking_hint: None,
            }),
        };
        let v = serde_json::to_value(&set).unwrap();
        assert_eq!(v["primary"]["kind"], "openUrl");
        assert_eq!(
            v["primary"]["action"]["invokeCapability"]["capabilityId"],
            "open_url"
        );
        assert_eq!(
            v["primary"]["action"]["invokeCapability"]["args"]["url"],
            "https://example.com"
        );
        assert_eq!(v["secondary"]["kind"], "revealInExplorer");
        assert_eq!(
            v["secondary"]["action"]["invokeCapability"]["args"]["path"],
            "C:\\tmp\\a.txt"
        );
    }

    /// 指纹对相同文本稳定、对不同文本区分（会话内）。
    #[test]
    fn text_fingerprint_stable_and_distinct() {
        assert_eq!(
            text_fingerprint("hello world"),
            text_fingerprint("hello world")
        );
        assert_ne!(
            text_fingerprint("hello world"),
            text_fingerprint("hello world!")
        );
    }

    /// Route → RouteSummary 摘要映射（eligibility 的编排层输入）。
    #[test]
    fn route_summary_from_route() {
        use super::super::{Candidate, ExecArg, Surface};
        let empty_mixed = Route::Mixed { candidates: vec![] };
        assert!(matches!(
            RouteSummary::from(&empty_mixed),
            RouteSummary::Open
        ));
        let non_empty = Route::Mixed {
            candidates: vec![Candidate {
                plugin_id: "p".into(),
                arg: ExecArg::None,
                surface: Surface::Inline,
                hint: None,
            }],
        };
        assert!(matches!(
            RouteSummary::from(&non_empty),
            RouteSummary::Routed
        ));
        let takeover = Route::Takeover {
            plugin_id: "p".into(),
            arg: ExecArg::None,
            view: Default::default(),
            hint: None,
        };
        assert!(matches!(
            RouteSummary::from(&takeover),
            RouteSummary::Routed
        ));
        let ai = Route::AiTrigger { arg: "hi".into() };
        assert!(matches!(
            RouteSummary::from(&ai),
            RouteSummary::AiTrigger { arg } if arg == "hi"
        ));
    }
}
