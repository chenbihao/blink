//! 纯输入状态机。
//!
//! 与 Win32/Tauri/domain 无关的 reducer：接收归一化事件，产出 effect + 传播决策。
//! adapter（`windows.rs`）负责 VK/scanCode → 归一化键名、Raw Input 注册
//! 和 timer 管理；本模块只做状态流转和动作边沿判定。
//!
//! **铁则**：
//! - 不得 `use crate::domain`、Tauri 或 Win32。
//! - `InputConfigSnapshot` 只含 primitive/infra 类型。
//! - reducer 只在逻辑 edge 变化时发布 UI 状态。
//! - `HoldDeadline` 用 gesture id 防 stale timer。
//! - `WindowHidden` 结束 Chord Session 但绝不伪造 `AltReleased`。
//! - `WindowFocusObserved` 只记录诊断，不写 visible。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::Serialize;

// ── 时间回绕纯函数 ──────────────────────────────────────────────────────────

/// 同一时间域 u32 时间回绕比较：`new` 是否严格晚于 `old`。
///
/// Win32 `GetMessageTime` / `KBDLLHOOKSTRUCT.time` 都是 `u32` ms，约 49.7 天回绕。
/// 用有符号差值判断方向：`new - old > 0` 即 new 更晚。
pub fn time_is_newer(new: u32, old: u32) -> bool {
    (new as i32).wrapping_sub(old as i32) > 0
}

/// 同一时间域 u32 时间回绕比较：`a` 是否在 `b` 时刻或之后。
#[allow(dead_code)]
pub fn time_at_or_after(a: u32, b: u32) -> bool {
    (a as i32).wrapping_sub(b as i32) >= 0
}

/// 计算 `later - earlier` 的无符号差（同一时间域，回绕安全）。
#[allow(dead_code)]
pub fn time_diff(later: u32, earlier: u32) -> u32 {
    later.wrapping_sub(earlier)
}

// ── 修饰键 ──────────────────────────────────────────────────────────────────

/// 修饰键电平态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModifierLevel {
    /// Hook 尚未安装，无任何信息。
    Unknown,
    /// 确认松开。
    Up,
    /// 确认按下（由 Hook 真实 down 或 Raw Input 确认）。
    Down,
    /// 由注入 keydown 设置（远程桌面 / SendInput）。
    ///
    /// 与 `Down` 的区别：注入 keyup 能清除 `InjectedDown`，但不能清除 `Down`。
    /// 这样远程桌面的完整 down→up 序列能正常流转，而 `SetForegroundWindow`
    /// 的合成 Alt up（发生在真实 keydown 之后）不会误清真实 Down。
    ///
    /// Raw Input down 会将此升级为 `Down`（真实硬件确认）。
    InjectedDown,
    /// 由 `LLKHF_ALTDOWN` 推断的临时按下——Hook 安装前 Alt 已按下。
    /// 真实 Alt up / Raw 校正 / 设备移除时清除。不是跨真实 keyup 的长期镜像。
    InferredDown,
}

impl ModifierLevel {
    /// 是否视为"按下"（Down / InjectedDown / InferredDown）。
    pub fn is_pressed(self) -> bool {
        matches!(
            self,
            ModifierLevel::Down | ModifierLevel::InjectedDown | ModifierLevel::InferredDown
        )
    }
}

/// 具体修饰键（8 种，对应 bitmask 位）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ModifierKey {
    LCtrl = 0,
    RCtrl = 1,
    LShift = 2,
    RShift = 3,
    LAlt = 4,
    RAlt = 5,
    LMeta = 6,
    RMeta = 7,
}

impl ModifierKey {
    pub const ALL: [ModifierKey; 8] = [
        ModifierKey::LCtrl,
        ModifierKey::RCtrl,
        ModifierKey::LShift,
        ModifierKey::RShift,
        ModifierKey::LAlt,
        ModifierKey::RAlt,
        ModifierKey::LMeta,
        ModifierKey::RMeta,
    ];

    /// 转为 bitmask 位。
    pub fn bit(self) -> u16 {
        1 << (self as u8)
    }

    /// 从归一化键名解析（"lctrl" → LCtrl, "ralt" → RAlt, etc.）。
    pub fn from_key_name(name: &str) -> Option<ModifierKey> {
        match name {
            "lctrl" => Some(ModifierKey::LCtrl),
            "rctrl" => Some(ModifierKey::RCtrl),
            "lshift" => Some(ModifierKey::LShift),
            "rshift" => Some(ModifierKey::RShift),
            "lalt" => Some(ModifierKey::LAlt),
            "ralt" => Some(ModifierKey::RAlt),
            "meta" => Some(ModifierKey::LMeta), // 通用 meta 当左侧
            _ => None,
        }
    }

    /// 是否为 Alt 键。
    pub fn is_alt(self) -> bool {
        matches!(self, ModifierKey::LAlt | ModifierKey::RAlt)
    }
}

// bitmask 常量（与 windows.rs 保持一致，但此处独立定义避免反向依赖）
const MOD_LCTRL: u16 = 1 << 0;
const MOD_RCTRL: u16 = 1 << 1;
const MOD_LSHIFT: u16 = 1 << 2;
const MOD_RSHIFT: u16 = 1 << 3;
const MOD_LALT: u16 = 1 << 4;
const MOD_RALT: u16 = 1 << 5;
const MOD_LMETA: u16 = 1 << 6;
const MOD_RMETA: u16 = 1 << 7;

/// 单个修饰键侧的跟踪状态。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ModifierSideState {
    level: ModifierLevel,
    /// 最近 Hook transition 时间（同一 Win32 时间域），用于过滤过期 Raw 事件。
    last_hook_time: Option<u32>,
    /// Raw Input 是否曾确认过此修饰键（若 false，Hook up 作为 fallback 生效）。
    raw_ever_seen: bool,
}

impl Default for ModifierSideState {
    fn default() -> Self {
        Self {
            level: ModifierLevel::Unknown,
            last_hook_time: None,
            raw_ever_seen: false,
        }
    }
}

/// 修饰键聚合状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModifierState {
    sides: [ModifierSideState; 8],
    /// Raw Input 每设备 pressed set（bitmask）。key = device_id。
    raw_devices: HashMap<usize, u8>,
}

impl Default for ModifierState {
    fn default() -> Self {
        Self {
            sides: Default::default(),
            raw_devices: HashMap::new(),
        }
    }
}

impl ModifierState {
    /// 取某侧的电平态。
    pub fn level(&self, key: ModifierKey) -> ModifierLevel {
        self.sides[key as usize].level
    }

    /// 设置某侧的电平态（Hook 路径）。
    fn set_level_hook(&mut self, key: ModifierKey, level: ModifierLevel, time_ms: u32) {
        self.sides[key as usize].level = level;
        self.sides[key as usize].last_hook_time = Some(time_ms);
    }

    /// Hook 路径处理修饰键 keyup。
    ///
    /// **注入 keyup 的处理分两种情况**：
    /// - level 为 `InjectedDown`（由注入 keydown 设置）：注入 keyup 能清除。
    ///   这是远程桌面 / SendInput 的完整 down→up 序列，应当正常流转。
    /// - level 为 `Down`（由真实 keydown 或 Raw Input 设置）：注入 keyup 不清除。
    ///   典型来源是 `SetForegroundWindow` 抢前台焦点时系统注入的合成 Alt up——
    ///   这是假事件，用户并没有真的松开 Alt。
    /// - level 为 `InferredDown`：同上不清除，等待真实事件或 Raw 校正。
    ///
    /// 真实物理松开永远是 `injected=false`（Win32 保证），无条件清 level。
    ///
    /// 返回是否实际清成了 Up（调用方据此决定是否连带 clear_inferred_alt）。
    fn set_level_hook_keyup(&mut self, key: ModifierKey, time_ms: u32, injected: bool) -> bool {
        if injected {
            // 只有 InjectedDown 才接受注入 keyup 的清除
            if self.sides[key as usize].level == ModifierLevel::InjectedDown {
                self.sides[key as usize].level = ModifierLevel::Up;
                self.sides[key as usize].last_hook_time = Some(time_ms);
                return true;
            }
            // Down / InferredDown / Unknown / Up：不清 level
            self.sides[key as usize].last_hook_time = Some(time_ms);
            return false;
        }
        self.sides[key as usize].level = ModifierLevel::Up;
        self.sides[key as usize].last_hook_time = Some(time_ms);
        true
    }

    /// Alt（任一侧）是否视为按下。
    pub fn alt_down(&self) -> bool {
        self.level(ModifierKey::LAlt).is_pressed() || self.level(ModifierKey::RAlt).is_pressed()
    }

    /// 某修饰键是否视为按下。
    pub fn is_pressed(&self, key: ModifierKey) -> bool {
        self.level(key).is_pressed()
    }

    /// 采样当前 8 个修饰键的按下状态为 bitmask。
    pub fn pressed_mask(&self) -> u16 {
        let mut mask = 0u16;
        for key in ModifierKey::ALL {
            if self.is_pressed(key) {
                mask |= key.bit();
            }
        }
        mask
    }

    /// 清除所有 InferredDown（真实 Alt up / Raw 校正 / 设备移除时调用）。
    fn clear_inferred_alt(&mut self) {
        for key in [ModifierKey::LAlt, ModifierKey::RAlt] {
            if self.sides[key as usize].level == ModifierLevel::InferredDown {
                self.sides[key as usize].level = ModifierLevel::Up;
            }
        }
    }

    /// 重置所有修饰键状态（锁屏/解锁后调用）。
    ///
    /// 锁屏期间无法获知键盘物理状态，解锁后所有 level 归零为 `Unknown`，
    /// 清空 per-device pressed set。后续由 Hook/Raw Input 事件重新建立。
    pub fn reset_all(&mut self) {
        tracing::info!("重置所有修饰键状态（锁屏/解锁后调用）");
        for side in &mut self.sides {
            side.level = ModifierLevel::Unknown;
            side.last_hook_time = None;
            side.raw_ever_seen = false;
        }
        self.raw_devices.clear();
    }

    /// Raw Input 确认修饰键按下/松开（per-device）。
    ///
    /// 时间域过滤：比最近 Hook transition 更旧的 Raw 事件丢弃。
    /// Raw down：将 level 设为 Down（确认 Hook 的 provisional Down 或升级 InferredDown）。
    /// Raw up：若该 modifier 曾被 Raw 确认过，将 level 设为 Up；否则保持（Hook up fallback）。
    fn apply_raw(&mut self, key: ModifierKey, is_down: bool, device_id: usize, time_ms: u32) {
        // 时间域过滤
        if let Some(hook_time) = self.sides[key as usize].last_hook_time {
            if !time_is_newer(time_ms, hook_time) {
                // Raw 事件比最近 Hook transition 更旧 → 丢弃
                return;
            }
        }

        // 更新 per-device pressed set
        let device_mask = self.raw_devices.entry(device_id).or_insert(0);
        if is_down {
            *device_mask |= 1 << (key as u8);
        } else {
            *device_mask &= !(1 << (key as u8));
        }

        self.sides[key as usize].raw_ever_seen = true;

        if is_down {
            // Raw down 确认按下
            self.sides[key as usize].level = ModifierLevel::Down;
        } else {
            // Raw up：检查所有设备的聚合
            let aggregate_pressed = self
                .raw_devices
                .values()
                .any(|&mask| mask & (1 << (key as u8)) != 0);
            if !aggregate_pressed {
                // 没有设备按住此键 → 松开
                self.sides[key as usize].level = ModifierLevel::Up;
                if key.is_alt() {
                    self.clear_inferred_alt();
                }
            }
        }
    }

