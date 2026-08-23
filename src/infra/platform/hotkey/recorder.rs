//! 快捷键录制:平台无关的状态机。
//!
//! 平台事件源(Windows 的 `ll_proc`、未来 macOS 的 `CGEventTap`)把原始
//! 按键归一化为 [`RecordInput`](键名字符串)后喂给本模块的状态机。状态机
//! 本身只认归一化键名,不接触任何平台特定的键码(VK / scancode / 事件类型),
//! 因此平台无关——未来加平台只需实现各自的事件源 + 「原始键码 → 键名」映射,
//! 喂给同一个状态机。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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
    /// 串行化录制启动，保证 sender/modifiers 完成初始化后才发布 recording=true。
    begin_lock: Mutex<()>,
    /// 是否正在录制。
    recording: AtomicBool,
    /// 录制会话号，仅用于把 command / hook / recorder 日志串起来。
    next_session_id: AtomicU64,
    active_session_id: AtomicU64,
    /// 完成通知通道(录制开始时创建,完成后发送)。
    sender: Mutex<Option<(u64, mpsc::Sender<RecordOutcome>)>>,
    /// 当前按下的修饰键集合(具体名,用于组合键快照与单独修饰键判定)。
    /// 会话号与内容绑定，避免超时边界上的旧 Hook 回调污染下一次录制。
    pressed_modifiers: Mutex<(u64, Vec<String>)>,
}

impl RecorderState {
    fn new() -> Self {
        Self {
            begin_lock: Mutex::new(()),
            recording: AtomicBool::new(false),
            next_session_id: AtomicU64::new(1),
            active_session_id: AtomicU64::new(0),
            sender: Mutex::new(None),
            pressed_modifiers: Mutex::new((0, Vec::new())),
        }
    }
}

static RECORDER: OnceLock<RecorderState> = OnceLock::new();

fn get_recorder() -> &'static RecorderState {
    RECORDER.get_or_init(RecorderState::new)
}

/// 原子地建立一次录制会话。返回 receiver 即表示 recorder 已 armed。
fn begin_recording(state: &RecorderState) -> Option<(mpsc::Receiver<RecordOutcome>, u64)> {
    let _begin_guard = state.begin_lock.lock().ok()?;
    if state.recording.load(Ordering::Acquire) {
        tracing::warn!(
            active_session_id = state.active_session_id.load(Ordering::Acquire),
            "hotkey_recorder_begin_rejected: already recording"
        );
        return None;
    }

    let session_id = state.next_session_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    if let Ok(mut slot) = state.sender.lock() {
        *slot = Some((session_id, tx));
    } else {
        return None;
    }
    if let Ok(mut mods) = state.pressed_modifiers.lock() {
        mods.0 = session_id;
        mods.1.clear();
    } else {
        if let Ok(mut slot) = state.sender.lock() {
            *slot = None;
        }
        return None;
    }

    state.active_session_id.store(session_id, Ordering::Release);
    state.recording.store(true, Ordering::Release);
    Some((rx, session_id))
}

