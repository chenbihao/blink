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
    ///
    /// 重复注入不会覆盖首次值，仅记录 `warn`。
    pub fn set_cap_registry(&self, reg: Arc<CapabilityRegistry>) {
        if self.cap_registry.set(reg).is_err() {
            tracing::warn!(slot = "cap_registry", "重复注入 CapabilityRegistry，已忽略（首次注入优先）");
        }
    }

    /// 注入 PluginEngine（构造后调用）。
    ///
    /// 重复注入不会覆盖首次值，仅记录 `warn`。
    pub fn set_plugin_engine(&self, engine: Arc<PluginEngine>) {
        if self.plugin_engine.set(engine).is_err() {
            tracing::warn!(slot = "plugin_engine", "重复注入 PluginEngine，已忽略（首次注入优先）");
        }
    }

    /// 注入 SearchService（构造后调用）。
    ///
    /// 重复注入不会覆盖首次值，仅记录 `warn`。
    pub fn set_search_service(&self, svc: Arc<SearchService>) {
        if self.search_service.set(svc).is_err() {
            tracing::warn!(slot = "search_service", "重复注入 SearchService，已忽略（首次注入优先）");
        }
    }

    /// 注入 ChatService（构造后调用）。
    ///
    /// 重复注入不会覆盖首次值，仅记录 `warn`。
    pub fn set_chat_service(&self, svc: Arc<ChatService>) {
        if self.chat_service.set(svc).is_err() {
            tracing::warn!(slot = "chat_service", "重复注入 ChatService，已忽略（首次注入优先）");
        }
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

    fn cap_registry(&self) -> Option<&Arc<CapabilityRegistry>> {
        self.cap_registry.get()
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// 最小运行时 fake env——不注入 CapabilityRegistry，
    /// 验证 `cap_registry()` 返回 `None` 且不 panic。
    struct FakeDomainEnv {
        pools: DbPools,
    }

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
        fn show_chat_window(&self) -> Result<(), String> {
            Ok(())
        }
        fn hide_main_window(&self, _reason: &str) {}
        fn hide_for_screenshot(&self) {}
        fn unhide_after_screenshot(&self) {}
        fn show_screenshot_overlay(
            &self,
            _meta: &ScreenCaptureMeta,
        ) -> Result<(), String> {
            Ok(())
        }
        fn invoke_main_window(&self) {}
        fn open_settings(&self) {}
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
