//! Windows 平台特定的热键实现：WH_KEYBOARD_LL 低级键盘钩子。
//!
//! 状态机设计：
//! - 修饰键按下：记录到 pressed_modifiers
//! - 非修饰键按下：记录为主键，快照当前修饰键
//! - 主键松开：检查匹配并触发
//! - 修饰键松开：如果是单独修饰键快捷键，检查匹配并触发

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::{HotkeyEvent, get_current_config, get_tap_threshold, send_event};

/// 修饰键 down→up 间隔小于此值即判为系统合成的瞬时事件对（人类松手不可能这么快）。
/// 用于过滤 IDEA 等程序在组合键触发后注入的虚假 Alt down+up（中和菜单激活），
/// 否则会让 pressed_modifiers 丢失 Alt，导致「按住 Alt 连按主键」第二次起匹配失败。
const SYNTHETIC_EVENT_THRESHOLD: Duration = Duration::from_millis(30);

/// hook 线程私有状态。
struct State {
    /// 主键按下时刻（用于 tap/hold 判定）。
    down_since: Option<Instant>,
    /// 按下期间是否出现过其他键（出现 → 判 hold，不唤起）。
    aborted: bool,
    /// 当前按下的修饰键集合（左右区分，如 "lalt", "rctrl"）。
    pressed_modifiers: Vec<String>,
    /// 当前按下的具体键名集合（用于检测 AltGr 模拟产生的 Ctrl）。
    pressed_keys: Vec<String>,
    /// 当前按下的主键。
    pressed_key: Option<String>,
    /// 主键按下时的修饰键快照（用于组合键匹配，即使修饰键先松开也能触发）。
    modifiers_snapshot: Vec<String>,
    /// 每个修饰键最近一次 DOWN 的时刻（用于识别系统合成的瞬时 down-up 对）。
    last_mod_down: std::collections::HashMap<String, Instant>,
}

thread_local! {
    static STATE: std::cell::RefCell<State> = std::cell::RefCell::new(State {
        down_since: None,
        aborted: false,
        pressed_modifiers: Vec::new(),
        pressed_keys: Vec::new(),
        pressed_key: None,
        modifiers_snapshot: Vec::new(),
        last_mod_down: std::collections::HashMap::new(),
    });
}

/// 启动 Windows 钩子线程。
pub fn start_hook_thread() {
    std::thread::Builder::new()
        .name("blink-hotkey".into())
        .spawn(hook_thread_main)
        .expect("failed to spawn hotkey thread");
}

/// 热键线程入口：安装钩子 → 消息循环 → 卸载。
fn hook_thread_main() {
    unsafe {
        let hhook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), None, 0)
            .expect("SetWindowsHookExW failed for WH_KEYBOARD_LL");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(hhook);
    }
}

/// 修饰键归一化（具体键名 → 通用名，用于兼容匹配）。
fn normalize_modifier_generic(name: &str) -> &str {
    match name {
        "lctrl" | "rctrl" => "ctrl",
        "lshift" | "rshift" => "shift",
        "lalt" | "ralt" => "alt",
        _ => name,
    }
}

/// 检查是否匹配配置的快捷键。
///
/// 匹配规则：
/// - 单独修饰键快捷键（如 "ralt"）：主键直接比较
/// - 组合键：配置中的修饰键必须全部被按下
///   - 具体键名（如 "ralt"）精确匹配
///   - 通用键名（如 "alt"）匹配左右任意一侧（兼容旧配置）
fn is_hotkey_match(config: &crate::config::HotkeyConfig, modifiers: &[String], key: &str) -> bool {
    // 单独修饰键快捷键（如右 Alt、右 Ctrl、右 Shift）
    if config.modifiers.is_empty() && super::recorder::is_standalone_modifier_key(&config.key) {
        return key == config.key;
    }

    // 组合键匹配
    if modifiers.len() != config.modifiers.len() {
        return false;
    }

    for config_mod in &config.modifiers {
        // 先尝试精确匹配（如 "ralt" == "ralt"）
        if modifiers.contains(config_mod) {
            continue;
        }
        // 归一化匹配：将两侧都归一化后比较
        // - 配置 "ralt" + 按下 "lalt" → "alt" == "alt" → 匹配（不应发生，但安全兜底）
        // - 配置 "alt" + 按下 "lalt" → "alt" == "alt" → 匹配（兼容旧配置）
        // - 配置 "ralt" + 按下 "rctrl" → "alt" != "ctrl" → 不匹配 ✓
        let config_generic = normalize_modifier_generic(config_mod);
        let matched = modifiers
            .iter()
            .any(|m| normalize_modifier_generic(m) == config_generic);
        if !matched {
            return false;
        }
    }

    key == config.key
}

