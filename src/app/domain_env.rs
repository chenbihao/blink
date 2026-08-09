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

use tauri::{AppHandle, Emitter};

use crate::domain::ai::chat_service::ChatService;
use crate::domain::capability::{CapabilityRegistry, ImageStash};
use crate::domain::event::{CapabilityEnv, DomainEnv};
use crate::domain::plugin::PluginEngine;
use crate::domain::search::SearchService;
use crate::domain::sticky::StickyService;
use crate::infra::data::pools::DbPools;
use crate::infra::platform::screenshot::ScreenCaptureMeta;

/// Tauri 运行时实现的 DomainEnv。
///
/// 内部持有 `AppHandle` + `DbPools` + 各 service 的 `OnceLock`。
/// ActionRegistry 刻意不暴露在 DomainEnv 上，避免 AI Capability 反向进入本地执行域。
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

    async fn create_sticky_and_show(
        &self,
        content: &str,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<i32>,
        h: Option<i32>,
    ) -> Result<String, String> {
        use tauri::Emitter;
        let svc = self.sticky_service().ok_or("StickyService 不可用")?.clone();

        // 创建便签（默认黄色、visible=true）
        let note = svc
            .create_note(content, crate::domain::sticky::StickyColor::default())
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

        // emit 事件让管理界面和其它监听者更新
        let _ = self.app.emit(
            crate::domain::event_names::EventNames::STICKY_CREATED,
            serde_json::json!({ "stickyId": note.id }),
        );
        Ok(note.id)
    }

    fn hide_sticky_and_notify_trashed(&self, sticky_id: &str) -> Result<(), String> {
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
            (Err(hide), Ok(())) => Err(format!("隐藏便签窗口失败: {hide}")),
            (Ok(()), Err(emit)) => Err(format!("通知便签管理器失败: {emit}")),
            (Err(hide), Err(emit)) => Err(format!(
                "隐藏便签窗口失败: {hide}; 通知便签管理器失败: {emit}"
            )),
        }
    }

    // ── pin 窗口操作（0.19.3 pin 能力化桥接）──────────────────────────

    fn show_pin_window(&self, png_bytes: Vec<u8>, x: i32, y: i32) -> Result<(), String> {
        // show_translating 固定 false——对 AI pin 场景无意义
        crate::infra::platform::window::show_pin_window(&self.app, png_bytes, x, y, false)
    }
}

#[async_trait::async_trait]
impl DomainEnv for TauriDomainEnv {
    fn capability_env(&self) -> &dyn CapabilityEnv {
        self
    }

    // ── 事件发射 ──────────────────────────────────────────────────────────

    fn emit(&self, event: &str, payload: serde_json::Value) -> Result<(), String> {
        self.app.emit(event, payload).map_err(|e| e.to_string())
    }

    fn emit_to(&self, target: &str, event: &str, payload: serde_json::Value) -> Result<(), String> {
        self.app
            .emit_to(target, event, payload)
            .map_err(|e| e.to_string())
    }

    // ── 状态访问 ──────────────────────────────────────────────────────────

    fn cap_registry(&self) -> Option<&Arc<CapabilityRegistry>> {
        self.cap_registry.get()
    }

    fn chat_service(&self) -> Option<&Arc<ChatService>> {
        self.chat_service.get()
    }

    // ── 窗口操作 ──────────────────────────────────────────────────────────

    fn show_chat_window(&self, initial_text: Option<&str>) -> Result<(), String> {
        crate::infra::platform::window::show_chat_window(&self.app, initial_text)
    }

    fn hide_main_window(&self, reason: &str) {
        crate::infra::platform::window::hide(&self.app, reason);
    }

    fn hide_for_screenshot(&self) {
        crate::infra::platform::window::hide_for_screenshot(&self.app);
    }

    fn unhide_after_screenshot(&self) {
        crate::infra::platform::window::unhide_after_screenshot(&self.app);
    }

    fn show_screenshot_overlay(&self, meta: &ScreenCaptureMeta) -> Result<(), String> {
        crate::infra::platform::window::show_screenshot_overlay(&self.app, meta.clone())
    }

    fn invoke_main_window(&self) {
        crate::infra::platform::window::invoke(&self.app);
    }

    fn open_settings(&self) {
        crate::infra::platform::window::open_settings(&self.app);
    }

    fn show_sticky_manager(&self) -> Result<(), String> {
        crate::infra::platform::window::show_sticky_manager_window(&self.app)
    }

    fn show_content_editor(
        &self,
        body: &str,
        title: Option<&str>,
        origin: &str,
        origin_ref: Option<&str>,
        save_policy: &str,
    ) -> Result<(), String> {
        use tauri::Manager;
        let payload = crate::app::commands::EditableContentPayload {
            body: body.to_string(),
            format: "plain".to_string(),
            title: title.map(|s| s.to_string()),
            origin: origin.to_string(),
            origin_ref: origin_ref.map(|s| s.to_string()),
            save_policy: save_policy.to_string(),
        };
        let pending = self
            .app
            .state::<crate::app::commands::PendingEditorPayload>();
        *pending.0.lock().map_err(|e| format!("锁失败: {e}"))? = Some(payload);
        crate::infra::platform::window::show_content_editor_window(&self.app)
    }

    fn exit_app(&self) {
        self.app.exit(0);
    }

    async fn wait_frame_after_hide(&self) {
        let app = self.app.clone();
        tokio::task::spawn_blocking(move || {
            crate::infra::platform::window::wait_frame_after_hide(&app);
        })
        .await
        .ok();
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
        fn image_stash(&self) -> Option<&Arc<ImageStash>> {
            None
        }
        fn sticky_service(&self) -> Option<&Arc<StickyService>> {
            None
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
        fn hide_sticky_and_notify_trashed(&self, _sticky_id: &str) -> Result<(), String> {
            Ok(())
        }

        fn show_pin_window(&self, _png_bytes: Vec<u8>, _x: i32, _y: i32) -> Result<(), String> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl DomainEnv for FakeDomainEnv {
        fn capability_env(&self) -> &dyn CapabilityEnv {
            self
        }
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
        fn cap_registry(&self) -> Option<&Arc<CapabilityRegistry>> {
            None // 关键：最小运行时不构造 CapabilityRegistry
        }
        fn chat_service(&self) -> Option<&Arc<ChatService>> {
            None
        }
        fn show_chat_window(&self, _initial_text: Option<&str>) -> Result<(), String> {
            Ok(())
        }
        fn hide_main_window(&self, _reason: &str) {}
        fn hide_for_screenshot(&self) {}
        fn unhide_after_screenshot(&self) {}
        fn show_screenshot_overlay(&self, _meta: &ScreenCaptureMeta) -> Result<(), String> {
            Ok(())
        }
        fn invoke_main_window(&self) {}
        fn open_settings(&self) {}
        fn show_sticky_manager(&self) -> Result<(), String> {
            Ok(())
        }
        fn show_content_editor(
            &self,
            _body: &str,
            _title: Option<&str>,
            _origin: &str,
            _origin_ref: Option<&str>,
            _save_policy: &str,
        ) -> Result<(), String> {
            Ok(())
        }
        fn exit_app(&self) {}
        async fn wait_frame_after_hide(&self) {}
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
        assert!(
            env.cap_registry().is_none(),
            "最小运行时 cap_registry 应返回 None"
        );
        assert!(env.plugin_engine().is_none());
        assert!(env.search_service().is_none());
        assert!(env.chat_service().is_none());
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
