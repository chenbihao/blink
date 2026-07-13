//! 剪贴板历史监听（0.8.5）：AddClipboardFormatListener 隐藏窗口 → WM_CLIPBOARDUPDATE → 存。
//!
//! **架构（低耦合，仿 selection 范式）**：监听器只依赖 `data::clipboard`（存）+
//! `config::ClipboardConfig`（配置）。不持有 AppHandle、不 emit 事件、不调 domain/commands。
//! 前端读 db（`get_clipboard_history` command）与监听器完全解耦——监听器只管写，
//! 前端只管读，两者不直接对接。
//!
//! **0.9.2.1 补丁**：为了修主窗口保持打开时剪贴板变化 AwarenessSnapshot 不刷新的 bug,
//! 引入 `set_change_hook()` 注册一个泛型回调——listener 侧仍不认 SearchService/domain,
//! 只在剪贴板文本通过 title 黑名单 + 去重后同步调 hook。调用侧（`main.rs`）负责
//! ContextConfig 门控 + 回写 SearchService。listener 架构解耦精神保持。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

use sqlx::SqlitePool;

use crate::infra::data::clipboard::ClipboardConfig;

#[cfg(target_os = "windows")]
mod windows;

pub(super) struct State {
    pool: SqlitePool,
    blacklist: RwLock<Vec<String>>,
    max_items: u32,
}

static STATE: OnceLock<State> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// 剪贴板文本变化的观察者回调（0.9.2.1）。
///
/// **契约**：只在 title 黑名单过滤 + 短窗口去重 + 非空文本三关都过后触发。
/// 参数是刚入库的文本引用——回调应尽快返回,不要阻塞监听线程（内部持有 clipboard
/// 消息循环）。跨 send 边界用 `Fn + Send + Sync`。
pub type ChangeHook = Box<dyn Fn(&str) + Send + Sync + 'static>;
static CHANGE_HOOK: OnceLock<ChangeHook> = OnceLock::new();

/// 启动剪贴板监听（幂等）。监听线程持有 pool + cfg，WM_CLIPBOARDUPDATE 时存。
/// 仿 selection：监听窗口一旦创建不卸，关闭态靠 ACTIVE 短路（跨线程卸载不安全）。
pub fn start_listener(pool: SqlitePool, cfg: ClipboardConfig) {
    let _ = STATE.set(State {
        pool,
        blacklist: RwLock::new(cfg.blacklist_keywords.clone()),
        max_items: cfg.max_items,
    });
    ACTIVE.store(cfg.enabled, Ordering::Relaxed);
    #[cfg(target_os = "windows")]
    {
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(windows::start_watcher_thread);
    }
    tracing::debug!(enabled = cfg.enabled, "剪贴板监听已就绪");
}

/// 热切换开关（0.8.7 起 `update_clipboard_enabled` 会调它做真热切）。
pub fn set_active(active: bool) {
    ACTIVE.store(active, Ordering::Relaxed);
    tracing::debug!(active, "剪贴板监听 active 切换");
}

/// 热更新黑名单（设置页改调）。
#[allow(dead_code)] // 设置页 API 预留（当前 commands 层直接更新 config）
pub fn set_blacklist(keywords: Vec<String>) {
    if let Some(s) = STATE.get() {
        *s.blacklist.write().unwrap() = keywords;
    }
}

#[cfg(target_os = "windows")]
pub(super) fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

#[cfg(target_os = "windows")]
pub(super) fn state() -> Option<&'static State> {
    STATE.get()
}

/// 注册剪贴板文本变化的观察者回调（0.9.2.1，一次性，OnceLock 兜底避免重复注册）。
///
/// **调用时机**：`main.rs::setup` 里在 SearchService 已构造 + AppHandle 可用后调用。
/// 回调闭包需自行 clone `Arc<SearchService>` 并持有 `AppHandle`。
///
/// **重复调用**：静默忽略（OnceLock 语义）；测试场景先 `hook` 后 `start_listener` 也 OK。
pub fn set_change_hook(hook: ChangeHook) {
    if CHANGE_HOOK.set(hook).is_err() {
        tracing::debug!("剪贴板 change hook 已注册过,忽略后续注册");
    }
}

/// 内部触发（windows.rs 调用）——非空文本 + 已入库策略后触发一次。
#[cfg(target_os = "windows")]
pub(super) fn notify_change(text: &str) {
    if let Some(hook) = CHANGE_HOOK.get() {
        hook(text);
    }
}

/// 把 **BGRA** 像素数据写入系统剪贴板（CF_DIB 格式）。
///
/// `pixels` 格式：BGRA、top-down、每行 `width * 4` 字节（BitBlt 原生输出即此格式）。
/// 内部只做 top-down → bottom-up 翻转，不做 R↔B swap，省掉全屏 shuffle 一次。
/// 写入后其他应用（画图/PPT/微信）可直接 Ctrl+V 粘贴。
#[cfg(target_os = "windows")]
pub fn write_bgra_to_clipboard(pixels: &[u8], width: u32, height: u32) -> Result<(), String> {
    windows::write_bgra_to_clipboard(pixels, width, height)
}

/// 读当前剪贴板文本（0.9.7 read_clipboard Capability）。
///
/// 返回 `Some(text)` = 文本剪贴板；`None` = 空/非文本（图片/文件列表）。
/// 含短重试（与监听器同一逻辑），读不到返回 None 不报错。
#[cfg(target_os = "windows")]
pub fn read_current_text() -> Option<String> {
    windows::read_current_text()
}

/// 把文本写入系统剪贴板（CF_UNICODETEXT 格式）（0.9.7 write_clipboard Capability）。
#[cfg(target_os = "windows")]
pub fn write_text_to_clipboard(text: &str) -> Result<(), String> {
    windows::write_text_to_clipboard(text)
}
