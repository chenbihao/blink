//! 快捷键录制:平台无关的状态机。
//!
//! 平台事件源(Windows 的 `ll_proc`、未来 macOS 的 `CGEventTap`)把原始
//! 按键归一化为 [`RecordInput`](键名字符串)后喂给本模块的状态机。状态机
//! 本身只认归一化键名,不接触任何平台特定的键码(VK / scancode / 事件类型),
//! 因此平台无关——未来加平台只需实现各自的事件源 + 「原始键码 → 键名」映射,
//! 喂给同一个状态机。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// 录制结果。
pub struct RecordResult {
    pub modifiers: Vec<String>,
    pub key: String,
    pub display: String,
}

/// 平台无关的语义输入:由平台事件源(如 `ll_proc`)归一化后喂入。
pub enum RecordInput {
    /// 修饰键按下(具体名,如 `"ralt"`、`"lctrl"`、`"meta"`)。
    ModifierDown(String),
    /// 修饰键松开。
    ModifierUp(String),
    /// 主键按下(如 `"a"`、`"F1"`、`"Enter"`、`"Escape"`)。
    /// 主键一旦按下即完成录制,故无对应的 KeyUp 变体。
    KeyDown(String),
}

/// 录制完成产物(内部)。
enum RecordOutcome {
    /// 录到一个快捷键。
    Recorded(RecordResult),
    /// 用户取消(Esc)。
    Cancelled,
}

/// 录制状态(全局单例)。
struct RecorderState {
    /// 是否正在录制。
    recording: AtomicBool,
    /// 完成通知通道(录制开始时创建,完成后发送)。
    sender: Mutex<Option<mpsc::Sender<RecordOutcome>>>,
    /// 当前按下的修饰键集合(具体名,用于组合键快照与单独修饰键判定)。
    pressed_modifiers: Mutex<Vec<String>>,
}

static RECORDER: OnceLock<RecorderState> = OnceLock::new();

fn get_recorder() -> &'static RecorderState {
    RECORDER.get_or_init(|| RecorderState {
        recording: AtomicBool::new(false),
        sender: Mutex::new(None),
        pressed_modifiers: Mutex::new(Vec::new()),
    })
}

/// 录制快捷键(阻塞,直到用户按下组合键、取消或超时)。
///
/// 返回 `None` 表示超时、取消或已有录制进行中。
///
/// 在 `spawn_blocking` 中调用:用 `mpsc::channel` + `recv_timeout` 等待
/// `feed` 的结果,替代了旧的 10ms 轮询。
pub fn record_hotkey_blocking() -> Option<RecordResult> {
    let state = get_recorder();

    // 先准备好通道与状态(此时 recording 仍为 false,feed 会直接 return、不介入)。
    let (tx, rx) = mpsc::channel();
    if let Ok(mut slot) = state.sender.lock() {
        *slot = Some(tx);
    }
    if let Ok(mut mods) = state.pressed_modifiers.lock() {
        mods.clear();
    }

    // 最后才开启录制(Release):保证上面的写在 feed 线程可见后它才能介入,
    // 避免「down 已 push 却被本函数的 clear 清掉」的竞态。CAS 同时防并发录制。
    if state
        .recording
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        // 已有录制进行中:回滚 sender。
        if let Ok(mut slot) = state.sender.lock() {
            *slot = None;
        }
        return None;
    }

    // 阻塞等待结果(超时 10s,与前端文案「10秒超时」一致)。
    let result = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(RecordOutcome::Recorded(r)) => {
            tracing::debug!("recorder: recv Recorded key={}", r.key);
            Some(r)
        }
        Ok(RecordOutcome::Cancelled) => {
            tracing::debug!("recorder: recv Cancelled");
            None
        }
        Err(e) => {
            tracing::warn!("recorder: recv Err {:?}", e);
            None
        }
    };

    // 清理
    state.recording.store(false, Ordering::Release);
    if let Ok(mut slot) = state.sender.lock() {
        *slot = None;
    }
    if let Ok(mut mods) = state.pressed_modifiers.lock() {
        mods.clear();
    }

    result
}

/// 是否正在录制(供平台事件源在回调中短路判断)。
pub fn is_recording() -> bool {
    get_recorder().recording.load(Ordering::Acquire)
}

/// 外部取消录制（如锁屏/会话重置时调用）。
///
/// 发送 `Cancelled` 到通道，解除 `record_hotkey_blocking` 的阻塞。
/// 如果当前未在录制，无操作。
pub fn cancel() {
    let state = get_recorder();
    if state.recording.load(Ordering::Acquire) {
        finish(state, RecordOutcome::Cancelled);
    }
}

