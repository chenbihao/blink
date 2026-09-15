//! 语音服务:hold-to-talk 管线编排。
//!
//! ## 管线
//!
//! ```text
//! Hold 事件 → start_recording()
//!   → 创建 AudioCapture + SttEngine
//!   → spawn 采集 task: audio chunk → SttEngine::transcribe_chunk → emit partial
//!
//! HoldRelease 事件 → stop_recording()
//!   → stop AudioCapture
//!   → SttEngine::finalize() → 最终文本
//!   → G1: emit EventNames::CHORD_FILL_QUERY(文本)
//!     G2: inject_text(文本)
//!     G3: emit EventNames::VOICE_PARTIAL(target="chat", 文本)
//! ```
//!
//! ## G1/G2/G3 区分
//!
//! - hold 时主窗口可见(先 tap 出窗)→ G1: 文字填 #query
//! - hold 时 chat 窗口可见 → G3: 文字填 chat composer textarea
//! - hold 时主窗口 + chat 均不可见 → G2: 文字注入前台应用

use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use tauri::{Emitter, Manager};

use crate::domain::event_names::EventNames;
use crate::domain::stt::dictation::EditorDictationTracker;
use crate::domain::stt::{
    AudioRange, DraftSpan, RecognitionProfile, StreamingSttPort, SttEngine, SttEvent,
};
use crate::infra::platform;
use crate::infra::platform::audio::{AudioCapture, AudioFormat};

/// 语音目标(G1 主窗口 / G2 前台应用 / G3 chat 窗口 / Editor 编辑器连续听写)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceTarget {
    /// G1: 文字填进 blink 主窗口 #query
    MainWindow,
    /// G2: 文字注入前台应用光标处
    ForegroundApp,
    /// G3: 文字填进 chat 窗口 composer textarea（0.12.2 §4.3）
    ///
    /// 0.12.2: 仅 IPC 驱动（start_chat_stt）。
    /// 0.12.3: 热键驱动也走此路径——chat 窗口可见时 hold Alt+Space
    /// 自动检测并走 G3 而非 G2（不唤起前台注入）。
    ChatWindow,
    /// Editor: 编辑器连续听写（0.23.3 §3.6）。
    ///
    /// 仅 IPC 驱动（start_editor_voice），由编辑器按钮显式开始；
    /// confirmed segment 按 epoch + seq 推送，preview 只进事件不进正文。
    Editor,
}

impl VoiceTarget {
    /// 序列化给前端的字符串标签。
    pub fn as_str(&self) -> &'static str {
        match self {
            VoiceTarget::MainWindow => "g1",
            VoiceTarget::ForegroundApp => "g2",
            VoiceTarget::ChatWindow => "chat",
            VoiceTarget::Editor => "editor",
        }
    }
}

/// 编辑器听写状态事件的「边沿触发」判定（0.24）。
///
/// 修复前每个音频块（~10ms）都会发一次 `EDITOR_VOICE_STATUS`——92 秒录音
/// 产生 8,791 条事件（约 95.5 条/秒），其中真正不同的状态只有约 25 种。
/// 现在仅当 phase / seq / preview / message / code 任一变化时才对外发送。
#[derive(Default)]
struct EditorStatusEdge {
    last: Option<EditorStatusKey>,
}

#[derive(PartialEq, Eq)]
struct EditorStatusKey {
    phase: String,
    seq: u64,
    preview: Option<String>,
    message: Option<String>,
    code: Option<String>,
}

impl EditorStatusEdge {
    /// 判断本次状态是否需要对外发送；返回 `true` 时同步记录新状态。
    fn should_emit(
        &mut self,
        phase: &str,
        seq: u64,
        preview: Option<&str>,
        message: Option<&str>,
        code: Option<&str>,
    ) -> bool {
        let key = EditorStatusKey {
            phase: phase.to_string(),
            seq,
            preview: preview.map(str::to_string),
            message: message.map(str::to_string),
            code: code.map(str::to_string),
        };
        if self.last.as_ref() == Some(&key) {
            return false;
        }
        self.last = Some(key);
        true
    }
}

/// 编辑器连续听写的有界 confirmed 快照状态（0.23.3 §3.6）。
///
/// 一个听写 epoch 一份；`VoiceService.editor_state` 持有共享句柄，
/// 事件消费 task 推段、command 层读取补齐。段缓冲有界（淘汰最旧），
/// 结束后保留到下一次听写开始（供前端迟到的补齐请求）。
pub struct EditorDictationState {
    /// 听写 epoch（每次 start_editor_voice 单调递增，从 1 开始）。
    pub epoch: u64,
    /// 冻结的编辑器会话身份（start 时校验，事件携带供前端过滤）。
    pub session_ref: String,
    pub generation: u64,
    /// 段推导器（confirmed 增量 + Final 收尾段 + seq 单调）。
    tracker: EditorDictationTracker,
    /// confirmed 段缓冲（有界，FIFO 淘汰）。
    segments: VecDeque<(u64, String)>,
    /// 类型化 Draft span 缓冲（与 `segments` 并行保留，兼容旧 snapshot DTO）。
    draft_spans: VecDeque<(u64, DraftSpan)>,
    /// 因缓冲满被淘汰的最旧段数（诊断用）。
    truncated: usize,
    /// 已对外发出的状态事件数（诊断用）。
    status_emitted: u64,
    /// 因状态未变化被去重抑制的状态数（诊断用）。
    status_suppressed: u64,
    /// 已交付的 confirmed 段数（诊断用）。
    segments_emitted: u64,
    /// 状态边沿触发器（去重，0.24）。
    status_edge: EditorStatusEdge,
}

/// 快照缓冲上限（段数）。一段通常是一句话（几十字符），256 段远超
/// 一次听写的合理长度，同时保证内存有界（§3.6 有界 snapshot）。
const SNAPSHOT_MAX_SEGMENTS: usize = 256;

/// 波形音量事件的最小间隔（0.24）。
///
/// 音频块约 10ms 一块；100/s 的音量事件对波形动画没有额外信息量，
/// 却带来同频的 IPC + JSON + DOM 更新。25/s 视觉上无差别。
const VOICE_LEVEL_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(40);

/// 单次录音的流式链路计数器（音频 task 写、事件 task 读；诊断用）。
#[derive(Default)]
struct StreamCounters {
    /// 实际推送给 STT 的音频块数。
    chunks_received: AtomicU64,
    /// 因暂停被丢弃的音频块数。
    chunks_skipped_paused: AtomicU64,
}

impl EditorDictationState {
    fn new(epoch: u64, session_ref: String, generation: u64) -> Self {
        Self {
            epoch,
            session_ref,
            generation,
            tracker: EditorDictationTracker::new(),
            segments: VecDeque::new(),
            draft_spans: VecDeque::new(),
            truncated: 0,
            status_emitted: 0,
            status_suppressed: 0,
            segments_emitted: 0,
            status_edge: EditorStatusEdge::default(),
        }
    }

    /// 推导并记录一个 confirmed 增量段；返回 `(seq, text)` 供事件发射。
    fn push_confirmed_delta(&mut self, confirmed: &str) -> Option<(u64, String)> {
        let (seq, text) = self.tracker.extract_delta(confirmed)?;
        self.remember(seq, text.clone());
        Some((seq, text))
    }

    /// 推导并记录 Final 收尾段。
    fn push_final_delta(&mut self, final_text: &str) -> Option<(u64, String)> {
        let (seq, text) = self.tracker.extract_final_delta(final_text)?;
        self.remember(seq, text.clone());
        Some((seq, text))
    }

    /// 接收类型化 Draft；按 `span_id`/音频范围去重，不从累计文本反推增量。
    fn push_draft_span(&mut self, span: DraftSpan) -> Option<(u64, DraftSpan)> {
        let (seq, span) = self.tracker.accept_draft_span(span)?;
        self.remember_draft(seq, span.clone());
        Some((seq, span))
    }

    fn remember(&mut self, seq: u64, text: String) {
        if self.segments.len() >= SNAPSHOT_MAX_SEGMENTS {
            self.segments.pop_front();
            self.truncated += 1;
        }
        self.segments.push_back((seq, text));
    }

    fn remember_draft(&mut self, seq: u64, span: DraftSpan) {
        if self.draft_spans.len() >= SNAPSHOT_MAX_SEGMENTS {
            self.draft_spans.pop_front();
        }
        self.draft_spans.push_back((seq, span));
    }

    /// 返回 seq > after_seq 的段（epoch 不匹配返回 None）。
    fn after(&self, epoch: u64, after_seq: u64) -> Option<Vec<(u64, String)>> {
        if self.epoch != epoch {
            return None;
        }
        Some(
            self.segments
                .iter()
                .filter(|(seq, _)| *seq > after_seq)
                .cloned()
                .collect(),
        )
    }

    /// 返回带 span 身份的 snapshot，供新 DTO/前端补齐协议使用。
    fn after_draft_spans(&self, epoch: u64, after_seq: u64) -> Option<Vec<(u64, DraftSpan)>> {
        if self.epoch != epoch {
            return None;
        }
        Some(
            self.draft_spans
                .iter()
                .filter(|(seq, _)| *seq > after_seq)
                .cloned()
                .collect(),
        )
    }

    fn last_seq(&self) -> u64 {
        self.tracker.last_seq()
    }
}

/// 编辑器连续听写启动结果（command 层投影为结构化错误码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorVoiceStart {
    /// 已启动，携带本次听写 epoch。
    Started(u64),
    /// STT 总开关未启用。
    Disabled,
    /// 已有 VoiceSession 在录音（G1/G2/G3/Editor 互斥，§6.4）。
    Busy,
    /// 服务未就绪 / 引擎创建失败等（详情已经 VOICE_ERROR 事件下发）。
    Failed,
}