/// 录制快捷键(阻塞,直到用户按下组合键、取消或超时)。
///
/// 返回 `None` 表示超时、取消或已有录制进行中。
///
/// 在 `spawn_blocking` 中调用:用 `mpsc::channel` + `recv_timeout` 等待
/// `feed` 的结果,替代了旧的 10ms 轮询。
pub fn record_hotkey_blocking(on_ready: impl FnOnce(u64)) -> Option<RecordResult> {
    let state = get_recorder();
    let started_at = Instant::now();

    // 启动过程必须整体串行：旧实现先覆盖 sender、最后才 CAS recording，第二个并发
    // 调用会覆盖活动录制的 sender 后再 CAS 失败，导致两个调用一起超时。
    let (rx, session_id) = begin_recording(state)?;
    super::InputController::update_recorder(super::RecorderMode::Recording {
        recorder_id: session_id,
    });
    tracing::info!(session_id, "hotkey_recorder_armed");

    // ready 回调发生在 armed 之后、阻塞等待之前。command 层据此通知前端，前端只有
    // 收到 ready 才展示“正在录制”，消除较慢机器上首键早于 recorder armed 的竞态。
    on_ready(session_id);

    // 阻塞等待结果(超时 10s,与前端文案「10秒超时」一致)。
    let result = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(RecordOutcome::Recorded(r)) => {
            tracing::info!(
                session_id,
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                display = %r.display,
                "hotkey_recorder_completed"
            );
            Some(r)
        }
        Ok(RecordOutcome::Cancelled) => {
            tracing::info!(
                session_id,
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                "hotkey_recorder_cancelled"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                session_id,
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                error = ?e,
                "hotkey_recorder_wait_failed"
            );
            None
        }
    };

    // 只有阻塞等待方拥有会话清理权。recording 必须最后复位，否则下一次录制
    // 可能在本会话清理 sender/modifiers 前启动并被旧清理误伤。
    cleanup_session(state, session_id);
    super::InputController::update_recorder(super::RecorderMode::Idle);

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
    let session_id = state.active_session_id.load(Ordering::Acquire);
    if session_id != 0 && state.recording.load(Ordering::Acquire) {
        finish(state, session_id, RecordOutcome::Cancelled);
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
    let session_id = state.active_session_id.load(Ordering::Acquire);
    if session_id == 0 {
        return;
    }

    match input {
        RecordInput::KeyDown(name) => {
            if name == "Escape" {
                finish(state, session_id, RecordOutcome::Cancelled);
                return;
            }
            // 主键按下即完成,快照当前修饰键
            let modifiers = state
                .pressed_modifiers
                .lock()
                .ok()
                .and_then(|mods| (mods.0 == session_id).then(|| mods.1.clone()));
            let Some(modifiers) = modifiers else {
                return;
            };
            let display = format_display(&modifiers, &name);
            finish(
                state,
                session_id,
                RecordOutcome::Recorded(RecordResult {
                    modifiers,
                    key: name,
                    display,
                }),
            );
        }
        RecordInput::ModifierDown(name) => {
            if let Ok(mut mods) = state.pressed_modifiers.lock()
                && mods.0 == session_id
                && !mods.1.contains(&name)
            {
                mods.1.push(name);
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
                    if mods.0 == session_id && mods.1.contains(&name) {
                        mods.1.retain(|m| m != &name);
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
                    session_id,
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
    let session_id = state.active_session_id.load(Ordering::Acquire);
    if let Ok(mut mods) = state.pressed_modifiers.lock()
        && mods.0 == session_id
    {
        mods.1.retain(|m| m != name);
    }
}

/// 完成录制：只发送当前会话结果。生命周期标志由阻塞等待方统一清理。
fn finish(state: &RecorderState, session_id: u64, outcome: RecordOutcome) {
    if state.active_session_id.load(Ordering::Acquire) != session_id {
        return;
    }
    if let Ok(mut slot) = state.sender.lock()
        && slot.as_ref().is_some_and(|(id, _)| *id == session_id)
        && let Some((_, tx)) = slot.take()
    {
        let _ = tx.send(outcome);
    }
}

/// 清理指定会话。begin_lock 与 session id 双重保护，旧会话绝不清理新会话。
fn cleanup_session(state: &RecorderState, session_id: u64) {
    let _begin_guard = state
        .begin_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.active_session_id.load(Ordering::Acquire) != session_id {
        return;
    }
    if let Ok(mut slot) = state.sender.lock()
        && slot.as_ref().is_some_and(|(id, _)| *id == session_id)
    {
        *slot = None;
    }
    if let Ok(mut mods) = state.pressed_modifiers.lock()
        && mods.0 == session_id
    {
        mods.1.clear();
        mods.0 = 0;
    }
    state.active_session_id.store(0, Ordering::Release);
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

/// 当前录制会话号；0 表示未录制。仅用于平台层诊断日志。
pub fn active_session_id() -> u64 {
    get_recorder().active_session_id.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_begin_does_not_overwrite_active_sender() {
        let state = RecorderState::new();
        let (rx, session_id) = begin_recording(&state).expect("first begin should arm recorder");
        assert_ne!(session_id, 0);
        assert!(state.recording.load(Ordering::Acquire));

        assert!(begin_recording(&state).is_none());
        let sender_present = state.sender.lock().unwrap().is_some();
        assert!(sender_present, "second begin must preserve active sender");

        state
            .sender
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .1
            .send(RecordOutcome::Cancelled)
            .unwrap();
        assert!(matches!(rx.recv().unwrap(), RecordOutcome::Cancelled));
    }

    #[test]
    fn completed_session_stays_exclusive_until_owner_cleanup() {
        let state = RecorderState::new();
        let (rx, first_id) = begin_recording(&state).expect("first begin should arm recorder");

        finish(&state, first_id, RecordOutcome::Cancelled);
        assert!(matches!(rx.recv().unwrap(), RecordOutcome::Cancelled));
        assert!(
            state.recording.load(Ordering::Acquire),
            "finish must not publish idle before owner cleanup"
        );
        assert!(
            begin_recording(&state).is_none(),
            "a new session must not enter the old cleanup window"
        );

        cleanup_session(&state, first_id);
        let (_, second_id) = begin_recording(&state).expect("begin should work after cleanup");
        assert_ne!(first_id, second_id);
        cleanup_session(&state, second_id);
    }

    #[test]
    fn stale_session_cannot_finish_current_sender() {
        let state = RecorderState::new();
        let (_, first_id) = begin_recording(&state).expect("first begin should arm recorder");
        cleanup_session(&state, first_id);
        let (second_rx, second_id) =
            begin_recording(&state).expect("second begin should arm recorder");

        finish(&state, first_id, RecordOutcome::Cancelled);
        assert!(matches!(
            second_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert_eq!(state.active_session_id.load(Ordering::Acquire), second_id);
        cleanup_session(&state, second_id);
    }
}