/// 喂入一个语义事件(供平台事件源调用)。
///
/// 状态机语义:
/// - 主键按下 → 立即完成(快照当前修饰键);主键为 `Escape` → 取消。
/// - 修饰键按下 → 记入 `pressed_modifiers`。
/// - 修饰键松开 → 若该键确在集合中,移除并完成(单独修饰键快捷键);
///   否则忽略(如 AltGr 模拟 `lctrl` 的残留松开)。
pub fn feed(input: RecordInput) {
    let state = get_recorder();
    if !state.recording.load(Ordering::Acquire) {
        return;
    }

    match input {
        RecordInput::KeyDown(name) => {
            if name == "Escape" {
                finish(state, RecordOutcome::Cancelled);
                return;
            }
            // 主键按下即完成,快照当前修饰键
            let modifiers = state
                .pressed_modifiers
                .lock()
                .map(|mods| mods.clone())
                .unwrap_or_default();
            let display = format_display(&modifiers, &name);
            finish(
                state,
                RecordOutcome::Recorded(RecordResult {
                    modifiers,
                    key: name,
                    display,
                }),
            );
        }
        RecordInput::ModifierDown(name) => {
            if let Ok(mut mods) = state.pressed_modifiers.lock() {
                if !mods.contains(&name) {
                    mods.push(name);
                }
            }
        }
        RecordInput::ModifierUp(name) => {
            // 仅当该键确实在 pressed_modifiers 中才处理:
            //   在 → 移除并完成(单独修饰键快捷键)
            //   不在 → 忽略(AltGr 模拟 lctrl 的残留松开,见 feed_recorder 的 AltGr 清理)
            let present = state
                .pressed_modifiers
                .lock()
                .map(|mut mods| {
                    if mods.contains(&name) {
                        mods.retain(|m| m != &name);
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if present {
                let display = format_display(&[], &name);
                finish(
                    state,
                    RecordOutcome::Recorded(RecordResult {
                        modifiers: vec![],
                        key: name,
                        display,
                    }),
                );
            }
        }
    }
}

/// 从当前按下修饰键集合中移除指定键(**不触发完成**)。
///
/// 供平台事件源处理平台特定行为:Windows 上右 Alt(`VK_RMENU`)按下时会
/// 附带一个模拟的左 Ctrl,需在 `VK_RMENU` 按下时调本函数清掉它,避免
/// 状态机把右 Alt 误录成 `LeftCtrl`。状态机自身不认识 AltGr,保持可移植。
pub fn drop_modifier(name: &str) {
    let state = get_recorder();
    if let Ok(mut mods) = state.pressed_modifiers.lock() {
        mods.retain(|m| m != name);
    }
}

/// 完成录制:发送结果并复位 `recording` 标志。
fn finish(state: &'static RecorderState, outcome: RecordOutcome) {
    if let Ok(mut slot) = state.sender.lock() {
        if let Some(tx) = slot.take() {
            let _ = tx.send(outcome);
        }
    }
    state.recording.store(false, Ordering::Release);
}

/// 格式化快捷键显示名称。
fn format_display(modifiers: &[String], key: &str) -> String {
    let mut parts = Vec::new();

    for m in modifiers {
        match m.as_str() {
            "ctrl" | "lctrl" => parts.push("Ctrl"),
            "rctrl" => parts.push("RightCtrl"),
            "shift" | "lshift" => parts.push("Shift"),
            "rshift" => parts.push("RightShift"),
            "alt" | "lalt" => parts.push("Alt"),
            "ralt" => parts.push("RightAlt"),
            "meta" => parts.push("Win"),
            _ => {}
        }
    }

    // 格式化主键
    let key_display = match key {
        " " => "Space",
        "ArrowUp" => "↑",
        "ArrowDown" => "↓",
        "ArrowLeft" => "←",
        "ArrowRight" => "→",
        "Escape" => "Esc",
        "Delete" => "Del",
        "Enter" => "Enter",
        "Backspace" => "Backspace",
        "Tab" => "Tab",
        "ralt" => "RightAlt",
        "lalt" => "LeftAlt",
        "rctrl" => "RightCtrl",
        "lctrl" => "LeftCtrl",
        "rshift" => "RightShift",
        "lshift" => "LeftShift",
        "meta" => "Win",
        _ => {
            if key.starts_with('F') && key.len() <= 3 {
                key
            } else {
                &key.to_uppercase()
            }
        }
    };
    parts.push(key_display);

    parts.join("+")
}