/// 语音会话状态。
struct VoiceSession {
    /// STT 引擎（旧接口，用于 GGUF 伪流式/非流式兼容）
    engine: Option<Arc<dyn SttEngine>>,
    /// 结构化 STT port（0.22.9：统一事件流）
    ///
    /// 存在时优先使用，替代旧的 transcribe_chunk/finalize 管线。
    /// 不存在时回退到旧管线（GGUF 伪流式适配器创建失败时）。
    stt_port: Option<Arc<dyn StreamingSttPort>>,
    /// 当前 session 的 generation（用于事件过滤）
    generation: Option<u64>,
    /// 事件消费 task 的 JoinHandle
    event_task: Option<tokio::task::JoinHandle<()>>,
    /// 音频采集器
    capture: Option<Box<dyn AudioCapture>>,
    /// 音频采集 task 的 JoinHandle（stop/cancel 时 abort，避免与 finalize 锁竞争）
    audio_task: Option<tokio::task::JoinHandle<()>>,
    /// 目标(G1/G2/G3/Editor)
    target: VoiceTarget,
    /// 是否正在录音
    recording: bool,
    /// G2: 录音开始时的前台窗口 HWND（用于注入前恢复焦点）
    prev_fg_hwnd: Option<isize>,
    /// Editor 连续听写模式（0.23.3）：录音持续到显式 stop，段式交付。
    continuous: bool,
    /// 暂停标志（Editor 连续听写）：true 时音频 task 丢弃 chunk 不推给 STT。
    paused: Arc<AtomicBool>,
    /// 终态文本交付闸门：每个 VoiceSession 最多注入/提交一次。
    final_delivery: Arc<AtomicBool>,
}

