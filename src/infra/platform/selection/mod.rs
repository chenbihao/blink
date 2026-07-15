//! 选中文本感知（0.8.0 §1.1）：抓取鼠标选中文本。
//!
//! 两条路径：
//! 1. **划词监听**（主）：全局鼠标钩子在「划词瞬间」抓取（焦点未失、选区未退化），
//!    缓存最近选区，绕开 Electron 应用失焦退化问题。见 `listener.rs`。
//! 2. **get_selected_text**：纯 UIA 抓取原语（按 HWND），供划词监听调用。见 `windows.rs`。
//!    三段式策略：`GetFocusedElement`(O(1)) → 祖先链 `FindFirst(Ancestors)`(O(深度))
//!    → 焦点子树 `FindFirst(Descendants)`(O(焦点子树))。无 FindAll 全树回退。
//!    带计时日志与阈值告警。
//!
//! 缓存独立于 SearchService（避免 infra→domain 反向依赖），由 `window::invoke` 合并进 snapshot。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::Instant;

// ── UIA 抓取原语 ──────────────────────────────────────────────

/// 抓取指定窗口当前的鼠标选区文本。
///
/// `hwnd_raw` 为窗口句柄原始值，0 视为无效。
/// 返回 None 表示：无选区、应用不支持 UIA TextPattern、或抓取任意环节失败。
/// 调用方应在后台线程中调用——UIA 是跨进程调用，单次几十 ms。
#[cfg(target_os = "windows")]
pub fn get_selected_text(hwnd_raw: isize) -> Option<String> {
    self::windows::get_selected_text(hwnd_raw)
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub fn get_selected_text(_hwnd_raw: isize) -> Option<String> {
    None
}

// ── 划词缓存 ──────────────────────────────────────────────────
//
// 划词监听在「黄金时机」抓到的选区缓存于此；invoke 唤起时读取。

struct SelectionCache {
    text: Option<String>,
    at: Option<Instant>,
}

static CACHE: OnceLock<RwLock<SelectionCache>> = OnceLock::new();

fn cache() -> &'static RwLock<SelectionCache> {
    CACHE.get_or_init(|| {
        RwLock::new(SelectionCache {
            text: None,
            at: None,
        })
    })
}

/// 缓存最近选区（划词抓取线程调用）。
pub(crate) fn set_last_selection(text: String) {
    let mut g = cache().write().unwrap();
    g.text = Some(text);
    g.at = Some(Instant::now());
}

/// 选区缓存 TTL：超过此时长视为失效（用户可能已编辑/清选）。
const SELECTION_TTL_SECS: u64 = 10;

/// 取最近选区（invoke 调用），带 TTL 过期判断。
pub fn get_last_selection() -> Option<String> {
    let g = cache().read().unwrap();
    let at = g.at?;
    if at.elapsed().as_secs() > SELECTION_TTL_SECS {
        return None;
    }
    g.text.clone()
}

// ── 划词监听（鼠标钩子） ──────────────────────────────────────
//
// 低级鼠标钩子 WH_MOUSE_LL 一旦安装，从其他线程 UnhookWindowsHookEx 会失败/竞态，
// 且钩子线程持有的 tls 状态也难以干净重置。因此策略是：
// - 钩子安装保持幂等（进程内只装一次，OnceLock 守卫）
// - 「关闭划词感知」不卸钩子，而是让回调发现开关为 false 时直接跳过选词判定与抓取
//   （代价：钩子链上一次极轻的 WPARAM/CoWord 分派，微秒级；比反复装卸钩子安全）

static LISTENER_STARTED: OnceLock<()> = OnceLock::new();
static LISTENER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 敏感应用黑名单（前台是这些应用时不抓取选区）。
///
/// 隐私门控：`on_selection` 拿到前台 HWND 后先查进程名，命中则直接跳过——
/// 抓取都不做，缓存永远干净。与 `ContextConfig::sensitive_apps` 同步（`set_active` 之外
/// 的另一路 hot swap，见 `commands::update_context_config`）。
///
/// TODO(0.9 awareness 重构)：目前是"划词自己维护一份影子列表"——sensitive 应该是
/// awareness 层的横切策略，所有通道（selection/clipboard/foreground）共用一份。见 memory。
static SENSITIVE_APPS: OnceLock<RwLock<Vec<String>>> = OnceLock::new();

