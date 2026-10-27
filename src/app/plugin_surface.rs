//! 插件独立窗口编排（0.28.2 §五.1-2）——宿主管理的插件页面窗口。
//!
//! 职责（app 层编排，窗口 primitive 复用 infra `get_or_create_window`）：
//! - 以 `plugin-{sanitized-id}` 为逻辑窗口身份：同一插件一个主窗口，重复唤起
//!   show/focus 复用，**不随安装数预热**（spec-frontend §1.4 动态窗口例外）；
//! - 会话绑定：`(label → plugin_id/page_id/handle)`；页面 command（plugin_ui.rs）
//!   以**调用窗口的 label** 反查会话——插件页面无法冒用其他 plugin_id（旧窗口/
//!   其他窗口直构造越权 IPC 被拒）；
//! - 初始 payload ready/pull：建窗时暂存，页面加载完成后 `plugin_page_ready` 取走；
//!   复用窗口的唤起载荷走会话 Channel（未 ready 时落回 pending，不丢失）；
//!   每次唤起必派发一次——无载荷时派发 null（空唤起，页面重置为新会话）；
//! - 关闭 = 隐藏保留会话（prevent_close + hide，应用退出放行）；禁用/更新/卸载
//!   由生命周期钩子调 [`close_plugin_windows`] 真销毁（撤销旧页面会话）；
//! - 窗口预算 [`MAX_PLUGIN_WINDOWS`]：超出时优先回收隐藏且最久未开的窗口，
//!   无可回收则明确报错——不随安装数无界增长（phase §3.2）。
//!
//! 空闲自动回收（含草稿迁移）0.28 首版不做：隐藏窗口保活 WebView（每窗
//! ~10-20MB，预算内），翻译正文等草稿仅存于页面内存——重启不恢复（phase
//! §3.2「重启默认不保存翻译正文」）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use blink_infra::platform::window;

use crate::domain::plugin::{PluginEngine, PluginHandle};

/// 同类插件窗口数量预算（含隐藏保活的复用窗口）。
pub const MAX_PLUGIN_WINDOWS: usize = 6;

/// 插件窗口 label 前缀（capabilities/plugin-windows.json 的 `plugin-*` 通配）。
const LABEL_PREFIX: &str = "plugin-";

/// 单个插件页面会话。`handle` 参与 0.28.1 的代次复核——更新发布后旧句柄
/// 失活，窗口被关闭重建，旧页面不能继续以旧代次身份调用。
struct PluginPageSession {
    plugin_id: String,
    page_id: String,
    handle: Arc<PluginHandle>,
    pending_payload: Option<serde_json::Value>,
    payload_channel: Option<tauri::ipc::Channel<serde_json::Value>>,
    opened_at: std::time::Instant,
}

/// 插件窗口面（managed state；label → 会话）。
#[derive(Default)]
pub struct PluginSurface {
    sessions: Mutex<HashMap<String, PluginPageSession>>,
    open_locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    reservation_lock: Mutex<()>,
}

impl PluginSurface {
    pub fn new() -> Self {
        Self::default()
    }

    fn open_lock(&self, label: &str) -> Arc<Mutex<()>> {
        let mut locks = self.open_locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(label).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(label.to_string(), Arc::downgrade(&lock));
        lock
    }

    fn with_sessions<R>(&self, f: impl FnOnce(&mut HashMap<String, PluginPageSession>) -> R) -> R {
        let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    /// 按 label 取会话；窗口已被销毁的陈旧会话顺带清理（懒回收）。
    fn session(&self, app: &AppHandle, label: &str) -> Option<(String, String, Arc<PluginHandle>)> {
        let stale = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            match sessions.get(label) {
                Some(_) => app.get_webview_window(label).is_none(),
                None => false,
            }
        };
        if stale {
            self.with_sessions(|s| {
                s.remove(label);
            });
            tracing::debug!(label, "插件窗口会话陈旧（窗口已销毁），已清理");
            return None;
        }
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.get(label).map(|s| {
            (
                s.plugin_id.clone(),
                s.page_id.clone(),
                s.handle.clone(),
            )
        })
    }

    /// 页面 ready：取走初始 payload，后续仅通过本会话的 Channel 交付。
    fn take_ready_payload(&self, label: &str, channel: tauri::ipc::Channel<serde_json::Value>) -> Option<serde_json::Value> {
        self.with_sessions(|sessions| {
            let s = sessions.get_mut(label)?;
            s.payload_channel = Some(channel);
            s.pending_payload.take()
        })
    }