impl Default for VoiceSession {
    fn default() -> Self {
        Self {
            engine: None,
            stt_port: None,
            generation: None,
            event_task: None,
            capture: None,
            audio_task: None,
            target: VoiceTarget::ForegroundApp,
            recording: false,
            prev_fg_hwnd: None,
            continuous: false,
            paused: Arc::new(AtomicBool::new(false)),
            final_delivery: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// 语音服务:管理 hold-to-talk 录音 + STT + 注入管线。
pub struct VoiceService {
    session: Mutex<VoiceSession>,
    app: tauri::AppHandle,
    /// 0.22.15: VoiceService 级单调 recording epoch。
    ///
    /// 不依赖 adapter generation（每次 begin_session 从 1 开始），
    /// 而是在 VoiceService 层维护单调递增的 epoch，
    /// 确保 new epoch 后旧 epoch 的任何 final/end 都不能影响 UI。
    recording_epoch: Arc<AtomicU64>,
    /// 0.23.3: 编辑器连续听写 epoch（每次 start_editor_voice 递增，从 1 开始）。
    ///
    /// 与 recording_epoch 是两个边界：后者过滤跨录音的迟到事件，
    /// 前者标识一次听写会话（事件携带、前端按 epoch+seq 去重补齐）。
    dictation_epoch: AtomicU64,
    /// 当前/最近一次听写的共享状态（事件 task 推段，command 层读补齐）。
    /// 结束后保留到下一次听写开始；hold/chat 路径不消费它。
    editor_state: Mutex<Option<Arc<Mutex<EditorDictationState>>>>,
}

impl VoiceService {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self {
            session: Mutex::new(VoiceSession::default()),
            app,
            recording_epoch: Arc::new(AtomicU64::new(0)),
            dictation_epoch: AtomicU64::new(0),
            editor_state: Mutex::new(None),
        }
    }

    /// 0.22.15: 获取当前 recording epoch。
    #[allow(dead_code)]
    pub fn current_epoch(&self) -> u64 {
        self.recording_epoch.load(Ordering::Acquire)
    }

    /// 0.23.9: 从当前生产会话的 Coordinator 产出只读诊断快照。
    ///
    /// 仅当 session 正在录音且引擎为伪流式引擎时返回 `Some`。
    /// 不修改调度状态、不触发推理、不推进水位。
    pub fn coordinator_trace(
        &self,
        max_uncommitted_s: u64,
    ) -> Option<crate::domain::stt::pseudo_streaming::CoordinatorTrace> {
        let session = self.session.lock().unwrap_or_else(|p| p.into_inner());
        let engine = session.engine.as_ref()?;
        // downcast Arc<dyn SttEngine> 到 PseudoStreamingSttEngine
        let pseudo = engine
            .as_any()
            .downcast_ref::<crate::domain::stt::pseudo_streaming::PseudoStreamingSttEngine>()?;
        pseudo.snapshot_coordinator_trace(max_uncommitted_s)
    }

    /// Hold 事件:开始录音。
    ///
    /// 根据 main 窗口是否可见决定 G1/G2 目标。
    ///
    /// async 因为需要检查模型加载状态（HTTP /health 请求）。
    /// 返回 `true` = 录音已真正启动（调用方据此决定是否启动托盘动画等副作用）。
    pub async fn start_recording(&self) -> bool {
        // ── 总开关检查：STT 未启用时静默忽略 hold 事件 ──
        let config = crate::app::stt_config::get_stt_config();
        if !config.enabled {
            tracing::debug!("语音未启用,忽略 hold 事件");
            return false;
        }

        // ── 立即通知输入状态机进入 Recording ──
        // hold_fired 期间 reducer 已吞 Space/Alt keydown（防系统菜单），此处同步 voice phase
        // 使 ESC 能产生 VoiceCancel，并延续吞键到 keyup -> stop_recording 之间。必须在任何
        // .await 之前设置--否则 await 期间 ESC 无法取消。guard 确保所有早退路径
        // （服务未就绪 / 模型加载中等）回 Idle。
        struct VoiceRecordingGuard {
            armed: bool,
        }
        impl VoiceRecordingGuard {
            fn new() -> Self {
                crate::infra::platform::hotkey::InputController::update_voice_phase(
                    crate::infra::platform::hotkey::VoicePhase::Recording { gesture_id: 0 },
                );
                Self { armed: true }
            }
            fn disarm(&mut self) {
                self.armed = false;
            }
        }
        impl Drop for VoiceRecordingGuard {
            fn drop(&mut self) {
                if self.armed {
                    crate::infra::platform::hotkey::InputController::update_voice_phase(
                        crate::infra::platform::hotkey::VoicePhase::Idle,
                    );
                }
            }
        }
        let mut _voice_guard = VoiceRecordingGuard::new();

        // ── G1/G2 判定 + 互斥检查（scoped block：MutexGuard 不跨 await） ──
        let target;
        {
            let mut session = self.session.lock().unwrap();

            if session.recording {
                tracing::warn!("start_recording: 已在录音中,忽略");
                return false;
            }

            // 判断 G1/G2/G3：主窗口可见->G1，chat 窗口可见->G3，否则->G2
            let main_visible = self
                .app
                .get_webview_window("main")
                .map(|w| w.is_visible().unwrap_or(false))
                .unwrap_or(false);
            let chat_visible = self
                .app
                .get_webview_window("chat")
                .map(|w| w.is_visible().unwrap_or(false))
                .unwrap_or(false);
            target = if main_visible {
                VoiceTarget::MainWindow
            } else if chat_visible {
                VoiceTarget::ChatWindow
            } else {
                VoiceTarget::ForegroundApp
            };
            session.target = target;
        }

        // G2: 在服务就绪检查之前立即显示 overlay，让用户瞬间看到反馈。
        // overlay 初始显示默认文案（"语音输入中…"），服务检查完成后再更新内容
        // （错误消息或录音开始）。避免服务检查阻塞导致窗口延迟出现。
        if target == VoiceTarget::ForegroundApp {
            platform::window::show_voice_overlay(&self.app);
        }

        // ── 共享录音启动逻辑 ──
        if self.begin_recording(&config, true).await {
            // 录音真正开始，解除 guard（标志由 stop_recording/cancel_recording 清除）
            _voice_guard.disarm();
            true
        } else {
            false
        }
    }

    /// Chat 窗口 IPC 驱动:开始录音（0.12.2 §4.3）。
    ///
    /// 与 `start_recording` 的区别：
    /// - 不走热键状态机，由 `start_chat_stt` IPC command 直接调用
    /// - target 固定为 `ChatWindow`，不做 G1/G2 检测
    /// - 不设置 `VOICE_RECORDING` 标志（chat 窗口无需吞 Alt+Space）
    /// - 与 G1/G2 三方互斥（`session.recording` 标志保证同一时刻只有一个 target）
    pub async fn start_chat_recording(&self) {
        // ── 总开关检查 ──
        let config = crate::app::stt_config::get_stt_config();
        if !config.enabled {
            tracing::debug!("语音未启用,忽略 chat STT 请求");
            self.emit_voice_error(VoiceTarget::ChatWindow, "语音输入未启用，请在设置中开启");
            return;
        }

        // ── 互斥检查 + 设置 target ──
        {
            let mut session = self.session.lock().unwrap();
            if session.recording {
                tracing::warn!("start_chat_recording: 已在录音中,忽略");
                return;
            }
            session.target = VoiceTarget::ChatWindow;
        }

        // ── 共享录音启动逻辑（不设置 hotkey flag） ──
        if self.begin_recording(&config, false).await {
            // 0.17.2：chat 录音开始 → 托盘呼吸动画
            crate::app::tray::start_breathing(&self.app);
        }
    }

    // ── Editor 连续听写（0.23.3 §3.6）─────────────────────────────────────

    /// 编辑器连续听写：显式开始（由 start_editor_voice IPC 驱动）。
    ///
    /// 与 hold/chat 路径的差异：
    /// - target 固定 `Editor`，continuous 模式（录音持续到显式 stop）；
    /// - 启动即创建听写 epoch + 有界 snapshot 状态；
    /// - 不设置 hotkey flag（编辑器窗口无需吞 Alt+Space）；
    /// - G1/G2/G3/Editor 互斥沿用 `session.recording` 单槽。
    ///
    /// 会话身份（session_ref + generation）由 command 层经 EditorSessionService
    /// 校验后才传入；本方法只做 VoiceSession 侧的编排。
    pub async fn start_editor_recording(
        &self,
        session_ref: String,
        generation: u64,
    ) -> EditorVoiceStart {
        // ── 总开关检查 ──
        let config = crate::app::stt_config::get_stt_config();
        if !config.enabled {
            tracing::debug!("语音未启用,忽略 editor 听写请求");
            return EditorVoiceStart::Disabled;
        }

        // ── 互斥检查 + 设置 Editor 模式字段 ──
        {
            let mut session = self.session.lock().unwrap();
            if session.recording {
                tracing::warn!(active_target = ?session.target, "start_editor_recording: 已在录音中,拒绝");
                return EditorVoiceStart::Busy;
            }
            session.target = VoiceTarget::Editor;
            session.continuous = true;
            session.paused = Arc::new(AtomicBool::new(false));
            session.prev_fg_hwnd = None;
        }

        // ── 听写 epoch + 有界 snapshot ──
        let epoch = self.dictation_epoch.fetch_add(1, Ordering::Release) + 1;
        *self.editor_state.lock().unwrap() = Some(Arc::new(Mutex::new(EditorDictationState::new(
            epoch,
            session_ref,
            generation,
        ))));

        // 浮窗就近反馈（用户刚点了编辑器麦克风按钮，光标即按钮附近）
        platform::window::show_voice_overlay(&self.app);

        if self.begin_recording(&config, false).await {
            crate::app::tray::start_breathing(&self.app);
            tracing::info!(epoch, "编辑器连续听写开始");
            EditorVoiceStart::Started(epoch)
        } else {
            // begin_recording 失败详情已经 VOICE_ERROR(target=editor) 下发；
            // 此处回收浮窗与听写状态，不留残留。
            platform::window::hide_voice_overlay(&self.app);
            *self.editor_state.lock().unwrap() = None;
            self.session.lock().unwrap().continuous = false;
            EditorVoiceStart::Failed
        }
    }

    /// 暂停编辑器听写：音频 task 丢弃 chunk，STT 不再收到新音频；
    /// confirmed/preview 保持。返回是否生效。
    pub fn pause_editor_recording(&self) -> bool {
        let (paused, was_idle) = {
            let session = self.session.lock().unwrap();
            (
                session.paused.clone(),
                !session.recording || !session.continuous,
            )
        };
        if was_idle {
            tracing::warn!("pause_editor_recording: 未在连续听写中,忽略");
            return false;
        }
        if paused.swap(true, Ordering::SeqCst) {
            return true; // 已是暂停态，幂等
        }
        if let Some(state) = self.editor_state.lock().unwrap().clone() {
            let seq = state.lock().unwrap().last_seq();
            self.emit_editor_status(&state, "paused", seq, None, None);
        }
        tracing::info!("编辑器连续听写已暂停");
        true
    }

    /// 继续编辑器听写。返回是否生效。
    pub fn resume_editor_recording(&self) -> bool {
        let (paused, was_idle) = {
            let session = self.session.lock().unwrap();
            (
                session.paused.clone(),
                !session.recording || !session.continuous,
            )
        };
        if was_idle {
            tracing::warn!("resume_editor_recording: 未在连续听写中,忽略");
            return false;
        }
        if !paused.swap(false, Ordering::SeqCst) {
            return true; // 已是录音态，幂等
        }
        if let Some(state) = self.editor_state.lock().unwrap().clone() {
            let seq = state.lock().unwrap().last_seq();
            self.emit_editor_status(&state, "recording", seq, None, None);
        }
        tracing::info!("编辑器连续听写已继续");
        true
    }

    /// 结束编辑器听写（显式 stop）：通知引擎收尾，尾段经 Final 事件补交，
    /// 保留 confirmed、丢弃 preview。事件 task 完成后由本方法收尾浮窗与状态。
    pub async fn stop_editor_recording(&self) {
        let (stt_port, engine, generation) = {
            let mut session = self.session.lock().unwrap();
            if !session.recording || !session.continuous {
                tracing::warn!("stop_editor_recording: 未在连续听写中,忽略");
                return;
            }
            if let Some(mut capture) = session.capture.take() {
                capture.stop();
            }
            if let Some(handle) = session.audio_task.take() {
                handle.abort();
            }
            session.recording = false;
            session.continuous = false;
            (
                session.stt_port.take(),
                session.engine.take(),
                session.generation.take(),
            )
        };

        // finalizing 状态先行（尾段识别可能耗时数秒）
        if let Some(state) = self.editor_state.lock().unwrap().clone() {
            let seq = state.lock().unwrap().last_seq();
            self.emit_editor_status(&state, "finalizing", seq, None, None);
        }

        if let (Some(port), Some(session_gen)) = (&stt_port, generation) {
            if let Err(e) = port.finish_session(session_gen).await {
                tracing::warn!(%e, "editor 听写 finish_session 失败，回退 finalize 路径");
                let final_text = finalize_engine(engine).await;
                self.deliver_editor_final(&final_text).await;
            }
            // 成功路径：Final 由事件 task 处理（补尾段 + ended 状态），
            // 此处等待事件 task 完成（与 stop_recording 同款超时）。
            let event_task = self.session.lock().unwrap().event_task.take();
            if let Some(handle) = event_task {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(12), handle).await;
            }
        } else {
            // 无 port（异常态）：按 finalize 回退路径收尾，避免听写悬挂
            let final_text = finalize_engine(engine).await;
            self.deliver_editor_final(&final_text).await;
            self.session.lock().unwrap().event_task.take();
        }

        platform::window::hide_voice_overlay(&self.app);
        crate::app::tray::stop_breathing(&self.app);
        let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
        tracing::info!("编辑器连续听写结束（confirmed 已保留）");
    }

    /// Editor 回退路径的 Final 交付（finish_session 失败 / 无 port 时）。
    /// 正常路径 Final 由事件 task 处理，不走这里。
    async fn deliver_editor_final(&self, final_text: &str) {
        let Some(state) = self.editor_state.lock().unwrap().clone() else {
            return;
        };
        let mut st = state.lock().unwrap();
        let last_seq = st.last_seq();
        match st.push_final_delta(final_text) {
            Some((seq, text)) => {
                drop(st);
                self.emit_editor_segment(&state, seq, &text);
                self.emit_editor_status(&state, "ended", seq, None, None);
            }
            None => {
                drop(st);
                self.emit_editor_status(&state, "ended", last_seq, None, None);
            }
        }
    }

    /// STT 引擎 Error 事件的终态清理（仅 Editor 路径）：保留 confirmed、
    /// 丢弃 preview，结束会话并回收录音资源。由事件 task 调用（此时该 task
    /// 即将退出，event_task 句柄置 None 即可）。
    pub fn handle_editor_terminal_error(&self, message: &str) {
        {
            let mut session = self.session.lock().unwrap();
            if !session.recording || !session.continuous {
                return;
            }
            if let Some(mut capture) = session.capture.take() {
                capture.stop();
            }
            if let Some(handle) = session.audio_task.take() {
                handle.abort();
            }
            session.recording = false;
            session.continuous = false;
            session.event_task = None;
        }
        if let Some(state) = self.editor_state.lock().unwrap().clone() {
            let seq = state.lock().unwrap().last_seq();
            self.emit_editor_status(&state, "error", seq, None, Some(message));
            self.emit_editor_status(&state, "ended", seq, None, None);
        }
        let _ = self.app.emit(
            EventNames::VOICE_ERROR,
            serde_json::json!({ "message": message, "target": VoiceTarget::Editor.as_str() }),
        );
        let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
        platform::window::hide_voice_overlay(&self.app);
        crate::app::tray::stop_breathing(&self.app);
        tracing::warn!("编辑器连续听写因 STT 错误结束（confirmed 已保留）");
    }

    /// 补齐快照：返回 epoch 匹配且 seq > after_seq 的 confirmed 段。
    /// epoch 不匹配或无听写状态返回 None（前端按 None 提示无法补齐）。
    pub fn editor_voice_snapshot(
        &self,
        epoch: u64,
        after_seq: u64,
    ) -> Option<(u64, Vec<(u64, String)>, usize)> {
        let state = self.editor_state.lock().unwrap().clone()?;
        let st = state.lock().unwrap();
        let segments = st.after(epoch, after_seq)?;
        Some((st.epoch, segments, st.truncated))
    }

    /// 类型化 Editor snapshot：保留 `seq + DraftSpan`，供新 DTO/前端补缺。
    /// 旧 `editor_voice_snapshot` 继续提供 `(seq, text)` 兼容形态，避免
    /// 未同步升级的 command/前端在协议切换期间丢失已交付段。
    pub fn editor_voice_snapshot_spans(
        &self,
        epoch: u64,
        after_seq: u64,
    ) -> Option<(u64, Vec<(u64, DraftSpan)>, usize)> {
        let state = self.editor_state.lock().unwrap().clone()?;
        let st = state.lock().unwrap();
        let spans = st.after_draft_spans(epoch, after_seq)?;
        Some((st.epoch, spans, st.truncated))
    }

    /// EditorSession 结束后的最终兜底：只释放匹配会话的连续听写与快照。
    /// 若仍在录音则按取消语义保留已交付 confirmed、丢弃 preview；绝不影响
    /// 此后可能启动的 G1/G2/G3 或新一代 Editor VoiceSession。
    pub fn release_editor_session(&self, session_ref: &str, generation: u64) {
        let state = {
            let guard = self.editor_state.lock().unwrap();
            guard
                .as_ref()
                .filter(|state| {
                    let st = state.lock().unwrap();
                    st.session_ref == session_ref && st.generation == generation
                })
                .cloned()
        };
        let Some(state) = state else { return };

        // 与 start_editor_recording 相同的锁顺序（session → editor_state）复核，
        // 避免旧会话释放与新一代听写启动交错时误取消新录音。
        let recording_matches = {
            let session = self.session.lock().unwrap();
            let mut editor_guard = self.editor_state.lock().unwrap();
            let still_current = editor_guard
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &state));
            if !still_current {
                return;
            }
            let matches =
                session.recording && session.continuous && session.target == VoiceTarget::Editor;
            if !matches {
                *editor_guard = None;
            }
            matches
        };
        if recording_matches {
            // 活动录音会阻止另一录音并发启动；释放锁后取消不会波及新会话。
            self.cancel_recording();
            let mut guard = self.editor_state.lock().unwrap();
            if guard
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &state))
            {
                *guard = None;
            }
        }
        tracing::debug!(%session_ref, generation, "编辑器听写状态已随会话释放");
    }

    /// 是否存在活动的编辑器连续听写（0.23.5 看门狗预留）。
    #[allow(dead_code)]
    pub fn is_editor_recording(&self) -> bool {
        let session = self.session.lock().unwrap();
        session.recording && session.continuous
    }

    /// 发射编辑器听写段事件（blink://editor-voice-segment）。
    fn emit_editor_segment(&self, state: &Arc<Mutex<EditorDictationState>>, seq: u64, text: &str) {
        emit_editor_segment(&self.app, state, seq, text);
    }

    /// 发射编辑器听写状态事件（blink://editor-voice-status）。
    fn emit_editor_status(
        &self,
        state: &Arc<Mutex<EditorDictationState>>,
        phase: &str,
        seq: u64,
        preview: Option<&str>,
        message: Option<&str>,
    ) {
        emit_editor_status(&self.app, state, phase, seq, preview, message);
    }

    /// 共享录音启动逻辑：服务就绪检查 + 模型加载检查 + 引擎创建 + 音频采集 + 采集 task。
    ///
    /// **调用方职责**：
    /// - 配置检查（STT enabled）
    /// - 设置 `session.target`（G1/G2 检测或 ChatWindow）
    /// - 检查 `session.recording`（互斥）
    /// - 管理 `VoiceRecordingGuard`（仅热键路径）
    ///
    /// `set_voice_flag`：是否通知输入状态机进入 Recording（仅热键路径需要）。
    /// 返回 `true` = 录音已启动。
    ///
    /// **0.22.6 批次 3**：本地模式下先从 `EngineManager` 获取连接快照
    /// （endpoint + token + engine_id + instance_id），再做 token-aware health 检查。
    /// 不再用配置中的 preferred port 做 port-only health——动态分配的 endpoint
    /// 可能与 preferred port 不一致，且 Python /health 强制要求 token 鉴权。
    async fn begin_recording(
        &self,
        config: &crate::app::stt_config::SttConfig,
        set_voice_flag: bool,
    ) -> bool {
        use crate::app::stt_config::SttMode;

        let target = self.session.lock().unwrap().target;

        // 0.22.7 GGUF worker：本地模式下从 EngineManager 获取连接快照，
        // 就绪检查走 worker transport 的 NDJSON hello（无 HTTP 端口）。
        // handoff-11：ONNX 真流式线路已退役，GGUF 伪流式是唯一本地路径。
        let connection = if config.mode == SttMode::Local {
            let svc = self
                .app
                .try_state::<std::sync::Arc<crate::app::local_engine::EngineManager>>();
            match svc {
                Some(s) => {
                    let engine_id = crate::infra::local_engine::runtime::EngineId::new(
                        crate::app::local_engine::funasr::FUNASR_ENGINE_ID,
                    )
                    .unwrap_or_else(|_| {
                        crate::infra::local_engine::runtime::EngineId::new("funasr").unwrap()
                    });
                    match s.get_connection(&engine_id).await {
                        Ok(Some(conn)) => {
                            // GGUF：投影 LocalEngineConnection → SttEngineConnection
                            // 0.22.7.4：worker 传输是唯一本地实现，endpoint
                            // 不承载地址语义（host/port 为诊断占位）。
                            match conn.worker {
                                Some(transport) => Some(crate::domain::stt::SttEngineConnection {
                                    host: "127.0.0.1".to_string(),
                                    port: 0,
                                    engine_id: conn.engine_id,
                                    instance_id: conn.instance_id,
                                    transport: Some(transport),
                                }),
                                None => {
                                    tracing::warn!(
                                        engine = %conn.engine_id,
                                        "运行中的本地引擎连接缺少 worker 通道（应为 StdioWorker）"
                                    );
                                    None
                                }
                            }
                        }
                        Ok(None) => None, // 服务未运行
                        Err(e) => {
                            tracing::warn!(%e, "get_connection 查询失败");
                            None
                        }
                    }
                }
                None => None, // EngineManager 未注册
            }
        } else {
            None
        };

        // ── 服务就绪检查 ──
        let need_check = match config.mode {
            SttMode::Local => true,
            SttMode::Cloud => config.cloud_provider.is_none(),
        };
        if need_check {
            // 0.22.6: 本地模式下必须先获取连接，再检查服务状态
            // 无连接 = 服务未运行，直接中止
            if config.mode == SttMode::Local && connection.is_none() {
                let msg = "FunASR 服务未运行，请在设置页「语音输入」中启动服务";
                tracing::warn!(target = ?target, %msg, "语音录音中止：无连接");
                self.emit_voice_error(target, msg);
                return false;
            }

            let (ready, msg) = match config.mode {
                SttMode::Local => {
                    let conn = connection.as_ref().expect("connection 已在上方验证为 Some");
                    // 0.22.7 GGUF worker：就绪检查走 NDJSON 通道 hello——
                    // 通道与实例绑定，模型就绪由 start 的 ready 握手保证。
                    match conn.transport.as_ref() {
                        Some(transport) => match transport.check_ready().await {
                            Ok(()) => (true, String::new()),
                            Err(e) => (false, format!("语音服务不可用：{e}。请在设置页重启服务。")),
                        },
                        None => (
                            false,
                            "语音服务连接缺少 worker 通道，请在设置页重启服务".to_string(),
                        ),
                    }
                }
                SttMode::Cloud => (false, "云端 STT 未配置供应商，请在设置页中配置".to_string()),
            };
            if !ready {
                tracing::warn!(target = ?target, %msg, "语音录音中止：服务未就绪");
                self.emit_voice_error(target, &msg);
                return false;
            }

            // ── 模型加载状态检查（本地模式）──
            // 0.22.7 GGUF worker：就绪已由上方通道 hello 验证（模型加载完成
            // 才有 ready），无需二次模型状态轮询。
        };

        // ── 重新获取 session 锁，创建引擎 + 启动采集 ──
        // 注意：std::sync::MutexGuard 不是 Send，所有锁操作必须在不含 await 的 block 内完成。
        let (
            stt_port,
            engine_arc,
            mut rx,
            target,
            _target_str,
            prev_fg_hwnd,
            paused_flag,
            final_delivery,
        ) = {
            let mut session = self.session.lock().unwrap();

            // 二次检查：模型加载等待期间可能已被 cancel
            if session.recording {
                tracing::warn!("begin_recording: 模型加载期间已被其他路径占用");
                return false;
            }

            // GGUF 引擎创建 + 伪流式适配器包装（handoff-11 后唯一本地路径）。
            let (engine_arc, stt_port): (Option<Arc<dyn SttEngine>>, Arc<dyn StreamingSttPort>) = {
                let profile = if matches!(target, VoiceTarget::ForegroundApp | VoiceTarget::Editor)
                {
                    RecognitionProfile::PreviewDraft
                } else {
                    RecognitionProfile::Legacy
                };
                let engine = match crate::domain::stt::create_engine_with_profile(
                    connection.clone(),
                    profile,
                ) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(target = ?session.target, %e, "语音录音中止：引擎创建失败");
                        let msg = e;
                        self.emit_voice_error(session.target, &msg);
                        return false;
                    }
                };
                engine.reset();
                let engine_arc: Arc<dyn SttEngine> = Arc::from(engine);
                let port: Arc<dyn StreamingSttPort> = Arc::new(
                    crate::domain::stt::streaming_port::GgufStreamingAdapter::new_with_profile(
                        engine_arc.clone(),
                        profile,
                    ),
                );
                (Some(engine_arc), port)
            };

            let mut capture = if let Some(dev_id) = &config.audio_device_id {
                platform::audio::create_capture_with_device(dev_id.clone())
            } else {
                platform::audio::create_capture()
            };

            let format = AudioFormat::default();
            match capture.start(format) {
                Ok(rx) => {
                    // capture 必须存入 session——它是块内局部变量，块结束时
                    // drop 会触发 CpalCapture::drop 停掉采集线程，音频通道
                    // 随即关闭，识别全程收不到任何样本（0.22.9 回归）。
                    session.capture = Some(capture);
                    session.recording = true;

                    // 通知输入状态机进入 Recording（仅热键路径，使 ESC 能产生 VoiceCancel）
                    if set_voice_flag {
                        crate::infra::platform::hotkey::InputController::update_voice_phase(
                            crate::infra::platform::hotkey::VoicePhase::Recording { gesture_id: 0 },
                        );
                    }

                    tracing::info!(
                        target = ?session.target,
                        "语音录音开始"
                    );

                    // G2: overlay 已在 start_recording 中提前显示，此处只需保存前台窗口 HWND
                    if session.target == VoiceTarget::ForegroundApp {
                        session.prev_fg_hwnd = platform::window::get_foreground_hwnd();
                    }

                    let target = session.target;
                    let target_str = session.target.as_str();
                    let prev_fg_hwnd = session.prev_fg_hwnd;
                    let paused_flag = session.paused.clone();
                    let final_delivery = Arc::new(AtomicBool::new(false));
                    session.final_delivery = final_delivery.clone();

                    // 通知前端录音已开始
                    let epoch_val = self.recording_epoch.fetch_add(1, Ordering::Release) + 1;
                    let _ = self.app.emit(
                        EventNames::VOICE_RECORDING_START,
                        serde_json::json!({ "target": target_str, "epoch": epoch_val }),
                    );

                    (
                        stt_port,
                        engine_arc,
                        rx,
                        target,
                        target_str,
                        prev_fg_hwnd,
                        paused_flag,
                        final_delivery,
                    )
                }
                Err(e) => {
                    tracing::error!(%e, "音频采集启动失败");
                    return false;
                }
            }
        }; // MutexGuard 在此释放，后续 await 安全

        // 0.22.9：begin session 获取 generation（在锁外 await）
        let port = stt_port.clone();
        let session_gen = match port.begin_session().await {
            Ok(g) => g,
            Err(e) => {
                tracing::error!(%e, "begin_session 失败");
                self.emit_voice_error(target, &e.to_string());
                {
                    let mut session = self.session.lock().unwrap();
                    session.recording = false;
                    // 立即 drop capture（停采集 + 关通道），不留残留实例
                    session.capture.take();
                }
                return false;
            }
        };

        // 0.22.15：recording epoch 已在 VOICE_RECORDING_START emit 时递增（epoch_val）
        let recording_epoch = {
            let e = self.recording_epoch.load(Ordering::Acquire);
            tracing::debug!(recording_epoch = e, session_gen, "录音 epoch");
            e
        };

        // 获取事件 receiver（在 begin_session 之后）
        let event_rx = port.events();

        // Editor 连续听写：事件 task 携带共享听写状态（推段/补齐）+ 暂停标志 +
        // VoiceService 句柄（STT 错误终态清理）。非 Editor 路径 editor_state 为 None。
        let editor_state_for_events = if target == VoiceTarget::Editor {
            self.editor_state.lock().unwrap().clone()
        } else {
            None
        };
        let voice_for_events = self
            .app
            .try_state::<std::sync::Arc<VoiceService>>()
            .map(|s| s.inner().clone());

        // spawn 事件消费 task：按 generation 过滤，emit 到前端
        let app_for_events = self.app.clone();
        let target_for_events = target;
        let prev_hwnd_for_events = prev_fg_hwnd;
        let epoch_for_events = recording_epoch;
        let epoch_arc = self.recording_epoch.clone();
        let paused_for_events = paused_flag.clone();
        let port_for_events = stt_port.clone();
        let counters = Arc::new(StreamCounters::default());
        let counters_for_events = counters.clone();
        let final_delivery_for_events = final_delivery.clone();
        let event_task = tokio::spawn(async move {
            consume_stt_events(
                event_rx,
                session_gen,
                epoch_for_events,
                epoch_arc,
                target_for_events,
                prev_hwnd_for_events,
                app_for_events,
                editor_state_for_events,
                paused_for_events,
                voice_for_events,
                port_for_events,
                counters_for_events,
                final_delivery_for_events,
            )
            .await;
        });

        // spawn 采集 task: audio chunk → push_audio（非阻塞）
        let app = self.app.clone();
        let port_for_audio = stt_port.clone();
        let target_for_audio = target;
        let counters_for_audio = counters.clone();

        let task_handle = tokio::spawn(async move {
            // 首块立即发一次音量，之后按 VOICE_LEVEL_MIN_INTERVAL 节流
            let mut last_level_emit = std::time::Instant::now() - VOICE_LEVEL_MIN_INTERVAL;
            while let Some(chunk) = rx.recv().await {
                // 0.23.3：Editor 听写暂停时丢弃 chunk（不推 STT、不发音量事件），
                // 恢复后从静音继续，暂停期间的话语不进识别。
                if paused_flag.load(Ordering::Relaxed) {
                    counters_for_audio
                        .chunks_skipped_paused
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                counters_for_audio
                    .chunks_received
                    .fetch_add(1, Ordering::Relaxed);

                // 计算 RMS 音量（0.0 ~ 1.0）——按 25/s 节流发音量事件：
                // 10ms/chunk 的 100/s 对波形动画没有额外信息量，只增加 IPC 负载
                if last_level_emit.elapsed() >= VOICE_LEVEL_MIN_INTERVAL {
                    last_level_emit = std::time::Instant::now();
                    let level = compute_rms(&chunk.samples);
                    let target_str = target_for_audio.as_str();
                    let _ = app.emit(
                        EventNames::VOICE_LEVEL,
                        serde_json::json!({
                            "level": level,
                            "target": target_str,
                        }),
                    );
                }

                // 0.22.9：通过统一 port 推送音频
                // push_audio 不阻塞——内部通过 channel 转发
                if let Err(e) = port_for_audio.push_audio(session_gen, &chunk.samples).await {
                    tracing::warn!(%e, "push_audio 失败");
                    break;
                }
            }
            tracing::debug!("音频采集 channel 已关闭");
        });

        // 重新获取锁写入剩余字段
        {
            let mut session = self.session.lock().unwrap();
            session.generation = Some(session_gen);
            // ONNX 真流式路径 engine 为 None（finalize 回退路径不适用，
            // port 的 finish/cancel 语义完整覆盖）
            session.engine = engine_arc;
            session.stt_port = Some(stt_port);
            session.audio_task = Some(task_handle);
            session.event_task = Some(event_task);
        }

        true
    }

    /// HoldRelease 事件:停止录音 → STT 最终识别 → 注入/填充。
    ///
    /// async 因为 `SttEngine::finalize` 是 async（HTTP 请求）。
    /// 调用方（HotkeyService）在 async task 中 .await 此方法。
    ///
    /// 0.12.2 §4.3：ChatWindow 路径由 `stop_chat_recording` 调用此方法，
    /// 最终文本通过 `voice-partial(target="chat")` emit 到 chat 窗口。
    pub async fn stop_recording(&self) {
        // 取出 engine + port + generation + 停止采集 + abort 音频 task，然后立即释放锁
        let (engine, stt_port, generation, target) = {
            let mut session = self.session.lock().unwrap();

            if !session.recording {
                tracing::warn!("stop_recording: 未在录音中,忽略");
                // 服务未就绪等早退路径：overlay 已在 start_recording 中提前显示，
                // emit_voice_error 已更新了内容（不再 spawn 延迟 hide task）。
                // 松键即隐藏 overlay + 回 Idle + 清理前端状态。
                crate::infra::platform::hotkey::InputController::update_voice_phase(
                    crate::infra::platform::hotkey::VoicePhase::Idle,
                );
                platform::window::hide_voice_overlay(&self.app);
                let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
                return;
            }

            // 停止采集
            if let Some(mut capture) = session.capture.take() {
                capture.stop();
            }

            // ★ abort 音频采集 task —— 释放 streaming engine 的 inner 锁
            // transcribe_chunk 可能在 ensure_connected 中阻塞（WebSocket 握手慢），
            // 持有 tokio::sync::Mutex。如果不 abort，finalize() 会永久阻塞在锁上。
            // abort 会取消 future，drop MutexGuard，释放锁。
            if let Some(handle) = session.audio_task.take() {
                handle.abort();
                tracing::debug!("音频采集 task 已 abort");
            }

            let target = session.target;
            let engine = session.engine.take();
            let stt_port = session.stt_port.take();
            let generation = session.generation.take();
            session.recording = false;

            // 通知输入状态机回 Idle
            crate::infra::platform::hotkey::InputController::update_voice_phase(
                crate::infra::platform::hotkey::VoicePhase::Idle,
            );

            (engine, stt_port, generation, target)
        }; // 锁在此释放，await 不持锁

        // 0.22.9：通过 StreamingSttPort::finish_session 通知引擎音频流结束
        // Final 结果将通过事件消费 task 异步产出

        // G2: 松键立即隐藏 overlay（识别 + 注入在后台进行）
        if target == VoiceTarget::ForegroundApp {
            platform::window::hide_voice_overlay(&self.app);
            let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
        }

        if let (Some(port), Some(session_gen)) = (&stt_port, generation) {
            if let Err(e) = port.finish_session(session_gen).await {
                tracing::warn!(%e, "finish_session 失败，回退旧 finalize 路径");
                // 回退：直接调 finalize_engine
                let final_text = finalize_engine(engine).await;
                self.deliver_final_text(target, final_text).await;
            } else {
                // finish_session 已产出 Final 事件，事件消费 task 会处理
                // 但需要给事件消费 task 时间处理 Final——等待它完成
                // 0.22.9：等待 event_task 完成或超时
                let event_task = self.session.lock().unwrap().event_task.take();
                if let Some(handle) = event_task {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(12), handle).await;
                }
                // 事件消费 task 已完成（或超时），清理
            }
            return;
        }

        // 回退：旧 finalize 路径（stt_port 不存在时）
        let final_text = finalize_engine(engine).await;
        self.deliver_final_text(target, final_text).await;
    }

    /// Chat 窗口 IPC 驱动:停止录音（0.12.2 §4.3）。
    ///
    /// 与 `stop_recording` 共用同一逻辑——target 已在 `start_chat_recording` 时
    /// 设为 `ChatWindow`，`stop_recording` 按 target 分支到 G3 路径。
    pub async fn stop_chat_recording(&self) {
        self.stop_recording().await;
        // 0.17.2：chat 录音结束 → 停止托盘呼吸动画
        crate::app::tray::stop_breathing(&self.app);
    }

    pub fn cancel_recording(&self) {
        // 取出 stt_port + generation + 停止采集 + abort tasks，然后释放锁
        let (stt_port, generation, _target, was_editor) = {
            let mut session = self.session.lock().unwrap();

            if !session.recording {
                return;
            }

            if let Some(mut capture) = session.capture.take() {
                capture.stop();
            }

            // abort 音频采集 task（与 stop_recording 一致）
            if let Some(handle) = session.audio_task.take() {
                handle.abort();
            }

            // abort 事件消费 task
            if let Some(handle) = session.event_task.take() {
                handle.abort();
            }

            let target = session.target;
            let stt_port = session.stt_port.take();
            let generation = session.generation.take();
            session.recording = false;
            session.engine = None;
            let was_editor = target == VoiceTarget::Editor && session.continuous;
            session.continuous = false;

            // 通知输入状态机回 Idle
            crate::infra::platform::hotkey::InputController::update_voice_phase(
                crate::infra::platform::hotkey::VoicePhase::Idle,
            );

            (stt_port, generation, target, was_editor)
        }; // 锁在此释放

        // 0.22.9：通过 StreamingSttPort::cancel_session 通知引擎丢弃在途结果。
        // cancel_session 是 async，但 cancel 不应阻塞 P0 主链路（ESC 后用户已离开），
        // 用 spawn 脱离调用方 effect 循环。cancel 幂等——即使 spawn 未执行也不影响正确性。
        if let (Some(port), Some(session_gen)) = (stt_port, generation) {
            tokio::spawn(async move {
                if let Err(e) = port.cancel_session(session_gen).await {
                    tracing::warn!(%e, "cancel_session 失败（可忽略——cancel 幂等）");
                }
            });
        }

        tracing::info!("语音录音已取消");

        // Editor 连续听写取消（0.23.3）：confirmed 已交付的段保留（不回撤正文），
        // 在途 preview 丢弃；通知编辑器前端回到 idle。
        if was_editor {
            if let Some(state) = self.editor_state.lock().unwrap().clone() {
                let seq = state.lock().unwrap().last_seq();
                self.emit_editor_status(&state, "ended", seq, None, None);
            }
            crate::app::tray::stop_breathing(&self.app);
        }

        // 隐藏 mini overlay(G2)
        platform::window::hide_voice_overlay(&self.app);

        // 通知前端录音已结束（G1 隐藏语音指示器 + 恢复 Ghost overlay / G3 chat 麦克风恢复）
        let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
    }

    /// 向用户反馈语音错误（G1 直接 emit，G2 先显示 overlay 再延迟 emit，G3 直接 emit）。
    ///
    /// **绝不**用 Mock 引擎的假文本上屏——错误就是错误，告知用户而非静默吞掉。
    fn emit_voice_error(&self, target: VoiceTarget, message: &str) {
        if target == VoiceTarget::ForegroundApp {
            // G2: overlay 已在 start_recording 中提前显示，此处只 emit 错误消息更新内容。
            // 不再 show_voice_overlay（已显示），也不 spawn 延迟 hide--
            // overlay 生命周期由 stop_recording/cancel_recording 统一管理（松键即隐藏）。
            let _ = self.app.emit(
                EventNames::VOICE_ERROR,
                serde_json::json!({
                    "message": message,
                    "target": target.as_str(),
                }),
            );
        } else {
            // G1/G3: 直接 emit（窗口已可见，事件就绪）
            let _ = self.app.emit(
                EventNames::VOICE_ERROR,
                serde_json::json!({
                    "message": message,
                    "target": target.as_str(),
                }),
            );
        }
    }

    /// 是否正在录音。
    pub fn is_recording(&self) -> bool {
        self.session.lock().unwrap().recording
    }

    /// 交付最终识别文本到目标（G1/G2/G3）。
    ///
    /// 0.22.9：从旧 `stop_recording` 的内联交付逻辑提取为独立方法。
    /// 0.22.15：改为调用统一的 `deliver_final` 函数，消除两份近似分支。
    ///
    /// - G1: emit `CHORD_FILL_QUERY` + `VOICE_RECORDING_END`
    /// - G2: spawn 后台 inject_text（脱离 effect 循环，恢复焦点 + 注入）
    /// - G3: emit `VOICE_PARTIAL(target="chat")` + `VOICE_RECORDING_END`
    async fn deliver_final_text(&self, target: VoiceTarget, final_text: String) {
        let (prev_hwnd, final_delivery) = {
            let session = self.session.lock().unwrap();
            (session.prev_fg_hwnd, session.final_delivery.clone())
        };
        deliver_final(
            &self.app,
            target,
            &final_text,
            prev_hwnd,
            Some(final_delivery.as_ref()),
        );
    }
}

