//! ColorEngine：确定性颜色字面量引擎（sync lane，0.20.3）。
//!
//! 仅当完整 trim query 可被 `domain::color::parse` 解析为颜色字面量时，
//! 返回单条 `Copy` 结果（score = 1.0，source = "color"）。
//! 不接管 Route（不 takeover），常规 Mixed 分支中与其他 sync 引擎一起召回。
//!
//! **设计动机**：
//! - 用户在搜索框输入 `#ff0000` 或 `rgb(255,0,0)` 时，期望立即看到颜色预览
//! - 与 CalcEngine 心智一致：完整输入 = 确定性结果，无需 keyword 触发
//! - 不 takeover：颜色解析非常快（纯字符串解析），且不会误触发——普通英文/
//!   应用名/CSS 命名色不会被 parse() 接受
//!
//! **输出契约**：
//! - `SearchAction::Copy { text: canonical_hex }`——Enter 复制同族 canonical HEX
//! - 前端通过 `source == "color"` 识别颜色结果，渲染 swatch 色块
//! - 右键菜单提供 HEX/RGB/HSL 三种格式复制（前端 contextmenu.js 处理）
//!
//! **不误触发保证**：
//! - `parse()` 只接受以 `#` 开头的 hex 或 `rgb(`/`hsl(` 函数语法
//! - 普通英文单词（如 "calc"、"settings"、"red"）不会被解析
//! - CSS 命名色（如 "red"、"blue"）不被支持

use super::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};
use crate::domain::color;

pub struct ColorEngine;

impl ColorEngine {
    pub fn new() -> Self {
        ColorEngine
    }
}

impl Default for ColorEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SearchEngine for ColorEngine {
    fn id(&self) -> &'static str {
        "color"
    }

    fn lane(&self) -> Lane {
        Lane::Sync
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn search(&self, query: &str, _ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        let result = match color::parse(query) {
            Some(r) => r,
            None => return Vec::new(),
        };

        // 颜色结果的分数 = 1.0（精确匹配，与 CalcEngine 一致）
        vec![SearchItem {
            id: format!("color:{}", result.original),
            title: result.hex.clone(),
            subtitle: Some(format!("{} · {}", result.rgb, result.hsl)),
            score: super::scorer::calc_score(), // 1.0
            action: SearchAction::Copy {
                text: result.hex.clone(),
                hit_id: None,
            },
            source: "color".into(),
            score_detail: Some("color=1.0".into()),
            context_aware: false,
            color_list_hex: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::platform::context::ContextSnapshot;
    use std::collections::HashMap;

    fn make_ctx<'a>(
        history: &'a HashMap<String, (i64, i64)>,
        snapshot: &'a ContextSnapshot,
    ) -> QueryContext<'a> {
        QueryContext {
            history,
            snapshot,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &[],
            language: "zh",
        }
    }

    #[tokio::test]
    async fn hex6_produces_result() {
        let engine = ColorEngine::new();
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        let items = engine.search("#ff0000", &ctx).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "#FF0000");
        assert_eq!(items[0].source, "color");
        assert!((items[0].score - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn rgb_produces_result() {
        let engine = ColorEngine::new();
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        let items = engine.search("rgb(255, 0, 0)", &ctx).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "#FF0000");
    }

    #[tokio::test]
    async fn hsl_produces_result() {
        let engine = ColorEngine::new();
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        let items = engine.search("hsl(0, 100%, 50%)", &ctx).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "#FF0000");
    }

    #[tokio::test]
    async fn non_color_query_no_result() {
        let engine = ColorEngine::new();
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        // 普通英文和应用名不应触发
        assert!(engine.search("calc", &ctx).await.is_empty());
        assert!(engine.search("settings", &ctx).await.is_empty());
        assert!(engine.search("red", &ctx).await.is_empty());
        assert!(engine.search("hello world", &ctx).await.is_empty());
        assert!(engine.search("", &ctx).await.is_empty());
    }

    #[tokio::test]
    async fn copy_action_uses_canonical_hex() {
        let engine = ColorEngine::new();
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        let items = engine.search("#abcd", &ctx).await;
        assert_eq!(items.len(), 1);
        match &items[0].action {
            SearchAction::Copy { text, .. } => {
                assert_eq!(text, "#AABBCCDD");
            }
            _ => panic!("expected Copy action"),
        }
    }

    #[tokio::test]
    async fn subtitle_contains_rgb_and_hsl() {
        let engine = ColorEngine::new();
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        let items = engine.search("#ff000080", &ctx).await;
        assert_eq!(items.len(), 1);
        let subtitle = items[0].subtitle.as_ref().unwrap();
        assert!(subtitle.contains("rgb("));
        assert!(subtitle.contains("hsl("));
    }
}
