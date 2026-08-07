//! 全局热键：平台接口 + 通用逻辑。
//!
//! 平台特定实现（如 Windows WH_KEYBOARD_LL）在对应平台模块中。
//!
//! 新状态机（`state.rs` reducer）是物理键/tap/hold/Chord exclusive 的唯一决策者。
//! 前端通过 `INPUT_STATE_CHANGED` 事件 + `register_main_input_view` 快照同步，
//! 不再轮询或反向控制 Alt/Chord。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

use tokio::sync::mpsc;

// 纯输入状态机
mod state;

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows;

// 快捷键录制
mod recorder;
pub use recorder::record_hotkey_blocking;

// ── 公开类型重导出 ────────────────────────────────────────────────────────────

pub use state::{
    HookKeyEvent, InputConfigSnapshot, InputEffect, InputEvent, InputSource, InputState,
    InputUiState, MainViewContext, NormalizedHotkey, Propagation, RecorderMode, VoicePhase,
    WindowTransitionReason,
};

// ── Effect channel（hook 线程 → 主线程）──────────────────────────────────────

/// Effect sender 全局（hook 线程写，start() 时初始化）。
static EFFECT_SENDER: OnceLock<mpsc::UnboundedSender<InputEffect>> = OnceLock::new();

/// 从 hook 线程发送 effect 到主线程。
pub fn send_effect(effect: InputEffect) {
    if let Some(tx) = EFFECT_SENDER.get() {
        let _ = tx.send(effect);
    }
}

// ── 最新 UI 状态快照（hook 线程写，主线程读）──────────────────────────────
//
// hook 线程在每次 reduce 后用原子 store 更新，主线程通过 `get_latest_ui_state()` 读取。
// 使用原子而非 RwLock：Hook 热路径无锁铁则。
// 四个原子非单次原子读，但 UI 状态不需要线性一致性——偶尔读到 revision 新但
// alt_down 旧只是让前端多渲染一帧，下次事件即修正。

static LATEST_ALT_DOWN: AtomicBool = AtomicBool::new(false);
static LATEST_WINDOW_VISIBLE: AtomicBool = AtomicBool::new(false);
static LATEST_CHORD_ACTIVE: AtomicBool = AtomicBool::new(false);
static LATEST_UI_REVISION: AtomicU64 = AtomicU64::new(0);

/// hook 线程更新最新 UI 状态（在每次 reduce 后调用）。
pub fn set_latest_ui_state(ui: &InputUiState) {
    LATEST_ALT_DOWN.store(ui.alt_down, Ordering::SeqCst);
    LATEST_WINDOW_VISIBLE.store(ui.window_visible, Ordering::SeqCst);
    LATEST_CHORD_ACTIVE.store(ui.exclusive_chord_active, Ordering::SeqCst);
    LATEST_UI_REVISION.store(ui.revision, Ordering::SeqCst);
}

/// 读取最新 UI 状态快照（主线程调用，如 `register_main_input_view` command）。
pub fn get_latest_ui_state() -> InputUiState {
    InputUiState {
        revision: LATEST_UI_REVISION.load(Ordering::SeqCst),
        alt_down: LATEST_ALT_DOWN.load(Ordering::SeqCst),
        window_visible: LATEST_WINDOW_VISIBLE.load(Ordering::SeqCst),
        exclusive_chord_active: LATEST_CHORD_ACTIVE.load(Ordering::SeqCst),
    }
}

// ── Config snapshot 存储（供 windows.rs 初始化用）─────────────────────────────

static CONFIG_SNAPSHOT: OnceLock<RwLock<InputConfigSnapshot>> = OnceLock::new();

fn ensure_config_snapshot() -> &'static RwLock<InputConfigSnapshot> {
    CONFIG_SNAPSHOT.get_or_init(|| RwLock::new(InputConfigSnapshot::default()))
}