/// 发射编辑器听写段事件（供 VoiceService 与事件 task 共用）。
fn emit_editor_segment(
    app: &tauri::AppHandle,
    state: &Arc<Mutex<EditorDictationState>>,
    seq: u64,
    text: &str,
) {
    emit_editor_segment_with_span(app, state, seq, text, None);
}

/// 发射带 DraftSpan 身份的编辑器听写段；顶层 text 保留给旧前端兼容。
fn emit_editor_draft_segment(
    app: &tauri::AppHandle,
    state: &Arc<Mutex<EditorDictationState>>,
    seq: u64,
    span: &DraftSpan,
) {
    emit_editor_segment_with_span(app, state, seq, &span.text, Some(span));
}

fn emit_editor_segment_with_span(
    app: &tauri::AppHandle,
    state: &Arc<Mutex<EditorDictationState>>,
    seq: u64,
    text: &str,
    span: Option<&DraftSpan>,
) {
    let (epoch, session_ref, generation) = {
        let mut st = state.lock().unwrap();
        st.segments_emitted = st.segments_emitted.saturating_add(1);
        (st.epoch, st.session_ref.clone(), st.generation)
    };
    let mut payload = serde_json::json!({
        "sessionRef": session_ref,
        "generation": generation,
        "epoch": epoch,
        "seq": seq,
        "text": text,
    });
    if let Some(span) = span {
        if let Ok(value) = serde_json::to_value(span) {
            payload["span"] = value;
        }
    }
    let _ = app.emit(EventNames::EDITOR_VOICE_SEGMENT, payload);
    tracing::debug!(
        session_ref = %session_ref,
        generation,
        epoch,
        seq,
        chars = text.chars().count(),
        "编辑器听写段已交付"
    );
}