    /// 复用窗口的新 payload：就绪即交付，未 ready 落 pending（保留最新）。
    fn deliver_payload(&self, label: &str, payload: serde_json::Value) {
        self.with_sessions(|sessions| {
            let Some(s) = sessions.get_mut(label) else {
                return;
            };
            s.pending_payload = Some(payload.clone());
            if let Some(channel) = &s.payload_channel
                && channel.send(payload).is_ok()
            {
                s.pending_payload = None;
            }
        });
    }

    /// 关闭（销毁）某插件的全部窗口并清会话。禁用/更新/卸载钩子调用。
    fn close_plugin(&self, app: &AppHandle, plugin_id: &str) -> usize {
        let labels: Vec<String> = self.with_sessions(|sessions| {
            sessions
                .iter()
                .filter(|(_, s)| s.plugin_id == plugin_id)
                .map(|(label, _)| label.clone())
                .collect()
        });
        for label in &labels {
            if let Some(win) = app.get_webview_window(label) {
                let _ = win.destroy();
            }
            self.with_sessions(|s| {
                s.remove(label);
            });
        }
        if !labels.is_empty() {
            tracing::info!(plugin = %plugin_id, count = labels.len(), "插件窗口已关闭（生命周期撤销）");
        }
        labels.len()
    }

    /// 预算检查 + 回收：新建前调用。返回 Err = 明确拒绝（无可回收的隐藏窗口）。
    fn enforce_budget(&self, app: &AppHandle) -> Result<(), String> {
        let count = self.with_sessions(|s| s.len());
        if count < MAX_PLUGIN_WINDOWS {
            return Ok(());
        }
        // 优先回收隐藏且最久未开的窗口；可见窗口（可能持有未交付会话）不抢关
        let victim: Option<String> = self.with_sessions(|sessions| {
            sessions
                .iter()
                .filter(|(label, _)| {
                    app.get_webview_window(label)
                        .is_some_and(|w| !w.is_visible().unwrap_or(true))
                })
                .min_by_key(|(_, s)| s.opened_at)
                .map(|(label, _)| label.clone())
        });
        match victim {
            Some(label) => {
                tracing::info!(label, "插件窗口数达预算，回收最久的隐藏窗口");
                if let Some(win) = app.get_webview_window(&label) {
                    let _ = win.destroy();
                }
                self.with_sessions(|s| {
                    s.remove(&label);
                });
                Ok(())
            }
            None => Err(format!(
                "插件窗口已达上限（{MAX_PLUGIN_WINDOWS}），请先关闭其他插件窗口"
            )),
        }
    }
}

/// plugin_id → 窗口 label（Tauri label 仅允许字母数字/-；`.`/`_` 折叠为 `-`）。
/// 不同 plugin_id 可能折叠到同一 label——由 open 时的会话归属检查兜住（显式报错）。
pub fn plugin_window_label(plugin_id: &str) -> String {
    let sanitized: String = plugin_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    format!("{LABEL_PREFIX}{sanitized}")
}