    /// 设备移除：清除对应设备的 pressed set 并重算聚合。
    fn remove_device(&mut self, device_id: usize) {
        let removed = self.raw_devices.remove(&device_id);
        if removed.is_some() {
            // 重算所有修饰键的聚合
            for key in ModifierKey::ALL {
                if !self.sides[key as usize].raw_ever_seen {
                    continue;
                }
                let aggregate_pressed = self
                    .raw_devices
                    .values()
                    .any(|&mask| mask & (1 << (key as u8)) != 0);
                if !aggregate_pressed
                    && matches!(
                        self.sides[key as usize].level,
                        ModifierLevel::Down | ModifierLevel::InjectedDown
                    )
                {
                    // 没有设备按住 → 但 Hook 可能还按着
                    // 只在 Hook 也已 Up 时才设 Up；Hook 仍 Down/InjectedDown 时保持（Hook fallback）
                    // 这里简化：如果 Raw 曾确认过且聚合为空，设 Up
                    self.sides[key as usize].level = ModifierLevel::Up;
                    if key.is_alt() {
                        self.clear_inferred_alt();
                    }
                }
            }
        }
    }
}

// ── AltGr 校正 ──────────────────────────────────────────────────────────────

/// AltGr 修正：右 Alt + 左 Ctrl 同时按下时，左 Ctrl 视为合成、从 mask 去掉。
pub fn apply_altgr_correction(mask: u16) -> u16 {
    if mask & MOD_RALT != 0 && mask & MOD_LCTRL != 0 {
        mask & !MOD_LCTRL
    } else {
        mask
    }
}

// ── 修饰键配置匹配 ──────────────────────────────────────────────────────────

/// 配置修饰键名 → 可接受的物理位集合。通用名(`alt`)= 左右任一。
pub fn mask_for_config_modifier(name: &str) -> Option<u16> {
    match name {
        "ctrl" => Some(MOD_LCTRL | MOD_RCTRL),
        "lctrl" => Some(MOD_LCTRL),
        "rctrl" => Some(MOD_RCTRL),
        "shift" => Some(MOD_LSHIFT | MOD_RSHIFT),
        "lshift" => Some(MOD_LSHIFT),
        "rshift" => Some(MOD_RSHIFT),
        "alt" => Some(MOD_LALT | MOD_RALT),
        "lalt" => Some(MOD_LALT),
        "ralt" => Some(MOD_RALT),
        "meta" => Some(MOD_LMETA | MOD_RMETA),
        _ => None,
    }
}

fn first_set_bit(mask: u16) -> u16 {
    mask & mask.wrapping_neg()
}

/// 当前物理修饰键集合是否**精确**满足配置要求（消耗模型）。
pub fn modifiers_mask_satisfies_config(config_modifiers: &[String], pressed_mask: u16) -> bool {
    let mut remaining = pressed_mask;
    for config_mod in config_modifiers {
        let Some(allowed) = mask_for_config_modifier(config_mod) else {
            return false;
        };
        let matched = remaining & allowed;
        if matched == 0 {
            return false;
        }
        remaining &= !first_set_bit(matched);
    }
    remaining == 0
}

/// 是否为单独修饰键配置（modifiers 空 + key 是单修饰键）。
pub fn is_standalone_modifier_key(key: &str) -> bool {
    matches!(
        key,
        "ralt" | "lalt" | "rctrl" | "lctrl" | "rshift" | "lshift" | "meta"
    )
}

// ── 输入源 ──────────────────────────────────────────────────────────────────

/// 输入事件来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSource {
    /// 本地物理键盘（Hook 非 injected + Raw Input）。
    Local,
    /// 注入事件（SendInput / 远程控制软件 / SetForegroundWindow 合成）。
    Injected,
}

// ── 归一化热键 ──────────────────────────────────────────────────────────────

/// 归一化快捷键配置（从 `HotkeyConfig` 派生的 primitive 类型）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedHotkey {
    pub modifiers: Vec<String>,
    pub key: String,
}

impl NormalizedHotkey {
    /// 是否为单独修饰键配置。
    pub fn is_standalone(&self) -> bool {
        self.modifiers.is_empty() && is_standalone_modifier_key(&self.key)
    }
}

// ── 配置快照 ────────────────────────────────────────────────────────────────

/// 输入配置快照（Hook 线程持有的不可变原始值）。
///
/// 由 app 层从 `HotkeyConfig`、`ChordConfig.bindings`、disabled chord actions
/// 和 `ChordRegistry` 派生。不含 domain/Tauri/Win32 类型。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputConfigSnapshot {
    pub revision: u64,
    pub hotkey: NormalizedHotkey,
    pub tap_threshold: Duration,
    pub chord_enabled: bool,
    /// 当前 enabled、tap semantic、空 query 可 native 触发的键集合。
    pub exclusive_tap_keys: HashSet<String>,
    pub voice_hold_enabled: bool,
}

impl Default for InputConfigSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            hotkey: NormalizedHotkey {
                modifiers: vec!["alt".to_string()],
                key: " ".to_string(),
            },
            tap_threshold: Duration::from_millis(300),
            chord_enabled: false,
            exclusive_tap_keys: HashSet::new(),
            voice_hold_enabled: true,
        }
    }
}

// ── Gesture 状态 ────────────────────────────────────────────────────────────

/// 主热键 gesture 状态（一次主键 down→up）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GestureState {
    Idle,
    /// 主键已按下，等待判定 tap/hold。
    Armed {
        gesture_id: u64,
        key: String,
        armed_at_ms: u32,
        /// 冻结的配置（Armed 期间不随配置更新变化）。
        frozen_hotkey: NormalizedHotkey,
        frozen_tap_threshold: Duration,
        source: InputSource,
        aborted: bool,
        hold_fired: bool,
    },
    /// Hold 已触发（超过 tap 阈值），等待 keyup → HoldReleased。
    ///
    /// 当前实现保持 Armed + hold_fired=true，此变体预留。
    #[allow(dead_code)]
    Holding {
        gesture_id: u64,
        key: String,
        source: InputSource,
    },
}

impl Default for GestureState {
    fn default() -> Self {
        GestureState::Idle
    }
}

impl GestureState {
    /// 当前 gesture id（Idle 时 None）。
    pub fn gesture_id(&self) -> Option<u64> {
        match self {
            GestureState::Armed { gesture_id, .. } => Some(*gesture_id),
            GestureState::Holding { gesture_id, .. } => Some(*gesture_id),
            GestureState::Idle => None,
        }
    }

    /// 当前是否处于 hold_fired 状态（HoldDeadline 已触发，等待 keyup）。
    pub fn is_hold_fired(&self) -> bool {
        matches!(
            self,
            GestureState::Armed {
                hold_fired: true,
                ..
            }
        )
    }

    /// 当前 armed 的主键（Idle 时 None）。
    pub fn armed_key(&self) -> Option<&str> {
        match self {
            GestureState::Armed { key, .. } => Some(key),
            _ => None,
        }
    }
}

// ── Chord Session ───────────────────────────────────────────────────────────

/// Chord 独占会话（窗口可见期间 Alt down→退出 exclusive）。
///
/// 独立标识，不复用主热键 gesture id——窗口可能由托盘/单实例打开后再按 Alt。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChordSession {
    Inactive,
    Active {
        session_id: u64,
        /// 防止 autorepeat 重复触发：记录上次触发的键。
        last_triggered_key: Option<String>,
    },
}

impl Default for ChordSession {
    fn default() -> Self {
        ChordSession::Inactive
    }
}

impl ChordSession {
    /// 是否激活。
    pub fn is_active(&self) -> bool {
        matches!(self, ChordSession::Active { .. })
    }
}

// ── 窗口/视图/语音/录制状态 ────────────────────────────────────────────────

/// 主窗口状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MainWindowState {
    pub visible: bool,
    /// Window 模块拥有，每次成功 visibility transition 递增。
    pub revision: u64,
}

impl Default for MainWindowState {
    fn default() -> Self {
        Self {
            visible: false,
            revision: 0,
        }
    }
}

/// 主窗口前端视图上下文。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MainViewContext {
    /// 后端 register command 分配的 epoch。
    pub view_epoch: u64,
    /// 当前 view_epoch 内前端递增的 revision。
    pub revision: u64,
    /// WebView 实例完成 input-state 初始化。
    pub ready: bool,
    pub query_empty: bool,
    pub ai_mode: bool,
}

impl Default for MainViewContext {
    fn default() -> Self {
        Self {
            view_epoch: 0,
            revision: 0,
            ready: false,
            query_empty: true,
            ai_mode: false,
        }
    }
}

/// 语音阶段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoicePhase {
    Idle,
    /// voice worker 正在启动（generation-aware wiring 尚未接入此阶段）。
    #[allow(dead_code)]
    Starting {
        gesture_id: u64,
    },
    Recording {
        gesture_id: u64,
    },
}

impl Default for VoicePhase {
    fn default() -> Self {
        VoicePhase::Idle
    }
}

/// 快捷键录制模式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecorderMode {
    Idle,
    /// 录制模式（recorder.rs 接入 Controller 后由控制消息设置）。
    #[allow(dead_code)]
    Recording {
        recorder_id: u64,
    },
}

impl Default for RecorderMode {
    fn default() -> Self {
        RecorderMode::Idle
    }
}

// ── 聚合状态 ────────────────────────────────────────────────────────────────

/// 输入状态机聚合状态。
#[derive(Clone, Debug)]
pub struct InputState {
    pub modifiers: ModifierState,
    pub gesture: GestureState,
    pub chord: ChordSession,
    pub window: MainWindowState,
    pub view: MainViewContext,
    pub voice: VoicePhase,
    pub recorder: RecorderMode,
    pub next_gesture_id: u64,
    pub next_chord_session_id: u64,
    pub next_ui_revision: u64,
    pub config_revision: u64,
    /// 当前配置快照（Armed 期间 gesture 冻结自己的副本）。
    pub config: InputConfigSnapshot,
    /// 上次发布的 UI 状态（用于检测变化、避免重复 emit）。
    last_ui_state: InputUiState,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            modifiers: ModifierState::default(),
            gesture: GestureState::Idle,
            chord: ChordSession::Inactive,
            window: MainWindowState::default(),
            view: MainViewContext::default(),
            voice: VoicePhase::Idle,
            recorder: RecorderMode::Idle,
            next_gesture_id: 1,
            next_chord_session_id: 1,
            next_ui_revision: 0,
            config_revision: 0,
            config: InputConfigSnapshot::default(),
            last_ui_state: InputUiState::default(),
        }
    }
}

