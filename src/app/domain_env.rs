//! TauriDomainEnv——DomainEnv trait 的 Tauri 运行时实现（0.14.6 §2.2）。
//!
//! app 层桥接器：把 `tauri::AppHandle` + managed state 包装成 `DomainEnv` trait，
//! domain 层通过此 trait emit 事件 / 访问 state / 操作窗口，不再直接 `use tauri::`。
//!
//! **生命周期**：
//! 1. `TauriDomainEnv::new(app, pools)` 在 main.rs setup 早期构造
//! 2. 各开放 service 构造后通过 `set_*` 注入
//! 3. `Arc<TauriDomainEnv>` 注册为 Tauri managed state 供 command 层取用

use std::sync::{Arc, OnceLock};

use tauri::{AppHandle, Emitter, Manager};

use crate::domain::ai::chat_service::ChatService;
use crate::domain::capability::policy::{
    ContentEditorRequest, EditorSourceRef, SurfaceError, SurfacePort,
};
use crate::domain::capability::{CapabilityRegistry, ImageStash};
use crate::domain::event::{CapabilityEnv, EventPort};
use crate::domain::plugin::PluginEngine;
use crate::domain::search::SearchService;
use crate::domain::sticky::{
    StickyChangeSource, StickyCloseOutcome, StickyService, StickyWorkflowError,
};
use crate::infra::data::pools::DbPools;
/// Tauri 运行时环境实现（0.21.14 最小 port 拆分）。
///
/// 同时实现 `EventPort`、`CapabilityEnv` 和 `SurfacePort`，但消费者只注入
/// 自身需要的最小 trait。内部持有 `AppHandle` + `DbPools` + 各 service 的 `OnceLock`。
pub struct TauriDomainEnv {
    app: AppHandle,
    db_pools: DbPools,
    cap_registry: OnceLock<Arc<CapabilityRegistry>>,
    plugin_engine: OnceLock<Arc<PluginEngine>>,
    search_service: OnceLock<Arc<SearchService>>,
    chat_service: OnceLock<Arc<ChatService>>,
    /// 进程级图片暂存（0.19.4）——构造时创建，生命周期与 app 相同。
    image_stash: Arc<ImageStash>,
}

impl TauriDomainEnv {
    pub fn new(app: AppHandle, db_pools: DbPools) -> Self {
        // 0.20.7：注册便签兜底关闭回调——CloseRequested 兜底路径降级时调用
        // （WebView 不可用或前端超时 2s 未完成关闭时触发）。
        // 降级只能基于 DB 中已持久化的数据：trash 将便签移入回收站，不声称
        // 保存了前端尚未确认的编辑器正文。infra 层通过回调槽调用，不反向依赖 app/domain。
        crate::infra::platform::window::set_sticky_close_fallback(std::sync::Arc::new(
            |app: &AppHandle, sticky_id: &str| {
                let app = app.clone();
                let sticky_id = sticky_id.to_string();
                tauri::async_runtime::spawn(async move {
                    let Some(env) = app.try_state::<std::sync::Arc<TauriDomainEnv>>() else {
                        tracing::warn!(sticky_id = %sticky_id, "便签关闭时 TauriDomainEnv 不可用，跳过");
                        return;
                    };
                    // 降级路径：基于 DB 已持久化数据 trash 便签
                    if let Err(e) = env.trash_sticky_and_notify(&sticky_id).await {
                        tracing::warn!(error = %e, sticky_id = %sticky_id, "便签兜底关闭失败");
                    }
                });
            },
        ));

        Self {
            app,
            db_pools,
            cap_registry: OnceLock::new(),
            plugin_engine: OnceLock::new(),
            search_service: OnceLock::new(),
            chat_service: OnceLock::new(),
            image_stash: Arc::new(ImageStash::new()),
        }
    }

    /// 注入 CapabilityRegistry（构造后调用）。
    ///
    /// 重复注入不会覆盖首次值，仅记录 `warn`。
    pub fn set_cap_registry(&self, reg: Arc<CapabilityRegistry>) {
        if self.cap_registry.set(reg).is_err() {
            tracing::warn!(
                slot = "cap_registry",
                "重复注入 CapabilityRegistry，已忽略（首次注入优先）"
            );
        }
    }

    /// 注入 PluginEngine（构造后调用）。
    ///
    /// 重复注入不会覆盖首次值，仅记录 `warn`。
    pub fn set_plugin_engine(&self, engine: Arc<PluginEngine>) {
        if self.plugin_engine.set(engine).is_err() {
            tracing::warn!(
                slot = "plugin_engine",
                "重复注入 PluginEngine，已忽略（首次注入优先）"
            );
        }
    }

