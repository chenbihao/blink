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

/// 便签错误——可序列化，IPC 边界保留 `kind` 字段供前端分类展示（spec §4.1）。
///
/// domain 层用此类型替代 `Result<_, String>`，command 层在 IPC 边界 `.to_string()` 拍平。
#[derive(Debug, serde::Serialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum StickyError {
    /// 数据库错误（连接失败 / 约束冲突 / 序列化失败等）。
    #[error("数据库错误: {detail}")]
    Db { detail: String },
    /// 便签不存在（id 无效或已被删除）。
    #[error("便签不存在: {id}")]
    NotFound { id: String },
}

impl From<String> for StickyError {
    fn from(s: String) -> Self {
        StickyError::Db { detail: s }
    }
}

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
    ) -> Result<StickyNote, StickyError> {
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
            trashed: false,
            deleted_at: None,
        };
        crate::infra::data::sticky::create(&self.history_pool, &note)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
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

    /// 列出回收站中的便签（0.17.7）。
    pub async fn list_trashed_notes(&self) -> Vec<StickyNote> {
        crate::infra::data::sticky::list_trashed(&self.history_pool).await
    }

    /// 更新便签内容。
    ///
    /// 前端 JS 做防抖（500ms 停顿后调用），后端即时写库。
    /// P1-#13 fix: 返回 Result 传播错误，不再吞错——前端需知道保存是否成功。
    pub async fn update_content_debounced(
        &self,
        id: &str,
        content: &str,
    ) -> Result<(), StickyError> {
        crate::infra::data::sticky::update_content(&self.history_pool, id, content)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        tracing::trace!(sticky_id = %id, "便签内容已保存");
        Ok(())
    }

    /// 更新便签外观（颜色 + 可选格式）。
    pub async fn update_appearance(
        &self,
        id: &str,
        color: StickyColor,
        format: Option<StickyFormat>,
    ) -> Result<(), StickyError> {
        crate::infra::data::sticky::update_appearance(
            &self.history_pool,
            id,
            &color,
            format.as_ref(),
        )
        .await
        .map_err(|e| StickyError::Db { detail: e })?;
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
    ) -> Result<(), StickyError> {
        crate::infra::data::sticky::update_geometry(&self.history_pool, id, x, y, width, height)
            .await
            .map_err(|e| StickyError::Db { detail: e })
    }

    /// 设置便签可见性。
    pub async fn set_visible(&self, id: &str, visible: bool) -> Result<(), StickyError> {
        crate::infra::data::sticky::set_visible(&self.history_pool, id, visible)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        tracing::info!(sticky_id = %id, visible, "便签可见性已变更");
        Ok(())
    }

    /// 将便签移入回收站（软删除，0.17.7）。
    ///
    /// `trashed=true` + `deleted_at=now`，保留数据。调用后窗口应 hide。
    pub async fn trash_note(&self, id: &str) -> Result<(), StickyError> {
        crate::infra::data::sticky::set_trashed(&self.history_pool, id, true)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        tracing::info!(sticky_id = %id, "便签已移入回收站");
        Ok(())
    }

    /// 从回收站恢复便签（0.17.7）。
    ///
    /// `trashed=false` + `deleted_at=null`，恢复到桌面。
    pub async fn restore_note(&self, id: &str) -> Result<(), StickyError> {
        crate::infra::data::sticky::set_trashed(&self.history_pool, id, false)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        tracing::info!(sticky_id = %id, "便签已从回收站恢复");
        Ok(())
    }

    /// 清空回收站（0.17.7）。返回删除的行数。
    pub async fn clear_trashed(&self) -> Result<u64, StickyError> {
        let count = crate::infra::data::sticky::clear_all_trashed(&self.history_pool)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        tracing::info!(deleted = count, "回收站已清空");
        Ok(count)
    }

    /// 清理过期回收站便签（0.17.7）。启动时调用。
    #[allow(dead_code)] // 启动清理在 pools.rs 直接调 data 层
    pub async fn cleanup_trashed(&self, retention_days: i64) -> u64 {
        crate::infra::data::sticky::cleanup_trashed(&self.history_pool, retention_days).await
    }

    /// 设置便签置顶。
    pub async fn set_always_on_top(
        &self,
        id: &str,
        always_on_top: bool,
    ) -> Result<(), StickyError> {
        crate::infra::data::sticky::set_always_on_top(&self.history_pool, id, always_on_top)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        Ok(())
    }

    /// 删除便签（永久）。
    pub async fn delete_note(&self, id: &str) -> Result<(), StickyError> {
        crate::infra::data::sticky::delete(&self.history_pool, id)
            .await
            .map_err(|e| StickyError::Db { detail: e })?;
        tracing::info!(sticky_id = %id, "便签已删除");
        Ok(())
    }

    /// 获取便签统计。
    pub async fn get_stats(&self) -> serde_json::Value {
        crate::infra::data::sticky::get_stats(&self.history_pool).await
    }

    /// 恢复服务：启动时异步加载所有便签。
    ///
    /// 返回 `trashed=false && visible=true` 的便签列表（需恢复窗口）。
    /// 回收站中的便签（`trashed=true`）不恢复窗口，只在管理界面显示。
    ///
    /// **单条失败隔离**：某条便签读取失败只记录 warn，不阻断其他便签。
    pub async fn load_for_recovery(&self) -> Vec<StickyNote> {
        let all = crate::infra::data::sticky::list(&self.history_pool).await;
        let total = all.len();
        let visible: Vec<_> = all.into_iter().filter(|n| n.visible).collect();
        let visible_count = visible.len();
        tracing::info!(total, visible_count, "便签恢复：加载完成");
        visible
    }
}