/// 发射编辑器听写状态事件（供 VoiceService 与事件 task 共用）。
///
/// **边沿触发（0.24）**：phase/seq/preview/message/code 全部未变化时不发送、
/// 不写日志，仅累加诊断计数——修复了"每 10ms 一条状态事件 + 一条 debug 日志"
/// 的风暴。
fn emit_editor_status(
    app: &tauri::AppHandle,
    state: &Arc<Mutex<EditorDictationState>>,
    phase: &str,
    seq: u64,
    preview: Option<&str>,
    message: Option<&str>,
) {
    let code = if phase == "error" {
        Some("stt_failed")
    } else {
        None
    };
    let (epoch, session_ref, generation) = {
        let mut st = state.lock().unwrap();
        if !st
            .status_edge
            .should_emit(phase, seq, preview, message, code)
        {
            st.status_suppressed = st.status_suppressed.saturating_add(1);
            return;
        }
        st.status_emitted = st.status_emitted.saturating_add(1);
        (st.epoch, st.session_ref.clone(), st.generation)
    };
    let _ = app.emit(
        EventNames::EDITOR_VOICE_STATUS,
        serde_json::json!({
            "sessionRef": session_ref,
            "generation": generation,
            "epoch": epoch,
            "phase": phase,
            "seq": seq,
            "preview": preview,
            "message": message,
            "code": code,
        }),
    );
    tracing::debug!(
        session_ref = %session_ref,
        generation,
        epoch,
        seq,
        phase,
        preview_chars = preview.map(str::chars).map(Iterator::count).unwrap_or(0),
        has_message = message.is_some(),
        "编辑器听写状态已更新"
    );
}