/// 将虚拟键码转换为配置中的键名。
fn vk_to_key(vk: u32) -> Option<String> {
    // 修饰键。通用码（VK_SHIFT / VK_CONTROL / VK_MENU，不分左右）当作左侧——
    // 兼容某些驱动/事件流只发通用码的情况；否则这些按键会被 vk_to_key 忽略，
    // 导致录制单独修饰键（如左 Alt）时永远等不到松开事件、无法结束。
    if vk == VK_LCONTROL.0 as u32 || vk == VK_CONTROL.0 as u32 { return Some("lctrl".to_string()); }
    if vk == VK_RCONTROL.0 as u32 { return Some("rctrl".to_string()); }
    if vk == VK_LSHIFT.0 as u32 || vk == VK_SHIFT.0 as u32 { return Some("lshift".to_string()); }
    if vk == VK_RSHIFT.0 as u32 { return Some("rshift".to_string()); }
    if vk == VK_LMENU.0 as u32 || vk == VK_MENU.0 as u32 { return Some("lalt".to_string()); }
    if vk == VK_RMENU.0 as u32 { return Some("ralt".to_string()); }
    if vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32 { return Some("meta".to_string()); }

    // 字母键 (A-Z)
    if (0x41..=0x5A).contains(&vk) {
        let c = char::from_u32(vk - 0x41 + b'a' as u32)?;
        return Some(c.to_string());
    }

    // 数字键 (0-9)
    if (0x30..=0x39).contains(&vk) {
        let c = char::from_u32(vk - 0x30 + b'0' as u32)?;
        return Some(c.to_string());
    }

    // 功能键 (F1-F12)
    if (0x70..=0x7B).contains(&vk) {
        let f_num = vk - 0x70 + 1;
        return Some(format!("F{}", f_num));
    }

    // 特殊键
    if vk == VK_SPACE.0 as u32 { return Some(" ".to_string()); }
    if vk == VK_RETURN.0 as u32 { return Some("Enter".to_string()); }
    if vk == VK_ESCAPE.0 as u32 { return Some("Escape".to_string()); }
    if vk == VK_BACK.0 as u32 { return Some("Backspace".to_string()); }
    if vk == VK_TAB.0 as u32 { return Some("Tab".to_string()); }
    if vk == VK_DELETE.0 as u32 { return Some("Delete".to_string()); }
    if vk == VK_UP.0 as u32 { return Some("ArrowUp".to_string()); }
    if vk == VK_DOWN.0 as u32 { return Some("ArrowDown".to_string()); }
    if vk == VK_LEFT.0 as u32 { return Some("ArrowLeft".to_string()); }
    if vk == VK_RIGHT.0 as u32 { return Some("ArrowRight".to_string()); }

    // 标点/符号键（OEM 键，按美式键盘布局命名）。非修饰键，作为主键录制。
    if vk == 0xBA { return Some(";".to_string()); }   // VK_OEM_1      ';'
    if vk == 0xBB { return Some("=".to_string()); }   // VK_OEM_PLUS   '='
    if vk == 0xBC { return Some(",".to_string()); }   // VK_OEM_COMMA  ','
    if vk == 0xBD { return Some("-".to_string()); }   // VK_OEM_MINUS  '-'
    if vk == 0xBE { return Some(".".to_string()); }   // VK_OEM_PERIOD '.'
    if vk == 0xBF { return Some("/".to_string()); }   // VK_OEM_2      '/'
    if vk == 0xC0 { return Some("`".to_string()); }   // VK_OEM_3      '`'
    if vk == 0xDB { return Some("[".to_string()); }   // VK_OEM_4      '['
    if vk == 0xDC { return Some("\\".to_string()); }  // VK_OEM_5      '\'
    if vk == 0xDD { return Some("]".to_string()); }   // VK_OEM_6      ']'
    if vk == 0xDE { return Some("'".to_string()); }   // VK_OEM_7      '''

    None
}