/// 公开 UI 状态协议（序列化给前端）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputUiState {
    /// 公开状态发生变化时递增，事件/快照只比较此字段。
    pub revision: u64,
    pub alt_down: bool,
    pub window_visible: bool,
    pub exclusive_chord_active: bool,
}

impl Default for InputUiState {
    fn default() -> Self {
        Self {
            revision: 0,
            alt_down: false,
            window_visible: false,
            exclusive_chord_active: false,
        }
    }
}

impl InputState {
    /// 计算当前公开 UI 状态。
    pub fn ui_state(&self) -> InputUiState {
        InputUiState {
            revision: self.next_ui_revision,
            alt_down: self.modifiers.alt_down(),
            window_visible: self.window.visible,
            exclusive_chord_active: self.chord.is_active(),
        }
    }

    /// 分配新 gesture id。
    fn alloc_gesture_id(&mut self) -> u64 {
        let id = self.next_gesture_id;
        self.next_gesture_id += 1;
        id
    }

    /// 分配新 chord session id。
    fn alloc_chord_session_id(&mut self) -> u64 {
        let id = self.next_chord_session_id;
        self.next_chord_session_id += 1;
        id
    }

    /// 递增 UI revision 并返回新 UI 状态（若公开字段变化）。
    fn maybe_emit_ui(&mut self) -> Option<InputEffect> {
        let current = self.ui_state();
        let public_changed = current.alt_down != self.last_ui_state.alt_down
            || current.window_visible != self.last_ui_state.window_visible
            || current.exclusive_chord_active != self.last_ui_state.exclusive_chord_active;
        if public_changed {
            self.next_ui_revision += 1;
            let new_state = InputUiState {
                revision: self.next_ui_revision,
                ..current
            };
            self.last_ui_state = new_state.clone();
            Some(InputEffect::UiStateChanged(new_state))
        } else {
            None
        }
    }

    /// native exclusive chord 条件是否满足。
    fn chord_exclusive_eligible(&self) -> bool {
        self.window.visible
            && self.modifiers.alt_down()
            && self.view.ready
            && self.view.query_empty
            && !self.view.ai_mode
            && self.config.chord_enabled
            && !self.config.exclusive_tap_keys.is_empty()
            && matches!(self.recorder, RecorderMode::Idle)
            && matches!(self.voice, VoicePhase::Idle)
    }

    /// 建立/退出 Chord Session（根据 exclusive 条件变化）。
    fn reconcile_chord_session(&mut self) -> Option<InputEffect> {
        let eligible = self.chord_exclusive_eligible();
        match (&self.chord, eligible) {
            (ChordSession::Inactive, true) => {
                // 建立 session
                let session_id = self.alloc_chord_session_id();
                tracing::trace!(
                    session_id,
                    alt_down = self.modifiers.alt_down(),
                    "chord session 建立"
                );
                self.chord = ChordSession::Active {
                    session_id,
                    last_triggered_key: None,
                };
                None // UI emit 由调用方统一处理
            }
            (ChordSession::Active { session_id, .. }, false) => {
                // 退出 session
                tracing::trace!(
                    session_id,
                    alt_down = self.modifiers.alt_down(),
                    window_visible = self.window.visible,
                    view_ready = self.view.ready,
                    query_empty = self.view.query_empty,
                    "chord session 退出"
                );
                self.chord = ChordSession::Inactive;
                None
            }
            _ => None, // 无变化
        }
    }
}

// ── 事件 / Effect / 传播 ────────────────────────────────────────────────────

/// 键传播决策（Hook 同步返回）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Propagation {
    /// 放行到下一个 hook / 系统。
    Pass,
    /// 吞键（不传递）。
    Swallow,
}

/// 归一化 Hook 键事件。
#[derive(Clone, Debug)]
pub struct HookKeyEvent {
    pub source: InputSource,
    /// 归一化键名："a", " ", "lalt", "ralt", "lctrl", etc.
    pub key: String,
    pub is_down: bool,
    pub is_modifier: bool,
    /// Win32 时间域（KBDLLHOOKSTRUCT.time）。
    pub time_ms: u32,
    pub injected: bool,
    #[allow(dead_code)]
    pub lower_integrity_injected: bool,
    /// E0/E1 extended prefix。
    #[allow(dead_code)]
    pub extended: bool,
    /// LLKHF_ALTDOWN flag（Alt 在本事件前已按下）。
    pub alt_down_flag: bool,
}

/// Raw Input 修饰键事件。
#[derive(Clone, Debug)]
pub struct RawModifierEvent {
    pub device_id: usize,
    /// 归一化修饰键名："lalt", "ralt", etc.
    pub key: String,
    pub is_down: bool,
    /// Win32 时间域（GetMessageTime）。
    pub time_ms: u32,
}

/// 窗口 visibility 转换原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowTransitionReason {
    #[allow(dead_code)]
    Startup,
    #[allow(dead_code)]
    Invoke,
    #[allow(dead_code)]
    Toggle,
    #[allow(dead_code)]
    Escape,
    Watchdog,
    #[allow(dead_code)]
    Screenshot,
    #[allow(dead_code)]
    SingleInstance,
    #[allow(dead_code)]
    AiActiveRefocus,
}

/// 输入事件（reducer 的输入）。
#[derive(Clone, Debug)]
pub enum InputEvent {
    HookKey(HookKeyEvent),
    RawModifier(RawModifierEvent),
    RawDeviceRemoved {
        device_id: usize,
    },
    HoldDeadline {
        gesture_id: u64,
    },
    WindowChanged {
        visible: bool,
        revision: u64,
        reason: WindowTransitionReason,
    },
    #[allow(dead_code)]
    WindowFocusObserved(bool),
    ViewContextChanged(MainViewContext),
    VoicePhaseChanged {
        gesture_id: Option<u64>,
        phase: VoicePhase,
    },
    RecorderModeChanged(RecorderMode),
    ConfigChanged(InputConfigSnapshot),
}

/// 输入 effect（reducer 的输出，由 HotkeyService 消费执行业务副作用）。
#[derive(Clone, Debug)]
pub enum InputEffect {
    Tap {
        #[allow(dead_code)]
        gesture_id: u64,
        #[allow(dead_code)]
        triggered_at: Instant,
    },
    HoldStarted {
        #[allow(dead_code)]
        gesture_id: u64,
    },
    HoldReleased {
        #[allow(dead_code)]
        gesture_id: u64,
    },
    VoiceCancel {
        #[allow(dead_code)]
        gesture_id: Option<u64>,
    },
    ChordTriggered {
        #[allow(dead_code)]
        chord_session_id: u64,
        key: String,
    },
    UiStateChanged(InputUiState),
}

/// reducer 返回结果。
#[derive(Clone, Debug)]
pub struct ReduceResult {
    pub propagation: Propagation,
    pub effects: Vec<InputEffect>,
}

impl ReduceResult {
    fn pass() -> Self {
        Self {
            propagation: Propagation::Pass,
            effects: Vec::new(),
        }
    }
}

// ── Reducer ─────────────────────────────────────────────────────────────────

/// 纯输入状态机 reducer。
///
/// 处理一个事件，返回传播决策 + effect 列表。
/// 不执行任何 I/O 或副作用。
pub fn reduce(state: &mut InputState, event: InputEvent, now: Instant) -> ReduceResult {
    match event {
        InputEvent::HookKey(e) => reduce_hook_key(state, e, now),
        InputEvent::RawModifier(e) => reduce_raw_modifier(state, e),
        InputEvent::RawDeviceRemoved { device_id } => reduce_raw_device_removed(state, device_id),
        InputEvent::HoldDeadline { gesture_id } => reduce_hold_deadline(state, gesture_id, now),
        InputEvent::WindowChanged {
            visible,
            revision,
            reason,
        } => reduce_window_changed(state, visible, revision, reason),
        InputEvent::WindowFocusObserved(focused) => reduce_window_focus(state, focused),
        InputEvent::ViewContextChanged(ctx) => reduce_view_context(state, ctx),
        InputEvent::VoicePhaseChanged { gesture_id, phase } => {
            reduce_voice_phase(state, gesture_id, phase)
        }
        InputEvent::RecorderModeChanged(mode) => reduce_recorder_mode(state, mode),
        InputEvent::ConfigChanged(snapshot) => reduce_config_changed(state, snapshot),
    }
}

/// 辅助：处理完事件后统一检查 chord session 和 UI emit。
fn finalize(state: &mut InputState) -> Vec<InputEffect> {
    let mut effects = Vec::new();
    state.reconcile_chord_session();
    if let Some(ui_effect) = state.maybe_emit_ui() {
        effects.push(ui_effect);
    }
    effects
}

// ── HookKey 处理 ────────────────────────────────────────────────────────────