/// 打开（或复用）插件主窗口。宿主入口——设置页/主窗调用，不受页面 label 校验。
///
/// `page_id: None` = manifest 首个页面。payload 为初始/唤起载荷（如待翻译文本）。
pub fn open_plugin_page(
    app: &AppHandle,
    plugin_id: &str,
    page_id: Option<&str>,
    payload: Option<serde_json::Value>,
) -> Result<(), String> {
    let engine = app
        .state::<Arc<PluginEngine>>()
        .inner()
        .clone();
    let handle = engine
        .plugin_set()
        .plugins
        .iter()
        .find(|p| p.id() == plugin_id)
        .cloned()
        .ok_or_else(|| format!("插件未加载: {plugin_id}"))?;
    if !engine.is_enabled(plugin_id) {
        return Err("插件已禁用，启用后再打开".into());
    }
    // 立即克隆页面 id/entry，避免 manifest 借用跨到后续 handle move
    let (page_id, page_entry) = {
        let manifest = handle.manifest();
        let page = match page_id {
            Some(id) => manifest
                .pages
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| format!("插件 {plugin_id} 没有页面 {id}"))?,
            None => manifest
                .pages
                .first()
                .ok_or_else(|| format!("插件 {plugin_id} 没有声明页面"))?,
        };
        (page.id.clone(), page.entry.clone())
    };
    // 入口存在性 + 越界校验（安装期已验过；打开时复核防包目录被改动）
    blink_infra::plugin_storage::pages::validate_entry(handle.package_dir(), &page_entry)
        .map_err(|e| format!("页面入口不可用: {e}"))?;

    let label = plugin_window_label(plugin_id);
    let surface = app.state::<Arc<PluginSurface>>().inner().clone();
    let opening = surface.open_lock(&label);
    let _opening = opening.lock().unwrap_or_else(|e| e.into_inner());

    // 复用路径：窗口存在且会话归属一致、句柄仍活跃
    if let Some(win) = app.get_webview_window(&label) {
        let reuse = match surface.session(app, &label) {
            Some((owner, current_page, session_handle)) => {
                if owner != plugin_id {
                    return Err(format!(
                        "窗口 label 冲突：{label} 已被插件 {owner} 占用"
                    ));
                }
                if current_page != page_id || !engine.is_active_handle(plugin_id, &session_handle) {
                    // 更新/替换后旧句柄失活——关旧窗重建（撤销旧页面会话）
                    tracing::info!(plugin = %plugin_id, "插件窗口句柄已失活，关闭重建");
                    let _ = win.destroy();
                    surface.with_sessions(|s| {
                        s.remove(&label);
                    });
                    false
                } else {
                    true
                }
            }
            None => {
                // 窗口存在但无会话（异常残留）——销毁重建，避免身份不明
                let _ = win.destroy();
                false
            }
        };
        if reuse {
            surface.with_sessions(|sessions| {
                if let Some(session) = sessions.get_mut(&label) {
                    session.opened_at = std::time::Instant::now();
                }
            });
            // 0.28.10：每次唤起都派发一次载荷——有 payload 即预填，无 payload
            // 派发 null（空唤起信号，页面据此重置为新会话，不遗留旧查询文本）。
            surface.deliver_payload(&label, payload.unwrap_or(serde_json::Value::Null));
            let _ = win.unminimize();
            win.show().map_err(|e| format!("显示插件窗口失败: {e}"))?;
            window::clamp_to_work_area(&win);
            win.set_focus()
                .map_err(|e| format!("聚焦插件窗口失败: {e}"))?;
            return Ok(());
        }
    }

    // 会话先占位后建窗：label 冲突/建窗失败都清理，不留半套状态
    let plugin_segment = encode_path_segment(plugin_id);
    let entry_path = entry_url_path(&page_entry);
    // CustomProtocol 形式：Tauri 按平台改写（Windows → http://blink-plugin.localhost/）
    let url = format!("blink-plugin://localhost/{plugin_segment}/{entry_path}");
    let url: tauri::Url = url.parse().map_err(|e| format!("页面 URL 非法: {e}"))?;
    let title = engine.get_display_name(plugin_id);
    let allow_prefix = format!("/{plugin_segment}/");
    let build_label = label.clone();
    let build_app = app.clone();
    let build = move || {
        let mut builder = WebviewWindowBuilder::new(
            &build_app,
            build_label.clone(),
            WebviewUrl::CustomProtocol(url.clone()),
        )
            .title(title.clone())
            .inner_size(460.0, 540.0)
            .min_inner_size(360.0, 320.0)
            .decorations(true)
            .transparent(false)
            .always_on_top(false)
            .skip_taskbar(false)
            .resizable(true)
            .focused(true)
            .visible(true)
            .background_color(tauri::window::Color(51, 51, 51, 255))
            .center();
        // 导航边界：仅允许本插件资源与宿主 @host 资源；外部跳转/另开页面一律拒绝。
        // Windows 上自定义协议被改写为 http://blink-plugin.localhost/，两种形态都放行。
        builder = builder.on_navigation(move |nav| {
            let host_ok = (nav.scheme() == "http" && nav.host_str() == Some("blink-plugin.localhost"))
                || (nav.scheme() == "blink-plugin" && nav.host_str() == Some("localhost"));
            let path_ok =
                nav.path().starts_with(&allow_prefix) || nav.path().starts_with("/@host/");
            let ok = host_ok && path_ok;
            if !ok {
                tracing::warn!(url = %nav, "插件窗口导航被拒绝");
            }
            ok
        });
        builder.build()
    };
    // build 期间前端就可能发 ready；必须先建立权威会话，失败再清理。
    let active_handle = handle.clone();
    {
        // 全局锁仅保护预算预留；各 label 的 build 仍可并行。
        let _reservation = surface.reservation_lock.lock().unwrap_or_else(|e| e.into_inner());
        surface.enforce_budget(app)?;
        surface.with_sessions(|sessions| {
            sessions.insert(label.clone(), PluginPageSession {
                plugin_id: plugin_id.to_string(),
                page_id: page_id.clone(),
                handle,
                pending_payload: payload,
                payload_channel: None,
                opened_at: std::time::Instant::now(),
            });
        });
    }
    let (win, _created) = match window::get_or_create_window(app, &label, build) {
        Ok(created) => created,
        Err(error) => {
            surface.with_sessions(|sessions| { sessions.remove(&label); });
            return Err(error);
        }
    };
    if !engine.is_active_handle(plugin_id, &active_handle) || !engine.is_enabled(plugin_id) {
        let _ = win.destroy();
        surface.with_sessions(|sessions| { sessions.remove(&label); });
        return Err("插件已更新、禁用或卸载，请重新打开".into());
    }

    // 关闭 = 隐藏保留（应用退出放行，与 sticky 复用模式一致）
    {
        let app_clone = app.clone();
        let label_for_close = label.clone();
        win.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if !window::is_app_exiting() {
                    api.prevent_close();
                    if let Some(w) = app_clone.get_webview_window(&label_for_close) {
                        let _ = w.hide();
                    }
                }
            }
        });
    }
    if let Ok(hwnd) = win.hwnd() {
        let hwnd = windows::Win32::Foundation::HWND(hwnd.0 as _);
        window::install_sysmenu_blocker(hwnd);
        window::enable_rounded_corners(hwnd);
    }

    tracing::info!(plugin = %plugin_id, page = %page_id, label = %label, "插件窗口已创建");
    Ok(())
}