/// 输出 STT 流式链路统计（0.24）。
///
/// 只含计数、布尔与长度，**不含正文/转写内容**（spec-backend §三）。
/// `final_report` 为 true 时用 info（会话终态汇总），否则用 debug（定期统计）。
///
/// 字段清单抽在此宏内，避免两条日志分支重复抄写（`tracing::event!` 的
/// level 必须是常量，故按级别分支调用）。
macro_rules! stream_stats_log {
    ($macro:ident, $target:expr, $stats:expr, $counters:expr, $status:expr) => {
        tracing::$macro!(
            voice_target = $target.as_str(),
            chunks_received = $counters.chunks_received.load(Ordering::Relaxed),
            chunks_skipped_paused = $counters.chunks_skipped_paused.load(Ordering::Relaxed),
            partial_events = $stats.partials_emitted,
            partial_events_suppressed = $stats.partials_suppressed,
            partial_events_coalesced = $stats.partials_coalesced,
            partial_events_dropped_closed = $stats.partials_dropped_closed,
            confirmed_backpressure = $stats.confirmed_backpressure,
            status_events = $status.0,
            status_events_suppressed = $status.1,
            segments_delivered = $status.2,
            queue_depth = $stats.queue_depth,
            queue_capacity = $stats.queue_capacity,
            max_queue_depth = $stats.max_queue_depth,
            pcm_samples = $stats.pcm_samples,
            pcm_committed_end = $stats.pcm_committed_end,
            in_flight_inferences = $stats.in_flight_inferences,
            confirmed_revision = $stats.confirmed_revision,
            preview_revision = $stats.preview_revision,
            "STT 流式链路统计"
        );
    };
}

fn log_stream_stats(
    target: VoiceTarget,
    stats: &crate::domain::stt::SttStreamStats,
    counters: &StreamCounters,
    editor: Option<&Arc<Mutex<EditorDictationState>>>,
    final_report: bool,
) {
    let status = editor
        .map(|state| {
            let st = state.lock().unwrap();
            (st.status_emitted, st.status_suppressed, st.segments_emitted)
        })
        .unwrap_or((0, 0, 0));

    if final_report {
        stream_stats_log!(info, target, stats, counters, status);
    } else {
        stream_stats_log!(debug, target, stats, counters, status);
    }
}