    /// 注入 SearchService（构造后调用）。
    ///
    /// 重复注入不会覆盖首次值，仅记录 `warn`。
    pub fn set_search_service(&self, svc: Arc<SearchService>) {
        if self.search_service.set(svc).is_err() {
            tracing::warn!(
                slot = "search_service",
                "重复注入 SearchService，已忽略（首次注入优先）"
            );
        }
    }

    /// 注入 ChatService（构造后调用）。
    ///
    /// 重复注入不会覆盖首次值，仅记录 `warn`。
    pub fn set_chat_service(&self, svc: Arc<ChatService>) {
        if self.chat_service.set(svc).is_err() {
            tracing::warn!(
                slot = "chat_service",
                "重复注入 ChatService，已忽略（首次注入优先）"
            );
        }
    }
}

#[async_trait::async_trait]
impl CapabilityEnv for TauriDomainEnv {
    fn db_pools(&self) -> &DbPools {
        &self.db_pools
    }

    fn plugin_engine(&self) -> Option<&Arc<PluginEngine>> {
        self.plugin_engine.get()
    }

    fn search_service(&self) -> Option<&Arc<SearchService>> {
        self.search_service.get()
    }

    async fn list_managed_settings(
        &self,
    ) -> Result<Vec<crate::domain::config::ManagedSetting>, String> {
        Ok(crate::app::setting_service::list_managed_settings(&self.app).await)
    }

    async fn update_managed_setting(
        &self,
        setting_id: &str,
        expected_old_value: serde_json::Value,
        new_value: serde_json::Value,
    ) -> Result<crate::domain::config::ManagedSettingUpdate, String> {
        crate::app::setting_service::update_managed_setting(
            &self.app,
            setting_id,
            expected_old_value,
            new_value,
        )
        .await
    }

    // ── 图片暂存（0.19.4 ImageStash 引用闭环）──────────────────────────

    fn image_stash(&self) -> Option<&Arc<ImageStash>> {
        Some(&self.image_stash)
    }

    // ── 便签窗口操作（0.19.3 从 DomainEnv 提升到 CapabilityEnv）─────────

    fn sticky_service(&self) -> Option<&Arc<StickyService>> {
        use tauri::Manager;
        self.app
            .try_state::<std::sync::Arc<StickyService>>()
            .map(|s| s.inner())
    }

    async fn create_sticky_and_notify(
        &self,
        content: &str,
        color: crate::domain::sticky::StickyColor,
    ) -> Result<crate::domain::sticky::StickyNote, StickyWorkflowError> {
        let svc = self
            .sticky_service()
            .ok_or_else(|| StickyWorkflowError::SideEffect {
                detail: "StickyService 不可用".into(),
            })?;
        let note = svc.create_note(content, color).await?;
        if let Err(error) = self.app.emit(
            crate::domain::event_names::EventNames::STICKY_CREATED,
            serde_json::json!({ "stickyId": note.id }),
        ) {
            tracing::warn!(sticky_id = %note.id, %error, "便签已创建，但创建事件发送失败");
        }
        Ok(note)
    }

    async fn create_sticky_and_show(
        &self,
        content: &str,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<i32>,
        h: Option<i32>,
    ) -> Result<String, String> {
        let svc = self.sticky_service().ok_or("StickyService 不可用")?.clone();
        let note = self
            .create_sticky_and_notify(content, crate::domain::sticky::StickyColor::default())
            .await
            .map_err(|e| e.to_string())?;

        // 应用可选尺寸（None 则用 create_note 的默认值）
        let width = w.unwrap_or(note.width);
        let height = h.unwrap_or(note.height);

        // 位置：有 x/y 则用指定位置，None 则居中到当前前台窗口所在显示器
        let (cx, cy) = match (x, y) {
            (Some(px), Some(py)) => (px, py),
            _ => crate::infra::platform::window::center_of_active_monitor(width, height),
        };

        svc.update_geometry(&note.id, cx, cy, width, height)
            .await
            .map_err(|e| e.to_string())?;

        // 显示桌面窗口（focus=true：用户需能立即输入便签，与 chord 路径行为一致）
        crate::infra::platform::window::show_sticky_window(
            &self.app,
            &note.id,
            cx,
            cy,
            width,
            height,
            note.always_on_top,
            true, // 0.16.11：用户操作需要聚焦
        )?;

        Ok(note.id)
    }

