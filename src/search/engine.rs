//! 多路搜索引擎抽象(0.2.2,见 production/0.2-core-plugin-design.md §2)。
//!
//! - `SearchEngine`:统一召回接口。引擎按 `Lane` 分两条通道——sync(本地、紧 budget、
//!   同步返回首批)与 async(慢引擎,完成后增量 emit)。0.2.2 sync = Calc/StartMenu,
//!   async = 仅 mock(真插件在 0.3)。
//! - `SearchItem`:引擎产出的**内部融合模型**(带归一化 score / source),用于 SearchService
//!   去重 + 排序。**不直接给前端**——融合后转回现有 `AppEntry` 形状返回(前端契约不变)。
//! - `QueryContext`:单次查询的共享上下文(当前仅历史权重),供引擎计算分数。

use std::collections::HashMap;

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
/// 区别于前端契约 `Action`(只有 kind/hint)——转换见 [`SearchItem::into_app_entry`]。
#[derive(Debug, Clone)]
pub enum SearchAction {
    /// 打开路径(应用/快捷方式/文件)。空 path = 纯展示项(前端 Enter 无动作)。
    Open { path: String },
    /// 复制文本到剪贴板(计算结果等)。
    /// `text` 为结构化 payload;0.2.2 前端复制实际从 title 去 "= " 前缀取值
    /// (见 `into_app_entry`),text 留给 0.3 插件 Copy 动作直接消费。
    Copy {
        #[allow(dead_code)]
        text: String,
    },
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
}

impl SearchItem {
    /// 转成前端契约 `AppEntry` 形状。
    ///
    /// - `Open` → `lnk_path=path`,`action.kind=Open`;空 path 即纯展示项。
    /// - `Copy` → `is_calc=true`(前端据此走复制样式 + `calcValue`),`action.kind=Copy`。
    ///   注:前端 `actions.js` 复制依赖从 `name`(去 "= " 前缀)取值,故 Copy 项的
    ///   title 须形如 `= <text>`(CalcEngine 已如此产出)。
    pub fn into_app_entry(self) -> AppEntry {
        match self.action {
            SearchAction::Open { path } => AppEntry {
                name: self.title,
                pinyin_name: String::new(),
                description: self.subtitle,
                lnk_path: path,
                is_calc: false,
                action: Action {
                    kind: ActionKind::Open,
                    hint: None,
                },
            },
            SearchAction::Copy { .. } => AppEntry {
                name: self.title,
                pinyin_name: String::new(),
                description: self.subtitle,
                lnk_path: String::new(),
                is_calc: true,
                action: Action {
                    kind: ActionKind::Copy,
                    hint: None,
                },
            },
        }
    }
}

/// 单次查询的共享上下文。0.2.2 仅含历史权重;后续可加意图/语言等。
pub struct QueryContext<'a> {
    /// lnk_path → 历史命中次数(频率加权用)。
    pub history: &'a HashMap<String, i64>,
}

/// 搜索引擎:一路召回源。
#[async_trait::async_trait]
pub trait SearchEngine: Send + Sync {
    /// 引擎 id(融合 tie-break / 日志 / 调试)。
    fn id(&self) -> &'static str;
    /// 所属通道(sync 进首批 / async 走增量)。
    fn lane(&self) -> Lane;
    /// 启动引擎后台任务(如缓存预扫)。默认空——无状态引擎无需启动。
    fn start(&self) {}
    /// 召回:返回归一化分数的结果项(空 query 行为由引擎自定)。
    async fn search(&self, query: &str, ctx: &QueryContext<'_>) -> Vec<SearchItem>;
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
        };
        let e = item.into_app_entry();
        assert_eq!(e.name, "App");
        assert_eq!(e.lnk_path, "C:\\a.lnk");
        assert!(!e.is_calc);
        assert!(matches!(e.action.kind, ActionKind::Open));
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
        };
        let e = item.into_app_entry();
        assert_eq!(e.name, "= 2"); // 前端从 name 去 "= " 前缀取 calcValue
        assert!(e.lnk_path.is_empty());
        assert!(e.is_calc);
        assert!(matches!(e.action.kind, ActionKind::Copy));
    }
}
