//! chord 全局快捷键注册（0.22.12，`RegisterHotKey`）。
//!
//! **线程约束**：注册/注销只允许在 blink-hotkey 线程调用——`RegisterHotKey`
//! 必须落在创建 `BlinkInputWindow` 的线程，`WM_HOTKEY` 才会进入该线程的
//! 消息循环。挂点：`hook_thread_main`（初始）、`process_control_message`
//! 的 `ConfigChanged` 应用后（重注册）、`destroy_window`（注销）。
//!
//! **与 hook 吞键的互斥**：LL hook 先于系统热键派发，主窗可见且 chord 可触发时
//! 吞键路径先命中（`WM_HOTKEY` 不产生）；注册层无需感知 chord 会话。
//! 「跟随触发键」模式的主窗让位规则由 app 层（HotkeyService）执行。
//!
//! **注册即冲突探测**：`ERROR_HOTKEY_ALREADY_REGISTERED` → 状态 `occupied`。
//! 只能探测同样走系统注册的软件；Snipaste 等LL hook 实现探测不到（尽力而为）。

use std::cell::RefCell;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{ERROR_HOTKEY_ALREADY_REGISTERED, HWND};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN, VK_F1, VK_SPACE,
};

use super::state::{GlobalHotkeyStatus, ResolvedGlobalHotkey};
use super::send_effect;
// BlinkInputWindow 句柄（windows.rs 同级共享）
use super::windows::WND_HWND;

/// 全局快捷键 hotkey id 基值（WM_HOTKEY 的 wparam 反查用）。
const HOTKEY_ID_BASE: i32 = 0x2200;

/// 当前已注册条目（仅 blink-hotkey 线程访问；wnd_proc 反查用）。
struct RegisteredEntry {
    hotkey_id: i32,
    action_id: String,
    follow_chord: bool,
}

thread_local! {
    static REGISTERED: RefCell<Vec<RegisteredEntry>> = const { RefCell::new(Vec::new()) };
}

/// 注册状态存储（hook 线程写，command 线程读）。
///
/// 写点在消息循环控制处理（非 `ll_proc` 回调），允许短暂加锁；
/// Hook 热路径无锁铁则不受影响。
static LAST_STATUS: OnceLock<Mutex<Option<Vec<GlobalHotkeyStatus>>>> = OnceLock::new();

fn status_slot() -> &'static Mutex<Option<Vec<GlobalHotkeyStatus>>> {
    LAST_STATUS.get_or_init(|| Mutex::new(None))
}

/// 读取最近一次注册结果（设置页查询用，任意线程可调）。
pub fn global_hotkey_statuses() -> Vec<GlobalHotkeyStatus> {
    status_slot()
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_default()
}

/// 按 hotkey id 反查触发目标（wnd_proc 的 WM_HOTKEY 分支调用，hook 线程）。
pub fn lookup_hotkey_target(hotkey_id: usize) -> Option<(String, bool)> {
    REGISTERED.with(|cell| {
        cell.borrow()
            .iter()
            .find(|e| e.hotkey_id as usize == hotkey_id)
            .map(|e| (e.action_id.clone(), e.follow_chord))
    })
}

/// 按快照全量重注册全局快捷键（先注销旧集合，再逐个注册）。
///
/// 单键注册失败不阻塞其他键，失败原因进状态投影（配置保留、标记未生效）。
/// 完成后与上次状态比对，变化时经 `InputEffect::GlobalHotkeysChanged` 推给
/// HotkeyService 转发前端。
pub fn apply_global_hotkeys(desired: &[ResolvedGlobalHotkey]) {
    unregister_all();

    let hwnd = match WND_HWND.get() {
        Some(&raw) => HWND(raw as *mut _),
        None => {
            tracing::warn!("全局快捷键注册跳过：BlinkInputWindow 未就绪");
            return;
        }
    };

    let mut statuses = Vec::with_capacity(desired.len());
    REGISTERED.with(|cell| {
        let mut registered = cell.borrow_mut();
        for (idx, entry) in desired.iter().enumerate() {
            let hotkey_id = HOTKEY_ID_BASE + idx as i32;
            match register_one(hwnd, hotkey_id, entry) {
                Ok(()) => {
                    tracing::info!(
                        action_id = %entry.action_id,
                        follow_chord = entry.follow_chord,
                        combo = %display_combo(&entry.modifiers, &entry.key),
                        hotkey_id,
                        "全局快捷键已注册"
                    );
                    registered.push(RegisteredEntry {
                        hotkey_id,
                        action_id: entry.action_id.clone(),
                        follow_chord: entry.follow_chord,
                    });
                    statuses.push(GlobalHotkeyStatus {
                        action_id: entry.action_id.clone(),
                        follow_chord: entry.follow_chord,
                        modifiers: entry.modifiers.clone(),
                        key: entry.key.clone(),
                        registered: true,
                        reason: None,
                    });
                }
                Err(reason) => {
                    tracing::warn!(
                        action_id = %entry.action_id,
                        combo = %display_combo(&entry.modifiers, &entry.key),
                        hotkey_id,
                        reason = %reason,
                        "全局快捷键注册失败（标记未生效）"
                    );
                    statuses.push(GlobalHotkeyStatus {
                        action_id: entry.action_id.clone(),
                        follow_chord: entry.follow_chord,
                        modifiers: entry.modifiers.clone(),
                        key: entry.key.clone(),
                        registered: false,
                        reason: Some(reason),
                    });
                }
            }
        }
    });

    publish_status(statuses);
}