    async fn update_sticky_content_and_notify(
        &self,
        sticky_id: &str,
        content: &str,
        expected_updated_at: Option<i64>,
        source: StickyChangeSource,
    ) -> Result<i64, StickyWorkflowError> {
        let svc = self
            .sticky_service()
            .ok_or_else(|| StickyWorkflowError::SideEffect {
                detail: "StickyService 不可用".into(),
            })?;
        let updated_at = svc
            .update_content(sticky_id, content, expected_updated_at)
            .await?;
        if let Err(error) = self.app.emit(
            crate::domain::event_names::EventNames::STICKY_CONTENT_CHANGED,
            serde_json::json!({
                "stickyId": sticky_id,
                "source": source.as_str(),
                "updatedAt": updated_at,
            }),
        ) {
            // DB 已成功写入，不能把已完成操作伪装成失败；记录同步降级即可。
            tracing::warn!(sticky_id, %error, "便签正文已更新，但变更事件发送失败");
        }
        Ok(updated_at)
    }

    async fn set_sticky_visibility_and_notify(
        &self,
        sticky_id: &str,
        visible: bool,
    ) -> Result<i64, StickyWorkflowError> {
        let svc = self
            .sticky_service()
            .ok_or_else(|| StickyWorkflowError::SideEffect {
                detail: "StickyService 不可用".into(),
            })?;
        let note = svc.get_active_note(sticky_id).await?;
        let updated_at = svc.set_visible(sticky_id, visible).await?;

        let window_result = if visible {
            crate::infra::platform::window::show_sticky_window(
                &self.app,
                sticky_id,
                note.x,
                note.y,
                note.width,
                note.height,
                note.always_on_top,
                false,
            )
        } else {
            crate::infra::platform::window::hide_sticky_window(&self.app, sticky_id)
        };
        let emit_result = self
            .app
            .emit(
                crate::domain::event_names::EventNames::STICKY_VISIBILITY_CHANGED,
                serde_json::json!({ "stickyId": sticky_id, "visible": visible }),
            )
            .map_err(|e| e.to_string());
        match (window_result, emit_result) {
            (Ok(()), Ok(())) => Ok(updated_at),
            (Err(window), Ok(())) => Err(StickyWorkflowError::SideEffect {
                detail: format!("同步便签窗口可见性失败: {window}"),
            }),
            (Ok(()), Err(emit)) => Err(StickyWorkflowError::SideEffect {
                detail: format!("通知便签管理器失败: {emit}"),
            }),
            (Err(window), Err(emit)) => Err(StickyWorkflowError::SideEffect {
                detail: format!("同步便签窗口可见性失败: {window}; 通知便签管理器失败: {emit}"),
            }),
        }
    }

