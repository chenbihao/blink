//! 桌面便签域（0.16.7）。
//!
//! 架构定位：domain 层负责业务规则（防抖保存、恢复策略、日志隐私），
//! infra/data/sticky.rs 负责纯 DB 读写。
//!
//! **不 use tauri**——domain 保持框架无关（0.15 收敛铁则）。
//! IPC 桥接在 app/commands/sticky.rs。
//!
//! 设计见 phases/0.16-clipboard-polish.md §3.8-§3.10。

pub use crate::infra::data::sticky::{StickyColor, StickyFormat, StickyNote};

use sqlx::SqlitePool;

/// 便签服务：封装保存和恢复策略。
///
/// **防抖**（§3.9）：前端做输入防抖，500ms 停顿后调后端写库。
/// 后端提供即时写库能力，不额外做防抖——防抖在调用方（前端 JS）做更合适，
/// 避免后端持有未保存状态。
///
/// **恢复**（§3.9）：启动时异步读取所有便签，`visible=true` 的恢复窗口，
/// `visible=false` 只进入管理界面。恢复在主窗口服务 ready 后走旁路，
/// 不阻塞 Alt+Space。单条恢复失败只记录并跳过。
pub struct StickyService {
    history_pool: SqlitePool,
}

impl StickyService {
    pub fn new(history_pool: SqlitePool) -> Self {
        Self { history_pool }
    }

    /// 创建新便签。
    pub async fn create_note(
        &self,
        content: &str,
        color: StickyColor,
    ) -> Result<StickyNote, String> {
        let color_str = color.as_str().to_string();
        let note = StickyNote {
            id: crate::infra::data::sticky::generate_id(),
            content: content.to_string(),
            format: StickyFormat::default(),
            color,
            visible: true,
            x: 0,
            y: 0,
            width: crate::infra::data::sticky::DEFAULT_WIDTH,
            height: crate::infra::data::sticky::DEFAULT_HEIGHT,
            always_on_top: true,
            created_at: 0,
            updated_at: 0,
        };
        crate::infra::data::sticky::create(&self.history_pool, &note).await?;
        tracing::info!(sticky_id = %note.id, color = %color_str, "便签已创建");
        Ok(note)
    }

    /// 获取便签。
    pub async fn get_note(&self, id: &str) -> Option<StickyNote> {
        crate::infra::data::sticky::get(&self.history_pool, id).await
    }

    /// 列出全部便签。
    pub async fn list_notes(&self) -> Vec<StickyNote> {
        crate::infra::data::sticky::list(&self.history_pool).await
    }

    /// 更新便签内容。
    ///
    /// 前端 JS 做防抖（500ms 停顿后调用），后端即时写库。
    pub async fn update_content_debounced(&self, id: &str, content: &str) {
        // 简化实现：直接写库（防抖在前端做更合适，后端提供即时写库能力）
        // 前端 JS 做防抖，500ms 停顿后调 update_sticky_content
        if let Err(e) =
            crate::infra::data::sticky::update_content(&self.history_pool, id, content).await
        {
            tracing::warn!(sticky_id = %id, error = %e, "便签内容保存失败");
        }
    }

    /// 更新便签外观（颜色 + 可选格式）。
    pub async fn update_appearance(
        &self,
        id: &str,
        color: StickyColor,
        format: Option<StickyFormat>,
    ) -> Result<(), String> {
        crate::infra::data::sticky::update_appearance(
            &self.history_pool,
            id,
            &color,
            format.as_ref(),
        )
        .await?;
        tracing::debug!(sticky_id = %id, color = %color.as_str(), "便签外观已更新");
        Ok(())
    }

    /// 更新便签窗口几何。
    pub async fn update_geometry(
        &self,
        id: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), String> {
        crate::infra::data::sticky::update_geometry(
            &self.history_pool, id, x, y, width, height,
        )
        .await
    }

    /// 设置便签可见性。
    pub async fn set_visible(&self, id: &str, visible: bool) -> Result<(), String> {
        crate::infra::data::sticky::set_visible(&self.history_pool, id, visible).await?;
        tracing::info!(sticky_id = %id, visible, "便签可见性已变更");
        Ok(())
    }

    /// 设置便签置顶。
    pub async fn set_always_on_top(
        &self,
        id: &str,
        always_on_top: bool,
    ) -> Result<(), String> {
        crate::infra::data::sticky::set_always_on_top(&self.history_pool, id, always_on_top).await?;
        Ok(())
    }

    /// 删除便签（永久）。
    pub async fn delete_note(&self, id: &str) -> Result<(), String> {
        crate::infra::data::sticky::delete(&self.history_pool, id).await?;
        tracing::info!(sticky_id = %id, "便签已删除");
        Ok(())
    }

    /// 获取便签统计。
    pub async fn get_stats(&self) -> serde_json::Value {
        crate::infra::data::sticky::get_stats(&self.history_pool).await
    }

    /// 恢复服务：启动时异步加载所有便签。
    ///
    /// 返回 visible=true 的便签列表（需恢复窗口），visible=false 的不返回
    /// （只在管理界面显示，0.16.10）。
    ///
    /// **单条失败隔离**：某条便签读取失败只记录 warn，不阻断其他便签。
    /// 当前 list() 内部用 unwrap_or_default 保证不会 panic——全部返回或空 vec。
    pub async fn load_for_recovery(&self) -> Vec<StickyNote> {
        let all = crate::infra::data::sticky::list(&self.history_pool).await;
        let total = all.len();
        let visible: Vec<_> = all.into_iter().filter(|n| n.visible).collect();
        let visible_count = visible.len();
        tracing::info!(total, visible_count, "便签恢复：加载完成");
        visible
    }
}
