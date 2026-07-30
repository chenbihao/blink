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
use crate::domain::capability::CapabilityRegistry;
use crate::domain::event::{CapabilityEnv, DomainEnv};
use crate::domain::plugin::PluginEngine;
use crate::domain::search::SearchService;
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
        }
    }

    /// 注入 CapabilityRegistry（构造后调用）。
    pub fn set_cap_registry(&self, reg: Arc<CapabilityRegistry>) {
        let _ = self.cap_registry.set(reg);
    }

    /// 注入 PluginEngine（构造后调用）。
    pub fn set_plugin_engine(&self, engine: Arc<PluginEngine>) {
        let _ = self.plugin_engine.set(engine);
    }

    /// 注入 SearchService（构造后调用）。
    pub fn set_search_service(&self, svc: Arc<SearchService>) {
        let _ = self.search_service.set(svc);
    }

    /// 注入 ChatService（构造后调用）。
    pub fn set_chat_service(&self, svc: Arc<ChatService>) {
        let _ = self.chat_service.set(svc);
    }
}

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

    fn cap_registry(&self) -> &Arc<CapabilityRegistry> {
        self.cap_registry
            .get()
            .expect("CapabilityRegistry not set — call set_cap_registry() after construction")
    }

    fn chat_service(&self) -> Option<&Arc<ChatService>> {
        self.chat_service.get()
    }

    // ── 窗口操作 ──────────────────────────────────────────────────────────

    fn show_chat_window(&self) -> Result<(), String> {
        crate::infra::platform::window::show_chat_window(&self.app)
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