/// 获取当前配置快照（供平台模块初始化用）。
pub fn get_config_snapshot() -> InputConfigSnapshot {
    ensure_config_snapshot()
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

// ── View epoch 分配（register_main_input_view command 用）────────────────────

static NEXT_VIEW_EPOCH: AtomicU64 = AtomicU64::new(1);

/// 分配一个新的 view_epoch（非 0，递增）。
pub fn alloc_view_epoch() -> u64 {
    NEXT_VIEW_EPOCH.fetch_add(1, Ordering::SeqCst)
}

// ── InputController ──────────────────────────────────────────────────────────

/// 输入控制器 handle（主线程持有，向 hook 线程发送控制消息）。
///
/// 所有方法是非阻塞的：把消息放入控制队列，再 `PostMessageW` 唤醒 hook 线程。
pub struct InputController;

impl InputController {
    /// 更新配置快照（app 层 `refresh_input_config` 调用）。
    pub fn update_config(snapshot: InputConfigSnapshot) {
        if let Ok(mut g) = ensure_config_snapshot().write() {
            *g = snapshot.clone();
        }
        send_control(ControlMsg::Config(snapshot));
    }

    /// 通知窗口可见性变化。
    #[allow(dead_code)]
    pub fn update_window(visible: bool, revision: u64) {
        send_control(ControlMsg::WindowChanged { visible, revision });
    }

    /// 更新前端视图上下文。
    pub fn update_view(ctx: MainViewContext) {
        send_control(ControlMsg::ViewContext(ctx));
    }

    /// 更新语音阶段。
    pub fn update_voice_phase(phase: VoicePhase) {
        send_control(ControlMsg::VoicePhase(phase));
    }

    /// 更新录制模式。
    #[allow(dead_code)]
    pub fn update_recorder(mode: RecorderMode) {
        send_control(ControlMsg::RecorderMode(mode));
    }

    /// 停止输入引擎。
    #[allow(dead_code)]
    pub fn stop() {
        send_control(ControlMsg::Stop);
    }
}

// ── 控制消息 ──────────────────────────────────────────────────────────────────

/// 控制消息（主线程 → hook 线程）。
#[derive(Debug)]
pub(crate) enum ControlMsg {
    Config(InputConfigSnapshot),
    #[allow(dead_code)]
    WindowChanged {
        visible: bool,
        revision: u64,
    },
    ViewContext(MainViewContext),
    VoicePhase(VoicePhase),
    #[allow(dead_code)]
    RecorderMode(RecorderMode),
    #[allow(dead_code)]
    Stop,
}

/// 控制消息队列（主线程写，hook 线程 WindowProc 读）。
static CONTROL_QUEUE: std::sync::Mutex<Vec<ControlMsg>> = std::sync::Mutex::new(Vec::new());

/// 向 hook 线程发送控制消息（主线程调用）。
fn send_control(msg: ControlMsg) {
    if let Ok(mut q) = CONTROL_QUEUE.lock() {
        q.push(msg);
    }
    // PostMessageW 唤醒 hook 线程（平台特定实现）
    #[cfg(target_os = "windows")]
    windows::post_control_wakeup();
}

/// 排空控制队列（hook 线程 WindowProc 调用）。
pub fn drain_control_messages() -> Vec<ControlMsg> {
    let mut q = CONTROL_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *q)
}

// ── 启动/停止 ──────────────────────────────────────────────────────────────────

/// 启动热键引擎，返回 effect 接收端。
///
/// 初始配置通过 `InputController::update_config()` 发送，
/// 通常在 `start()` 返回后立即调用。
pub fn start() -> mpsc::UnboundedReceiver<InputEffect> {
    let (tx, rx) = mpsc::unbounded_channel::<InputEffect>();
    let _ = EFFECT_SENDER.set(tx);

    // 启动平台特定的钩子线程
    #[cfg(target_os = "windows")]
    windows::start_hook_thread();

    rx
}
