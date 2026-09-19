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
//!     G2: 渐进上屏（0.23.13）——Draft 定稿按保留窗口分批 inject_text，
//!         终态只补交剩余
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
use crate::domain::stt::dictation::DictationLedger;
use crate::domain::stt::{
    DraftSpan, PreviewSegment, RecognitionProfile, StreamingSttPort, SttEngine, SttEvent,
    settle_preview_segments,
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
/// 现在仅当 phase / seq / confirmed / preview / message / code 任一变化时才对外发送。
#[derive(Default)]
struct EditorStatusEdge {
    last: Option<EditorStatusKey>,
}

#[derive(PartialEq, Eq)]
struct EditorStatusKey {
    phase: String,
    seq: u64,
    confirmed: Option<String>,
    preview: Option<String>,
    message: Option<String>,
    code: Option<String>,
}

impl EditorStatusEdge {
    /// 判断本次状态是否需要对外发送；返回 `true` 时同步记录新状态。
    #[allow(clippy::too_many_arguments)]
    fn should_emit(
        &mut self,
        phase: &str,
        seq: u64,
        confirmed: Option<&str>,
        preview: Option<&str>,
        message: Option<&str>,
        code: Option<&str>,
    ) -> bool {
        let key = EditorStatusKey {
            phase: phase.to_string(),
            seq,
            confirmed: confirmed.map(str::to_string),
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
    /// 统一听写账本（0.23.13）：Draft 去重 + 保留窗口 + Final 裁剪。
    ///
    /// Editor 保留窗口 = 1：最新定稿句在浮窗短暂停留，下一句定稿时
    /// 前一句写入正文（与 G2 渐进上屏同一调度器，仅窗口大小不同）。
    ledger: DictationLedger,
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

/// 0.23.17 渐进上屏保留窗口（段数）的 app 层最后一道收敛。
///
/// 配置层 `RecognitionConfig::sanitize` 已在持久化与引擎构造时收敛过；
/// 这里再收敛一次，防止外部构造的 `SttConfig`（测试替身 / 迁移路径）
/// 绕过配置命令把越界值送进 `DictationLedger`。
///
/// 语义（0.23.13 起，不是实现细节）：
/// - G2 默认 0 —— 每段定稿立即注入前台应用，浮窗只显示实时预览；
/// - Editor 默认 1 —— 最新一段定稿停留在浮窗，下一段定稿时才写入正文。
fn retention_segments(configured: u32) -> usize {
    configured.clamp(
        crate::app::stt_config::RECOGNITION_RETENTION_MIN_SEGMENTS,
        crate::app::stt_config::RECOGNITION_RETENTION_MAX_SEGMENTS,
    ) as usize
}

/// 读取 G2 渐进上屏保留窗口（段数）。
fn g2_retention_from_config() -> usize {
    retention_segments(
        crate::app::stt_config::get_stt_config()
            .local_engine
            .recognition
            .g2_retention_segments,
    )
}

/// 读取编辑器连续听写保留窗口（段数）。
fn editor_retention_from_config() -> usize {
    retention_segments(
        crate::app::stt_config::get_stt_config()
            .local_engine
            .recognition
            .editor_retention_segments,
    )
}

/// 松键后等待事件消费 task 收口的预算（0.22.9 起为 12s）。
///
/// 0.23.14.7 P1-2：超时不再等价于"task 已结束"——超时分支显式 abort 并从
/// 引擎侧恢复终态，竞态修复不依赖延长该预算。
const EVENT_TASK_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(12);

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
    /// `retention` 为听写保留窗口（段数），由调用方从配置读取——
    /// 不在此处隐式读全局配置，便于测试直接构造确定性的窗口。
    fn new(epoch: u64, session_ref: String, generation: u64, retention: usize) -> Self {
        Self {
            epoch,
            session_ref,
            generation,
            ledger: DictationLedger::new(retention),
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
        let (seq, text) = self.ledger.extract_delta(confirmed)?;
        self.remember(seq, text.clone());
        Some((seq, text))
    }

    /// 接收类型化 Draft；按 `span_id`/音频范围去重。0.23.13 起不再直接
    /// 产段——段由 [`EditorDictationState::drain_flushable_spans`] 按保留
    /// 窗口调度产出。返回是否新接受。
    fn push_draft_span(&mut self, span: DraftSpan) -> bool {
        self.ledger.accept_draft_span(span)
    }

    /// 产出保留窗口外可交付段（最新段停留在浮窗，前一段写正文）。
    ///
    /// 0.23.14：Editor 的段经事件同步写正文（事件到达即交付），投递后
    /// 立即 ack——confirmed 窗口行为与 0.23.13 一致。
    fn drain_flushable_spans(&mut self) -> Vec<(u64, DraftSpan)> {
        let flushed = self.ledger.queue_flushable();
        self.ledger.ack_delivered(None);
        for (seq, span) in &flushed {
            self.remember_draft(*seq, span.clone());
        }
        flushed
    }

    /// 冲刷全部未交付段（终态：Final/error/cancel 补交）。
    fn flush_pending_spans(&mut self) -> Vec<(u64, DraftSpan)> {
        let pending = self.ledger.take_pending();
        self.ledger.ack_delivered(None);
        for (seq, span) in &pending {
            self.remember_draft(*seq, span.clone());
        }
        pending
    }

    /// 未交付段文本（浮窗 confirmed 窗口展示）。
    fn pending_text(&self) -> String {
        self.ledger.pending_text()
    }

    /// Final 终态拆解：先冲刷全部未交付段，再从 Final 全文推导尾段。
    /// 两部分分别发射（段带 span 身份，尾段为纯文本段）。
    fn finalize_parts(
        &mut self,
        final_text: &str,
    ) -> (Vec<(u64, DraftSpan)>, Option<(u64, String)>) {
        let pending = self.flush_pending_spans();
        let tail = self.ledger.extract_final_tail(final_text).map(|(seq, text)| {
            self.remember(seq, text.clone());
            (seq, text)
        });
        (pending, tail)
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
        self.ledger.last_seq()
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
    /// G2 终态交付状态机（0.23.14.7 P1-2）：仅 ForegroundApp（有账本）会话
    /// 持有；G2 路径的终态归属与 barrier 排入全部经此裁决。
    g2_terminal: Option<Arc<G2TerminalGate>>,
    /// 0.23.13 G2 渐进上屏：PreviewDraft 听写账本（事件 task 接受/冲刷，
    /// cancel/error 终态补冲刷共享访问）。
    dictation_ledger: Option<Arc<Mutex<DictationLedger>>>,
    /// 0.23.13 G2 注入 worker 发送端：保序冲刷（worker 在所有 sender
    /// drop 后自动退出）。
    g2_flush_tx: Option<tokio::sync::mpsc::UnboundedSender<G2FlushJob>>,
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
            g2_terminal: None,
            dictation_ledger: None,
            g2_flush_tx: None,
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
            .downcast_ref::<crate::domain::stt::pseudo_streaming::PseudoStreamingSttEngine>(
        )?;
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
        // 定位：此刻前台仍是注入目标，先记下它的窗口句柄——浮窗先按鼠标即时
        // 显示，随后后台精化到该窗口的文本光标处（0.23.x 跟随光标）。
        if target == VoiceTarget::ForegroundApp {
            let owner_hwnd = platform::window::get_foreground_hwnd();
            platform::window::show_voice_overlay(&self.app, owner_hwnd);
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
            editor_retention_from_config(),
        ))));

        // 浮窗就近反馈（用户刚点了编辑器麦克风按钮，光标即按钮附近）；
        // 编辑器听写保持鼠标定位，不做 caret 精化（caret 在 blink 自己的 webview 内）
        platform::window::show_voice_overlay(&self.app, None);

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
            let (seq, confirmed) = {
                let st = state.lock().unwrap();
                (st.last_seq(), st.pending_text())
            };
            self.emit_editor_status(
                &state,
                "paused",
                seq,
                (!confirmed.is_empty()).then_some(confirmed.as_str()),
                None,
                None,
            );
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
            let (seq, confirmed) = {
                let st = state.lock().unwrap();
                (st.last_seq(), st.pending_text())
            };
            self.emit_editor_status(
                &state,
                "recording",
                seq,
                (!confirmed.is_empty()).then_some(confirmed.as_str()),
                None,
                None,
            );
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
            let (seq, confirmed) = {
                let st = state.lock().unwrap();
                (st.last_seq(), st.pending_text())
            };
            self.emit_editor_status(
                &state,
                "finalizing",
                seq,
                (!confirmed.is_empty()).then_some(confirmed.as_str()),
                None,
                None,
            );
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
    /// 0.23.13：与事件路径同一拆解——冲刷全部 pending 段 + 尾段收尾。
    async fn deliver_editor_final(&self, final_text: &str) {
        let Some(state) = self.editor_state.lock().unwrap().clone() else {
            return;
        };
        let (pending, tail) = {
            let mut st = state.lock().unwrap();
            st.finalize_parts(final_text)
        };
        for (seq, span) in &pending {
            emit_editor_draft_segment(&self.app, &state, *seq, span);
        }
        if let Some((seq, text)) = tail {
            self.emit_editor_segment(&state, seq, &text);
        }
        let last_seq = state.lock().unwrap().last_seq();
        self.emit_editor_status(&state, "ended", last_seq, None, None, None);
    }

    /// STT 引擎 Error 事件的终态清理（仅 Editor 路径）：保留 confirmed、
    /// 丢弃 preview，结束会话并回收录音资源。由事件 task 调用（此时该 task
    /// 即将退出，event_task 句柄置 None 即可）。
    /// 0.23.13：保留窗口内已定稿段补交写正文（错误不丢已定稿内容）。
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
            let flushed = {
                let mut st = state.lock().unwrap();
                st.flush_pending_spans()
            };
            for (seq, span) in &flushed {
                emit_editor_draft_segment(&self.app, &state, *seq, span);
            }
            let seq = state.lock().unwrap().last_seq();
            self.emit_editor_status(&state, "error", seq, None, None, Some(message));
            self.emit_editor_status(&state, "ended", seq, None, None, None);
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
        confirmed: Option<&str>,
        preview: Option<&str>,
        message: Option<&str>,
    ) {
        emit_editor_status(&self.app, state, phase, seq, confirmed, preview, message);
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

                    // 0.23.13 G2 渐进上屏：PreviewDraft 账本（保留窗口默认 0，
                    // 定稿即投递注入 worker；0.23.17 起可配置）+ 单消费者
                    // 保序注入 worker + 交付 ack task（0.23.14：ack 前浮窗
                    // 保持待交付段）。其他 target 清空旧会话残留。
                    // 保留窗口在会话开始时冻结——中途改配置不改变在途会话
                    // 的交付节奏（避免"已经上屏的段又回退到保留窗口"）。
                    session.dictation_ledger = None;
                    session.g2_flush_tx = None;
                    let mut pending_epoch: Option<u64> = None;
                    if session.target == VoiceTarget::ForegroundApp {
                        let ledger =
                            Arc::new(Mutex::new(DictationLedger::new(g2_retention_from_config())));
                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                        let (ack_tx, ack_rx) = tokio::sync::mpsc::unbounded_channel();
                        let deferred = Arc::new(AtomicBool::new(false));
                        // 0.23.14.7 P1-2：终态交付状态机与账本同生命周期。
                        let g2_terminal = Arc::new(G2TerminalGate::new());
                        tokio::spawn(run_g2_flush_worker(rx, ack_tx, G2Runtime::production()));
                        session.dictation_ledger = Some(ledger.clone());
                        session.g2_flush_tx = Some(tx);
                        session.g2_terminal = Some(g2_terminal.clone());
                        // ack task 需要本会话 epoch（竞态墙）；epoch 在下方
                        // 统一递增，这里先取 +1 后复用同一值。
                        let epoch = self.recording_epoch.fetch_add(1, Ordering::Release) + 1;
                        pending_epoch = Some(epoch);
                        tokio::spawn(run_g2_ack_task(
                            self.app.clone(),
                            ack_rx,
                            ledger,
                            deferred,
                            g2_terminal,
                            epoch,
                        ));
                    }

                    // 通知前端录音已开始
                    let epoch_val = pending_epoch.unwrap_or_else(|| {
                        self.recording_epoch.fetch_add(1, Ordering::Release) + 1
                    });
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

        // 0.23.10.2：会话开始即预热 worker——首次推理的懒加载开销（冷启
        // ~650ms）被移到用户尚未说话的窗口，首个预览/短语结果提前落地。
        if let Err(e) = port.warm_up().await {
            tracing::debug!(%e, "worker 预热触发失败（忽略）");
        }

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
        // 0.23.13 G2 渐进上屏：事件 task 携带账本与注入 worker 发送端
        // （0.23.14.6 起 worker deferred 标志仅诊断，不再传入事件 task；
        // 0.23.14.7 P1-2 事件 task 另携终态状态机）。
        let (g2_ledger_for_events, g2_flush_tx_for_events, g2_terminal_for_events) = {
            let session = self.session.lock().unwrap();
            (
                session.dictation_ledger.clone(),
                session.g2_flush_tx.clone(),
                session.g2_terminal.clone(),
            )
        };
        let event_task = tokio::spawn(async move {
            consume_stt_events(
                event_rx,
                session_gen,
                epoch_for_events,
                epoch_arc,
                target_for_events,
                prev_hwnd_for_events,
                g2_ledger_for_events,
                g2_flush_tx_for_events,
                g2_terminal_for_events,
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
                self.ensure_g2_terminal_barrier(target);
            } else {
                // finish_session 已产出 Final/Error 事件，事件消费 task 会处理。
                // 0.23.14.7 P1-2：等待必须区分"task 已退出"与"超时仍在运行"——
                // 退出后兜底 barrier 才拥有终态归属（不再有消费者能产出新
                // 尾段）；超时则 task 仍可能即将处理 Final，此时抢排空
                // barrier 会吞掉迟到的终态尾段，必须 abort 后从引擎侧恢复。
                let event_task = self.session.lock().unwrap().event_task.take();
                match wait_event_task_bounded(event_task, EVENT_TASK_DRAIN_BUDGET).await {
                    EventTaskWait::Exited => {
                        self.ensure_g2_terminal_barrier(target);
                    }
                    EventTaskWait::TimedOut(handle) => {
                        tracing::warn!(
                            budget_ms = EVENT_TASK_DRAIN_BUDGET.as_millis() as u64,
                            "事件消费 task 等待超时：abort 后走引擎侧终态恢复路径"
                        );
                        handle.abort();
                        // 等待 abort 生效：此后不再有任何消费者能处理 Final。
                        let _ = handle.await;
                        let recovered_text = finalize_engine(engine).await;
                        // 0.23.14.7 P1：恢复文本对 G1/G2/G3 都是应交付的最终
                        // 全文（`finalize_engine` 与 Final 事件同源），必须按
                        // target 分流——只走 G2 专用恢复会把 MainWindow /
                        // ChatWindow 的全文静默丢弃。
                        match timeout_recovery_path(target) {
                            TimeoutRecoveryPath::G2LedgerTerminal => {
                                self.recover_g2_terminal_after_event_timeout(
                                    target,
                                    &recovered_text,
                                );
                            }
                            TimeoutRecoveryPath::LegacyDeliverFinal => {
                                self.deliver_final_text(target, recovered_text).await;
                            }
                        }
                    }
                }
            }
            // 0.23.13：释放 G2 注入 worker 的 session 发送端——终态 barrier
            // 已排入，worker 处理完队列后自动收尾。
            self.session.lock().unwrap().g2_flush_tx.take();
            return;
        }

        // 回退：旧 finalize 路径（stt_port 不存在时）
        let final_text = finalize_engine(engine).await;
        self.deliver_final_text(target, final_text).await;
        self.ensure_g2_terminal_barrier(target);
        self.session.lock().unwrap().g2_flush_tx.take();
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
        let (stt_port, generation, target, was_editor, prev_fg_hwnd, ledger, flush_tx, gate) = {
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
            let prev_fg_hwnd = session.prev_fg_hwnd;
            let ledger = session.dictation_ledger.clone();
            // 取出发送端：本方法补交完 pending 后 worker 随最后一个 sender
            // drop 自动退出。
            let flush_tx = session.g2_flush_tx.take();
            let gate = session.g2_terminal.clone();

            // 通知输入状态机回 Idle
            crate::infra::platform::hotkey::InputController::update_voice_phase(
                crate::infra::platform::hotkey::VoicePhase::Idle,
            );

            (
                stt_port,
                generation,
                target,
                was_editor,
                prev_fg_hwnd,
                ledger,
                flush_tx,
                gate,
            )
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

        // 0.23.13 G2 渐进上屏：取消 = 停止后续识别，已定稿草稿全部补注入
        // （在途未识别音频仍丢弃）。录音已结束，可走完整注入路径（允许
        // 剪贴板降级），与此前已入队的渐进冲刷经 worker 保序衔接。
        // 0.23.14.6：终态 barrier **无条件排入**——pending 为空、worker
        // deferred 标志为 false 也必须排（标志异步维护，不可作为跳过依据），
        // 否则前台漂移期间挂起的渐进文本随 worker 退出丢失。
        // 0.23.14.7 P1-2：事件消费 task 已在本方法入口 abort（deliver_g2_
        // remaining 无 await 点，已开始的同步交付会先完成），终态归属经
        // 状态机与 Final 路径互斥。
        if target == VoiceTarget::ForegroundApp
            && let (Some(ledger), Some(gate)) = (ledger.as_ref(), gate.as_ref())
            && gate.claim(G2TerminalPhase::BarrierQueued)
        {
            let pending = ledger.lock().unwrap().take_pending();
            let text: String = pending.iter().map(|(_, s)| s.text.as_str()).collect();
            tracing::debug!(
                spans = pending.len(),
                chars = text.chars().count(),
                "G2 取消：无条件终态 barrier（含 worker deferred 冲刷）"
            );
            if let Err(e) = send_g2_flush(&flush_tx, text, prev_fg_hwnd, false, None) {
                // 未进入队列：回滚水位、重开闸门，错误可见。
                gate.reopen();
                ledger.lock().unwrap().unqueue_spans(&pending);
                emit_g2_delivery_error(&self.app, &e);
            }
        }

        // Editor 连续听写取消（0.23.3）：confirmed 已交付的段保留（不回撤正文），
        // 在途 preview 丢弃；0.23.13 起保留窗口内已定稿段同样补交写正文。
        if was_editor {
            if let Some(state) = self.editor_state.lock().unwrap().clone() {
                let flushed = {
                    let mut st = state.lock().unwrap();
                    st.flush_pending_spans()
                };
                for (seq, span) in &flushed {
                    emit_editor_draft_segment(&self.app, &state, *seq, span);
                }
                let seq = state.lock().unwrap().last_seq();
                self.emit_editor_status(&state, "ended", seq, None, None, None);
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
        let (prev_hwnd, final_delivery, ledger, flush_tx, gate) = {
            let session = self.session.lock().unwrap();
            (
                session.prev_fg_hwnd,
                session.final_delivery.clone(),
                session.dictation_ledger.clone(),
                session.g2_flush_tx.clone(),
                session.g2_terminal.clone(),
            )
        };
        // 0.23.13 G2 渐进上屏：终态只补交剩余文本（经注入 worker 保序），
        // 与事件 Final 路径共用同一交付函数（0.23.14.7 P1-2：经终态状态机）。
        if let (VoiceTarget::ForegroundApp, Some(ledger), Some(gate)) = (target, ledger, gate) {
            if let Err(e) = deliver_g2_remaining(
                &ledger,
                &flush_tx,
                &gate,
                prev_hwnd,
                &final_text,
                G2TerminalPhase::FinalObserved,
            ) {
                emit_g2_delivery_error(&self.app, &e);
            }
            return;
        }
        deliver_final(
            &self.app,
            target,
            &final_text,
            prev_hwnd,
            Some(final_delivery.as_ref()),
        );
    }

    /// G2 终态兜底 barrier（0.23.14.6 P1-3；0.23.14.7 P1-2 收敛归属证据）：
    /// **仅在事件消费 task 已退出后**调用——Error 会话（事件 task 在 Error
    /// 处退出，只发过渐进 job）与 Final 已消费但闸门未关的通道关闭退出。
    /// 此时不再有任何消费者能产出新尾段，补排终态 barrier 是安全的；
    /// worker 内 deferred 文本经完整注入路径补交后才允许收尾。
    fn ensure_g2_terminal_barrier(&self, target: VoiceTarget) {
        if target != VoiceTarget::ForegroundApp {
            return;
        }
        let (ledger, flush_tx, gate, prev_hwnd) = {
            let session = self.session.lock().unwrap();
            (
                session.dictation_ledger.clone(),
                session.g2_flush_tx.clone(),
                session.g2_terminal.clone(),
                session.prev_fg_hwnd,
            )
        };
        let (Some(ledger), Some(gate)) = (ledger, gate) else {
            return;
        };
        // Final/cancel/恢复路径已接管终态（含投递失败后重开的场景——重开
        // 意味着上一次没进去，这里正是重试点）。
        if !gate.claim(G2TerminalPhase::BarrierQueued) {
            tracing::debug!(phase = ?gate.phase(), "G2 终态兜底：终态已由其他路径收口，跳过");
            return;
        }
        tracing::debug!("G2 终态兜底：事件路径未收口，补排终态 barrier");
        let pending = ledger.lock().unwrap().take_pending();
        let text: String = pending.iter().map(|(_, s)| s.text.as_str()).collect();
        if let Err(e) = send_g2_flush(&flush_tx, text, prev_hwnd, false, None) {
            // 未进入队列：回滚水位、重开闸门，错误可见。
            gate.reopen();
            ledger.lock().unwrap().unqueue_spans(&pending);
            emit_g2_delivery_error(&self.app, &e);
        }
    }

    /// G2 事件消费 task 等待超时后的终态恢复（0.23.14.7 P1-2）。
    ///
    /// 调用前置条件：事件消费 task 已 abort 并 await 退出——此后不可能再
    /// 有消费者处理迟到 Final。终态文本从**引擎侧**恢复（`finalize` 返回
    /// 累计 confirmed 全文，是 Final 事件的真源；终态已提交时二次 finalize
    /// 安全返回全文），迟到 Final 携带的尾段因此不丢。交付与既有路径共用
    /// `deliver_g2_remaining`（闸门 `BarrierQueued` 归属 + 先投递后落账 +
    /// 失败重开）——若退出前 Final 恰好已被消费（闸门非 Open），本次恢复
    /// 自动跳过，不会重复上屏。
    fn recover_g2_terminal_after_event_timeout(&self, target: VoiceTarget, recovered_text: &str) {
        if target != VoiceTarget::ForegroundApp {
            return;
        }
        let (ledger, flush_tx, gate, prev_hwnd) = {
            let session = self.session.lock().unwrap();
            (
                session.dictation_ledger.clone(),
                session.g2_flush_tx.clone(),
                session.g2_terminal.clone(),
                session.prev_fg_hwnd,
            )
        };
        let (Some(ledger), Some(gate)) = (ledger, gate) else {
            return;
        };
        if let Err(e) = deliver_g2_remaining(
            &ledger,
            &flush_tx,
            &gate,
            prev_hwnd,
            recovered_text,
            G2TerminalPhase::BarrierQueued,
        ) {
            emit_g2_delivery_error(&self.app, &e);
        }
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
/// **边沿触发（0.24）**：phase/seq/confirmed/preview/message/code 全部未变化
/// 时不发送、不写日志，仅累加诊断计数——修复了"每 10ms 一条状态事件 +
/// 一条 debug 日志"的风暴。
///
/// `confirmed` 为保留窗口内未交付段文本（0.23.13 浮窗双层展示的白色层；
/// 进正文的段落不再重复展示）。
#[allow(clippy::too_many_arguments)]
fn emit_editor_status(
    app: &tauri::AppHandle,
    state: &Arc<Mutex<EditorDictationState>>,
    phase: &str,
    seq: u64,
    confirmed: Option<&str>,
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
            .should_emit(phase, seq, confirmed, preview, message, code)
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
            "confirmed": confirmed,
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
        confirmed_chars = confirmed.map(str::chars).map(Iterator::count).unwrap_or(0),
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
            stale_before_worker = $stats.stale_before_worker,
            preview_retreat_chars = $stats.preview_retreat_chars,
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
    // 终态交付预检（0.23.14.7 P1 抽出为纯函数）：空文本与重复终态都跳过，
    // 差别只在空文本不消耗闸门、仍需收尾事件。G1/G3 的事件路径与超时恢复
    // 路径共用同一 guard，保证同会话恰好交付一次。
    if !claim_final_delivery(text, delivery_guard) {
        if text.is_empty() {
            tracing::debug!("识别结果为空,跳过交付");
            let _ = app.emit(EventNames::VOICE_RECORDING_END, ());
        } else {
            tracing::debug!(target = ?target, "忽略重复的 STT 最终交付");
        }
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

/// 终态交付预检（0.23.14.7 P1）：返回是否应执行交付副作用。
///
/// - 空文本：无可交付正文，返回 false 且**不消耗**闸门（与旧 `deliver_final`
///   行为一致——空文本不是终态交付，final_delivery 保持可被后续真实终态领取）。
/// - 非空文本经 `swap` 领取闸门：首次领取返回 true，重复领取返回 false。
///   finish/fallback 与事件 task 可能在边界上同时看到终态；同一 session
///   只允许一次最终交付，防止 G1/G3 重复提交。
fn claim_final_delivery(text: &str, guard: Option<&AtomicBool>) -> bool {
    if text.is_empty() {
        return false;
    }
    if let Some(guard) = guard
        && guard.swap(true, Ordering::AcqRel)
    {
        return false;
    }
    true
}

/// 事件 task 等待超时后的终态恢复路由（0.23.14.7 P1）。
///
/// 恢复文本来自引擎侧 `finalize()`，与 Final 事件同源，对三个 target 都是
/// 应交付的最终全文：
/// - G2（ForegroundApp）走账本终态恢复——经 [`G2TerminalGate`] 状态机按
///   账本补交剩余（`BarrierQueued` 归属），保证最多一次终态；
/// - G1/G3 无账本语义，走既有 `deliver_final_text` Legacy 全文交付
///   （`final_delivery` 闸门防重复）。
///
/// 旧缺陷：恢复文本只进 G2 专用恢复函数，该函数对非 ForegroundApp 直接
/// 返回——MainWindow / ChatWindow 的恢复全文被静默丢弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutRecoveryPath {
    G2LedgerTerminal,
    LegacyDeliverFinal,
}

fn timeout_recovery_path(target: VoiceTarget) -> TimeoutRecoveryPath {
    if target == VoiceTarget::ForegroundApp {
        TimeoutRecoveryPath::G2LedgerTerminal
    } else {
        TimeoutRecoveryPath::LegacyDeliverFinal
    }
}

/// G2 终态交付阶段（0.23.14.7 P1-2 状态机）。
///
/// 取代原 `final_delivery: AtomicBool` 在 G2 路径上的模糊语义——旧布尔值
/// 无法区分"Final 已被消费者完整交付"与"非 Final 路径排入了兜底 barrier"，
/// 事件消费等待超时后兜底 barrier 先抢占布尔值，迟到的 Final 交付会被
/// 误判为重复而丢弃尾段。显式阶段让每条路径只在自己的证据下收口：
///
/// - `Open` → `FinalObserved`：事件消费者处理 Final（`deliver_g2_remaining`）；
/// - `Open` → `BarrierQueued`：cancel / stop 兜底 / 事件超时恢复路径排入
///   终态 barrier——这些路径调用时**事件消费者已不可能再产出新尾段**
///   （task 已退出、已 abort，或恢复路径先从引擎侧取回终态文本）；
/// - `FinalObserved`/`BarrierQueued` → `Delivered`：worker ack 确认终态注入；
/// - `*` → `Failed`：终态注入失败（错误事件已携带原文，可见可恢复）；
/// - 投递失败（未进入 worker 队列）→ 回 `Open`，保留重试权（既有语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum G2TerminalPhase {
    /// 终态交付权开放。
    Open,
    /// 事件消费者已观察 Final 并完成剩余交付（含 barrier 排入）。
    FinalObserved,
    /// 非 Final 路径已排入终态 barrier（cancel / stop 兜底 / 超时恢复）。
    BarrierQueued,
    /// 终态注入已确认完成（worker ack）。
    Delivered,
    /// 终态注入失败，未上屏原文已随错误事件可见。
    Failed,
}

/// G2 终态交付闸门：`Open` 单写位状态机（0.23.14.7 P1-2）。
#[derive(Debug)]
struct G2TerminalGate(std::sync::Mutex<G2TerminalPhase>);

impl G2TerminalGate {
    fn new() -> Self {
        Self(std::sync::Mutex::new(G2TerminalPhase::Open))
    }

    /// 仅当处于 `Open` 时迁移到 `target`；返回是否拿到终态交付权。
    ///
    /// 失败即"终态已被其他路径接管"——调用方必须跳过交付（防重复上屏），
    /// 不允许覆盖既有阶段。
    fn claim(&self, target: G2TerminalPhase) -> bool {
        let mut phase = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if *phase != G2TerminalPhase::Open {
            return false;
        }
        *phase = target;
        true
    }

    /// 投递失败（文本未进入 worker 队列）：回到 `Open`，保留重试权。
    fn reopen(&self) {
        *self.0.lock().unwrap_or_else(|poison| poison.into_inner()) = G2TerminalPhase::Open;
    }

    /// 按回执精化阶段（`Delivered`/`Failed`）；不覆盖 `Open`——回执迟到
    /// 而闸门已被重开重试时，保留重试中的开放语义。
    fn observe_receipt(&self, phase: G2TerminalPhase) {
        debug_assert!(matches!(
            phase,
            G2TerminalPhase::Delivered | G2TerminalPhase::Failed
        ));
        let mut current = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if *current != G2TerminalPhase::Open {
            *current = phase;
        }
    }

    fn phase(&self) -> G2TerminalPhase {
        *self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

/// G2 渐进上屏注入 job（0.23.13；0.23.14 增加 seq 身份与交付 ack）。
struct G2FlushJob {
    text: String,
    /// 录音开始时的前台 HWND（终态 job 恢复焦点用）。
    hwnd: Option<isize>,
    /// 录音进行中（热键仍按住）：仅 Unicode 注入——剪贴板降级会注入真实
    /// Ctrl+V keydown，触发输入状态机 armed→aborted（该分支不区分
    /// injected 键），破坏 hold-to-talk 会话。
    unicode_only: bool,
    /// 本 job 覆盖的最大 ledger seq；成功注入后 ack。`None` = 终态 job
    /// （允许剪贴板降级，交付 deferred + 剩余），ack 全部已投递段。
    ///
    /// 0.23.14.6：终态 job 同时是**顺序 barrier**——Final/cancel/stop 无条件
    /// 排入（即使文本为空），排在此前所有渐进 job 之后，负责冲刷 worker
    /// 内 deferred 文本后统一 ack。`g2_deferred` 标志只用于诊断，不参与
    /// 是否排入的决策（标志由异步 ack task 维护，与 Final 的先后无保证）。
    ack_seq: Option<u64>,
}

/// G2 注入 worker 的交付回执（0.23.14）。
#[derive(Debug)]
enum G2FlushAck {
    /// 成功注入到 `upto_seq`（None = 终态 job，确认全部已投递段）。
    /// `elapsed_ms` 含排队等待，用于诊断注入确认延迟。
    Delivered {
        upto_seq: Option<u64>,
        elapsed_ms: u64,
    },
    /// 渐进 job 推迟（前台漂移 / Unicode 失败挂起）：文本保留在 worker
    /// 内等终态补交；账本不动（段保持浮窗可见），仅诊断。
    Deferred { chars: usize },
    /// 终态注入失败（0.23.14.6）：文本未能上屏且 worker 已无重试机会，
    /// 回传原文供可见错误事件保留——终态失败不得只记日志静默丢字。
    Failed { text: String },
}

/// G2 注入执行环境：生产实现走 Win32（焦点修复 + SendInput），单测注入
/// 可控替身（0.23.14.6 抽象，使 worker 的终态/deferred 语义可确定性测试）。
#[derive(Clone)]
struct G2Runtime {
    /// `(text, hwnd, unicode_only) -> 注入结果`。阻塞执行（调用方在
    /// spawn_blocking 内调用）。
    inject: Arc<dyn Fn(&str, Option<isize>, bool) -> Result<(), String> + Send + Sync>,
    /// 当前前台窗口探测（渐进 job 的漂移判定）。
    foreground_hwnd: Arc<dyn Fn() -> Option<isize> + Send + Sync>,
}

impl G2Runtime {
    /// 生产实现：渐进与终态统一先修焦点（WM_CANCELMODE 关 Alt+Space 系统
    /// 菜单 + 前台确认）；前台已是目标窗口时 SetForegroundWindow 近似
    /// no-op，不构成渐进抢焦点。
    fn production() -> Self {
        Self {
            inject: Arc::new(|text, hwnd, unicode_only| {
                if let Some(hwnd) = hwnd {
                    platform::window::restore_foreground_g2(hwnd);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                // 诊断：焦点修复后的前台与目标对比——SendInput 只投给"此刻
                // 前台"，两者不一致说明字符会进别的窗口（"成功但没上屏"
                // 的定位证据）。
                let fg_after = platform::window::get_foreground_hwnd();
                tracing::debug!(
                    target_hwnd = ?hwnd,
                    fg_after = ?fg_after,
                    unicode_only,
                    chars = text.chars().count(),
                    "G2 冲刷执行注入"
                );
                if unicode_only {
                    // 录音中不降级剪贴板：真实 Ctrl+V keydown 会打断 hold 状态机
                    platform::inject::inject_text_unicode_strict(text).map_err(|e| e.to_string())
                } else {
                    platform::inject::inject_text(text).map_err(|e| e.to_string())
                }
            }),
            foreground_hwnd: Arc::new(platform::window::get_foreground_hwnd),
        }
    }
}

/// G2 注入 worker：单消费者保序执行冲刷 job（0.23.13）。
///
/// 渐进 job（unicode_only）在前台已离开目标窗口时挂起（deferred），
/// 终态 job 恢复前台后一并补交。Unicode 失败挂起，等终态 job 走完整
/// 路径（允许剪贴板降级）重试。空文本 job（终态 barrier 无剩余）不触
/// 发注入，仅按 job 语义 ack——此前所有 job 已交付或其文本为空。
/// 所有 sender drop 后 worker 退出；退出时仍有未上屏文本则回传
/// `Failed`（终态失败可见，不静默丢字）。
async fn run_g2_flush_worker(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<G2FlushJob>,
    ack_tx: tokio::sync::mpsc::UnboundedSender<G2FlushAck>,
    runtime: G2Runtime,
) {
    let mut deferred = String::new();
    while let Some(job) = rx.recv().await {
        if job.unicode_only
            && job
                .hwnd
                .is_some_and(|h| (runtime.foreground_hwnd)() != Some(h))
        {
            let chars = job.text.chars().count();
            tracing::debug!(chars, "G2 渐进上屏推迟：前台已离开目标窗口");
            deferred.push_str(&job.text);
            let _ = ack_tx.send(G2FlushAck::Deferred { chars });
            continue;
        }
        let mut text = std::mem::take(&mut deferred);
        text.push_str(&job.text);
        if text.is_empty() {
            // 空 barrier：无内容可注入，不触发焦点修复/SendInput；此前所有
            // job 必已交付（否则 text 非空），按 job 语义 ack 即收口。
            tracing::debug!("G2 空 barrier 收口（无剩余亦无 deferred）");
            let _ = ack_tx.send(G2FlushAck::Delivered {
                upto_seq: if job.unicode_only { job.ack_seq } else { None },
                elapsed_ms: 0,
            });
            continue;
        }
        let hwnd = job.hwnd;
        let unicode_only = job.unicode_only;
        let ack_seq = job.ack_seq;
        let queued_at = std::time::Instant::now();
        let chars = text.chars().count();
        // join 失败（blocking task panic）时保留原文，终态路径按 Failed 回传。
        let text_on_join_failure = text.clone();
        let inject = Arc::clone(&runtime.inject);
        // await 保序：下一次冲刷等本次注入完成，文本顺序不乱。
        let outcome = tokio::task::spawn_blocking(move || inject(&text, hwnd, unicode_only))
            .await
            .map(|result| (result, String::new()));
        let outcome = match outcome {
            Ok((result, _)) => Ok((result, text_on_join_failure)),
            Err(e) => {
                tracing::error!(%e, "G2 注入 blocking task join 失败");
                Err(text_on_join_failure)
            }
        };
        match outcome {
            Ok((Ok(()), _)) => {
                tracing::debug!(chars, "G2 冲刷上屏完成");
                // 0.23.14：交付确认回传账本——浮窗 confirmed 窗口此刻才
                // 清退本 job 覆盖的段（终态 job ack 全部已投递段）。
                let _ = ack_tx.send(G2FlushAck::Delivered {
                    upto_seq: if unicode_only { ack_seq } else { None },
                    elapsed_ms: queued_at.elapsed().as_millis() as u64,
                });
            }
            Ok((Err(e), text)) => {
                if unicode_only {
                    // 渐冲失败不丢文本：挂起等终态 job（完整注入路径）重试。
                    // 无 ack——段保持浮窗可见直到终态补交。
                    let failed_chars = text.chars().count();
                    tracing::warn!(%e, chars, "G2 渐进上屏 Unicode 失败，挂起等终态重试");
                    deferred.push_str(&text);
                    let _ = ack_tx.send(G2FlushAck::Deferred {
                        chars: failed_chars,
                    });
                } else {
                    tracing::error!(%e, chars, "G2 终态注入失败（文本随回执保留）");
                    let _ = ack_tx.send(G2FlushAck::Failed { text });
                }
            }
            Err(text) => {
                if unicode_only {
                    deferred.push_str(&text);
                    let _ = ack_tx.send(G2FlushAck::Deferred {
                        chars: text.chars().count(),
                    });
                } else {
                    let _ = ack_tx.send(G2FlushAck::Failed { text });
                }
            }
        }
    }
    if !deferred.is_empty() {
        tracing::warn!(
            chars = deferred.chars().count(),
            "G2 注入 worker 退出时仍有未上屏文本（终态 barrier 缺失）"
        );
        let _ = ack_tx.send(G2FlushAck::Failed {
            text: std::mem::take(&mut deferred),
        });
    }
}

/// G2 交付 ack 消费 task（0.23.14）：worker 回执推进账本 ack 水位，
/// confirmed 窗口投影（仅 ack 后清退）经 `VOICE_G2_DELIVERY` 事件下发
/// 浮窗。epoch 竞态墙与 VOICE_PARTIAL 同源。
/// 0.23.14.7 P1-2：终态回执精化状态机阶段（Delivered/Failed 只作表达，
/// 不改变投递权语义——重开仍只发生在投递失败路径）。
async fn run_g2_ack_task(
    app: tauri::AppHandle,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<G2FlushAck>,
    ledger: Arc<Mutex<DictationLedger>>,
    worker_deferred: Arc<AtomicBool>,
    gate: Arc<G2TerminalGate>,
    epoch: u64,
) {
    while let Some(ack) = rx.recv().await {
        match ack {
            G2FlushAck::Delivered {
                upto_seq,
                elapsed_ms,
            } => {
                worker_deferred.store(false, Ordering::Release);
                if upto_seq.is_none() {
                    gate.observe_receipt(G2TerminalPhase::Delivered);
                }
                tracing::debug!(?upto_seq, elapsed_ms, "G2 注入交付确认（ack）");
                let confirmed = {
                    let mut ledger = ledger.lock().unwrap();
                    ledger.ack_delivered(upto_seq);
                    ledger.pending_text()
                };
                let _ = app.emit(
                    EventNames::VOICE_G2_DELIVERY,
                    serde_json::json!({
                        "target": "g2",
                        "epoch": epoch,
                        "confirmed": confirmed,
                    }),
                );
            }
            G2FlushAck::Deferred { chars } => {
                // 不改账本：段保持可见，等终态 job 补交后一并 ack。
                // 置位 deferred 标志（仅诊断）：终态 barrier 无条件排入，
                // 不依据该标志决策（0.23.14.6）。
                worker_deferred.store(true, Ordering::Release);
                tracing::debug!(chars, "G2 注入推迟，待交付文本保持浮窗可见");
            }
            G2FlushAck::Failed { text } => {
                // 0.23.14.6：终态注入失败不得静默丢字——错误事件携带原文，
                // 用户可见可恢复；账本水位不推进（文本保持待交付语义）。
                // 0.23.14.7 P1-2：状态机精化为 Failed。
                worker_deferred.store(true, Ordering::Release);
                gate.observe_receipt(G2TerminalPhase::Failed);
                tracing::error!(
                    chars = text.chars().count(),
                    "G2 终态注入失败，未上屏文本随错误事件保留"
                );
                let _ = app.emit(
                    EventNames::VOICE_ERROR,
                    serde_json::json!({
                        "message": "语音文本未能注入目标窗口，以下文本未上屏",
                        "text": text,
                        "target": "g2",
                        "epoch": epoch,
                    }),
                );
            }
        }
    }
}

/// G2 冲刷投递失败：文本未进入 worker 队列，调用方必须回滚账本水位并
/// 保留可恢复文本（0.23.14.6——channel 缺失/关闭时不得把文本视为已被
/// worker 接管）。
#[derive(Debug)]
struct G2SendError {
    /// 未进入队列的文本。
    text: String,
    /// 失败原因（诊断）。
    reason: &'static str,
}

/// 向 G2 注入 worker 投递冲刷 job；失败时把文本带回给调用方（0.23.14.6）。
fn send_g2_flush(
    flush_tx: &Option<tokio::sync::mpsc::UnboundedSender<G2FlushJob>>,
    text: String,
    hwnd: Option<isize>,
    unicode_only: bool,
    ack_seq: Option<u64>,
) -> Result<(), G2SendError> {
    let Some(tx) = flush_tx else {
        return Err(G2SendError {
            text,
            reason: "worker 未启动",
        });
    };
    if let Err(send_error) = tx.send(G2FlushJob {
        text,
        hwnd,
        unicode_only,
        ack_seq,
    }) {
        // SendError 携带原 job：文本随之带回，不视为已被 worker 接管。
        return Err(G2SendError {
            text: send_error.0.text,
            reason: "worker 已退出",
        });
    }
    Ok(())
}

/// G2 终态剩余交付（0.23.13；0.23.14.6 重构为无条件 barrier + 先投递后落账；
/// 0.23.14.7 P1-2 终态归属走 [`G2TerminalGate`] 状态机）：
/// Final 全文剥去已投递前缀（pending 窗口 + 未上报 terminal span + 尾段），经
/// 注入 worker 保序上屏。
///
/// **终态 barrier 无条件排入**：即使剩余为空、`g2_deferred` 标志为 false——
/// 该标志由独立 ack task 异步维护，Final 与 Deferred 回执的先后没有保证，
/// 依据它跳过 barrier 会让前台漂移期间挂起的渐进文本随 worker 退出丢失。
///
/// **终态归属**：`via` 声明本次交付的证据链——事件消费者持 Final 全文走
/// [`G2TerminalPhase::FinalObserved`]；事件超时后的恢复路径已从引擎侧取回
/// 累计全文（含迟到 Final 的尾段），走 [`G2TerminalPhase::BarrierQueued`]。
/// 只有闸门处于 `Open` 时才取得交付权：正常 Final 与恢复路径互斥且都恰好
/// 执行一次，空 barrier 不可能再抢占迟到的 Final 交付。
///
/// 投递失败（worker 不存在/已退出）时不推进账本水位、闸门重开 `Open`
/// （保留重试权），文本经 `Err` 返回由调用方发出可见错误。
fn deliver_g2_remaining(
    ledger: &Arc<Mutex<DictationLedger>>,
    flush_tx: &Option<tokio::sync::mpsc::UnboundedSender<G2FlushJob>>,
    gate: &G2TerminalGate,
    prev_fg_hwnd: Option<isize>,
    final_text: &str,
    via: G2TerminalPhase,
) -> Result<(), G2SendError> {
    let diverged = {
        let ledger = ledger.lock().unwrap();
        !final_text.is_empty()
            && !ledger.flushed_text().is_empty()
            && !final_text.starts_with(ledger.flushed_text())
    };
    if diverged {
        tracing::warn!(
            final_chars = final_text.chars().count(),
            "G2 Final 与已上屏前缀不一致，按公共前缀裁剪剩余（引擎整段重排？）"
        );
    }
    let remaining = ledger.lock().unwrap().peek_remaining_from_final(final_text);
    if !gate.claim(via) {
        tracing::debug!(?via, phase = ?gate.phase(), "忽略重复的 G2 终态交付");
        return Ok(());
    }
    if remaining.is_empty() {
        tracing::debug!("G2 Final 无新增剩余，仍排入终态 barrier 冲刷 worker 内 deferred 文本");
    } else {
        tracing::debug!(chars = remaining.chars().count(), "G2 终态交付剩余文本");
    }
    match send_g2_flush(flush_tx, remaining.clone(), prev_fg_hwnd, false, None) {
        Ok(()) => {
            // 投递成功才推进水位：失败时文本不属于 worker，pending 保持可见。
            ledger.lock().unwrap().commit_remaining(&remaining);
            Ok(())
        }
        Err(e) => {
            gate.reopen();
            Err(e)
        }
    }
}

/// Legacy Final 组合（G1/G3）：旧 profile 返回累计全文，PreviewDraft 可返回
/// terminal tail。只基于 confirmed 累计做明显前缀判断，绝不使用 Preview
/// 作为最终文本。
fn compose_final_text(confirmed_cache: &str, final_text: &str) -> String {
    if confirmed_cache.is_empty() {
        return final_text.to_string();
    }
    if final_text.is_empty() {
        return confirmed_cache.to_string();
    }
    if final_text.starts_with(confirmed_cache) {
        return final_text.to_string();
    }
    if confirmed_cache.starts_with(final_text) {
        return confirmed_cache.to_string();
    }
    format!("{confirmed_cache}{final_text}")
}

/// 预览组成段的拼接投影（0.23.14.6）：UI 展示文本 = 全部段文本顺序拼接。
fn preview_projection(spans: &[PreviewSegment]) -> String {
    spans.iter().map(|segment| segment.text.as_str()).collect()
}

/// 0.23.16.4 组合预览双层投影：除末段外视为已冻结短语（只增不减，较稳），
/// 末段是活动尾部（可变，正在重复识别）。G2 浮窗以两级灰度呈现
/// "稳定递增"的视觉语义——冻结短语比活动尾部更实。空清单返回两个空串。
fn preview_two_layer_projection(spans: &[PreviewSegment]) -> (String, String) {
    match spans.split_last() {
        Some((tail, head)) => {
            let frozen: String = head.iter().map(|segment| segment.text.as_str()).collect();
            (frozen, tail.text.clone())
        }
        None => (String::new(), String::new()),
    }
}

/// G2 投递失败的可见错误（0.23.14.6）：错误事件携带未上屏原文——终态
/// 失败不得只记日志静默丢字，用户必须能拿回文本。
fn emit_g2_delivery_error(app: &tauri::AppHandle, e: &G2SendError) {
    tracing::error!(reason = e.reason, chars = e.text.chars().count(), "G2 冲刷未进入注入队列");
    let _ = app.emit(
        EventNames::VOICE_ERROR,
        serde_json::json!({
            "message": "语音文本未能交付注入服务，以下文本未上屏",
            "text": e.text,
            "target": "g2",
        }),
    );
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
    g2_ledger: Option<Arc<Mutex<DictationLedger>>>,
    g2_flush_tx: Option<tokio::sync::mpsc::UnboundedSender<G2FlushJob>>,
    g2_terminal: Option<Arc<G2TerminalGate>>,
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

    // Legacy 路径（G1/G3）的 confirmed 累计缓存——引擎在 confirmed 未变化时
    // 只发空串（不再每块重复搬运全量正文），前端契约仍是累计文本。
    let mut confirmed_cache = String::new();
    // 0.23.13：PreviewDraft 路径的 Draft 统一进 DictationLedger——
    // G2 按保留窗口渐进上屏（g2_ledger + 注入 worker），
    // Editor 经 EditorDictationState 的同款账本延迟写正文。
    // 0.23.14.6：Preview 的范围真源是组成段清单（preview_spans）——
    // Draft 到达即按 span 音频范围本地 settle（未覆盖后缀继续显示），
    // 不等引擎下一个 Preview 事件，杜绝 `confirmed=A, preview=A+B+C`
    // 的一帧重复投影；Legacy Partial 路径没有范围，直接用事件文本。
    let mut preview_spans: Vec<PreviewSegment> = Vec::new();
    // 已见 Draft 的最大提交边界：迟到的 Preview 快照（组合事件经 latest
    // slot 合并，可能在 Draft 之后才送达）不得让已清退前缀回场。
    let mut preview_settled_boundary: u64 = 0;
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

                // 0.23.14.6：Draft 提交即按音频范围本地 settle 预览段——
                // 覆盖/跨界段退场、未覆盖后缀继续显示，与引擎
                // settle_preview_after_commit 同一语义（共享函数），不依赖
                // 引擎下一个 Preview 事件，也不做字符串裁剪。
                settle_preview_segments(&mut preview_spans, span.audio_range.end_sample);
                preview_settled_boundary =
                    preview_settled_boundary.max(span.audio_range.end_sample);
                let preview_now = preview_projection(&preview_spans);

                // ── Editor：接受进账本 → 保留窗口外冲刷写正文 ──
                if is_editor {
                    let Some(state) = editor_state.as_ref() else {
                        continue;
                    };
                    let accepted = {
                        let mut st = state.lock().unwrap();
                        st.push_draft_span(span)
                    };
                    if accepted {
                        let flushed = {
                            let mut st = state.lock().unwrap();
                            st.drain_flushable_spans()
                        };
                        for (seq, span) in &flushed {
                            emit_editor_draft_segment(&app, state, *seq, span);
                        }
                    }
                    let phase = if paused.load(Ordering::Relaxed) {
                        "paused"
                    } else {
                        "recording"
                    };
                    let (last_seq, confirmed) = {
                        let st = state.lock().unwrap();
                        (st.last_seq(), st.pending_text())
                    };
                    emit_editor_status(
                        &app,
                        state,
                        phase,
                        last_seq,
                        (!confirmed.is_empty()).then_some(confirmed.as_str()),
                        (!preview_now.is_empty()).then_some(preview_now.as_str()),
                        None,
                    );
                    continue;
                }

                // ── G2 PreviewDraft：接受进账本 → 保留窗口外渐进上屏 ──
                let Some(ledger) = g2_ledger.as_ref() else {
                    continue;
                };
                let (accepted, flushed) = {
                    let mut ledger = ledger.lock().unwrap();
                    let accepted = ledger.accept_draft_span(span);
                    let flushed = if accepted {
                        ledger.queue_flushable()
                    } else {
                        Vec::new()
                    };
                    (accepted, flushed)
                };
                if !accepted {
                    continue;
                }
                if !flushed.is_empty() {
                    let text: String = flushed.iter().map(|(_, s)| s.text.as_str()).collect();
                    // 0.23.14：job 携带覆盖的最大 seq；注入成功 ack 后
                    // 浮窗 confirmed 窗口才清退（VOICE_G2_DELIVERY）。
                    let ack_seq = flushed.last().map(|(seq, _)| *seq);
                    tracing::debug!(
                        spans = flushed.len(),
                        chars = text.chars().count(),
                        "G2 渐进上屏：投递保留窗口外草稿（等待注入 ack）"
                    );
                    // 0.23.14.6：投递失败回滚 queued/flushed 水位——账本
                    // 不得把未进入 worker 队列的段视为已接管。
                    if let Err(e) = send_g2_flush(&g2_flush_tx, text, prev_fg_hwnd, true, ack_seq) {
                        ledger.lock().unwrap().unqueue_spans(&flushed);
                        emit_g2_delivery_error(&app, &e);
                    }
                }
                // 浮窗 confirmed 投影 = 待交付文本（queued 未 ack 的段保持
                // 可见，直到注入成功；ack 到达时由 ack task 更新）
                confirmed_cache = ledger.lock().unwrap().pending_text();
                // 0.23.16.4：携带双层投影（冻结短语 / 活动尾部），浮窗两级灰度。
                let (preview_frozen, preview_tail) = preview_two_layer_projection(&preview_spans);
                let _ = app.emit(
                    EventNames::VOICE_PARTIAL,
                    serde_json::json!({
                        "confirmed": confirmed_cache.as_str(),
                        "preview": preview_now.as_str(),
                        "previewFrozen": preview_frozen.as_str(),
                        "previewTail": preview_tail.as_str(),
                        "target": target_str,
                        "epoch": recording_epoch,
                    }),
                );
            }
            SttEvent::Preview {
                generation,
                request_id,
                audio_range: _,
                revision: _,
                text,
                spans,
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
                // 0.23.14.6：引擎的组成段清单是范围真源，整体替换本地清单；
                // 再按已见 Draft 边界 settle 一次——组合事件经 latest slot
                // 合并，快照可能早于刚处理的 Draft，不得让已清退前缀回场。
                preview_spans = spans;
                settle_preview_segments(&mut preview_spans, preview_settled_boundary);
                let preview_now = preview_projection(&preview_spans);
                if preview_now != text {
                    tracing::debug!(
                        event_chars = text.chars().count(),
                        settled_chars = preview_now.chars().count(),
                        "Preview 快照按已提交边界收敛（快照早于最近 Draft）"
                    );
                }
                if is_editor {
                    let Some(state) = editor_state.as_ref() else {
                        continue;
                    };
                    let phase = if paused.load(Ordering::Relaxed) {
                        "paused"
                    } else {
                        "recording"
                    };
                    let (last_seq, confirmed) = {
                        let st = state.lock().unwrap();
                        (st.last_seq(), st.pending_text())
                    };
                    emit_editor_status(
                        &app,
                        state,
                        phase,
                        last_seq,
                        (!confirmed.is_empty()).then_some(confirmed.as_str()),
                        (!preview_now.is_empty()).then_some(preview_now.as_str()),
                        None,
                    );
                } else {
                    // G2 PreviewDraft：confirmed 投影为保留窗口文本（0.23.13），
                    // 已上屏段落不再重复展示；Legacy 无账本时退回累计缓存。
                    let confirmed = if let Some(ledger) = g2_ledger.as_ref() {
                        ledger.lock().unwrap().pending_text()
                    } else {
                        confirmed_cache.clone()
                    };
                    // 0.23.9.10：Preview 事件到达即意味着预览状态变化（引擎边沿
                    // 触发，只在短语追加/尾部更新/显式清空时产出）。confirmed 与
                    // preview 均空的显式清空也必须发送——否则句尾旧虚字残留到
                    // 下一事件才被覆盖。
                    // 0.23.16.4：携带双层投影（冻结短语 / 活动尾部）。
                    let (preview_frozen, preview_tail) =
                        preview_two_layer_projection(&preview_spans);
                    let _ = app.emit(
                        EventNames::VOICE_PARTIAL,
                        serde_json::json!({
                            "confirmed": confirmed.as_str(),
                            "preview": preview_now.as_str(),
                            "previewFrozen": preview_frozen.as_str(),
                            "previewTail": preview_tail.as_str(),
                            "previewRequestId": request_id,
                            "target": target_str,
                            "epoch": recording_epoch,
                        }),
                    );
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
                    emit_editor_status(
                        &app,
                        state,
                        phase,
                        last_seq,
                        None,
                        (!preview.is_empty()).then_some(preview.as_str()),
                        None,
                    );
                    continue;
                }

                // 0.22.15：confirmed 和 preview 都为空时不发 VOICE_PARTIAL
                if confirmed_changed || !confirmed.is_empty() {
                    confirmed_cache = confirmed;
                }
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

                // ── Editor：冲刷全部 pending 段 + 尾段收尾 + ended 状态 ──
                if is_editor {
                    if let Some(state) = editor_state.as_ref() {
                        let (pending, tail) = {
                            let mut st = state.lock().unwrap();
                            st.finalize_parts(&text)
                        };
                        for (seq, span) in &pending {
                            emit_editor_draft_segment(&app, state, *seq, span);
                        }
                        if let Some((seq, tail_text)) = tail {
                            emit_editor_segment(&app, state, seq, &tail_text);
                        } else {
                            tracing::debug!("editor Final 无新增尾段");
                        }
                        let last_seq = state.lock().unwrap().last_seq();
                        emit_editor_status(&app, state, "ended", last_seq, None, None, None);
                    }
                    // Final 是 session 的最后一个事件，退出循环
                    break;
                }

                // ── G2 PreviewDraft：终态只补交剩余（渐进上屏后的差量）──
                if let (Some(ledger), Some(gate)) = (g2_ledger.as_ref(), g2_terminal.as_ref()) {
                    if let Err(e) = deliver_g2_remaining(
                        ledger,
                        &g2_flush_tx,
                        gate,
                        prev_fg_hwnd,
                        &text,
                        G2TerminalPhase::FinalObserved,
                    ) {
                        emit_g2_delivery_error(&app, &e);
                    }
                    // Final 是 session 的最后一个事件，退出循环
                    break;
                }

                // ── G1/G3 Legacy：confirmed 累计 + Final 组合，一次性交付 ──
                let final_text = compose_final_text(&confirmed_cache, &text);
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

                // ── G2：已定稿未上屏草稿补注入（0.23.13 语义：错误不丢已定稿）。
                // 录音尚未停止（热键可能仍按住），仅 Unicode 注入。
                // 0.23.14.6：Error 不再是"只发渐进 job 就退出"的终态——完整
                // 注入路径（含剪贴板降级与 deferred 冲刷）由松键 stop /
                // ESC cancel 的无条件终态 barrier 收口（ensure_g2_terminal_
                // barrier），录音期间不注入真实 Ctrl+V、不破坏 hold 状态机。
                // 此处不关闭 final_delivery，保留 barrier 的排入权。
                if let Some(ledger) = g2_ledger.as_ref() {
                    let pending = {
                        let mut ledger = ledger.lock().unwrap();
                        ledger.take_pending()
                    };
                    if !pending.is_empty() {
                        let text: String = pending.iter().map(|(_, s)| s.text.as_str()).collect();
                        let ack_seq = pending.last().map(|(seq, _)| *seq);
                        tracing::debug!(
                            spans = pending.len(),
                            chars = text.chars().count(),
                            "G2 错误终态：补注入已定稿未上屏草稿"
                        );
                        if let Err(e) =
                            send_g2_flush(&g2_flush_tx, text, prev_fg_hwnd, true, ack_seq)
                        {
                            ledger.lock().unwrap().unqueue_spans(&pending);
                            emit_g2_delivery_error(&app, &e);
                        }
                    }
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

/// 事件消费 task 的有界等待结果（0.23.14.7 P1-2）。
///
/// 旧实现 `tokio::time::timeout(budget, handle)` 在超时分支会把 JoinHandle
/// 一并 drop——task 被 detach 后仍在运行，调用方却当作"已结束"继续收口，
/// 迟到的 Final 处理从此失去协调对象。这里把超时分支的句柄还给调用方，
/// 由调用方显式 abort + await 后再走恢复路径。
pub(crate) enum EventTaskWait {
    /// 事件消费 task 已退出（Final/Error/通道关闭，其终态语义已落实）。
    Exited,
    /// 等待超时，事件消费 task 仍在运行；句柄随返回值交还。
    TimedOut(tokio::task::JoinHandle<()>),
}

/// 有界等待事件消费 task；超时不 drop 句柄（可注入短预算，测试不真实
/// 等待生产超时）。
pub(crate) async fn wait_event_task_bounded(
    event_task: Option<tokio::task::JoinHandle<()>>,
    budget: std::time::Duration,
) -> EventTaskWait {
    let Some(mut handle) = event_task else {
        return EventTaskWait::Exited;
    };
    let deadline = tokio::time::Instant::now() + budget;
    // 以 &mut 等待：Elapsed 分支不消费句柄，任务保持受控。
    match tokio::time::timeout_at(deadline, &mut handle).await {
        Ok(_) => EventTaskWait::Exited,
        Err(_) => EventTaskWait::TimedOut(handle),
    }
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
        let mut state = EditorDictationState::new(7, "ed_test".into(), 3, 1);
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
        let mut state = EditorDictationState::new(1, "ed_test".into(), 1, 1);

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
        // Final 收尾段继续单调追加（finalize_parts：pending 空 + 尾段），
        // 重复 Final 不产段
        let (pending, tail) = state.finalize_parts("第一句。第二句。第三句。");
        assert!(pending.is_empty(), "Legacy 增量路径无 pending 段");
        assert_eq!(tail, Some((3, "第三句。".to_string())));
        assert_eq!(
            state.finalize_parts("第一句。第二句。第三句。"),
            (Vec::new(), None)
        );
        assert_eq!(state.last_seq(), 3, "seq 必须严格单调且不跳号");

        // 快照按序可补齐（前端恢复路径）
        let segments = state.after(1, 0).expect("epoch 匹配应可读快照");
        assert_eq!(
            segments.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(state.after(2, 0), None, "旧 epoch 不得读取当前快照");
    }

    /// 验收（0.23.13）：保留窗口外段先行冲刷，Final 冲刷 pending + 尾段。
    #[test]
    fn editor_retention_window_flushes_and_finalizes() {
        let mut state = EditorDictationState::new(1, "ed_test".into(), 1, 1);
        let mk = |id: u64, text: &str| {
            crate::domain::stt::DraftSpan::new(
                id,
                crate::domain::stt::AudioRange::new(id * 80_000, id * 80_000 + 80_000),
                text,
                1,
            )
        };

        // 第一段停留在保留窗口（浮窗展示），不写正文
        assert!(state.push_draft_span(mk(1, "第一句。")));
        assert!(state.drain_flushable_spans().is_empty());
        assert_eq!(state.pending_text(), "第一句。");

        // 第二段定稿 → 第一段冲刷写正文，第二段停留窗口
        assert!(state.push_draft_span(mk(2, "第二句。")));
        let flushed = state.drain_flushable_spans();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].1.text, "第一句。");
        assert_eq!(state.pending_text(), "第二句。");

        // Final：冲刷 pending + 尾段，seq 连续
        let (pending, tail) = state.finalize_parts("第一句。第二句。尾段。");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.text, "第二句。");
        assert_eq!(tail, Some((3, "尾段。".to_string())));
        assert_eq!(state.pending_text(), "");
        // 重复 Final 无新增
        assert_eq!(
            state.finalize_parts("第一句。第二句。尾段。"),
            (Vec::new(), None)
        );
    }

    #[test]
    fn editor_status_edge_emits_only_on_change() {
        let mut edge = EditorStatusEdge::default();

        assert!(edge.should_emit("recording", 0, None, Some("预览"), None, None));
        assert!(
            !edge.should_emit("recording", 0, None, Some("预览"), None, None),
            "完全相同的状态不得重复发送"
        );
        assert!(
            edge.should_emit("recording", 0, None, Some("预览2"), None, None),
            "preview 变化必须发送"
        );
        assert!(
            edge.should_emit("recording", 1, None, Some("预览2"), None, None),
            "seq 变化必须发送"
        );
        assert!(
            edge.should_emit("recording", 1, Some("窗口定稿"), Some("预览2"), None, None),
            "confirmed 窗口变化必须发送"
        );
        assert!(
            edge.should_emit("paused", 1, Some("窗口定稿"), Some("预览2"), None, None),
            "phase 变化必须发送"
        );
        assert!(edge.should_emit(
            "error",
            1,
            Some("窗口定稿"),
            Some("预览2"),
            Some("boom"),
            Some("stt_failed")
        ));
        assert!(
            !edge.should_emit(
                "error",
                1,
                Some("窗口定稿"),
                Some("预览2"),
                Some("boom"),
                Some("stt_failed")
            ),
            "重复的 error 状态不得重复发送"
        );
        assert!(
            edge.should_emit(
                "error",
                1,
                Some("窗口定稿"),
                Some("预览2"),
                Some("boom2"),
                Some("stt_failed")
            ),
            "message 变化必须发送"
        );
        assert!(
            edge.should_emit("ended", 1, None, None, None, None),
            "ended 与上一状态不同必须发送"
        );
    }

    #[test]
    fn repeated_identical_partial_yields_single_status_change() {
        // 验收：相同 Partial 连续出现时只产生一次对外状态变化。
        let mut edge = EditorStatusEdge::default();
        let mut emitted = 0;
        for _ in 0..100 {
            if edge.should_emit("recording", 7, None, Some("同一预览"), None, None) {
                emitted += 1;
            }
        }
        assert_eq!(emitted, 1, "100 次相同 Partial 只允许一次对外状态变化");
    }

    // ── 0.23.14.6 G2 终态 barrier / 注入 worker（可控替身）──────────

    use super::{
        EventTaskWait, G2FlushAck, G2Runtime, G2SendError, G2TerminalGate, G2TerminalPhase,
        TimeoutRecoveryPath, VoiceTarget, claim_final_delivery, deliver_g2_remaining,
        preview_projection, run_g2_flush_worker, send_g2_flush, timeout_recovery_path,
        wait_event_task_bounded,
    };
    use crate::domain::stt::dictation::DictationLedger;
    use crate::domain::stt::{AudioRange, DraftSpan, PreviewSegment, settle_preview_segments};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    type InjectLog = Arc<Mutex<Vec<(String, bool)>>>;

    /// 注入替身：记录 (text, unicode_only)；`fail_unicode` 让渐进注入失败，
    /// `foreground` 模拟前台窗口（None 匹配任意 job hwnd = 无漂移判定源）。
    fn g2_test_runtime(log: InjectLog, fail_unicode: bool, foreground: Option<isize>) -> G2Runtime {
        G2Runtime {
            inject: Arc::new(move |text, _hwnd, unicode_only| {
                log.lock().unwrap().push((text.to_string(), unicode_only));
                if unicode_only && fail_unicode {
                    Err("unicode 注入失败（测试模拟）".to_string())
                } else {
                    Ok(())
                }
            }),
            foreground_hwnd: Arc::new(move || foreground),
        }
    }

    async fn next_ack(rx: &mut tokio::sync::mpsc::UnboundedReceiver<G2FlushAck>) -> G2FlushAck {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("ack 应及时到达")
            .expect("ack 通道不应关闭")
    }

    /// 前台漂移 → 渐进 job deferred；终态 barrier 恢复后一并补交——
    /// worker 单次完整注入合并文本（deferred + job 文本），ack 确认全部。
    #[tokio::test]
    async fn g2_worker_defers_on_fg_drift_then_terminal_flushes() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel();
        let log: InjectLog = Arc::new(Mutex::new(Vec::new()));
        // 前台 = 99，job 目标 hwnd = 1 → 渐进 job 必然判定漂移。
        tokio::spawn(run_g2_flush_worker(
            rx,
            ack_tx,
            g2_test_runtime(log.clone(), false, Some(99)),
        ));

        send_g2_flush(
            &Some(tx.clone()),
            "渐进文本。".into(),
            Some(1),
            true,
            Some(3),
        )
        .expect("渐进 job 入队");
        match next_ack(&mut ack_rx).await {
            G2FlushAck::Deferred { chars } => assert!(chars > 0),
            other => panic!("期望 Deferred，实际 {other:?}"),
        }
        assert!(log.lock().unwrap().is_empty(), "漂移期间不得注入");

        // 终态 barrier：无新增剩余（空文本）也必须冲刷 deferred。
        send_g2_flush(&Some(tx.clone()), String::new(), Some(1), false, None)
            .expect("barrier 入队");
        match next_ack(&mut ack_rx).await {
            G2FlushAck::Delivered { upto_seq, .. } => {
                assert_eq!(upto_seq, None, "终态 job ack 全部已投递段")
            }
            other => panic!("期望 Delivered，实际 {other:?}"),
        }
        {
            let calls = log.lock().unwrap();
            assert_eq!(calls.len(), 1, "终态一次合并注入");
            assert_eq!(calls[0], ("渐进文本。".to_string(), false));
        }
        drop(tx);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // worker 退出时无 deferred → 不得有 Failed 回执残留
        assert!(ack_rx.try_recv().is_err());
    }

    /// Unicode 严格注入失败（前台未漂移）：渐进 job 挂起 deferred，终态
    /// barrier 走完整注入路径（允许剪贴板降级）重试成功——已定稿文本不丢。
    #[tokio::test]
    async fn g2_worker_unicode_failure_defers_then_terminal_full_path() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel();
        let log: InjectLog = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run_g2_flush_worker(
            rx,
            ack_tx,
            g2_test_runtime(log.clone(), true, Some(1)),
        ));

        send_g2_flush(&Some(tx.clone()), "待上屏。".into(), Some(1), true, Some(1))
            .expect("渐进 job 入队");
        match next_ack(&mut ack_rx).await {
            G2FlushAck::Deferred { .. } => {}
            other => panic!("期望 Deferred，实际 {other:?}"),
        }

        send_g2_flush(&Some(tx.clone()), "尾段。".into(), Some(1), false, None)
            .expect("barrier 入队");
        match next_ack(&mut ack_rx).await {
            G2FlushAck::Delivered { upto_seq, .. } => assert_eq!(upto_seq, None),
            other => panic!("期望 Delivered，实际 {other:?}"),
        }
        {
            let calls = log.lock().unwrap();
            // 第一次：unicode 失败（挂起）；第二次：完整路径合并 deferred + 尾段
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0], ("待上屏。".to_string(), true));
            assert_eq!(calls[1], ("待上屏。尾段。".to_string(), false));
        }
        drop(tx);
    }

    /// 空文本终态 barrier 不触发注入（不恢复焦点/不 SendInput），仅按
    /// job 语义 ack 收口——纯静音会话的 stop/cancel 不产生副作用。
    #[tokio::test]
    async fn g2_empty_terminal_barrier_skips_injection() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel();
        let log: InjectLog = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run_g2_flush_worker(
            rx,
            ack_tx,
            g2_test_runtime(log.clone(), false, None),
        ));

        send_g2_flush(&Some(tx.clone()), String::new(), None, false, None)
            .expect("空 barrier 入队");
        match next_ack(&mut ack_rx).await {
            G2FlushAck::Delivered { upto_seq, .. } => assert_eq!(upto_seq, None),
            other => panic!("期望 Delivered，实际 {other:?}"),
        }
        assert!(log.lock().unwrap().is_empty(), "空 barrier 不得注入");
        drop(tx);
    }

    /// worker 退出时仍有 deferred（终态 barrier 缺失）→ Failed 回执携带
    /// 原文，不静默丢字。
    #[tokio::test]
    async fn g2_worker_exit_with_deferred_reports_failed() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel();
        let log: InjectLog = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run_g2_flush_worker(
            rx,
            ack_tx,
            g2_test_runtime(log.clone(), false, Some(99)),
        ));
        send_g2_flush(
            &Some(tx.clone()),
            "未上屏文本。".into(),
            Some(1),
            true,
            Some(2),
        )
        .expect("渐进 job 入队");
        match next_ack(&mut ack_rx).await {
            G2FlushAck::Deferred { .. } => {}
            other => panic!("期望 Deferred，实际 {other:?}"),
        }
        drop(tx); // sender 全部释放 → worker 退出
        match next_ack(&mut ack_rx).await {
            G2FlushAck::Failed { text } => assert_eq!(text, "未上屏文本。"),
            other => panic!("期望 Failed，实际 {other:?}"),
        }
    }

    fn g2_span(id: u64, text: &str) -> DraftSpan {
        DraftSpan::new(
            id,
            AudioRange::new(id * 160_000, id * 160_000 + 160_000),
            text,
            1,
        )
    }

    /// Final 与 flushed 完全一致（remaining 为空）仍必须排入终态 barrier
    /// （冲刷 worker 内 deferred）；投递成功后才 commit 落账。
    #[tokio::test]
    async fn g2_final_empty_remaining_still_sends_terminal_barrier() {
        let ledger = Arc::new(Mutex::new(DictationLedger::new(0)));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(G2TerminalGate::new());
        {
            let mut l = ledger.lock().unwrap();
            l.accept_draft_span(g2_span(1, "已全部渐进。"));
            l.queue_flushable();
            l.ack_delivered(Some(1));
        }
        deliver_g2_remaining(
            &ledger,
            &Some(tx),
            &gate,
            None,
            "已全部渐进。",
            G2TerminalPhase::FinalObserved,
        )
        .expect("终态交付成功");
        assert_eq!(gate.phase(), G2TerminalPhase::FinalObserved, "终态闸门关闭");
        let job = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("barrier job 必须排入")
            .expect("通道不应关闭");
        assert!(job.text.is_empty(), "无剩余时 barrier 文本为空");
        assert!(!job.unicode_only, "终态 job 允许完整注入路径");
        {
            let l = ledger.lock().unwrap();
            assert_eq!(
                l.peek_remaining_from_final("已全部渐进。"),
                "",
                "commit 后无剩余"
            );
            assert!(l.pending_text().is_empty());
        }
    }

    /// channel 不存在：文本不能视为 worker 接管——账本水位不动、pending
    /// 保持可见、终态闸门重开保留重试权。
    #[tokio::test]
    async fn g2_send_failure_keeps_ledger_and_reopens_gate() {
        let ledger = Arc::new(Mutex::new(DictationLedger::new(0)));
        let gate = Arc::new(G2TerminalGate::new());
        {
            let mut l = ledger.lock().unwrap();
            l.accept_draft_span(g2_span(1, "待交付。"));
        }
        let err: G2SendError = deliver_g2_remaining(
            &ledger,
            &None,
            &gate,
            None,
            "待交付。",
            G2TerminalPhase::FinalObserved,
        )
        .expect_err("worker 不存在必须失败");
        assert!(!err.text.is_empty(), "失败回执必须携带可恢复文本");
        assert_eq!(
            gate.phase(),
            G2TerminalPhase::Open,
            "终态闸门必须重开（保留重试权）"
        );
        {
            let l = ledger.lock().unwrap();
            assert_eq!(l.pending_text(), "待交付。", "水位未推进，pending 保持可见");
            assert_eq!(l.peek_remaining_from_final("待交付。"), "待交付。");
        }
        // 闸门重开后重试（有 worker 时）仍可交付
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        deliver_g2_remaining(
            &ledger,
            &Some(tx),
            &gate,
            None,
            "待交付。",
            G2TerminalPhase::FinalObserved,
        )
        .expect("重试交付成功");
        let job = rx.recv().await.expect("重试 job 排入");
        assert_eq!(job.text, "待交付。");
    }

    /// 重复 Final/stop 不重复上屏：终态闸门关闭后第二次调用直接跳过，
    /// 不再排 job。
    #[tokio::test]
    async fn g2_repeated_terminal_delivery_is_gated() {
        let ledger = Arc::new(Mutex::new(DictationLedger::new(0)));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(G2TerminalGate::new());
        deliver_g2_remaining(
            &ledger,
            &Some(tx.clone()),
            &gate,
            None,
            "",
            G2TerminalPhase::FinalObserved,
        )
        .expect("首次终态交付");
        let first = rx.recv().await.expect("首个 barrier job");
        assert_eq!(first.text, "");
        deliver_g2_remaining(
            &ledger,
            &Some(tx),
            &gate,
            None,
            "迟到 Final",
            G2TerminalPhase::FinalObserved,
        )
        .expect("重复终态按 Ok 跳过");
        assert!(
            rx.try_recv().is_err(),
            "闸门关闭后不得再排终态 job（防重复上屏）"
        );
    }

    /// 0.23.14.7 P1-2 核心竞态：事件消费 task 等待超时后，终态收口必须经
    /// 引擎侧恢复路径携带迟到 Final 的尾段，而不是抢排空 barrier 把尾段
    /// 吞掉。旧缺陷：兜底 barrier 先置 final_delivery=true（空 barrier），
    /// 迟到 Final 的 `deliver_g2_remaining` 被当作重复跳过——尾段永久丢失。
    #[tokio::test]
    async fn g2_event_timeout_recovery_delivers_late_final_tail() {
        let ledger = Arc::new(Mutex::new(DictationLedger::new(0)));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(G2TerminalGate::new());
        {
            let mut l = ledger.lock().unwrap();
            l.accept_draft_span(g2_span(1, "已上屏。"));
            l.queue_flushable();
            l.ack_delivered(Some(1));
        }

        // 1) 等待超时本身不得关闭终态交付权（空 barrier 不得提前排入）。
        let never_ready = tokio::spawn(std::future::pending::<()>());
        let timed_out = wait_event_task_bounded(Some(never_ready), Duration::from_millis(20)).await;
        let handle = match timed_out {
            EventTaskWait::TimedOut(handle) => handle,
            EventTaskWait::Exited => panic!("未完成任务不应被判为已退出"),
        };
        assert_eq!(gate.phase(), G2TerminalPhase::Open, "超时本身不得抢占终态");

        // 2) abort 消费者后走恢复路径：终态文本从引擎侧取回（含迟到 Final
        //    的尾段），经 BarrierQueued 归属交付——尾段必须完整进入 worker。
        handle.abort();
        let _ = handle.await;
        let recovered_text = "已上屏。迟到的尾段。";
        deliver_g2_remaining(
            &ledger,
            &Some(tx.clone()),
            &gate,
            None,
            recovered_text,
            G2TerminalPhase::BarrierQueued,
        )
        .expect("恢复路径交付成功");
        let job = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("恢复 barrier job 必须排入")
            .expect("通道不应关闭");
        assert_eq!(
            job.text, "迟到的尾段。",
            "迟到 Final 的尾段必须经恢复路径进入 worker，不得被空 barrier 吞掉"
        );
        assert!(!job.unicode_only, "终态 job 走完整注入路径");
        assert_eq!(gate.phase(), G2TerminalPhase::BarrierQueued);

        // 3) 迟到 Final 随后到达（竞态窗口的另一半）：闸门已收口，按重复
        //    跳过——不重复注入，尾段已由恢复路径交付过一次。
        deliver_g2_remaining(
            &ledger,
            &Some(tx),
            &gate,
            None,
            recovered_text,
            G2TerminalPhase::FinalObserved,
        )
        .expect("重复终态按 Ok 跳过");
        assert!(rx.try_recv().is_err(), "迟到 Final 不得二次排 job");
    }

    /// 有界等待必须区分"已退出"与"超时仍在运行"；超时分支返回的句柄
    /// 保持可控（可 abort、可 await），不再被 drop 成 detached task。
    #[tokio::test]
    async fn g2_event_task_wait_distinguishes_exited_and_timed_out() {
        let finished = tokio::spawn(async {});
        assert!(
            matches!(
                wait_event_task_bounded(Some(finished), Duration::from_secs(5)).await,
                EventTaskWait::Exited
            ),
            "正常完成的 task 必须判为 Exited"
        );

        let slow = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let handle = match wait_event_task_bounded(Some(slow), Duration::from_millis(20)).await {
            EventTaskWait::TimedOut(handle) => handle,
            EventTaskWait::Exited => panic!("超时不得判为已退出"),
        };
        handle.abort();
        let _ = handle.await;
    }

    /// 0.23.14.7 P1 G1/G2/G3 超时恢复矩阵：注入短预算触发事件 task 等待
    /// 超时 → abort → 按 target 分流恢复。旧缺陷：恢复全文只进 G2 专用
    /// 恢复函数，MainWindow / ChatWindow 被静默丢弃。
    ///
    /// - G1/G3：必须走 Legacy 交付路径；非空恢复文本首次领取即交付、重复
    ///   领取被 final_delivery 闸门拒绝——恰好一次，无静默丢失、无重复 Final；
    ///   空文本不消耗闸门（终态未发生，后续真实 Final 仍可交付）。
    /// - G2：必须走账本终态恢复（BarrierQueued 归属）；迟到 Final 被终态
    ///   闸门挡住，恢复文本恰好上屏一次。
    #[tokio::test]
    async fn event_timeout_recovery_matrix_covers_g1_g2_g3() {
        // 通用前置：可注入短预算把"仍在运行的事件 task"判为 TimedOut，
        // 且句柄保持可控（abort + await 后才走恢复）。
        let slow = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let handle = match wait_event_task_bounded(Some(slow), Duration::from_millis(20)).await {
            EventTaskWait::TimedOut(handle) => handle,
            EventTaskWait::Exited => panic!("超时不得判为已退出"),
        };
        handle.abort();
        let _ = handle.await;

        // ── G1 / G3：Legacy 交付路径 + final_delivery 闸门恰好一次 ──
        for target in [VoiceTarget::MainWindow, VoiceTarget::ChatWindow] {
            assert_eq!(
                timeout_recovery_path(target),
                TimeoutRecoveryPath::LegacyDeliverFinal,
                "{target:?} 必须按 Legacy 语义交付恢复文本（旧实现静默丢弃）"
            );
            let guard = Arc::new(AtomicBool::new(false));
            let recovered = "恢复的最终全文。";
            assert!(
                claim_final_delivery(recovered, Some(&guard)),
                "{target:?} 恢复文本首次领取必须交付（不得静默丢失）"
            );
            assert!(
                !claim_final_delivery(recovered, Some(&guard)),
                "{target:?} 迟到 Final / 重复恢复不得二次交付"
            );
        }

        // 空文本（恢复失败/无正文）：跳过交付且不消耗闸门——同一会话的
        // 真实终态仍可交付。
        let guard = Arc::new(AtomicBool::new(false));
        assert!(!claim_final_delivery("", Some(&guard)));
        assert!(
            claim_final_delivery("后到的全文。", Some(&guard)),
            "空文本恢复不得关闭终态交付权"
        );

        // ── G2：账本终态恢复路径；迟到 Final 被闸门挡住 ──
        assert_eq!(
            timeout_recovery_path(VoiceTarget::ForegroundApp),
            TimeoutRecoveryPath::G2LedgerTerminal
        );
        let ledger = Arc::new(Mutex::new(DictationLedger::new(0)));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(G2TerminalGate::new());
        {
            let mut l = ledger.lock().unwrap();
            l.accept_draft_span(g2_span(1, "已上屏。"));
            l.queue_flushable();
            l.ack_delivered(Some(1));
        }
        deliver_g2_remaining(
            &ledger,
            &Some(tx.clone()),
            &gate,
            None,
            "已上屏。恢复的尾段。",
            G2TerminalPhase::BarrierQueued,
        )
        .expect("G2 恢复路径交付成功");
        let job = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("恢复 barrier job 必须排入")
            .expect("通道不应关闭");
        assert_eq!(job.text, "恢复的尾段。", "恢复尾段必须进入 worker");
        // 迟到 Final（竞态窗口另一半）：闸门已收口，不重复上屏。
        deliver_g2_remaining(
            &ledger,
            &Some(tx),
            &gate,
            None,
            "已上屏。恢复的尾段。",
            G2TerminalPhase::FinalObserved,
        )
        .expect("重复终态按 Ok 跳过");
        assert!(rx.try_recv().is_err(), "G2 迟到 Final 不得二次排 job");
    }

    /// 终态状态机阶段迁移：单写位、失败重开、回执精化——每条路径只在自己
    /// 的证据下收口。
    #[test]
    fn g2_terminal_gate_phase_transitions() {
        let gate = G2TerminalGate::new();
        assert_eq!(gate.phase(), G2TerminalPhase::Open);
        assert!(gate.claim(G2TerminalPhase::FinalObserved));
        // 非 Open 一律拒绝（防 Final 与兜底 barrier 双写）。
        assert!(!gate.claim(G2TerminalPhase::BarrierQueued));
        assert!(!gate.claim(G2TerminalPhase::FinalObserved));
        assert_eq!(gate.phase(), G2TerminalPhase::FinalObserved);

        // 投递失败重开 → 可重试。
        gate.reopen();
        assert_eq!(gate.phase(), G2TerminalPhase::Open);
        assert!(gate.claim(G2TerminalPhase::BarrierQueued));

        // 回执精化：Delivered/Failed 不覆盖 Open（重试中的开放语义优先）。
        gate.reopen();
        gate.observe_receipt(G2TerminalPhase::Delivered);
        assert_eq!(gate.phase(), G2TerminalPhase::Open, "Open 时回执不得写阶段");
        assert!(gate.claim(G2TerminalPhase::BarrierQueued));
        gate.observe_receipt(G2TerminalPhase::Delivered);
        assert_eq!(gate.phase(), G2TerminalPhase::Delivered);
        assert!(
            !gate.claim(G2TerminalPhase::FinalObserved),
            "交付完成不得再重开交付"
        );
        gate.observe_receipt(G2TerminalPhase::Failed);
        assert_eq!(gate.phase(), G2TerminalPhase::Failed);
    }

    /// 消费方预览投影（0.23.14.6）：Draft 到达即按 span 范围 settle 并重算
    /// 投影，不等引擎下一个 Preview 事件——confirmed=A 时预览必须立即变为
    /// B+尾部，杜绝 `confirmed=A, preview=A+B+C` 的一帧重复。
    #[test]
    fn preview_projection_settles_on_draft_without_next_preview() {
        let mut spans = vec![
            PreviewSegment::new(AudioRange::new(0, 160_000), "A"),
            PreviewSegment::new(AudioRange::new(160_000, 320_000), "B"),
            PreviewSegment::new(AudioRange::new(320_000, 368_000), "尾部"),
        ];
        // Draft span 覆盖 A 的音频范围 [0, 160_000)。
        settle_preview_segments(&mut spans, 160_000);
        let projection = preview_projection(&spans);
        assert_eq!(projection, "B尾部", "A 必须立即退场，后缀继续显示");
        assert!(!projection.starts_with('A'), "预览不得重复 confirmed 前缀");
    }

    #[test]
    fn status_edge_handles_preview_cleared_and_empty() {
        let mut edge = EditorStatusEdge::default();
        assert!(edge.should_emit("recording", 0, None, Some("有预览"), None, None));
        // 句尾清空预览 → None 与 Some("") 都是不同状态，必须各自发送一次
        assert!(edge.should_emit("recording", 0, None, Some(""), None, None));
        assert!(!edge.should_emit("recording", 0, None, Some(""), None, None));
        assert!(edge.should_emit("recording", 0, None, None, None, None));
        assert!(!edge.should_emit("recording", 0, None, None, None, None));
    }
}