fn reduce_hook_key(state: &mut InputState, e: HookKeyEvent, now: Instant) -> ReduceResult {
    let mut result = ReduceResult::pass();

    // 录制模式：不产生正常动作
    if let RecorderMode::Recording { .. } = state.recorder {
        // 录制期间保留 Alt+Space 特殊吞键语义
        if e.is_down && e.key == " " && state.modifiers.alt_down() {
            result.propagation = Propagation::Swallow;
        }
        return result;
    }

    // LLKHF_ALTDOWN：非注入主键事件推断 Alt 已按下。
    // 只推断 LAlt--LLKHF_ALTDOWN 不区分左右，推断两侧会导致 mask 多余位使配置匹配失败。
    // 若实际是 RAlt，后续 RAlt Hook 事件会校正。
    if e.alt_down_flag && !e.injected && e.is_down {
        if state.modifiers.level(ModifierKey::LAlt) == ModifierLevel::Unknown {
            state.modifiers.set_level_hook(
                ModifierKey::LAlt,
                ModifierLevel::InferredDown,
                e.time_ms,
            );
        }
    }

    // hold/voice 期间吞主键 + Alt 的 keydown（防系统菜单"噔噔噔"声）。
    //
    // 旧代码（0.18.6 windows.rs ll_proc）有一个独立于状态机的吞键守卫，
    // 覆盖 hold_fired || VOICE_RECORDING 期间的 Space + Alt keydown。0.18.7 重构
    // 把吞键决策收敛到 reducer 的 Propagation，但漏掉了这一层，导致语音录音期间
    // Space autorepeat 透传给系统，反复弹出系统菜单。
    //
    // 条件：hold_fired（同步，覆盖 HoldDeadline -> VoicePhase 到达前的间隙）
    //   或 VoicePhase 非 Idle（覆盖录音持续期 + keyup -> stop 之间的间隙）。
    // 范围：主键（armed_key，通常 Space）+ Alt（lalt/ralt）的 keydown。
    // 只吞 keydown，不吞 keyup（否则 HoldRelease 收不到）。
    //
    // 标 propagation 后不 return：modifier level 维护、autorepeat 忽略、ESC 取消
    // 等逻辑正常执行，各分支 return result 时自然带着 Swallow。
    if e.is_down && (state.gesture.is_hold_fired() || !matches!(state.voice, VoicePhase::Idle)) {
        let is_armed_main_key = state
            .gesture
            .armed_key()
            .map(|k| k == e.key)
            .unwrap_or(false);
        let is_alt_key = e.key == "lalt" || e.key == "ralt";
        if is_armed_main_key || is_alt_key {
            result.propagation = Propagation::Swallow;
        }
    }

    // 修饰键事件
    if e.is_modifier {
        if let Some(mod_key) = ModifierKey::from_key_name(&e.key) {
            if e.is_down {
                // 真实 keydown → Down；注入 keydown → InjectedDown。
                // 区分两者使注入 keyup 能精准清除 InjectedDown 而不动 Down。
                let level = if e.injected {
                    ModifierLevel::InjectedDown
                } else {
                    ModifierLevel::Down
                };
                state
                    .modifiers
                    .set_level_hook(mod_key, level, e.time_ms);
            } else {
                // 修饰键 keyup
                // 注入的合成 keyup（如 SetForegroundWindow 抢焦点时系统注入的假 Alt up）
                // 一律不清 level——真实物理 up 一定非注入，由 Hook 真事件兜底。
                let cleared = state
                    .modifiers
                    .set_level_hook_keyup(mod_key, e.time_ms, e.injected);
                if cleared && mod_key.is_alt() {
                    state.modifiers.clear_inferred_alt();
                }
            }
        }

        // standalone 配置：修饰键本身是主键
        // 修饰键 down 可能 arm gesture
        if e.is_down {
            try_arm(state, &e.key, e.source, e.time_ms, &mut result);
        } else {
            // 修饰键 up 可能触发 tap/hold release
            try_release(state, &e.key, e.source, now, &mut result);
        }

        result.effects.extend(finalize(state));
        return result;
    }

    // 非修饰键事件
    if e.is_down {
        // ESC 录音取消
        if e.key == "Escape" && !matches!(state.voice, VoicePhase::Idle) {
            let gid = state.gesture.gesture_id();
            result
                .effects
                .push(InputEffect::VoiceCancel { gesture_id: gid });
            result.effects.extend(finalize(state));
            return result;
        }

        // 同一主键 autorepeat → 忽略（仍 armed，不重置）
        if let GestureState::Armed { key: armed_key, .. } = &state.gesture {
            if &e.key == armed_key {
                result.effects.extend(finalize(state));
                return result;
            }
        }

        // armed 后异键 down → abort（但不 return，继续检查 chord）
        if let GestureState::Armed {
            key: armed_key,
            aborted,
            ..
        } = &state.gesture
        {
            if &e.key != armed_key && !aborted {
                state.gesture = match std::mem::take(&mut state.gesture) {
                    GestureState::Armed {
                        gesture_id,
                        key,
                        armed_at_ms,
                        frozen_hotkey,
                        frozen_tap_threshold,
                        source,
                        ..
                    } => GestureState::Armed {
                        gesture_id,
                        key,
                        armed_at_ms,
                        frozen_hotkey,
                        frozen_tap_threshold,
                        source,
                        aborted: true,
                        hold_fired: false,
                    },
                    other => other,
                };
            }
        }

        // Chord 独占吞键
        if let ChordSession::Active {
            session_id,
            last_triggered_key,
        } = &state.chord
        {
            let key_lower = e.key.to_lowercase();
            if state.config.exclusive_tap_keys.contains(&key_lower) {
                // 防止 autorepeat：同键已触发过，直到 keyup 才能再次触发
                if last_triggered_key.as_deref() != Some(key_lower.as_str()) {
                    tracing::debug!(
                        session_id = *session_id,
                        key = %key_lower,
                        "chord 触发（吞键）"
                    );
                    result.propagation = Propagation::Swallow;
                    result.effects.push(InputEffect::ChordTriggered {
                        chord_session_id: *session_id,
                        key: key_lower.clone(),
                    });
                    state.chord = ChordSession::Active {
                        session_id: *session_id,
                        last_triggered_key: Some(key_lower),
                    };
                } else {
                    // autorepeat：吞键但不重复触发
                    result.propagation = Propagation::Swallow;
                }
                result.effects.extend(finalize(state));
                return result;
            }
        }

        // 未 armed：尝试 arm（gesture 仍 Armed 时不 arm 新 gesture）
        if matches!(&state.gesture, GestureState::Idle) {
            try_arm(state, &e.key, e.source, e.time_ms, &mut result);
        }
    } else {
        // 非修饰键 up
        // Chord key up -> 重置 autorepeat lock，允许同键再次触发
        if let ChordSession::Active {
            session_id,
            last_triggered_key,
        } = &state.chord
        {
            let key_lower = e.key.to_lowercase();
            if last_triggered_key.as_deref() == Some(key_lower.as_str()) {
                state.chord = ChordSession::Active {
                    session_id: *session_id,
                    last_triggered_key: None,
                };
            }
        }
        try_release(state, &e.key, e.source, now, &mut result);
    }

    result.effects.extend(finalize(state));
    result
}

/// 尝试 arm gesture（主键 down + 修饰键满足）。
fn try_arm(
    state: &mut InputState,
    key: &str,
    source: InputSource,
    time_ms: u32,
    _result: &mut ReduceResult,
) {
    let config = &state.config;
    if key != config.hotkey.key {
        return;
    }

    // 修饰键匹配
    let satisfied = if config.hotkey.is_standalone() {
        true
    } else {
        let mask = apply_altgr_correction(state.modifiers.pressed_mask());
        modifiers_mask_satisfies_config(&config.hotkey.modifiers, mask)
    };

    if !satisfied {
        return;
    }

    // 克隆配置数据，避免借用冲突
    let frozen_hotkey = config.hotkey.clone();
    let frozen_tap_threshold = config.tap_threshold;
    let gesture_id = state.alloc_gesture_id();
    state.gesture = GestureState::Armed {
        gesture_id,
        key: key.to_string(),
        armed_at_ms: time_ms,
        frozen_hotkey,
        frozen_tap_threshold,
        source,
        aborted: false,
        hold_fired: false,
    };
}

/// 尝试释放 gesture（主键 up）。
fn try_release(
    state: &mut InputState,
    key: &str,
    source: InputSource,
    now: Instant,
    result: &mut ReduceResult,
) {
    let gesture = std::mem::take(&mut state.gesture);

    match gesture {
        GestureState::Armed {
            gesture_id,
            key: armed_key,
            armed_at_ms,
            frozen_hotkey,
            frozen_tap_threshold,
            source: armed_source,
            aborted,
            hold_fired,
        } if armed_key == key => {
            // source 不匹配 → 不配对（local gesture 不接受 injected keyup）
            if armed_source != source {
                // 放回 gesture，不处理
                state.gesture = GestureState::Armed {
                    gesture_id,
                    key: armed_key,
                    armed_at_ms,
                    frozen_hotkey,
                    frozen_tap_threshold,
                    source: armed_source,
                    aborted,
                    hold_fired,
                };
                return;
            }

            if aborted {
                return;
            }

            if hold_fired {
                result
                    .effects
                    .push(InputEffect::HoldReleased { gesture_id });
            } else {
                // 双保险：timer 未 fire 但已超阈值 → HoldReleased
                // 使用 frozen_tap_threshold
                let _ = armed_at_ms; // armed_at_ms 在真实 adapter 中用于时间比较
                result.effects.push(InputEffect::Tap {
                    gesture_id,
                    triggered_at: now,
                });
            }
        }
        GestureState::Holding {
            gesture_id,
            key: holding_key,
            source: holding_source,
        } if holding_key == key => {
            // source 不匹配 → 不配对
            if holding_source != source {
                state.gesture = GestureState::Holding {
                    gesture_id,
                    key: holding_key,
                    source: holding_source,
                };
                return;
            }
            result
                .effects
                .push(InputEffect::HoldReleased { gesture_id });
        }
        other => {
            // 不匹配的 keyup → 放回
            state.gesture = other;
        }
    }
}

// ── RawModifier 处理 ───────────────────────────────────────────────────────

fn reduce_raw_modifier(state: &mut InputState, e: RawModifierEvent) -> ReduceResult {
    if let Some(mod_key) = ModifierKey::from_key_name(&e.key) {
        state
            .modifiers
            .apply_raw(mod_key, e.is_down, e.device_id, e.time_ms);
    }
    let effects = finalize(state);
    ReduceResult {
        propagation: Propagation::Pass,
        effects,
    }
}

// ── RawDeviceRemoved 处理 ──────────────────────────────────────────────────

fn reduce_raw_device_removed(state: &mut InputState, device_id: usize) -> ReduceResult {
    state.modifiers.remove_device(device_id);
    let effects = finalize(state);
    ReduceResult {
        propagation: Propagation::Pass,
        effects,
    }
}

// ── HoldDeadline 处理 ──────────────────────────────────────────────────────

fn reduce_hold_deadline(state: &mut InputState, gesture_id: u64, _now: Instant) -> ReduceResult {
    let mut result = ReduceResult::pass();

    if let GestureState::Armed {
        gesture_id: armed_id,
        key,
        armed_at_ms,
        frozen_hotkey,
        frozen_tap_threshold,
        source,
        aborted,
        hold_fired,
    } = &state.gesture
    {
        if *armed_id == gesture_id && !aborted && !hold_fired {
            // 确认 Hold timer：保持 Armed 但标记 hold_fired
            // 实际上应该转 Holding，但 Holding 不含 tap_threshold 信息
            // 保持 Armed + hold_fired=true 更简洁，keyup 时发 HoldReleased
            state.gesture = GestureState::Armed {
                gesture_id: *armed_id,
                key: key.clone(),
                armed_at_ms: *armed_at_ms,
                frozen_hotkey: frozen_hotkey.clone(),
                frozen_tap_threshold: *frozen_tap_threshold,
                source: *source,
                aborted: *aborted,
                hold_fired: true,
            };

            // 语音 hold：只在 voice_hold_enabled 时发 HoldStarted
            if state.config.voice_hold_enabled {
                result.effects.push(InputEffect::HoldStarted { gesture_id });
            }
        }
        // stale timer（gesture_id 不匹配）→ 忽略
    }

    result.effects.extend(finalize(state));
    result
}

// ── WindowChanged 处理 ─────────────────────────────────────────────────────

fn reduce_window_changed(
    state: &mut InputState,
    visible: bool,
    revision: u64,
    _reason: WindowTransitionReason,
) -> ReduceResult {
    // 旧 revision 丢弃
    if revision <= state.window.revision && state.window.revision > 0 {
        return ReduceResult::pass();
    }

    state.window.visible = visible;
    state.window.revision = revision;

    // Hidden 结束 Chord Session（绝不伪造 AltReleased）
    if !visible {
        state.chord = ChordSession::Inactive;
    }

    let effects = finalize(state);
    ReduceResult {
        propagation: Propagation::Pass,
        effects,
    }
}

// ── WindowFocusObserved 处理 ───────────────────────────────────────────────