/// 检查是否为修饰键。
fn is_modifier_key(vk: u32) -> bool {
    vk == VK_LCONTROL.0 as u32
        || vk == VK_RCONTROL.0 as u32
        || vk == VK_CONTROL.0 as u32
        || vk == VK_LSHIFT.0 as u32
        || vk == VK_RSHIFT.0 as u32
        || vk == VK_SHIFT.0 as u32
        || vk == VK_LMENU.0 as u32
        || vk == VK_RMENU.0 as u32
        || vk == VK_MENU.0 as u32
        || vk == VK_LWIN.0 as u32
        || vk == VK_RWIN.0 as u32
}

/// 获取修饰键的具体名称（区分左右）。
fn normalize_modifier(vk: u32) -> Option<String> {
    if vk == VK_LCONTROL.0 as u32 || vk == VK_CONTROL.0 as u32 {
        return Some("lctrl".to_string());
    }
    if vk == VK_RCONTROL.0 as u32 {
        return Some("rctrl".to_string());
    }
    if vk == VK_LSHIFT.0 as u32 || vk == VK_SHIFT.0 as u32 {
        return Some("lshift".to_string());
    }
    if vk == VK_RSHIFT.0 as u32 {
        return Some("rshift".to_string());
    }
    if vk == VK_LMENU.0 as u32 || vk == VK_MENU.0 as u32 {
        return Some("lalt".to_string());
    }
    if vk == VK_RMENU.0 as u32 {
        return Some("ralt".to_string());
    }
    if vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32 {
        return Some("meta".to_string());
    }
    None
}

/// 用真实物理按键状态补全修饰键快照。
///
/// 解决：Windows 对带 Alt 的 WM_SYSKEYDOWN 会补发「虚假 Alt UP」，使事件累积的
/// pressed_modifiers 丢失 Alt。GetAsyncKeyState 高位为 1 表示该键**当前物理按下**，
/// 不受虚假 UP 影响。把物理按着却不在快照里的修饰键补回（左右区分，与配置键名一致）。
fn merge_physical_modifiers(snapshot: &mut Vec<String>) {
    // (虚拟键, 键名) —— 左右分开查，对齐 normalize_modifier 的命名
    const KEYS: &[(VIRTUAL_KEY, &str)] = &[
        (VK_LMENU, "lalt"),
        (VK_RMENU, "ralt"),
        (VK_LCONTROL, "lctrl"),
        (VK_RCONTROL, "rctrl"),
        (VK_LSHIFT, "lshift"),
        (VK_RSHIFT, "rshift"),
        (VK_LWIN, "meta"),
        (VK_RWIN, "meta"),
    ];
    for (vk, name) in KEYS {
        // 高位置 1 = 物理按下
        let pressed = unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0;
        if pressed && !snapshot.iter().any(|m| m == name) {
            snapshot.push(name.to_string());
        }
    }
}