/// 注销全部全局快捷键（配置变更 / 应用退出路径）。
pub fn unregister_all() {
    let Some(&raw) = WND_HWND.get() else {
        return;
    };
    let hwnd = HWND(raw as *mut _);
    REGISTERED.with(|cell| {
        let mut registered = cell.borrow_mut();
        for entry in registered.drain(..) {
            // 注销失败仅记录——进程退出场景无需重试
            if let Err(e) = unsafe { UnregisterHotKey(Some(hwnd), entry.hotkey_id) } {
                tracing::warn!(hotkey_id = entry.hotkey_id, error = %e, "UnregisterHotKey 失败");
            }
        }
    });
}

/// 注册单个组合键。返回 Err(原因代号) 表示未生效。
fn register_one(hwnd: HWND, hotkey_id: i32, entry: &ResolvedGlobalHotkey) -> Result<(), String> {
    let (fs, known_mods) = fs_modifiers(&entry.modifiers);
    if !known_mods {
        return Err("invalid".to_string());
    }
    let Some(vk) = vk_for_key(&entry.key) else {
        return Err("invalid".to_string());
    };
    // MOD_NOREPEAT：按住不重复触发（tap 语义动作无需 autorepeat）
    let fs: HOT_KEY_MODIFIERS = fs | MOD_NOREPEAT;
    unsafe { RegisterHotKey(Some(hwnd), hotkey_id, fs, vk) }.map_err(|e| {
        // HRESULT_FROM_WIN32(ERROR_HOTKEY_ALREADY_REGISTERED) = 0x8007 | 1409
        if e.code().0 as u32 == (0x8007_0000u32 | ERROR_HOTKEY_ALREADY_REGISTERED.0) {
            "occupied".to_string()
        } else {
            tracing::debug!(error = %e, "RegisterHotKey 系统错误");
            "error".to_string()
        }
    })
}

/// 修饰键名 → `MOD_*` 位。含未知名时返回 `(_, false)`（组合键不受支持）。
fn fs_modifiers(modifiers: &[String]) -> (HOT_KEY_MODIFIERS, bool) {
    let mut fs = HOT_KEY_MODIFIERS(0);
    for m in modifiers {
        match m.as_str() {
            "ctrl" => fs |= MOD_CONTROL,
            "alt" => fs |= MOD_ALT,
            "shift" => fs |= MOD_SHIFT,
            "meta" => fs |= MOD_WIN,
            _ => return (fs, false),
        }
    }
    (fs, true)
}

/// 主键名 → VK 码。白名单：字母 / 数字 / F1-F12 / 空格。
fn vk_for_key(key: &str) -> Option<u32> {
    let lower = key.to_lowercase();
    let mut chars = lower.chars();
    if let (1, Some(c)) = (lower.len(), chars.next()) {
        match c {
            'a'..='z' => return Some(c.to_ascii_uppercase() as u32),
            '0'..='9' => return Some(c as u32),
            ' ' => return Some(VK_SPACE.0 as u32),
            _ => {}
        }
    }
    if lower == "space" {
        return Some(VK_SPACE.0 as u32);
    }
    if let Some(n) = lower
        .strip_prefix('f')
        .and_then(|num| num.parse::<u32>().ok())
        .filter(|n| (1..=12).contains(n))
    {
        return Some(VK_F1.0 as u32 + n - 1);
    }
    None
}

/// 组合键显示名（日志用）。
fn display_combo(modifiers: &[String], key: &str) -> String {
    let mut parts: Vec<String> = modifiers.to_vec();
    parts.push(if key == " " {
        "space".to_string()
    } else {
        key.to_string()
    });
    parts.join("+")
}

/// 状态入库 + 变化时广播（去重，避免每次配置刷新都推事件）。
fn publish_status(statuses: Vec<GlobalHotkeyStatus>) {
    let changed = {
        let Ok(mut guard) = status_slot().lock() else {
            return;
        };
        let changed = guard.as_ref() != Some(&statuses);
        *guard = Some(statuses.clone());
        changed
    };
    if changed {
        send_effect(super::InputEffect::GlobalHotkeysChanged(statuses));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vk_for_key_covers_whitelist() {
        assert_eq!(vk_for_key("a"), Some(0x41));
        assert_eq!(vk_for_key("S"), Some(0x53));
        assert_eq!(vk_for_key("5"), Some(0x35));
        assert_eq!(vk_for_key("f12"), Some(VK_F1.0 as u32 + 11));
        assert_eq!(vk_for_key(" "), Some(VK_SPACE.0 as u32));
        assert_eq!(vk_for_key("space"), Some(VK_SPACE.0 as u32));
        // 非白名单主键拒绝
        assert_eq!(vk_for_key("tab"), None);
        assert_eq!(vk_for_key(""), None);
        assert_eq!(vk_for_key("f13"), None);
    }

    #[test]
    fn fs_modifiers_rejects_unknown_names() {
        let (fs, ok) = fs_modifiers(&["ctrl".to_string(), "alt".to_string()]);
        assert!(ok);
        assert_ne!(fs.0 & MOD_CONTROL.0, 0);
        assert_ne!(fs.0 & MOD_ALT.0, 0);
        assert_eq!(fs.0 & MOD_SHIFT.0, 0);

        let (_, ok) = fs_modifiers(&["ctrl".to_string(), "fn".to_string()]);
        assert!(!ok);
    }
}