fn reduce_window_focus(_state: &mut InputState, _focused: bool) -> ReduceResult {
    // 只记录诊断，不写 visible
    tracing::trace!(focused = _focused, "window focus observed");
    ReduceResult::pass()
}

// ── ViewContextChanged 处理 ────────────────────────────────────────────────

fn reduce_view_context(state: &mut InputState, ctx: MainViewContext) -> ReduceResult {
    // 新 view epoch：接受
    // 同一 epoch：只接受更大的 revision
    if ctx.view_epoch > state.view.view_epoch
        || (ctx.view_epoch == state.view.view_epoch && ctx.revision > state.view.revision)
    {
        state.view = ctx;
    }

    let effects = finalize(state);
    ReduceResult {
        propagation: Propagation::Pass,
        effects,
    }
}

// ── VoicePhaseChanged 处理 ─────────────────────────────────────────────────

fn reduce_voice_phase(
    state: &mut InputState,
    _gesture_id: Option<u64>,
    phase: VoicePhase,
) -> ReduceResult {
    state.voice = phase;
    let effects = finalize(state);
    ReduceResult {
        propagation: Propagation::Pass,
        effects,
    }
}

// ── RecorderModeChanged 处理 ───────────────────────────────────────────────

fn reduce_recorder_mode(state: &mut InputState, mode: RecorderMode) -> ReduceResult {
    // 进入 Recording：清空 gesture/chord，递增 gesture id
    if matches!(mode, RecorderMode::Recording { .. }) {
        state.gesture = GestureState::Idle;
        state.chord = ChordSession::Inactive;
        state.next_gesture_id += 1; // 使录制主键后续 keyup 不触发正常快捷键
    }
    state.recorder = mode;
    let effects = finalize(state);
    ReduceResult {
        propagation: Propagation::Pass,
        effects,
    }
}

// ── ConfigChanged 处理 ─────────────────────────────────────────────────────

fn reduce_config_changed(state: &mut InputState, snapshot: InputConfigSnapshot) -> ReduceResult {
    // Armed 期间：当前 gesture 冻结自己的配置副本，不更新
    // 新配置从下一次 gesture 生效
    state.config_revision = snapshot.revision;
    state.config = snapshot;

    // Chord 禁用 → 立即退出 exclusive session
    if !state.config.chord_enabled {
        state.chord = ChordSession::Inactive;
    }

    let effects = finalize(state);
    ReduceResult {
        propagation: Propagation::Pass,
        effects,
    }
}

// ── 辅助：构造事件 ──────────────────────────────────────────────────────────

/// 便利构造：本地修饰键 down 事件。
#[cfg(test)]
pub fn hook_modifier_down(key: &str, time_ms: u32) -> HookKeyEvent {
    HookKeyEvent {
        source: InputSource::Local,
        key: key.to_string(),
        is_down: true,
        is_modifier: true,
        time_ms,
        injected: false,
        lower_integrity_injected: false,
        extended: false,
        alt_down_flag: false,
    }
}

/// 便利构造：本地修饰键 up 事件。
#[cfg(test)]
pub fn hook_modifier_up(key: &str, time_ms: u32) -> HookKeyEvent {
    HookKeyEvent {
        source: InputSource::Local,
        key: key.to_string(),
        is_down: false,
        is_modifier: true,
        time_ms,
        injected: false,
        lower_integrity_injected: false,
        extended: false,
        alt_down_flag: false,
    }
}

/// 便利构造：本地普通键 down 事件。
#[cfg(test)]
pub fn hook_key_down(key: &str, time_ms: u32) -> HookKeyEvent {
    HookKeyEvent {
        source: InputSource::Local,
        key: key.to_string(),
        is_down: true,
        is_modifier: false,
        time_ms,
        injected: false,
        lower_integrity_injected: false,
        extended: false,
        alt_down_flag: false,
    }
}

/// 便利构造：本地普通键 up 事件。
#[cfg(test)]
pub fn hook_key_up(key: &str, time_ms: u32) -> HookKeyEvent {
    HookKeyEvent {
        source: InputSource::Local,
        key: key.to_string(),
        is_down: false,
        is_modifier: false,
        time_ms,
        injected: false,
        lower_integrity_injected: false,
        extended: false,
        alt_down_flag: false,
    }
}

/// 便利构造：注入普通键 down 事件。
#[cfg(test)]
pub fn injected_key_down(key: &str, time_ms: u32) -> HookKeyEvent {
    HookKeyEvent {
        source: InputSource::Injected,
        key: key.to_string(),
        is_down: true,
        is_modifier: false,
        time_ms,
        injected: true,
        lower_integrity_injected: false,
        extended: false,
        alt_down_flag: false,
    }
}

/// 便利构造：注入普通键 up 事件。
#[cfg(test)]
pub fn injected_key_up(key: &str, time_ms: u32) -> HookKeyEvent {
    HookKeyEvent {
        source: InputSource::Injected,
        key: key.to_string(),
        is_down: false,
        is_modifier: false,
        time_ms,
        injected: true,
        lower_integrity_injected: false,
        extended: false,
        alt_down_flag: false,
    }
}

/// 便利构造：注入修饰键 down 事件。
#[cfg(test)]
pub fn injected_modifier_down(key: &str, time_ms: u32) -> HookKeyEvent {
    HookKeyEvent {
        source: InputSource::Injected,
        key: key.to_string(),
        is_down: true,
        is_modifier: true,
        time_ms,
        injected: true,
        lower_integrity_injected: false,
        extended: false,
        alt_down_flag: false,
    }
}

