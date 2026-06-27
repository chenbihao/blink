//! 多路搜索引擎抽象(0.2.2,见 production-design/phases/0.2-core-plugin-design.md §2)。
//!
//! - `SearchEngine`:统一召回接口。引擎按 `Lane` 分两条通道——sync(本地、紧 budget、
//!   同步返回首批)与 async(慢引擎,完成后增量 emit)。0.2.2 sync = Calc/StartMenu,
//!   async = 仅 mock(真插件在 0.3)。
//! - `SearchItem`:引擎产出的**内部融合模型**(带归一化 score / source),用于 SearchService
//!   去重 + 排序。**不直接给前端**——融合后转回现有 `AppEntry` 形状返回(前端契约不变)。
//! - `QueryContext`:单次查询的共享上下文(当前仅历史权重),供引擎计算分数。

use std::collections::HashMap;

use crate::context::ContextSnapshot;

use super::{Action, ActionKind, AppEntry};

/// 引擎延迟通道:sync 进首批(同步返回),async 走增量(emit 推送)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// 本地快引擎,纳入 invoke 同步返回的首批结果。
    Sync,
    /// 慢引擎(插件/网络),完成后异步增量推送,不阻塞首批。
    #[allow(dead_code)] // 0.2.2 仅 mock 用;真异步引擎在 0.3
    Async,
}

/// 引擎产出项的动作(后端内部模型,含 payload)。
/// 与前端契约 `Action`(同样带 payload)一一对应——转换见 [`SearchItem::into_app_entry`]。
#[derive(Debug, Clone)]
pub enum SearchAction {
    /// 打开路径(应用/快捷方式/文件/URL)。空 path = 纯展示项(前端 Enter 无动作)。
    Open { path: String },
    /// 复制文本到剪贴板(计算结果 / 插件 Copy)。
    /// `text` 为结构化 payload,经 `into_app_entry` 透传到前端 `Action.payload`。
    Copy { text: String },
}

/// 引擎召回的单个结果项(内部融合模型)。
#[derive(Debug, Clone)]
pub struct SearchItem {
    /// 去重键(Open 用路径;Copy 用表达式/文本)。
    pub id: String,
    /// 主行显示文本。
    pub title: String,
    /// 副行显示文本(路径/提示)。
    pub subtitle: Option<String>,
    /// 归一化分数 0.0..=1.0(引擎内自行归一化;融合层据此排序)。
    pub score: f32,
    /// 动作(含 payload)。
    pub action: SearchAction,
    /// 产出该项的引擎/插件 id(tie-break + 调试)。引擎 id 多为静态,但插件 id 是
    /// 运行时字符串,故用 String。
    pub source: String,
    /// 分数构成详情（可选，用于 debug 日志可观测）。
    /// 格式如 "fuzzy=0.8 hist=+0.2 src=+0.4"，方便调参时理解排序原因。
    pub score_detail: Option<String>,
}

