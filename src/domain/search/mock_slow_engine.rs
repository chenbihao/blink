//! MockSlowEngine:异步慢引擎(async lane),仅用于验证渐进式搜索链路。
//!
//! 0.2.2 无真实慢引擎(插件在 0.3),故用本 mock 占位 async lane:sleep 数百 ms 后
//! 返回稳定假结果,验证「sync 首批立即出 → async 增量 emit → 前端 merge」确实工作。
//!
//! **启用门槛**:`cfg!(debug_assertions) && 环境变量 BLINK_MOCK_SLOW_ENGINE=1`。
//! release 永不启用;debug 默认也不启用(避免日常开发污染),需显式设环境变量。
//!
//! 产出 `Open` + 空 path 项:前端 Enter 时 `activateItem` 无 lnkPath → 无动作,
//! 纯展示,绝不误触发(见 0.2.2 计划「关键约束」)。

use std::time::Duration;

use super::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};

/// 模拟查询耗时。
const MOCK_DELAY: Duration = Duration::from_millis(600);

pub struct MockSlowEngine;

impl MockSlowEngine {
    /// 按门槛判断是否启用(见模块文档)。
    pub fn enabled() -> bool {
        cfg!(debug_assertions)
            && std::env::var("BLINK_MOCK_SLOW_ENGINE").ok().as_deref() == Some("1")
    }
}

#[async_trait::async_trait]
impl SearchEngine for MockSlowEngine {
    fn id(&self) -> &'static str {
        "mock_slow"
    }

    fn lane(&self) -> Lane {
        Lane::Async
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn search(&self, query: &str, _ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        if query.is_empty() {
            return Vec::new();
        }
        tokio::time::sleep(MOCK_DELAY).await;
        vec![SearchItem {
            id: format!("mock:{query}"),
            title: format!("[mock] 异步结果: {query}"),
            subtitle: Some("MockSlowEngine — 验证渐进式增量".into()),
            score: 0.5,
            action: SearchAction::None, // 纯展示，无操作
            source: "mock_slow".into(),
            score_detail: Some("mock=0.5".into()),
            context_aware: false,
        }]
    }
}