    async fn trash_sticky_and_notify(&self, sticky_id: &str) -> Result<(), StickyWorkflowError> {
        let svc = self
            .sticky_service()
            .ok_or_else(|| StickyWorkflowError::SideEffect {
                detail: "StickyService 不可用".into(),
            })?;
        svc.trash_note(sticky_id).await?;
        let hide_result = crate::infra::platform::window::hide_sticky_window(&self.app, sticky_id);
        let emit_result = self
            .app
            .emit(
                crate::domain::event_names::EventNames::STICKY_TRASHED,
                serde_json::json!({ "stickyId": sticky_id }),
            )
            .map_err(|e| e.to_string());
        match (hide_result, emit_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(hide), Ok(())) => Err(StickyWorkflowError::SideEffect {
                detail: format!("隐藏便签窗口失败: {hide}"),
            }),
            (Ok(()), Err(emit)) => Err(StickyWorkflowError::SideEffect {
                detail: format!("通知便签管理器失败: {emit}"),
            }),
            (Err(hide), Err(emit)) => Err(StickyWorkflowError::SideEffect {
                detail: format!("隐藏便签窗口失败: {hide}; 通知便签管理器失败: {emit}"),
            }),
        }
    }

    async fn close_sticky_and_notify(
        &self,
        sticky_id: &str,
        final_content: &str,
        expected_updated_at: Option<i64>,
    ) -> Result<StickyCloseOutcome, StickyWorkflowError> {
        let svc = self
            .sticky_service()
            .ok_or_else(|| StickyWorkflowError::SideEffect {
                detail: "StickyService 不可用".into(),
            })?;

        // 原子关闭：revision 校验 → 保存最终内容 → delete/trash 决策
        // DB 操作不可逆——成功后 hide/emit 失败不能伪装成"关闭未发生"
        let outcome = svc
            .close_note(sticky_id, final_content, expected_updated_at)
            .await?;

        // 根据结果广播事件 + 隐藏窗口
        // DB 已提交：side effect 失败只记 warn，不回滚已完成操作（spec-backend §4）
        let (event_name, payload) = match outcome {
            StickyCloseOutcome::DeletedEmpty => (
                crate::domain::event_names::EventNames::STICKY_DELETED,
                serde_json::json!({ "stickyId": sticky_id }),
            ),
            StickyCloseOutcome::Trashed => (
                crate::domain::event_names::EventNames::STICKY_TRASHED,
                serde_json::json!({ "stickyId": sticky_id }),
            ),
        };

        let hide_result = crate::infra::platform::window::hide_sticky_window(&self.app, sticky_id);
        let emit_result = self
            .app
            .emit(event_name, payload)
            .map_err(|e| e.to_string());

        // DB 已成功，side effect 失败只记 warn，返回已提交的 outcome
        if let Err(hide) = &hide_result {
            tracing::warn!(
                sticky_id = sticky_id,
                error = %hide,
                "便签已关闭但隐藏窗口失败（DB 已提交，不回滚）"
            );
        }
        if let Err(emit) = &emit_result {
            tracing::warn!(
                sticky_id = sticky_id,
                error = %emit,
                "便签已关闭但通知管理器失败（DB 已提交，不回滚）"
            );
        }

        Ok(outcome)
    }

    // ── pin 窗口操作（0.19.3 pin 能力化桥接）──────────────────────────

    fn show_pin_image(
        &self,
        png_bytes: Vec<u8>,
        x: Option<i32>,
        y: Option<i32>,
    ) -> Result<(i32, i32), String> {
        let (width, height) = crate::infra::platform::screenshot::parse_png_size(&png_bytes)
            .and_then(|(width, height)| {
                Some((i32::try_from(width).ok()?, i32::try_from(height).ok()?))
            })
            .unwrap_or((400, 300));
        let (center_x, center_y) =
            crate::infra::platform::window::get_primary_monitor_center(width, height);
        let position = (x.unwrap_or(center_x), y.unwrap_or(center_y));
        // show_translating 固定 false——仅截图翻译 UI 状态机需要该状态。
        crate::infra::platform::window::show_pin_window(
            &self.app,
            crate::infra::platform::window::PinImage::Png(std::sync::Arc::new(png_bytes)),
            position.0,
            position.1,
            false,
        )?;
        Ok(position)
    }
}

// ── EventPort 实现（0.21.14）─────────────────────────────────────────────
//
// 领域事件发射 port——替代旧 DomainEnv 的 emit/emit_to 方法。

impl EventPort for TauriDomainEnv {
    fn emit(&self, event: &str, payload: serde_json::Value) -> Result<(), String> {
        self.app.emit(event, payload).map_err(|e| e.to_string())
    }

    fn emit_to(&self, target: &str, event: &str, payload: serde_json::Value) -> Result<(), String> {
        self.app
            .emit_to(target, event, payload)
            .map_err(|e| e.to_string())
    }
}

// ── SurfacePort 实现（0.21.1）─────────────────────────────────────────────
//
// TauriDomainEnv 实现 SurfacePort trait，供 GUI starter Capability 通过
// InvokeContext.runtime.surface 调用。Capability 不直接接触窗口方法，只通过最小化端口访问。

#[async_trait::async_trait]
impl SurfacePort for TauriDomainEnv {
    fn open_settings(&self) -> Result<(), SurfaceError> {
        crate::infra::platform::window::open_settings(&self.app);
        Ok(())
    }

    fn open_sticky_manager(&self) -> Result<(), SurfaceError> {
        crate::infra::platform::window::show_sticky_manager_window(&self.app)
            .map_err(|e| SurfaceError::CreateFailed { detail: e })
    }

    fn open_chat(&self, prefill: Option<&str>) -> Result<(), SurfaceError> {
        crate::infra::platform::window::show_chat_window(&self.app, prefill)
            .map_err(|e| SurfaceError::CreateFailed { detail: e })
    }

    fn open_clipboard_mode(&self) -> Result<(), SurfaceError> {
        // 0.21.2：Chord clipboard_history binding 的 GUI starter target。
        // 旧 ClipboardHistoryAction 的行为：主窗 show + emit CHORD_ENTER_MODE
        crate::app::window_orchestrator::invoke(&self.app);
        let _ = self.app.emit(
            crate::domain::event_names::EventNames::CHORD_ENTER_MODE,
            serde_json::json!({ "mode": "clipboard" }),
        );
        Ok(())
    }