/// 低级键盘钩子回调：tap/hold 状态机。全程放行，绝不吞键。
unsafe extern "system" fn ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    const HC_ACTION: i32 = 0;
    if code == HC_ACTION {
        let kb = unsafe { &*(lparam.0 as usize as *const KBDLLHOOKSTRUCT) };
        let vk = kb.vkCode;
        let msg = wparam.0 as u32;
        let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;

        // 录制短路：录制期间把事件喂给 recorder，且不碰触发的 thread_local STATE。
        if super::recorder::is_recording() {
            feed_recorder(vk, wparam);
            // 录制期间吞掉 Alt+Space：WebView2 在底层把 WM_SYSKEYDOWN(VK_SPACE)+Alt
            // 转发给宿主，前端 preventDefault 拦不住，会呼出左上角系统菜单并冻结
            // webview 消息泵。仅在录制期间、仅此组合吞键，不破坏日常「不吞键」原则。
            if vk == VK_SPACE.0 as u32
                && unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0
            {
                return LRESULT(1);
            }
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        let config = get_current_config();
        let tap_threshold = get_tap_threshold();

        STATE.with(|cell| {
            let mut s = cell.borrow_mut();

            if is_modifier_key(vk) {
                // ── 修饰键处理 ──
                let modifier = normalize_modifier(vk);
                let specific_key = vk_to_key(vk);

                if is_down {
                    // 记录修饰键
                    if let Some(m) = &modifier {
                        if !s.pressed_modifiers.contains(m) {
                            s.pressed_modifiers.push(m.clone());
                        }
                        // 记录本次 DOWN 时刻，供 UP 时识别系统合成的瞬时 down-up 对
                        s.last_mod_down.insert(m.clone(), Instant::now());
                    }
                    // 记录按下的具体键（用于 AltGr 模拟检测）
                    if let Some(ref sk) = specific_key {
                        if !s.pressed_keys.contains(sk) {
                            s.pressed_keys.push(sk.clone());
                        }
                    }

                    // AltGr 模拟处理：VK_RMENU 按下时，如果 "lctrl" 在 pressed_modifiers
                    // 中但不是真实按下的键（pressed_keys 中没有 "lctrl"），说明是系统
                    // 模拟的 Ctrl+Alt，需要移除模拟的 "lctrl"。
                    // 注：如果用户真的先按了 Ctrl 再按 RightAlt，Ctrl 也会被移除（极少数场景）。
                    if vk == VK_RMENU.0 as u32 {
                        if !s.pressed_keys.contains(&"lctrl".to_string()) {
                            s.pressed_modifiers.retain(|x| x != "lctrl");
                        }
                    }

                    // 检查是否是配置的单独修饰键快捷键
                    if let Some(ref sk) = specific_key {
                        if config.modifiers.is_empty() && *sk == config.key {
                            s.pressed_key = Some(sk.clone());
                            s.down_since = Some(Instant::now());
                        }
                    }
                } else if is_up {
                    // 检查单独修饰键快捷键
                    if let Some(ref key) = s.pressed_key {
                        if super::recorder::is_standalone_modifier_key(key) {
                            if !s.aborted {
                                if let Some(since) = s.down_since.take() {
                                    if since.elapsed() <= Duration::from_millis(tap_threshold) {
                                        send_event(HotkeyEvent::Tap(Instant::now()));
                                    }
                                }
                            }
                            // 重置状态
                            s.pressed_key = None;
                            s.down_since = None;
                            s.aborted = false;
                        }
                    }

                    // 移除修饰键。
                    // 例外：配置为组合键时，识别并忽略系统合成的「瞬时 down-up 对」——
                    // IDEA 等把 Alt 当菜单助记键的程序，会在组合键触发后注入一对间隔极短的
                    // Alt down+up（中和菜单激活），使 pressed_modifiers 丢失 Alt，导致「按住 Alt
                    // 连按主键」第二次起匹配失败。人类松开不可能这么快，故据此判为合成、跳过移除。
                    // 仅组合键启用（单独修饰键快捷键 config.modifiers 为空，不受影响，避免误伤其快速 tap）。
                    if let Some(m) = &modifier {
                        let synthetic = !config.modifiers.is_empty()
                            && s
                                .last_mod_down
                                .get(m)
                                .map_or(false, |t| t.elapsed() < SYNTHETIC_EVENT_THRESHOLD);
                        if synthetic {
                            tracing::trace!(modifier = %m, "hotkey: 忽略系统合成的瞬时 Alt UP（保留按下态）");
                        } else {
                            s.pressed_modifiers.retain(|x| x != m);
                            s.last_mod_down.remove(m); // 真实松开：清理记录，避免陈旧时刻误判
                        }
                    }
                    if let Some(sk) = &specific_key {
                        s.pressed_keys.retain(|x| x != sk);
                    }

                    // 如果所有修饰键都松开，重置状态
                    if s.pressed_modifiers.is_empty() && s.pressed_key.is_none() {
                        s.down_since = None;
                        s.aborted = false;
                    }
                }
            } else {
                // ── 非修饰键处理 ──
                let key = vk_to_key(vk);

                if is_down {
                    if let Some(k) = key {
                        // 记录主键和修饰键快照
                        s.pressed_key = Some(k.clone());
                        s.modifiers_snapshot = s.pressed_modifiers.clone();
                        // 物理状态补全：Windows 在带 Alt 的 WM_SYSKEYDOWN 触发后会补发
                        // 一个「虚假的 Alt UP」（结束系统菜单激活），导致 pressed_modifiers
                        // 丢失 Alt。但用户物理上仍按着——用 GetAsyncKeyState 查真实物理状态，
                        // 把按着却不在快照里的修饰键补回，否则按住 Alt 连按主键时第二次起匹配失败。
                        merge_physical_modifiers(&mut s.modifiers_snapshot);
                        s.down_since = Some(Instant::now());
                        s.aborted = false;
                    }
                } else if is_up {
                    // 检查组合键匹配
                    if let Some(ref key) = s.pressed_key {
                        if !s.aborted {
                            if is_hotkey_match(&config, &s.modifiers_snapshot, key) {
                                if let Some(since) = s.down_since.take() {
                                    if since.elapsed() <= Duration::from_millis(tap_threshold) {
                                        send_event(HotkeyEvent::Tap(Instant::now()));
                                    }
                                }
                            }
                        }
                    }

                    // 重置状态
                    s.pressed_key = None;
                    s.down_since = None;
                    s.aborted = false;
                    s.modifiers_snapshot.clear();
                }
            }

            // ── aborted 检测 ──
            // 如果在主键按下期间按下其他非修饰键，标记为 aborted
            if is_down && !is_modifier_key(vk) {
                if s.pressed_key.is_some() && s.down_since.is_some() {
                    let key = vk_to_key(vk).unwrap_or_default();
                    // 如果按下的键不是配置的主键，标记为 aborted
                    if key != config.key {
                        s.aborted = true;
                    }
                }
            }
        });
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 录制期间把原始 VK 事件归一化为语义事件喂给 [`super::recorder`]。
///
/// 平台特定逻辑集中在此：VK→键名映射复用 [`vk_to_key`]，AltGr 去模拟
/// （右 Alt 附带的左 Ctrl）通过 [`super::recorder::drop_modifier`] 清除。
fn feed_recorder(vk: u32, wparam: WPARAM) {
    let msg = wparam.0 as u32;
    let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    if !is_down && !is_up {
        return;
    }

    if is_modifier_key(vk) {
        let Some(name) = vk_to_key(vk) else { return };
        if is_down {
            // AltGr 去模拟：右 Alt（VK_RMENU）按下会附带一个模拟的左 Ctrl，
            // 清掉它，避免状态机把右 Alt 误录成 LeftCtrl。
            if vk == VK_RMENU.0 as u32 {
                super::recorder::drop_modifier("lctrl");
            }
            super::recorder::feed(super::recorder::RecordInput::ModifierDown(name));
        } else {
            super::recorder::feed(super::recorder::RecordInput::ModifierUp(name));
        }
    } else if is_down {
        // 非修饰键:按下即完成录制;松开不关心。
        let Some(name) = vk_to_key(vk) else { return };
        super::recorder::feed(super::recorder::RecordInput::KeyDown(name));
    }
}