/// 0.22.15：统一交付最终文本——供 `deliver_final_text`（stop 路径）
/// 和 `consume_stt_events`（事件路径）共用，避免两份近似分支。
///
/// - G1: emit `CHORD_FILL_QUERY` + `VOICE_RECORDING_END`
/// - G2: spawn 后台 inject_text
/// - G3: emit `VOICE_PARTIAL(target="chat")` + `VOICE_RECORDING_END`
fn deliver_final(
    app: &tauri::AppHandle,
    target: VoiceTarget,
    text: &str,
    prev_fg_hwnd: Option<isize>,
    delivery_guard: Option<&AtomicBool>,
) {
    if text.is_empty() {
        tracing::debug!("识别结果为空,跳过交付");
        let _ = app.emit(EventNames::VOICE_RECORDING_END, ());
        return;
    }

    // finish/fallback 与事件 task 可能在边界上同时看到终态；同一 session
    // 只允许一次最终交付，防止 G2 重复注入或 G1/G3 重复提交。
    if let Some(guard) = delivery_guard
        && guard.swap(true, Ordering::AcqRel)
    {
        tracing::debug!(target = ?target, "忽略重复的 STT 最终交付");
        return;
    }

    tracing::debug!(
        target = ?target,
        text_len = text.chars().count(),
        "交付最终文本"
    );

    match target {
        VoiceTarget::MainWindow => {
            let _ = app.emit(
                EventNames::CHORD_FILL_QUERY,
                serde_json::Value::String(text.to_string()),
            );
            tracing::debug!("G1: 文字已 emit chord-fill-query");
            let _ = app.emit(EventNames::VOICE_RECORDING_END, ());
        }
        VoiceTarget::ForegroundApp => {
            let text_owned = text.to_string();
            tokio::spawn(async move {
                tokio::task::spawn_blocking(move || {
                    if let Some(hwnd) = prev_fg_hwnd {
                        platform::window::restore_foreground_g2(hwnd);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    if let Err(e) = platform::inject::inject_text(&text_owned) {
                        tracing::error!(%e, "G2: 文本注入失败");
                    }
                });
            });
        }
        VoiceTarget::ChatWindow => {
            let _ = app.emit(
                EventNames::VOICE_PARTIAL,
                serde_json::json!({
                    "text": text,
                    "target": "chat",
                }),
            );
            let _ = app.emit(EventNames::VOICE_RECORDING_END, ());
        }
        // Editor 不经 deliver_final（连续听写走 EDITOR_VOICE_* 事件路径）；
        // 此分支仅为穷尽性兜底。
        VoiceTarget::Editor => {
            tracing::debug!("deliver_final: Editor 目标不应到达此处，忽略");
        }
    }
}

/// 把类型化 Draft ledger 投影为旧 G1/G2/G3 的累计 confirmed 文本。
/// 这里是展示/提交层的投影，不是领域层的音频结果合并；Draft 本身仍按
/// 非重叠时间范围进入 ledger。
fn draft_ledger_text(ledger: &[DraftSpan]) -> String {
    ledger.iter().map(|span| span.text.as_str()).collect()
}

fn append_draft_ledger(ledger: &mut Vec<DraftSpan>, span: DraftSpan) -> bool {
    if span.text.is_empty()
        || ledger.iter().any(|existing| {
            existing.span_id == span.span_id
                || (existing.audio_range.len() > 0
                    && span.audio_range.len() > 0
                    && (existing.audio_range.contains(span.audio_range)
                        || span.audio_range.contains(existing.audio_range)
                        || existing.audio_range.overlaps(span.audio_range)))
        })
    {
        return false;
    }
    ledger.push(span);
    true
}

/// 兼容两种 Final 语义：旧 profile 返回累计全文，PreviewDraft 可返回
/// terminal tail。只基于已提交 Draft ledger 做明显前缀判断，绝不使用 Preview
/// 作为最终文本，也不对 Draft 之间做字符串相似度合并。
fn compose_final_text(ledger: &[DraftSpan], confirmed_cache: &str, final_text: &str) -> String {
    let committed = if ledger.is_empty() {
        confirmed_cache.to_string()
    } else {
        draft_ledger_text(ledger)
    };
    if committed.is_empty() {
        return final_text.to_string();
    }
    if final_text.is_empty() {
        return committed;
    }
    if final_text.starts_with(&committed) {
        return final_text.to_string();
    }
    if committed.starts_with(final_text) {
        return committed;
    }
    format!("{committed}{final_text}")
}

/// STT 事件消费 task：循环接收 `SttEvent`，按 generation + epoch 双层过滤旧事件，
/// 将有效事件 emit 到前端或调用 `deliver_final` 交付最终文本。
///
/// 0.22.9 Handoff 05：此 task 在 `begin_recording` 时 spawn，
/// 在 `stop_recording`（等待完成或超时）或 `cancel_recording`（abort）时终止。
///
/// **事件处理**：
/// - `Partial` → emit `VOICE_PARTIAL`（confirmed + preview 都空时跳过）；
///   Editor 路径（0.23.3）改推 `EDITOR_VOICE_SEGMENT`（confirmed 增量段）
///   与 `EDITOR_VOICE_STATUS`（preview 投影，边沿触发去重）
/// - `Final` → Editor 路径补收尾段 + ended 状态；其余调 `deliver_final` 交付
/// - `Busy` → 打 debug 日志
/// - `Error` → emit `VOICE_ERROR`；Editor 路径额外做终态清理（保留 confirmed）
///
/// **双层过滤**：
/// - adapter generation：每次 begin_session 从 1 开始
/// - recording epoch（0.22.15）：VoiceService 级单调递增
///
/// 两者是不同边界的校验，缺一不可。
///
/// **可观测性（0.24）**：每 5 秒输出一次链路统计（收到多少块音频、实际发出多少
/// 状态、被去重/合并多少、队列深度、PCM 大小、在途推理任务数），退出时输出终态汇总。
#[allow(clippy::too_many_arguments)]
async fn consume_stt_events(
    mut rx: tokio::sync::mpsc::Receiver<SttEvent>,
    expected_gen: u64,
    recording_epoch: u64,
    current_epoch: Arc<AtomicU64>,
    target: VoiceTarget,
    prev_fg_hwnd: Option<isize>,
    app: tauri::AppHandle,
    editor_state: Option<Arc<Mutex<EditorDictationState>>>,
    paused: Arc<AtomicBool>,
    voice: Option<Arc<VoiceService>>,
    port: Arc<dyn StreamingSttPort>,
    counters: Arc<StreamCounters>,
    final_delivery: Arc<AtomicBool>,
) {
    let target_str = target.as_str();
    let is_editor = target == VoiceTarget::Editor;
    let profile = port.recognition_profile();
    tracing::debug!(target = ?target, profile = ?profile, "STT 事件投影 profile");

    /// 统计输出间隔。
    const STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

    let mut stats_tick = tokio::time::interval(STATS_INTERVAL);
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 消耗立即触发的首拍：首次统计在 5 秒后
    stats_tick.tick().await;

    // 非 Editor 路径的 confirmed 累计缓存——引擎在 confirmed 未变化时
    // 只发空串（不再每块重复搬运全量正文），前端契约仍是累计文本。
    let mut confirmed_cache = String::new();
    // 0.23.9 typed path：Draft 以非重叠 audio span 形成 G2/G1/G3 的兼容
    // confirmed 视图；Editor 直接交给 EditorDictationState。Preview 只保留
    // 一个可替换值及其时间范围。
    let mut draft_ledger: Vec<DraftSpan> = Vec::new();
    let mut preview_cache = String::new();
    let mut preview_range: Option<AudioRange> = None;
    let mut latest_preview_request = 0u64;

    loop {
        let event = tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(event) => event,
                None => break,
            },
            _ = stats_tick.tick() => {
                let stats = port.stream_stats();
                log_stream_stats(target, &stats, &counters, editor_state.as_ref(), false);
                continue;
            }
        };

        // 0.22.15：epoch 墙——实时对比当前 VoiceService epoch，
        // 旧 epoch 的事件（Partial/Final/Error）全部丢弃。
        // 这与 generation 校验是两层不同边界：generation 过滤同一 adapter 内的旧 session，
        // epoch 过滤跨录音的迟到事件（新录音已开始，旧 finish/final 才到达）。
        let now_epoch = current_epoch.load(Ordering::Acquire);
        if now_epoch != recording_epoch {
            tracing::debug!(
                event_epoch = recording_epoch,
                current_epoch = now_epoch,
                "丢弃旧 epoch 事件（新录音已开始）"
            );
            continue;
        }

        tracing::trace!(lane = ?event.lane(), "收到 STT 事件");
        match event {
            SttEvent::Draft { generation, span } => {
                if generation != expected_gen {
                    tracing::debug!(
                        gen = generation,
                        expected = expected_gen,
                        "丢弃旧 generation 的 Draft 事件"
                    );
                    continue;
                }

                // Draft 覆盖的 Preview 整体失效；部分重叠也不裁剪字符串。
                if preview_range.is_some_and(|range| range.overlaps(span.audio_range)) {
                    preview_cache.clear();
                    preview_range = None;
                }

                if is_editor {
                    let Some(state) = editor_state.as_ref() else {
                        continue;
                    };
                    let next = {
                        let mut st = state.lock().unwrap();
                        st.push_draft_span(span)
                    };
                    if let Some((seq, span)) = next {
                        emit_editor_draft_segment(&app, state, seq, &span);
                    }
                    let phase = if paused.load(Ordering::Relaxed) {
                        "paused"
                    } else {
                        "recording"
                    };
                    let last_seq = state.lock().unwrap().last_seq();
                    emit_editor_status(
                        &app,
                        state,
                        phase,
                        last_seq,
                        (!preview_cache.is_empty()).then_some(preview_cache.as_str()),
                        None,
                    );
                    continue;
                }

                if append_draft_ledger(&mut draft_ledger, span) {
                    confirmed_cache = draft_ledger_text(&draft_ledger);
                    let _ = app.emit(
                        EventNames::VOICE_PARTIAL,
                        serde_json::json!({
                            "confirmed": confirmed_cache.as_str(),
                            "preview": preview_cache.as_str(),
                            "target": target_str,
                            "epoch": recording_epoch,
                        }),
                    );
                }
            }
            SttEvent::Preview {
                generation,
                request_id,
                audio_range,
                revision: _,
                text,
            } => {
                if generation != expected_gen || request_id < latest_preview_request {
                    tracing::debug!(
                        gen = generation,
                        expected = expected_gen,
                        request_id,
                        latest_preview_request,
                        "丢弃过期 Preview 事件"
                    );
                    continue;
                }
                latest_preview_request = request_id;
                preview_cache = text;
                preview_range = Some(audio_range);
                if is_editor {
                    let Some(state) = editor_state.as_ref() else {
                        continue;
                    };
                    let phase = if paused.load(Ordering::Relaxed) {
                        "paused"
                    } else {
                        "recording"
                    };
                    let last_seq = state.lock().unwrap().last_seq();
                    emit_editor_status(
                        &app,
                        state,
                        phase,
                        last_seq,
                        (!preview_cache.is_empty()).then_some(preview_cache.as_str()),
                        None,
                    );
                } else {
                    let confirmed = if draft_ledger.is_empty() {
                        confirmed_cache.as_str()
                    } else {
                        confirmed_cache = draft_ledger_text(&draft_ledger);
                        confirmed_cache.as_str()
                    };
                    if !confirmed.is_empty() || !preview_cache.is_empty() {
                        let _ = app.emit(
                            EventNames::VOICE_PARTIAL,
                            serde_json::json!({
                                "confirmed": confirmed,
                                "preview": preview_cache.as_str(),
                                "previewRequestId": request_id,
                                "target": target_str,
                                "epoch": recording_epoch,
                            }),
                        );
                    }
                }
            }
            SttEvent::Partial {
                generation,
                revision: _,
                confirmed,
                confirmed_changed,
                preview,
            } => {
                if generation != expected_gen {
                    tracing::debug!(
                        gen = generation,
                        expected = expected_gen,
                        "丢弃旧 generation 的 Partial 事件"
                    );
                    continue;
                }

                // ── Editor 连续听写：confirmed 增量成段 + preview 状态投影 ──
                if is_editor {
                    let Some(state) = editor_state.as_ref() else {
                        continue;
                    };
                    let next_seq = {
                        let mut st = state.lock().unwrap();
                        st.push_confirmed_delta(&confirmed)
                    };
                    if let Some((seq, text)) = next_seq {
                        emit_editor_segment(&app, state, seq, &text);
                    }
                    let phase = if paused.load(Ordering::Relaxed) {
                        "paused"
                    } else {
                        "recording"
                    };
                    let last_seq = state.lock().unwrap().last_seq();
                    // 边沿触发：状态未变化时该调用直接返回（不发事件、不写日志）
                    emit_editor_status(&app, state, phase, last_seq, Some(&preview), None);
                    continue;
                }

                // 0.22.15：confirmed 和 preview 都为空时不发 VOICE_PARTIAL
                if confirmed_changed || !confirmed.is_empty() {
                    confirmed_cache = confirmed;
                }
                preview_cache = preview.clone();
                if confirmed_cache.is_empty() && preview.is_empty() {
                    continue;
                }
                let _ = app.emit(
                    EventNames::VOICE_PARTIAL,
                    serde_json::json!({
                        "confirmed": confirmed_cache.as_str(),
                        "preview": preview,
                        "target": target_str,
                        "epoch": recording_epoch,
                    }),
                );
            }
            SttEvent::Final { generation, text } => {
                if generation != expected_gen {
                    tracing::debug!(
                        gen = generation,
                        expected = expected_gen,
                        "丢弃旧 generation 的 Final 事件"
                    );
                    continue;
                }
                tracing::debug!(
                    target = ?target,
                    text_len = text.chars().count(),
                    "收到 Final 事件"
                );

                // ── Editor：补收尾段（confirmed 之后的尾段定稿）+ ended 状态 ──
                if is_editor {
                    if let Some(state) = editor_state.as_ref() {
                        let next = {
                            let mut st = state.lock().unwrap();
                            st.push_final_delta(&text)
                        };
                        match next {
                            Some((seq, seg)) => emit_editor_segment(&app, state, seq, &seg),
                            None => tracing::debug!("editor Final 无新增尾段"),
                        }
                        let last_seq = state.lock().unwrap().last_seq();
                        emit_editor_status(&app, state, "ended", last_seq, None, None);
                    }
                    // Final 是 session 的最后一个事件，退出循环
                    break;
                }

                let final_text = compose_final_text(&draft_ledger, &confirmed_cache, &text);

                // 0.22.15：统一调用 deliver_final。Preview 不参与最终兜底；
                // typed path 只允许已交付 Draft ledger + terminal text。
                deliver_final(
                    &app,
                    target,
                    &final_text,
                    prev_fg_hwnd,
                    Some(final_delivery.as_ref()),
                );

                // Final 是 session 的最后一个事件，退出循环
                break;
            }
            SttEvent::Error {
                generation,
                message,
            } => {
                if generation != expected_gen {
                    continue;
                }
                tracing::error!(%message, "STT 引擎错误事件");

                // ── Editor：终态清理（保留 confirmed、丢弃 preview、回收资源）──
                if is_editor {
                    if let Some(v) = voice.as_ref() {
                        v.handle_editor_terminal_error(&message);
                    }
                    // Error 是终止事件，退出循环
                    break;
                }

                let _ = app.emit(
                    EventNames::VOICE_ERROR,
                    serde_json::json!({
                        "message": message,
                        "target": target_str,
                    }),
                );
                let _ = app.emit(EventNames::VOICE_RECORDING_END, ());
                // Error 是终止事件，退出循环
                break;
            }
        }
    }

    // 会话终态汇总（info；只含计数与长度）
    let stats = port.stream_stats();
    log_stream_stats(target, &stats, &counters, editor_state.as_ref(), true);
}