fn sensitive_apps() -> &'static RwLock<Vec<String>> {
    SENSITIVE_APPS.get_or_init(|| RwLock::new(Vec::new()))
}

/// 划词感知当前是否启用。回调线程读它决定是否处理选词。
pub(crate) fn is_active() -> bool {
    LISTENER_ACTIVE.load(Ordering::Relaxed)
}

/// 敏感应用列表是否非空。钩子线程用它决定是否需要查进程名（省两次 syscall）。
pub(crate) fn has_sensitive_apps() -> bool {
    !sensitive_apps().read().unwrap().is_empty()
}

/// 查询进程名是否命中敏感应用黑名单（大小写不敏感、前后空白）。
/// 回调线程用它决定是否跳过抓取。
pub(crate) fn is_process_sensitive(process_name: &str) -> bool {
    let name = process_name.trim();
    if name.is_empty() {
        return false;
    }
    let list = sensitive_apps().read().unwrap();
    list.iter().any(|s| s.trim().eq_ignore_ascii_case(name))
}

/// 热更新敏感应用列表。`update_context_config` 保存后调它同步。
pub fn set_sensitive_apps(apps: Vec<String>) {
    let mut g = sensitive_apps().write().unwrap();
    *g = apps;
    tracing::debug!(count = g.len(), "划词感知：敏感应用列表已同步");
}

/// 启动划词监听（幂等）。首次调用装钩子；之后调用只翻转激活位。
/// 主线程 setup 阶段调用。运行期热切换（用户在设置里 toggle）走 `set_active`。
#[cfg(target_os = "windows")]
pub fn start_listener() {
    LISTENER_ACTIVE.store(true, Ordering::Relaxed);
    LISTENER_STARTED.get_or_init(|| {
        self::listener::start();
    });
    tracing::debug!("划词监听已启用");
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub fn start_listener() {
    LISTENER_ACTIVE.store(true, Ordering::Relaxed);
}

/// 热更新激活状态：`true` 允许钩子回调抓取；`false` 直接跳过（钩子仍在链上但不做事）。
/// 首次 true 时若钩子未装则装上（配合 `start_listener` 幂等）。
pub fn set_active(active: bool) {
    let prev = LISTENER_ACTIVE.swap(active, Ordering::Relaxed);
    if prev == active {
        return;
    }
    if active {
        #[cfg(target_os = "windows")]
        LISTENER_STARTED.get_or_init(|| {
            self::listener::start();
        });
        tracing::debug!("划词感知：已启用（钩子活跃）");
    } else {
        // 关闭时清一次缓存，避免残留选区在下次开启前被 invoke 读到
        if let Some(lock) = CACHE.get() {
            let mut g = lock.write().unwrap();
            g.text = None;
            g.at = None;
        }
        tracing::debug!("划词感知：已关闭（钩子链保留，回调跳过）");
    }
}

#[cfg(target_os = "windows")]
mod listener;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(test)]
mod tests {
    use super::*;

    // 单测合并：SENSITIVE_APPS 是静态全局，cargo 默认多线程跑会互相污染。
    // 用一个 test 串行覆盖所有 case，比引入 serial_test 依赖轻。
    #[test]
    fn sensitive_apps_matching() {
        // 大小写不敏感、trim
        set_sensitive_apps(vec!["Bitwarden.exe".into(), "  1Password.exe  ".into()]);
        assert!(is_process_sensitive("bitwarden.exe"));
        assert!(is_process_sensitive("BITWARDEN.EXE"));
        assert!(is_process_sensitive("1password.exe"));
        assert!(is_process_sensitive("  Bitwarden.exe  "));

        // 空进程名不命中（防误伤）
        assert!(!is_process_sensitive(""));
        assert!(!is_process_sensitive("   "));

        // 非黑名单不命中；部分匹配不算命中（必须完整进程名等价）
        assert!(!is_process_sensitive("chrome.exe"));
        assert!(!is_process_sensitive("Bitwarden"));

        // 清空列表 → 一切不命中
        set_sensitive_apps(vec![]);
        assert!(!is_process_sensitive("Bitwarden.exe"));
    }
}