impl SearchItem {
    /// 转成前端契约 `AppEntry` 形状。
    ///
    /// - `Open` → `lnk_path=path`,`action.kind=Open`;空 path 即纯展示项。
    /// - `Copy` → `is_calc=true`,`action.kind=Copy` 且 `action.payload=Some(text)`。
    ///   前端 `actions.js` 复制优先取 `payload`(= text);无 payload 才回退 `calcValue`
    ///   (从 name 去 "= " 前缀;CalcEngine 的 title 形如 `= <text>` 故二者一致)。
    pub fn into_app_entry(self) -> AppEntry {
        // 负分 = 插件错误信息，不 bake source boost（保留负分排到最后）
        let is_error = self.score < 0.0;
        let score = if is_error {
            self.score
        } else {
            super::scorer::bake_source_boost(self.score, &self.source)
        };

        // 追加 source boost 信息到 score_detail
        let score_detail = self.score_detail.map(|mut d| {
            if !is_error {
                let src_boost = score - self.score;
                d.push_str(&format!(" src=+{:.2}", src_boost));
            }
            d
        });

        match self.action {
            SearchAction::Open { path } => AppEntry {
                name: self.title,
                pinyin_name: String::new(),
                description: self.subtitle,
                lnk_path: path,
                is_calc: false,
                score,
                is_placeholder: false,
                is_error,
                source: self.source.clone(),
                action: Action {
                    kind: ActionKind::Open,
                    hint: None,
                    payload: None,
                },
                score_detail,
            },
            SearchAction::Copy { text } => AppEntry {
                name: self.title,
                pinyin_name: String::new(),
                description: self.subtitle,
                lnk_path: String::new(),
                // is_calc 仅标记计算结果(驱动前端 calc 样式 + calcValue);插件 Copy
                // 不该套计算样式,故按来源判定(CalcEngine 的 source == "calc")。
                is_calc: self.source == "calc",
                score,
                is_placeholder: false,
                is_error,
                source: self.source.clone(),
                action: Action {
                    kind: ActionKind::Copy,
                    hint: None,
                    payload: Some(text),
                },
                score_detail,
            },
        }
    }
}

/// 单次查询的共享上下文。0.2.2 仅含历史权重;后续可加意图/语言等。
#[allow(dead_code)] // 0.4+ 意图路由扩展时启用
pub struct QueryContext<'a> {
    /// lnk_path → (hit_count, last_used_at) 历史权重（0.7.5 含时间衰减）。
    pub history: &'a HashMap<String, (i64, i64)>,
    /// 唤起时的上下文快照（前台应用、剪贴板等）。
    pub snapshot: &'a ContextSnapshot,
}

/// 搜索引擎:一路召回源。
#[async_trait::async_trait]
pub trait SearchEngine: Send + Sync + std::any::Any {
    /// 引擎 id(融合 tie-break / 日志 / 调试)。
    fn id(&self) -> &'static str;
    /// 所属通道(sync 进首批 / async 走增量)。
    fn lane(&self) -> Lane;
    /// 启动引擎后台任务(如缓存预扫)。默认空——无状态引擎无需启动。
    fn start(&self) {}
    /// 召回:返回归一化分数的结果项(空 query 行为由引擎自定)。
    async fn search(&self, query: &str, ctx: &QueryContext<'_>) -> Vec<SearchItem>;
    /// 支持 downcast 到具体类型（用于配置更新）。
    fn as_any(&self) -> &dyn std::any::Any;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_item_maps_to_app_entry() {
        let item = SearchItem {
            id: "C:\\a.lnk".into(),
            title: "App".into(),
            subtitle: Some("C:\\a.lnk".into()),
            score: 0.8,
            action: SearchAction::Open {
                path: "C:\\a.lnk".into(),
            },
            source: "start_menu".into(),
            score_detail: Some("fuzzy=0.8".into()),
        };
        let e = item.into_app_entry();
        assert_eq!(e.name, "App");
        assert_eq!(e.lnk_path, "C:\\a.lnk");
        assert!(!e.is_calc);
        assert!(matches!(e.action.kind, ActionKind::Open));
        assert!(e.action.payload.is_none());
        assert_eq!(e.description.as_deref(), Some("C:\\a.lnk"));
    }

    #[test]
    fn copy_item_maps_to_calc_app_entry() {
        let item = SearchItem {
            id: "1+1".into(),
            title: "= 2".into(),
            subtitle: Some("按 Enter 复制结果".into()),
            score: 1.0,
            action: SearchAction::Copy { text: "2".into() },
            source: "calc".into(),
            score_detail: Some("calc=1.0".into()),
        };
        let e = item.into_app_entry();
        assert_eq!(e.name, "= 2");
        assert!(e.lnk_path.is_empty());
        assert!(e.is_calc);
        assert!(matches!(e.action.kind, ActionKind::Copy));
        // text 透传到 payload(前端复制优先取 payload)
        assert_eq!(e.action.payload.as_deref(), Some("2"));
    }
}