/// 生命周期钩子：关闭某插件全部窗口（禁用/更新/卸载时 commands 层调用）。
pub fn close_plugin_windows(app: &AppHandle, plugin_id: &str) {
    if let Some(surface) = app.try_state::<Arc<PluginSurface>>() {
        surface.inner().close_plugin(app, plugin_id);
    }
}

/// 页面就绪：取初始 payload（plugin_ui command 桥接）。
pub(crate) fn take_ready_payload(
    surface: &PluginSurface,
    label: &str,
    channel: tauri::ipc::Channel<serde_json::Value>,
) -> Option<serde_json::Value> {
    surface.take_ready_payload(label, channel)
}

/// 校验调用窗口身份并返回 (plugin_id, page_id, handle)——plugin_ui 全部页面
/// command 的公共门禁。调用方传入 Tauri 注入的窗口（非页面自报参数）。
pub(crate) fn page_session(
    surface: &PluginSurface,
    app: &AppHandle,
    window: &tauri::WebviewWindow,
    engine: &PluginEngine,
    claimed_plugin_id: &str,
) -> Result<(String, String, Arc<PluginHandle>), String> {
    let label = window.label();
    if !label.starts_with(LABEL_PREFIX) {
        return Err(format!("非插件窗口不得调用页面接口: {label}"));
    }
    let (plugin_id, page_id, handle) = surface
        .session(app, label)
        .ok_or_else(|| "插件窗口会话不存在".to_string())?;
    // 冒用拒绝：页面自报的 plugin_id 与窗口会话归属不一致
    if plugin_id != claimed_plugin_id {
        return Err(format!(
            "窗口身份不匹配：本窗口属于 {plugin_id}，不能以 {claimed_plugin_id} 身份调用"
        ));
    }
    // 代次复核：句柄失活（更新/卸载后）拒绝——旧代次不继承新出口
    if !engine.is_active_handle(&plugin_id, &handle) {
        return Err("插件已更新或卸载，本窗口会话已失效".into());
    }
    Ok((plugin_id, page_id, handle))
}

pub(crate) fn surface_state(app: &AppHandle) -> Arc<PluginSurface> {
    app.state::<Arc<PluginSurface>>().inner().clone()
}

/// URL 路径段编码：plugin id 中的 `.` 保留（路径合法字符），仅编码不安全字符。
/// plugin id 受 contract 字符集约束（字母数字/./_/-），此处防御性编码空格等。
fn encode_path_segment(segment: &str) -> String {
    segment
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '/' => c.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

/// 页面入口的 URL 路径：反斜杠归一为 `/`（资源引用同根相对路径）。
fn entry_url_path(entry: &str) -> String {
    entry.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_sanitizes_dot_and_underscore() {
        assert_eq!(
            plugin_window_label("builtin.translate"),
            "plugin-builtin-translate"
        );
        assert_eq!(plugin_window_label("a_b"), "plugin-a-b");
    }

    #[test]
    fn url_segments_keep_plugin_id_readable() {
        assert_eq!(encode_path_segment("builtin.ip"), "builtin.ip");
        assert_eq!(encode_path_segment("a b"), "a%20b");
        assert_eq!(entry_url_path("web\\index.html"), "web/index.html");
    }
}