    async fn start_region_capture(&self) -> Result<(), SurfaceError> {
        // 0.21.2：Chord screenshot binding 的 GUI starter target。
        // 旧 ScreenshotAction 的截图时序：
        // 1. record_fgHwnd  2. hide_for_screenshot  3. wait_frame_after_hide
        // 4. begin_session  5. unhide_after_screenshot  6. show_screenshot_overlay
        crate::infra::platform::screenshot::record_fg_hwnd();
        crate::infra::platform::window::hide_for_screenshot(&self.app);
        // 等 DWM 合成——用 spawn_blocking 包装同步等待
        let app = self.app.clone();
        tokio::task::spawn_blocking(move || {
            crate::infra::platform::window::wait_frame_after_hide(&app);
        })
        .await
        .map_err(|e| SurfaceError::CreateFailed {
            detail: format!("等待 DWM 合成失败: {e}"),
        })?;

        let meta = tokio::task::spawn_blocking(crate::infra::platform::screenshot::begin_session)
            .await
            .map_err(|e| SurfaceError::CreateFailed {
                detail: format!("截屏任务崩溃: {e}"),
            })?
            .map_err(|e| {
                // 截屏失败也要撤销 cloak
                crate::infra::platform::window::unhide_after_screenshot(&self.app);
                SurfaceError::CreateFailed { detail: e }
            })?;

        crate::infra::platform::window::unhide_after_screenshot(&self.app);
        crate::infra::platform::window::show_screenshot_overlay(&self.app, meta).map_err(|e| {
            crate::infra::platform::screenshot::end_session();
            SurfaceError::CreateFailed { detail: e }
        })?;
        Ok(())
    }

    fn start_image_editor(&self, source: EditorSourceRef) -> Result<(), SurfaceError> {
        match source {
            EditorSourceRef::ClipboardImage(png_data) => {
                let meta = crate::infra::platform::image_editor::begin_session(png_data)
                    .map_err(|e| SurfaceError::CreateFailed { detail: e })?;
                crate::infra::platform::window::show_image_editor_window(
                    &self.app,
                    meta,
                    "clipboard",
                    None,
                )
                .map_err(|e| {
                    crate::infra::platform::image_editor::end_session();
                    SurfaceError::CreateFailed { detail: e }
                })
            }
            EditorSourceRef::StashRef(_) => Err(SurfaceError::Unavailable {
                detail: "StashRef 来源的图片编辑器尚未接入".into(),
            }),
        }
    }

    fn start_content_editor(&self, request: ContentEditorRequest) -> Result<(), SurfaceError> {
        use tauri::Manager;
        let payload = crate::app::commands::EditableContentPayload {
            body: request.body,
            format: "plain".to_string(),
            title: request.title,
            origin: request.origin,
            origin_ref: request.origin_ref,
            save_policy: request.save_policy,
        };
        let pending = self
            .app
            .state::<crate::app::commands::PendingEditorPayload>();
        *pending.0.lock().map_err(|e| SurfaceError::CreateFailed {
            detail: format!("锁失败: {e}"),
        })? = Some(payload);
        crate::infra::platform::window::show_content_editor_window(&self.app)
            .map_err(|e| SurfaceError::CreateFailed { detail: e })
    }

    fn hide_main_window(&self, reason: &str) {
        crate::infra::platform::window::hide(&self.app, reason);
    }