/// STT finalize + 10s 超时保护（G1/G2/G3 三路共用）。
///
/// engine 为 None 时返回空字符串；finalize 成功返回识别文本；
/// 失败或超时返回空字符串并打 warn 日志。
async fn finalize_engine(engine: Option<Arc<dyn SttEngine>>) -> String {
    match engine {
        Some(e) => {
            match tokio::time::timeout(std::time::Duration::from_secs(10), e.finalize()).await {
                Ok(Ok(text)) => text,
                Ok(Err(e)) => {
                    tracing::warn!(%e, "STT finalize 失败");
                    String::new()
                }
                Err(_) => {
                    tracing::warn!("STT finalize 超时（10s），放弃等待");
                    String::new()
                }
            }
        }
        None => String::new(),
    }
}

/// 计算 PCM 样本的音量级别（0.0 ~ 1.0），用于前端波形条可视化。
///
/// 使用 RMS（均方根）+ 噪声门限 + 平方根曲线：
/// - RMS < 0.001（噪声门限）→ 0.0（静默）
/// - RMS 0.001~0.15 映射到 0.0~1.0，用 sqrt 曲线增强小信号区域
/// - 平方根曲线让安静说话也能有 20-30% 的音量指示，不会"看起来没反应"
fn compute_rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    let rms = (sum_sq / samples.len() as f64).sqrt();

    // 噪声门限：低于此值视为静默
    const NOISE_FLOOR: f64 = 0.001;
    if rms < NOISE_FLOOR {
        return 0.0;
    }

    // 线性映射到 0~1（参考电平 0.15 = 正常说话音量）
    let normalized = ((rms - NOISE_FLOOR) / (0.15 - NOISE_FLOOR)).min(1.0);

    // 平方根曲线：增强小信号区域，让安静说话也有明显指示
    normalized.sqrt()
}

#[cfg(test)]
mod tests {
    use super::{EditorDictationState, EditorStatusEdge, SNAPSHOT_MAX_SEGMENTS, compute_rms};

    #[test]
    fn rms_empty_returns_zero() {
        assert_eq!(compute_rms(&[]), 0.0);
    }

    #[test]
    fn rms_silence_returns_zero() {
        // 全零样本 → RMS = 0 < NOISE_FLOOR → 0.0
        assert_eq!(compute_rms(&[0.0; 1000]), 0.0);
    }

    #[test]
    fn rms_noise_floor_returns_zero() {
        // 极小值（低于 NOISE_FLOOR = 0.001）→ 静默
        let samples = vec![0.0001f32; 100];
        assert_eq!(compute_rms(&samples), 0.0);
    }

    #[test]
    fn rms_normal_speech_nonzero() {
        // 模拟正常说话音量（幅度 ~0.3）
        let samples: Vec<f32> = (0..1600)
            .map(|i| {
                let t = i as f32 / 16000.0;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.3
            })
            .collect();
        let level = compute_rms(&samples);
        // RMS ≈ 0.3 / √2 ≈ 0.212，远超 0.15 上限 → normalized = 1.0 → level = 1.0
        assert!(level > 0.0, "正常音量不应返回 0");
        assert!(level <= 1.0, "音量不应超过 1.0");
    }

    #[test]
    fn rms_clamped_to_one() {
        // 最大幅度 → 不超过 1.0
        let samples = vec![1.0f32; 100];
        let level = compute_rms(&samples);
        assert!(level <= 1.0, "音量上限为 1.0，got {level}");
    }

    #[test]
    fn rms_sqrt_curve_enhances_small_signals() {
        // 小信号（RMS 略高于 NOISE_FLOOR）应因 sqrt 曲线得到增强
        // RMS ≈ 0.01 → normalized = (0.01 - 0.001) / (0.15 - 0.001) ≈ 0.0604
        // sqrt(0.0604) ≈ 0.246
        let samples = vec![0.01f32; 100];
        let level = compute_rms(&samples);
        assert!(level > 0.0, "小信号应非零");
        // 线性值 ≈ 0.06，sqrt 后 ≈ 0.25，应明显大于线性值
        assert!(level > 0.06, "sqrt 曲线应增强小信号: got {level}");
    }

    #[test]
    fn editor_snapshot_is_bounded_and_reports_eviction() {
        let mut state = EditorDictationState::new(7, "ed_test".into(), 3);
        for seq in 1..=(SNAPSHOT_MAX_SEGMENTS as u64 + 4) {
            state.remember(seq, format!("segment-{seq}"));
        }

        assert_eq!(state.segments.len(), SNAPSHOT_MAX_SEGMENTS);
        assert_eq!(state.truncated, 4);
        assert_eq!(state.segments.front().map(|(seq, _)| *seq), Some(5));
        let snapshot = state.after(7, 0).unwrap();
        assert_eq!(snapshot.len(), SNAPSHOT_MAX_SEGMENTS);
        assert!(state.after(8, 0).is_none(), "旧 epoch 不得读取当前快照");
    }

    // ── 0.24: 状态边沿触发 ────────────────────────────────────────────────

    /// 验收：confirmed 句段可靠、有序且只追加一次（重复快照不重复成段）。
    #[test]
    fn editor_confirmed_segments_are_ordered_and_single_shot() {
        let mut state = EditorDictationState::new(1, "ed_test".into(), 1);

        assert_eq!(
            state.push_confirmed_delta("第一句。"),
            Some((1, "第一句。".to_string()))
        );
        assert_eq!(
            state.push_confirmed_delta("第一句。"),
            None,
            "重复的同一累计快照不得重复成段"
        );
        assert_eq!(
            state.push_confirmed_delta("第一句。第二句。"),
            Some((2, "第二句。".to_string())),
            "新增部分只追加一次"
        );
        // Final 收尾段继续单调追加，重复 Final 不产段
        assert_eq!(
            state.push_final_delta("第一句。第二句。第三句。"),
            Some((3, "第三句。".to_string()))
        );
        assert_eq!(state.push_final_delta("第一句。第二句。第三句。"), None);
        assert_eq!(state.last_seq(), 3, "seq 必须严格单调且不跳号");

        // 快照按序可补齐（前端恢复路径）
        let segments = state.after(1, 0).expect("epoch 匹配应可读快照");
        assert_eq!(
            segments.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(state.after(2, 0), None, "旧 epoch 不得读取当前快照");
    }

    #[test]
    fn editor_status_edge_emits_only_on_change() {
        let mut edge = EditorStatusEdge::default();

        assert!(edge.should_emit("recording", 0, Some("预览"), None, None));
        assert!(
            !edge.should_emit("recording", 0, Some("预览"), None, None),
            "完全相同的状态不得重复发送"
        );
        assert!(
            edge.should_emit("recording", 0, Some("预览2"), None, None),
            "preview 变化必须发送"
        );
        assert!(
            edge.should_emit("recording", 1, Some("预览2"), None, None),
            "seq 变化必须发送"
        );
        assert!(
            edge.should_emit("paused", 1, Some("预览2"), None, None),
            "phase 变化必须发送"
        );
        assert!(edge.should_emit("error", 1, Some("预览2"), Some("boom"), Some("stt_failed")));
        assert!(
            !edge.should_emit("error", 1, Some("预览2"), Some("boom"), Some("stt_failed")),
            "重复的 error 状态不得重复发送"
        );
        assert!(
            edge.should_emit("error", 1, Some("预览2"), Some("boom2"), Some("stt_failed")),
            "message 变化必须发送"
        );
        assert!(
            edge.should_emit("ended", 1, None, None, None),
            "ended 与上一状态不同必须发送"
        );
    }

    #[test]
    fn repeated_identical_partial_yields_single_status_change() {
        // 验收：相同 Partial 连续出现时只产生一次对外状态变化。
        let mut edge = EditorStatusEdge::default();
        let mut emitted = 0;
        for _ in 0..100 {
            if edge.should_emit("recording", 7, Some("同一预览"), None, None) {
                emitted += 1;
            }
        }
        assert_eq!(emitted, 1, "100 次相同 Partial 只允许一次对外状态变化");
    }

    #[test]
    fn status_edge_handles_preview_cleared_and_empty() {
        let mut edge = EditorStatusEdge::default();
        assert!(edge.should_emit("recording", 0, Some("有预览"), None, None));
        // 句尾清空预览 → None 与 Some("") 都是不同状态，必须各自发送一次
        assert!(edge.should_emit("recording", 0, Some(""), None, None));
        assert!(!edge.should_emit("recording", 0, Some(""), None, None));
        assert!(edge.should_emit("recording", 0, None, None, None));
        assert!(!edge.should_emit("recording", 0, None, None, None));
    }
}