/// 便利构造：注入修饰键 up 事件。
#[cfg(test)]
pub fn injected_modifier_up(key: &str, time_ms: u32) -> HookKeyEvent {
    HookKeyEvent {
        source: InputSource::Injected,
        key: key.to_string(),
        is_down: false,
        is_modifier: true,
        time_ms,
        injected: true,
        lower_integrity_injected: false,
        extended: false,
        alt_down_flag: false,
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn alt_space_config() -> InputConfigSnapshot {
        InputConfigSnapshot {
            revision: 1,
            hotkey: NormalizedHotkey {
                modifiers: vec!["alt".to_string()],
                key: " ".to_string(),
            },
            tap_threshold: Duration::from_millis(300),
            chord_enabled: true,
            exclusive_tap_keys: ["a", "c", "q"].iter().map(|s| s.to_string()).collect(),
            voice_hold_enabled: true,
        }
    }

    fn ready_view() -> MainViewContext {
        MainViewContext {
            view_epoch: 1,
            revision: 0,
            ready: true,
            query_empty: true,
            ai_mode: false,
        }
    }

    fn window_visible() -> InputEvent {
        InputEvent::WindowChanged {
            visible: true,
            revision: 1,
            reason: WindowTransitionReason::Invoke,
        }
    }

    /// 标准设置：config=Alt+Space, window visible, view ready, Alt down。
    fn armed_state() -> InputState {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );
        reduce(&mut s, window_visible(), Instant::now());
        reduce(
            &mut s,
            InputEvent::ViewContextChanged(ready_view()),
            Instant::now(),
        );
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 100)),
            Instant::now(),
        );
        s
    }

    fn has_tap(effects: &[InputEffect]) -> bool {
        effects.iter().any(|e| matches!(e, InputEffect::Tap { .. }))
    }

    fn has_hold_started(effects: &[InputEffect]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, InputEffect::HoldStarted { .. }))
    }

    fn has_hold_released(effects: &[InputEffect]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, InputEffect::HoldReleased { .. }))
    }

    fn has_chord_triggered(effects: &[InputEffect]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, InputEffect::ChordTriggered { .. }))
    }

    fn has_voice_cancel(effects: &[InputEffect]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, InputEffect::VoiceCancel { .. }))
    }

    // ── 时间回绕 ──

    #[test]
    fn time_wrap_basic() {
        assert!(time_is_newer(101, 100));
        assert!(!time_is_newer(100, 100));
        assert!(!time_is_newer(99, 100));
    }

    #[test]
    fn time_wrap_around() {
        // u32 回绕：0x00000001 比 0xFFFFFFFF 更晚
        assert!(time_is_newer(1, 0xFFFF_FFFF));
        assert!(!time_is_newer(0xFFFF_FFFF, 1));
    }

    #[test]
    fn time_diff_wrap() {
        assert_eq!(time_diff(100, 50), 50);
        // 回绕：0 - 0xFFFF_FFFF = 1
        assert_eq!(time_diff(0, 0xFFFF_FFFF), 1);
    }

    // ── tap / hold / hold release / aborted / autorepeat ──

    #[test]
    fn tap_basic() {
        let mut s = armed_state();
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        assert!(!has_tap(&r.effects)); // down 不触发 tap
        assert!(matches!(s.gesture, GestureState::Armed { .. }));

        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 250)),
            Instant::now(),
        );
        assert!(has_tap(&r.effects));
        assert!(matches!(s.gesture, GestureState::Idle));
    }

    #[test]
    fn hold_started_and_released() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        let gid = s.gesture.gesture_id().unwrap();

        // HoldDeadline fires
        let r = reduce(
            &mut s,
            InputEvent::HoldDeadline { gesture_id: gid },
            Instant::now(),
        );
        assert!(has_hold_started(&r.effects));

        // hold_fired 期间 Space autorepeat -> 吞键（防系统菜单"噔噔噔"声）
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 210)),
            Instant::now(),
        );
        assert_eq!(r.propagation, Propagation::Swallow);

        // hold_fired 期间 Alt keydown autorepeat -> 也吞
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 220)),
            Instant::now(),
        );
        assert_eq!(r.propagation, Propagation::Swallow);

        // keyup -> HoldReleased（keyup 不吞，确保 HoldRelease 能收到）
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 600)),
            Instant::now(),
        );
        assert!(has_hold_released(&r.effects));
        assert_eq!(r.propagation, Propagation::Pass);
    }

    #[test]
    fn aborted_by_other_key() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        // 异键 down → aborted
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down("a", 210)),
            Instant::now(),
        );
        // keyup → 不触发 tap（aborted）
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 250)),
            Instant::now(),
        );
        assert!(!has_tap(&r.effects));
        assert!(!has_hold_released(&r.effects));
    }

    #[test]
    fn autorepeat_ignored() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        // 同一主键 repeat down -> 忽略（仍 armed，不重置）
        // tap 窗口内（hold_fired=false, voice=Idle）-> 放行
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 210)),
            Instant::now(),
        );
        assert!(!has_tap(&r.effects));
        assert!(matches!(s.gesture, GestureState::Armed { .. }));
        assert_eq!(r.propagation, Propagation::Pass);

        // keyup -> tap
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 250)),
            Instant::now(),
        );
        assert!(has_tap(&r.effects));
    }

    // ── stale HoldDeadline ──

    #[test]
    fn stale_hold_deadline_ignored() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        let gid = s.gesture.gesture_id().unwrap();

        // keyup → tap, gesture cleared
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 250)),
            Instant::now(),
        );

        // stale HoldDeadline → 忽略
        let r = reduce(
            &mut s,
            InputEvent::HoldDeadline { gesture_id: gid },
            Instant::now(),
        );
        assert!(!has_hold_started(&r.effects));
    }

    // ── 左右 Alt 分开 ──

    #[test]
    fn left_right_alt_separate() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );
        reduce(&mut s, window_visible(), Instant::now());
        reduce(
            &mut s,
            InputEvent::ViewContextChanged(ready_view()),
            Instant::now(),
        );

        // 左 Alt down
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 100)),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());

        // 右 Alt down（两侧同时按）
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("ralt", 110)),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());

        // 左 Alt up → 右 Alt 还按着，alt_down 仍 true
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_up("lalt", 120)),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());

        // 右 Alt up → alt_down false
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_up("ralt", 130)),
            Instant::now(),
        );
        assert!(!s.modifiers.alt_down());
    }

    // ── WindowHidden 结束 Chord 但保留 Alt ──

    #[test]
    fn window_hidden_ends_chord_preserves_alt() {
        let mut s = armed_state();
        // chord session 应已建立
        assert!(s.chord.is_active());

        // Alt 仍按住
        assert!(s.modifiers.alt_down());

        // Window hidden
        reduce(
            &mut s,
            InputEvent::WindowChanged {
                visible: false,
                revision: 2,
                reason: WindowTransitionReason::Escape,
            },
            Instant::now(),
        );
        // Chord 退出
        assert!(!s.chord.is_active());
        // Alt 仍按住（不伪造 AltReleased）
        assert!(s.modifiers.alt_down());
    }

    // ── WindowFocusObserved 不改变 visible ──

    #[test]
    fn window_focus_does_not_change_visible() {
        let mut s = armed_state();
        assert!(s.window.visible);

        reduce(
            &mut s,
            InputEvent::WindowFocusObserved(true),
            Instant::now(),
        );
        assert!(s.window.visible); // 不变

        let mut s2 = InputState::default();
        s2.config = alt_space_config();
        reduce(
            &mut s2,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );
        assert!(!s2.window.visible);

        reduce(
            &mut s2,
            InputEvent::WindowFocusObserved(true),
            Instant::now(),
        );
        assert!(!s2.window.visible); // Hidden 窗口不因 focus 变 visible
    }

    // ── query empty/non-empty 切换 exclusive ──

    #[test]
    fn query_non_empty_exits_exclusive() {
        let mut s = armed_state();
        assert!(s.chord.is_active());

        // query 变非空
        let mut ctx = ready_view();
        ctx.query_empty = false;
        ctx.revision = 1;
        reduce(&mut s, InputEvent::ViewContextChanged(ctx), Instant::now());
        assert!(!s.chord.is_active()); // 退出 exclusive
    }

    #[test]
    fn query_empty_re_enters_exclusive() {
        let mut s = armed_state();
        // 先变非空 → 退出
        let mut ctx = ready_view();
        ctx.query_empty = false;
        ctx.revision = 1;
        reduce(&mut s, InputEvent::ViewContextChanged(ctx), Instant::now());
        assert!(!s.chord.is_active());

        // 再变空 → 重新进入
        let mut ctx2 = ready_view();
        ctx2.revision = 2;
        reduce(&mut s, InputEvent::ViewContextChanged(ctx2), Instant::now());
        assert!(s.chord.is_active());
    }

    // ── Chord autorepeat 单触发 ──

    #[test]
    fn chord_autorepeat_single_trigger() {
        let mut s = armed_state();
        assert!(s.chord.is_active());

        // 第一次 'a' down → ChordTriggered + Swallow
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down("a", 200)),
            Instant::now(),
        );
        assert_eq!(r.propagation, Propagation::Swallow);
        assert!(has_chord_triggered(&r.effects));

        // autorepeat 'a' down → Swallow 但不重复触发
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down("a", 210)),
            Instant::now(),
        );
        assert_eq!(r.propagation, Propagation::Swallow);
        assert!(!has_chord_triggered(&r.effects));

        // 'a' up → 解锁
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up("a", 220)),
            Instant::now(),
        );

        // 再次 'a' down → 可以再次触发
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down("a", 230)),
            Instant::now(),
        );
        assert_eq!(r.propagation, Propagation::Swallow);
        assert!(has_chord_triggered(&r.effects));
    }

    // ── 窗口由托盘/单实例打开后再按 Alt → Chord Session ──

    #[test]
    fn chord_session_without_main_gesture() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );

        // 窗口由托盘打开（无主热键 gesture）
        reduce(&mut s, window_visible(), Instant::now());
        reduce(
            &mut s,
            InputEvent::ViewContextChanged(ready_view()),
            Instant::now(),
        );

        // 此时 Alt 未按下 → chord 未建立
        assert!(!s.chord.is_active());

        // 用户按下 Alt
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 100)),
            Instant::now(),
        );

        // Chord session 建立（不依赖主热键 gesture）
        assert!(s.chord.is_active());
    }

    // ── local gesture 不接受 injected keyup ──

    #[test]
    fn local_gesture_rejects_injected_keyup() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );

        // injected keyup → 不配对，不触发 tap
        let r = reduce(
            &mut s,
            InputEvent::HookKey(injected_key_up(" ", 210)),
            Instant::now(),
        );
        assert!(!has_tap(&r.effects));
        assert!(matches!(s.gesture, GestureState::Armed { .. })); // 仍 armed

        // local keyup → 触发 tap
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 220)),
            Instant::now(),
        );
        assert!(has_tap(&r.effects));
    }

    // ── injected gesture 完整 down/up ──

    #[test]
    fn injected_gesture_complete_down_up() {
        let mut s = armed_state();
        // injected Space down → armed (source=Injected)
        reduce(
            &mut s,
            InputEvent::HookKey(injected_key_down(" ", 200)),
            Instant::now(),
        );
        assert!(matches!(s.gesture, GestureState::Armed { .. }));

        // injected Space up → tap
        let r = reduce(
            &mut s,
            InputEvent::HookKey(injected_key_up(" ", 250)),
            Instant::now(),
        );
        assert!(has_tap(&r.effects));
    }

    // ── 注入修饰键 down/up（远程桌面场景）──

    /// 注入 Alt keydown → InjectedDown；注入 Alt keyup → 清为 Up。
    /// 远程桌面完整 down→up 序列应正常流转，不卡死。
    #[test]
    fn injected_alt_down_up_clears_level() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );

        // 注入 LAlt down → InjectedDown
        reduce(
            &mut s,
            InputEvent::HookKey(injected_modifier_down("lalt", 100)),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());
        assert_eq!(
            s.modifiers.level(ModifierKey::LAlt),
            ModifierLevel::InjectedDown
        );

        // 注入 LAlt up → 清为 Up（InjectedDown 接受注入 keyup）
        reduce(
            &mut s,
            InputEvent::HookKey(injected_modifier_up("lalt", 200)),
            Instant::now(),
        );
        assert!(!s.modifiers.alt_down());
        assert_eq!(
            s.modifiers.level(ModifierKey::LAlt),
            ModifierLevel::Up
        );
    }

    /// 本地 Alt keydown → Down；注入 Alt keyup 不清除（SetForegroundWindow 合成事件）。
    #[test]
    fn local_alt_down_ignores_injected_keyup() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );

        // 本地 LAlt down → Down
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 100)),
            Instant::now(),
        );
        assert_eq!(
            s.modifiers.level(ModifierKey::LAlt),
            ModifierLevel::Down
        );

        // 注入 LAlt up → 不清除（Down 不接受注入 keyup）
        reduce(
            &mut s,
            InputEvent::HookKey(injected_modifier_up("lalt", 200)),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());
        assert_eq!(
            s.modifiers.level(ModifierKey::LAlt),
            ModifierLevel::Down
        );

        // 本地 LAlt up → 清为 Up
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_up("lalt", 300)),
            Instant::now(),
        );
        assert!(!s.modifiers.alt_down());
    }

    /// InjectedDown 被 Raw Input down 升级为 Down 后，注入 keyup 不再清除。
    #[test]
    fn raw_down_upgrades_injected_down() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );

        // 注入 LAlt down → InjectedDown
        reduce(
            &mut s,
            InputEvent::HookKey(injected_modifier_down("lalt", 100)),
            Instant::now(),
        );
        assert_eq!(
            s.modifiers.level(ModifierKey::LAlt),
            ModifierLevel::InjectedDown
        );

        // Raw Input LAlt down → 升级为 Down（真实硬件确认）
        reduce(
            &mut s,
            InputEvent::RawModifier(RawModifierEvent {
                device_id: 1,
                key: "lalt".to_string(),
                is_down: true,
                time_ms: 110,
            }),
            Instant::now(),
        );
        assert_eq!(
            s.modifiers.level(ModifierKey::LAlt),
            ModifierLevel::Down
        );

        // 注入 LAlt up → 不清除（已升级为 Down）
        reduce(
            &mut s,
            InputEvent::HookKey(injected_modifier_up("lalt", 200)),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());
        assert_eq!(
            s.modifiers.level(ModifierKey::LAlt),
            ModifierLevel::Down
        );
    }

    /// 远程桌面完整序列：注入 Alt down → 注入 Space down → 注入 Space up → Tap
    /// → 注入 Alt up → alt_down=false。此前注入 Alt up 被忽略导致 Alt 卡死。
    #[test]
    fn injected_alt_space_complete_sequence_no_stuck() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );

        // 注入 Alt down
        reduce(
            &mut s,
            InputEvent::HookKey(injected_modifier_down("lalt", 100)),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());

        // 注入 Space down → armed
        reduce(
            &mut s,
            InputEvent::HookKey(injected_key_down(" ", 110)),
            Instant::now(),
        );
        assert!(matches!(s.gesture, GestureState::Armed { .. }));

        // 注入 Space up → Tap
        let r = reduce(
            &mut s,
            InputEvent::HookKey(injected_key_up(" ", 150)),
            Instant::now(),
        );
        assert!(has_tap(&r.effects));

        // 注入 Alt up → 清除（修复后）
        reduce(
            &mut s,
            InputEvent::HookKey(injected_modifier_up("lalt", 200)),
            Instant::now(),
        );
        assert!(!s.modifiers.alt_down(), "注入 Alt up 后应清除，不卡死");
    }

    /// 注入 Ctrl down → InjectedDown；注入 Ctrl up → 清除。
    /// 验证 send_paste (Ctrl+V) 回退路径不残留 Ctrl Down。
    #[test]
    fn injected_ctrl_down_up_clears_level() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );

        // 注入 LCtrl down → InjectedDown
        reduce(
            &mut s,
            InputEvent::HookKey(injected_modifier_down("lctrl", 100)),
            Instant::now(),
        );
        assert!(s.modifiers.is_pressed(ModifierKey::LCtrl));

        // 注入 LCtrl up → 清为 Up
        reduce(
            &mut s,
            InputEvent::HookKey(injected_modifier_up("lctrl", 200)),
            Instant::now(),
        );
        assert!(!s.modifiers.is_pressed(ModifierKey::LCtrl));
    }

    // ── LLKHF_ALTDOWN 临时证据 ──

    #[test]
    fn alt_down_flag_infers_alt_pressed() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );
        reduce(&mut s, window_visible(), Instant::now());
        reduce(
            &mut s,
            InputEvent::ViewContextChanged(ready_view()),
            Instant::now(),
        );

        // Hook 安装前 Alt 已按下：非注入 Space down 带 alt_down_flag
        reduce(
            &mut s,
            InputEvent::HookKey(HookKeyEvent {
                source: InputSource::Local,
                key: " ".to_string(),
                is_down: true,
                is_modifier: false,
                time_ms: 100,
                injected: false,
                lower_integrity_injected: false,
                extended: false,
                alt_down_flag: true,
            }),
            Instant::now(),
        );

        // Alt 被推断为按下
        assert!(s.modifiers.alt_down());
        // gesture armed（Alt+Space 配置满足）
        assert!(matches!(s.gesture, GestureState::Armed { .. }));

        // Space up → tap
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 150)),
            Instant::now(),
        );
        assert!(has_tap(&r.effects));
    }

    // ── InferredDown 在 tap→WindowShown 后维持 Chord ──

    #[test]
    fn inferred_down_maintains_chord_after_tap() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );
        reduce(&mut s, window_visible(), Instant::now());
        reduce(
            &mut s,
            InputEvent::ViewContextChanged(ready_view()),
            Instant::now(),
        );

        // 非注入 Space down 带 alt_down_flag
        reduce(
            &mut s,
            InputEvent::HookKey(HookKeyEvent {
                source: InputSource::Local,
                key: " ".to_string(),
                is_down: true,
                is_modifier: false,
                time_ms: 100,
                injected: false,
                lower_integrity_injected: false,
                extended: false,
                alt_down_flag: true,
            }),
            Instant::now(),
        );

        // Space up → tap
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 150)),
            Instant::now(),
        );
        assert!(has_tap(&r.effects));

        // 窗口已 visible，Alt InferredDown → Chord 应建立
        assert!(s.chord.is_active());
        assert!(s.modifiers.alt_down());

        // 真实 Alt up → InferredDown 清除，Chord 退出
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_up("lalt", 200)),
            Instant::now(),
        );
        assert!(!s.modifiers.alt_down());
        assert!(!s.chord.is_active());
    }

    // ── window revision 不使 HoldDeadline 过期 ──

    #[test]
    fn window_revision_does_not_expire_hold() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        let gid = s.gesture.gesture_id().unwrap();

        // Window revision 变化（如 watchdog 检测）
        reduce(
            &mut s,
            InputEvent::WindowChanged {
                visible: true,
                revision: 2,
                reason: WindowTransitionReason::Watchdog,
            },
            Instant::now(),
        );

        // HoldDeadline 仍应生效（window revision 不影响 gesture id 匹配）
        let r = reduce(
            &mut s,
            InputEvent::HoldDeadline { gesture_id: gid },
            Instant::now(),
        );
        assert!(has_hold_started(&r.effects));
    }

    // ── UI revision 不与 gesture id 混用 ──

    #[test]
    fn ui_revision_independent_of_gesture_id() {
        let mut s = armed_state();
        let initial_ui_rev = s.next_ui_revision;
        let initial_gesture_id = s.next_gesture_id;

        // Alt up → UI 变化（alt_down: true→false），ui_revision 递增
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_up("lalt", 200)),
            Instant::now(),
        );
        assert!(s.next_ui_revision > initial_ui_rev);
        // gesture id 不因 UI 变化而递增
        assert_eq!(s.next_gesture_id, initial_gesture_id);
    }

    // ── 配置在 Armed 中途更新 ──

    #[test]
    fn config_update_during_armed_freezes_current_gesture() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        let _gid = s.gesture.gesture_id().unwrap();

        // 配置更新：tap_threshold 变大
        let mut new_config = alt_space_config();
        new_config.revision = 2;
        new_config.tap_threshold = Duration::from_millis(500);
        reduce(
            &mut s,
            InputEvent::ConfigChanged(new_config.clone()),
            Instant::now(),
        );

        // 当前 gesture 仍用冻结的配置
        if let GestureState::Armed {
            frozen_tap_threshold,
            ..
        } = &s.gesture
        {
            assert_eq!(*frozen_tap_threshold, Duration::from_millis(300)); // 旧配置
        } else {
            panic!("should still be armed");
        }

        // keyup → tap（旧 gesture 仍有效）
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 250)),
            Instant::now(),
        );
        assert!(has_tap(&r.effects));

        // 下一次 gesture 用新配置
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 300)),
            Instant::now(),
        );
        if let GestureState::Armed {
            frozen_tap_threshold,
            ..
        } = &s.gesture
        {
            assert_eq!(*frozen_tap_threshold, Duration::from_millis(500)); // 新配置
        }
    }

    // ── Chord 禁用立即退出 session ──

    #[test]
    fn chord_disable_exits_session() {
        let mut s = armed_state();
        assert!(s.chord.is_active());

        // 禁用 chord
        let mut cfg = alt_space_config();
        cfg.revision = 2;
        cfg.chord_enabled = false;
        reduce(&mut s, InputEvent::ConfigChanged(cfg), Instant::now());
        assert!(!s.chord.is_active());
    }

    // ── RightAlt standalone 配置 ──

    #[test]
    fn right_alt_standalone_config() {
        let mut s = InputState::default();
        let cfg = InputConfigSnapshot {
            revision: 1,
            hotkey: NormalizedHotkey {
                modifiers: vec![],
                key: "ralt".to_string(),
            },
            tap_threshold: Duration::from_millis(300),
            chord_enabled: false,
            exclusive_tap_keys: HashSet::new(),
            voice_hold_enabled: true,
        };
        s.config = cfg.clone();
        reduce(&mut s, InputEvent::ConfigChanged(cfg), Instant::now());

        // 右 Alt down → armed（standalone，无需额外修饰键）
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("ralt", 100)),
            Instant::now(),
        );
        assert!(matches!(s.gesture, GestureState::Armed { .. }));

        // 右 Alt up → tap
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_up("ralt", 150)),
            Instant::now(),
        );
        assert!(has_tap(&r.effects));
    }

    // ── Ctrl+Space 配置匹配 ──

    #[test]
    fn ctrl_space_config_matching() {
        let mut s = InputState::default();
        let cfg = InputConfigSnapshot {
            revision: 1,
            hotkey: NormalizedHotkey {
                modifiers: vec!["ctrl".to_string()],
                key: " ".to_string(),
            },
            tap_threshold: Duration::from_millis(300),
            chord_enabled: false,
            exclusive_tap_keys: HashSet::new(),
            voice_hold_enabled: true,
        };
        s.config = cfg.clone();
        reduce(&mut s, InputEvent::ConfigChanged(cfg), Instant::now());

        // Ctrl down + Space down → armed
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lctrl", 100)),
            Instant::now(),
        );
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 110)),
            Instant::now(),
        );
        assert!(matches!(s.gesture, GestureState::Armed { .. }));

        // Space up → tap
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 150)),
            Instant::now(),
        );
        assert!(has_tap(&r.effects));
    }

    // ── Ctrl+Alt+Key 配置匹配 ──

    #[test]
    fn ctrl_alt_key_config_matching() {
        let mut s = InputState::default();
        let cfg = InputConfigSnapshot {
            revision: 1,
            hotkey: NormalizedHotkey {
                modifiers: vec!["ctrl".to_string(), "alt".to_string()],
                key: "a".to_string(),
            },
            tap_threshold: Duration::from_millis(300),
            chord_enabled: false,
            exclusive_tap_keys: HashSet::new(),
            voice_hold_enabled: true,
        };
        s.config = cfg.clone();
        reduce(&mut s, InputEvent::ConfigChanged(cfg), Instant::now());

        // Ctrl + Alt + A → armed
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lctrl", 100)),
            Instant::now(),
        );
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 110)),
            Instant::now(),
        );
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down("a", 120)),
            Instant::now(),
        );
        assert!(matches!(s.gesture, GestureState::Armed { .. }));

        // A up → tap
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up("a", 150)),
            Instant::now(),
        );
        assert!(has_tap(&r.effects));
    }

    // ── AltGr 不误判成 Ctrl+Alt ──

    #[test]
    fn altgr_not_misjudged_as_ctrl_alt() {
        // AltGr = RAlt + LCtrl（系统合成）→ apply_altgr_correction 去掉 LCtrl
        let mask = MOD_RALT | MOD_LCTRL;
        let corrected = apply_altgr_correction(mask);
        // 配置 "alt" + "a"：corrected 只含 RAlt → 满足
        assert!(modifiers_mask_satisfies_config(
            &["alt".to_string()],
            corrected
        ));
        // 配置 "ctrl" + "alt" + "a"：corrected 不含 LCtrl → 不满足
        assert!(!modifiers_mask_satisfies_config(
            &["ctrl".to_string(), "alt".to_string()],
            corrected
        ));
    }

    // ── startup Unknown 不凭空触发 ──

    #[test]
    fn startup_unknown_does_not_trigger() {
        let s = InputState::default();
        assert!(!s.modifiers.alt_down());
        assert!(matches!(s.gesture, GestureState::Idle));
        assert!(!s.chord.is_active());
    }

    // ── Hidden while Alt down 不伪造释放 ──

    #[test]
    fn hidden_while_alt_down_no_fake_release() {
        let mut s = armed_state();
        assert!(s.modifiers.alt_down());

        reduce(
            &mut s,
            InputEvent::WindowChanged {
                visible: false,
                revision: 2,
                reason: WindowTransitionReason::Escape,
            },
            Instant::now(),
        );
        // Alt 仍按下
        assert!(s.modifiers.alt_down());
    }

    // ── Focused(true) 不改变 Hidden ──

    #[test]
    fn focused_true_does_not_change_hidden() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );
        assert!(!s.window.visible);

        reduce(
            &mut s,
            InputEvent::WindowFocusObserved(true),
            Instant::now(),
        );
        assert!(!s.window.visible);
    }

    // ── ESC 仅在 Starting/Recording 时产生 VoiceCancel ──

    #[test]
    fn esc_voice_cancel_only_when_voice_active() {
        let mut s = armed_state();

        // Voice Idle → ESC 不产生 VoiceCancel
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down("Escape", 200)),
            Instant::now(),
        );
        assert!(!has_voice_cancel(&r.effects));

        // Voice Recording → ESC 产生 VoiceCancel
        reduce(
            &mut s,
            InputEvent::VoicePhaseChanged {
                gesture_id: None,
                phase: VoicePhase::Recording { gesture_id: 1 },
            },
            Instant::now(),
        );
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down("Escape", 210)),
            Instant::now(),
        );
        assert!(has_voice_cancel(&r.effects));
    }

    // ── hold_fired 期间吞 Space autorepeat（防系统菜单"噔噔噔"声）──

    #[test]
    fn hold_fired_swallows_space_autorepeat() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        let gid = s.gesture.gesture_id().unwrap();

        // HoldDeadline -> hold_fired=true
        reduce(
            &mut s,
            InputEvent::HoldDeadline { gesture_id: gid },
            Instant::now(),
        );
        assert!(s.gesture.is_hold_fired());

        // Space autorepeat -> Swallow（不透传给系统）
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 210)),
            Instant::now(),
        );
        assert_eq!(r.propagation, Propagation::Swallow);
        // 仍 armed，不重置
        assert!(s.gesture.is_hold_fired());

        // keyup -> HoldReleased + Pass
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 600)),
            Instant::now(),
        );
        assert!(has_hold_released(&r.effects));
        assert_eq!(r.propagation, Propagation::Pass);
    }

    // ── VoicePhase::Recording 期间吞 Space + Alt keydown，不吞 keyup ──

    #[test]
    fn voice_recording_swallows_space_and_alt() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        // 进入 Voice Recording（模拟主线程 update_voice_phase）
        reduce(
            &mut s,
            InputEvent::VoicePhaseChanged {
                gesture_id: None,
                phase: VoicePhase::Recording { gesture_id: 1 },
            },
            Instant::now(),
        );

        // Space keydown -> Swallow
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 210)),
            Instant::now(),
        );
        assert_eq!(r.propagation, Propagation::Swallow);

        // Alt keydown -> Swallow
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 220)),
            Instant::now(),
        );
        assert_eq!(r.propagation, Propagation::Swallow);

        // 非主键非 Alt 的 keydown -> 仍放行（不影响其他键）
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down("a", 230)),
            Instant::now(),
        );
        assert_eq!(r.propagation, Propagation::Pass);
    }

    #[test]
    fn voice_recording_keyup_passes() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        let gid = s.gesture.gesture_id().unwrap();
        // hold_fired + Voice Recording
        reduce(
            &mut s,
            InputEvent::HoldDeadline { gesture_id: gid },
            Instant::now(),
        );
        reduce(
            &mut s,
            InputEvent::VoicePhaseChanged {
                gesture_id: None,
                phase: VoicePhase::Recording { gesture_id: gid },
            },
            Instant::now(),
        );

        // Space keyup -> Pass（HoldRelease 必须能收到）+ HoldReleased
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 600)),
            Instant::now(),
        );
        assert_eq!(r.propagation, Propagation::Pass);
        assert!(has_hold_released(&r.effects));
    }

    // ── Recorder 进入 Recording 清空 gesture ──

    #[test]
    fn recorder_mode_clears_gesture() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        assert!(matches!(s.gesture, GestureState::Armed { .. }));

        // 进入录制
        reduce(
            &mut s,
            InputEvent::RecorderModeChanged(RecorderMode::Recording { recorder_id: 1 }),
            Instant::now(),
        );
        assert!(matches!(s.gesture, GestureState::Idle));
        assert!(!s.chord.is_active());

        // Space up → 不触发 tap（gesture 已清空，且在录制模式）
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_key_up(" ", 250)),
            Instant::now(),
        );
        assert!(!has_tap(&r.effects));
    }

    // ── 配置热更新不在 gesture 中途产生额外动作 ──

    #[test]
    fn config_update_no_extra_action_during_gesture() {
        let mut s = armed_state();
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );

        let mut new_config = alt_space_config();
        new_config.revision = 2;
        new_config.hotkey = NormalizedHotkey {
            modifiers: vec!["ctrl".to_string()],
            key: " ".to_string(),
        };
        let r = reduce(
            &mut s,
            InputEvent::ConfigChanged(new_config),
            Instant::now(),
        );
        // 配置更新不产生 tap/hold
        assert!(!has_tap(&r.effects));
        assert!(!has_hold_started(&r.effects));
        assert!(!has_hold_released(&r.effects));

        // 旧 gesture 仍 armed
        assert!(matches!(s.gesture, GestureState::Armed { .. }));
    }

    // ── 旧 view epoch context update 丢弃 ──

    #[test]
    fn old_view_epoch_discarded() {
        let mut s = armed_state();
        assert_eq!(s.view.view_epoch, 1);

        // 旧 epoch 的 context → 丢弃
        let old_ctx = MainViewContext {
            view_epoch: 0,
            revision: 99,
            ready: true,
            query_empty: false,
            ai_mode: true,
        };
        reduce(
            &mut s,
            InputEvent::ViewContextChanged(old_ctx),
            Instant::now(),
        );
        assert_eq!(s.view.view_epoch, 1); // 未变
        assert!(s.view.query_empty); // 未变
    }

    // ── 旧 window revision 丢弃 ──

    #[test]
    fn old_window_revision_discarded() {
        let mut s = armed_state();
        let rev = s.window.revision;

        // 旧 revision → 丢弃
        reduce(
            &mut s,
            InputEvent::WindowChanged {
                visible: false,
                revision: rev, // 相同 revision → 丢弃
                reason: WindowTransitionReason::Watchdog,
            },
            Instant::now(),
        );
        assert!(s.window.visible); // 未变
    }

    // ── 修饰键匹配：多余修饰键不匹配 ──

    #[test]
    fn extra_modifier_does_not_match() {
        // 配置 Alt+Space，物理 Ctrl+Alt → 不匹配
        let mask = MOD_LALT | MOD_LCTRL;
        assert!(!modifiers_mask_satisfies_config(&["alt".to_string()], mask));
    }

    // ── Raw Input 确认修饰键 ──

    #[test]
    fn raw_input_confirms_modifier() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );
        reduce(&mut s, window_visible(), Instant::now());
        reduce(
            &mut s,
            InputEvent::ViewContextChanged(ready_view()),
            Instant::now(),
        );

        // Hook: LAlt down
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 100)),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());

        // Raw: LAlt down (确认)
        reduce(
            &mut s,
            InputEvent::RawModifier(RawModifierEvent {
                device_id: 1,
                key: "lalt".to_string(),
                is_down: true,
                time_ms: 101,
            }),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());

        // Raw: LAlt up
        reduce(
            &mut s,
            InputEvent::RawModifier(RawModifierEvent {
                device_id: 1,
                key: "lalt".to_string(),
                is_down: false,
                time_ms: 200,
            }),
            Instant::now(),
        );
        assert!(!s.modifiers.alt_down());
    }

    // ── 多设备 modifier 聚合 ──

    #[test]
    fn multi_device_modifier_aggregate() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );
        reduce(&mut s, window_visible(), Instant::now());
        reduce(
            &mut s,
            InputEvent::ViewContextChanged(ready_view()),
            Instant::now(),
        );

        // Device 1: LAlt down
        reduce(
            &mut s,
            InputEvent::RawModifier(RawModifierEvent {
                device_id: 1,
                key: "lalt".to_string(),
                is_down: true,
                time_ms: 100,
            }),
            Instant::now(),
        );
        // Device 2: LAlt down
        reduce(
            &mut s,
            InputEvent::RawModifier(RawModifierEvent {
                device_id: 2,
                key: "lalt".to_string(),
                is_down: true,
                time_ms: 101,
            }),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());

        // Device 1: LAlt up → Device 2 还按着
        reduce(
            &mut s,
            InputEvent::RawModifier(RawModifierEvent {
                device_id: 1,
                key: "lalt".to_string(),
                is_down: false,
                time_ms: 200,
            }),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());

        // Device 2: LAlt up → 无人按
        reduce(
            &mut s,
            InputEvent::RawModifier(RawModifierEvent {
                device_id: 2,
                key: "lalt".to_string(),
                is_down: false,
                time_ms: 201,
            }),
            Instant::now(),
        );
        assert!(!s.modifiers.alt_down());
    }

    // ── 设备移除自愈 ──

    #[test]
    fn device_removal_self_heals() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );
        reduce(&mut s, window_visible(), Instant::now());
        reduce(
            &mut s,
            InputEvent::ViewContextChanged(ready_view()),
            Instant::now(),
        );

        // Device 1: LAlt down (按住 Alt 的键盘)
        reduce(
            &mut s,
            InputEvent::RawModifier(RawModifierEvent {
                device_id: 1,
                key: "lalt".to_string(),
                is_down: true,
                time_ms: 100,
            }),
            Instant::now(),
        );
        assert!(s.modifiers.alt_down());

        // 拔掉键盘 → 设备移除 → 自愈
        reduce(
            &mut s,
            InputEvent::RawDeviceRemoved { device_id: 1 },
            Instant::now(),
        );
        assert!(!s.modifiers.alt_down());
    }

    // ── 过期 Raw down 被时间规则丢弃 ──

    #[test]
    fn stale_raw_down_discarded() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );

        // Hook: LAlt down at 200, then up at 210
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 200)),
            Instant::now(),
        );
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_up("lalt", 210)),
            Instant::now(),
        );
        assert!(!s.modifiers.alt_down());

        // Raw: LAlt down at 100 (older than Hook's 210) → 丢弃
        reduce(
            &mut s,
            InputEvent::RawModifier(RawModifierEvent {
                device_id: 1,
                key: "lalt".to_string(),
                is_down: true,
                time_ms: 100,
            }),
            Instant::now(),
        );
        assert!(!s.modifiers.alt_down()); // 不被回写为 Down
    }

    // ── UI 状态仅在变化时 emit ──

    #[test]
    fn ui_state_only_emitted_on_change() {
        let mut s = InputState::default();
        s.config = alt_space_config();
        reduce(
            &mut s,
            InputEvent::ConfigChanged(alt_space_config()),
            Instant::now(),
        );
        reduce(&mut s, window_visible(), Instant::now());
        reduce(
            &mut s,
            InputEvent::ViewContextChanged(ready_view()),
            Instant::now(),
        );

        // Alt down → UI 变化（alt_down: false→true, chord: false→true）
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 100)),
            Instant::now(),
        );
        let ui_count = r
            .effects
            .iter()
            .filter(|e| matches!(e, InputEffect::UiStateChanged { .. }))
            .count();
        assert!(ui_count >= 1);

        // 同一个 Alt autorepeat down → 无变化，不 emit
        let r = reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 110)),
            Instant::now(),
        );
        let ui_count = r
            .effects
            .iter()
            .filter(|e| matches!(e, InputEffect::UiStateChanged { .. }))
            .count();
        assert_eq!(ui_count, 0);
    }

    // ── voice_hold_enabled=false 时不发 HoldStarted ──

    #[test]
    fn voice_hold_disabled_no_hold_started() {
        let mut s = InputState::default();
        let mut cfg = alt_space_config();
        cfg.voice_hold_enabled = false;
        s.config = cfg.clone();
        reduce(&mut s, InputEvent::ConfigChanged(cfg), Instant::now());
        reduce(&mut s, window_visible(), Instant::now());
        reduce(
            &mut s,
            InputEvent::ViewContextChanged(ready_view()),
            Instant::now(),
        );
        reduce(
            &mut s,
            InputEvent::HookKey(hook_modifier_down("lalt", 100)),
            Instant::now(),
        );
        reduce(
            &mut s,
            InputEvent::HookKey(hook_key_down(" ", 200)),
            Instant::now(),
        );
        let gid = s.gesture.gesture_id().unwrap();

        let r = reduce(
            &mut s,
            InputEvent::HoldDeadline { gesture_id: gid },
            Instant::now(),
        );
        assert!(!has_hold_started(&r.effects));
    }
}