    fn exit_app(&self) {
        self.app.exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// 最小运行时 fake env——不注入 CapabilityRegistry，
    /// 验证 `cap_registry()` 返回 `None` 且不 panic。
    struct FakeDomainEnv {
        pools: DbPools,
    }

    #[async_trait::async_trait]
    impl CapabilityEnv for FakeDomainEnv {
        fn db_pools(&self) -> &DbPools {
            &self.pools
        }
        fn plugin_engine(&self) -> Option<&Arc<PluginEngine>> {
            None
        }
        fn search_service(&self) -> Option<&Arc<SearchService>> {
            None
        }
        async fn list_managed_settings(
            &self,
        ) -> Result<Vec<crate::domain::config::ManagedSetting>, String> {
            Ok(Vec::new())
        }
        async fn update_managed_setting(
            &self,
            setting_id: &str,
            expected_old_value: serde_json::Value,
            new_value: serde_json::Value,
        ) -> Result<crate::domain::config::ManagedSettingUpdate, String> {
            Ok(crate::domain::config::ManagedSettingUpdate {
                setting_id: setting_id.into(),
                old_value: expected_old_value,
                new_value,
                immediately_effective: true,
                requires_restart: false,
            })
        }
        fn image_stash(&self) -> Option<&Arc<ImageStash>> {
            None
        }
        fn sticky_service(&self) -> Option<&Arc<StickyService>> {
            None
        }
        async fn create_sticky_and_notify(
            &self,
            content: &str,
            color: crate::domain::sticky::StickyColor,
        ) -> Result<crate::domain::sticky::StickyNote, StickyWorkflowError> {
            Ok(crate::domain::sticky::StickyNote {
                id: "fake_sticky_id".into(),
                content: content.into(),
                format: crate::domain::sticky::StickyFormat::default(),
                color,
                visible: true,
                x: 0,
                y: 0,
                width: 280,
                height: 320,
                always_on_top: true,
                created_at: 0,
                updated_at: 0,
                trashed: false,
                deleted_at: None,
            })
        }
        async fn create_sticky_and_show(
            &self,
            _content: &str,
            _x: Option<i32>,
            _y: Option<i32>,
            _w: Option<i32>,
            _h: Option<i32>,
        ) -> Result<String, String> {
            Ok("fake_sticky_id".to_string())
        }
        async fn update_sticky_content_and_notify(
            &self,
            _sticky_id: &str,
            _content: &str,
            expected_updated_at: Option<i64>,
            _source: StickyChangeSource,
        ) -> Result<i64, StickyWorkflowError> {
            Ok(expected_updated_at.unwrap_or_default() + 1)
        }
        async fn trash_sticky_and_notify(
            &self,
            _sticky_id: &str,
        ) -> Result<(), StickyWorkflowError> {
            Ok(())
        }

        async fn close_sticky_and_notify(
            &self,
            _sticky_id: &str,
            _final_content: &str,
            _expected_updated_at: Option<i64>,
        ) -> Result<StickyCloseOutcome, StickyWorkflowError> {
            Ok(StickyCloseOutcome::Trashed)
        }

        async fn set_sticky_visibility_and_notify(
            &self,
            _sticky_id: &str,
            _visible: bool,
        ) -> Result<i64, StickyWorkflowError> {
            Ok(0)
        }

        fn show_pin_image(
            &self,
            _png_bytes: Vec<u8>,
            x: Option<i32>,
            y: Option<i32>,
        ) -> Result<(i32, i32), String> {
            Ok((x.unwrap_or(0), y.unwrap_or(0)))
        }
    }

    impl EventPort for FakeDomainEnv {
        fn emit(&self, _event: &str, _payload: serde_json::Value) -> Result<(), String> {
            Ok(())
        }
        fn emit_to(
            &self,
            _target: &str,
            _event: &str,
            _payload: serde_json::Value,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    async fn make_in_memory_pools() -> DbPools {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        DbPools {
            config: pool.clone(),
            history: pool.clone(),
            ai: pool.clone(),
            cache: pool,
        }
    }

    /// 无 CapabilityRegistry 的最小运行时——所有 Option getter 返回 None，不 panic。
    #[tokio::test]
    async fn minimal_env_without_cap_registry() {
        let pools = make_in_memory_pools().await;
        let env = FakeDomainEnv { pools };
        assert!(env.plugin_engine().is_none());
        assert!(env.search_service().is_none());
    }

    /// 0.21.14：验证最小 port 可作为 trait object 使用——消费者只需 `&dyn EventPort`
    /// 或 `&dyn CapabilityEnv`，不需知道具体实现类型。
    #[test]
    fn port_trait_object_coercion() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pools = rt.block_on(make_in_memory_pools());
        let env = FakeDomainEnv { pools };
        let _event_port: &dyn EventPort = &env;
        let _cap_env: &dyn CapabilityEnv = &env;
        // 若编译通过，说明 trait object 转换成功
    }

    /// 验证 set_* 方法的 OnceLock 语义：首次注入成功，二次注入不覆盖。
    #[test]
    fn once_lock_set_does_not_overwrite() {
        let lock: OnceLock<i32> = OnceLock::new();
        assert!(lock.set(1).is_ok());
        assert!(lock.set(2).is_err(), "二次 set 应失败");
        assert_eq!(*lock.get().unwrap(), 1, "首次注入的值应保留");
    }
}
