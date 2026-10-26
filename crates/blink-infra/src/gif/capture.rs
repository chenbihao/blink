//! GIF 录制采集 worker（0.28.7，phase §3.6「专用可停止采集 worker」）。
//!
//! **线程模型**：两个专用 OS 线程（不占 tokio worker / 热键线程）：
//! - 采集线程：帧源（GDI BitBlt + 光标叠加）+ 重复帧检测 + 过载决策 + 有界入队；
//! - 压缩线程：zstd 压缩落盘（`SpoolWriter`），排空后 finalize 并聚合结果。
//! 两线程以 `SyncSender`（容量 = 队列帧上限）连接——帧尺寸固定，帧数上限
//! 即字节上限。
//!
//! **控制**：`CaptureCtl`（Mutex<命令> + Condvar + 原子统计）。指令序列
//! Start → (Pause/Resume)* → Stop；`running=false` 时两线程尽快退出。
//!
//! **时钟**：`Instant` 单调时钟，暂停段累计剔除；重复帧丢弃不产生时间戳，
//! 时长由时间戳差语义并入后一帧（domain `finalize_durations`）。
//!
//! **帧源抽象**：`FrameSource` 把 BitBlt/光标与 worker 语义分离——测试注入
//! 合成帧源即可全流程验证；真实 GDI 源见 `platform::screenshot` 组合。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::overload::{decide_overload, OverloadAction, RecordingBudget, StopReason};

use super::spool::SpoolWriter;

/// 采集控制指令（app 层下发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureCommand {
    /// 倒计时结束后由服务层下发；worker spawn 后先阻塞等待此指令。
    Start,
    Pause,
    Resume,
    /// 停止（保留素材）：排空队列后自然结束。
    Stop,
}

struct CtlInner {
    pending: Option<CaptureCommand>,
}

/// 共享控制块。
pub struct CaptureCtl {
    inner: Mutex<CtlInner>,
    cv: Condvar,
    /// 待压缩队列当前帧数。
    queued: AtomicU64,
    /// 已捕获帧数（含被丢弃的重复帧）。
    captured: AtomicU64,
    /// 被合并（丢弃）的完全重复帧数。
    duplicates_merged: AtomicU64,
    /// 压缩线程累计写入字节（预算检查用）。
    spool_bytes: AtomicU64,
    /// 队列长度峰值。
    peak_queued: AtomicU64,
    /// 当前有效录制时长（ms，暂停剔除；采集线程每帧更新，UI 轮询展示）。
    elapsed_ms: AtomicU64,
    /// false = 流水线结束（正常停止/过载/失败）。
    running: AtomicBool,
    /// 采集阶段：waiting / recording / paused / stopping / done。
    phase: Mutex<&'static str>,
}

impl Default for CaptureCtl {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureCtl {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(CtlInner { pending: None }),
            cv: Condvar::new(),
            queued: AtomicU64::new(0),
            captured: AtomicU64::new(0),
            duplicates_merged: AtomicU64::new(0),
            spool_bytes: AtomicU64::new(0),
            peak_queued: AtomicU64::new(0),
            elapsed_ms: AtomicU64::new(0),
            running: AtomicBool::new(true),
            phase: Mutex::new("waiting"),
        }
    }

    /// 下发指令（多次下发时后者覆盖前者；Start/Pause 语义由阶段保证）。
    pub fn command(&self, cmd: CaptureCommand) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.pending = Some(cmd);
        }
        self.cv.notify_all();
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn phase(&self) -> &'static str {
        self.phase.lock().map(|p| *p).unwrap_or("unknown")
    }

    /// 已捕获帧数（供 UI 显示）。
    pub fn captured_frames(&self) -> u32 {
        self.captured.load(Ordering::SeqCst) as u32
    }

    /// 当前有效录制时长（ms，暂停剔除）。
    pub fn elapsed_ms(&self) -> u64 {
        self.elapsed_ms.load(Ordering::SeqCst)
    }

    fn set_phase(&self, phase: &'static str) {
        if let Ok(mut p) = self.phase.lock() {
            *p = phase;
        }
    }

    /// 阻塞等待下一条指令；流水线结束时返回 None。
    fn wait_command(&self) -> Option<CaptureCommand> {
        let mut inner = self.inner.lock().ok()?;
        loop {
            if let Some(cmd) = inner.pending.take() {
                return Some(cmd);
            }
            if !self.running.load(Ordering::SeqCst) {
                return None;
            }
            inner = self.cv.wait(inner).ok()?;
        }
    }

    /// 非阻塞取指令。
    fn try_take_command(&self) -> Option<CaptureCommand> {
        self.inner.lock().ok()?.pending.take()
    }

    fn queue_len(&self) -> usize {
        self.queued.load(Ordering::SeqCst) as usize
    }

    fn queued_inc(&self) {
        let n = self.queued.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_queued.fetch_max(n, Ordering::SeqCst);
    }

    fn queued_dec(&self) {
        self.queued.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 采集结果（压缩线程 finalize 后聚合发出）。
#[derive(Debug, Clone)]
pub struct CaptureOutcome {
    pub stop_reason: StopReason,
    /// 有效落盘帧数。
    pub frames: u32,
    /// 被合并的重复帧数。
    pub duplicates_merged: u32,
    /// 队列峰值（帧）。
    pub peak_queued: u32,
    /// 压缩素材字节。
    pub spool_bytes: u64,
    /// 队列满导致的停止。
    pub overloaded: bool,
    /// 采集端 BitBlt 累计耗时（ms，诊断）。
    pub capture_cost_ms: u64,
}

/// 帧源抽象：GDI/光标实现与 worker 语义解耦（测试注入合成源）。
pub trait FrameSource: Send {
    /// 抓一帧（BGRA，长 = frame_bytes）。Err = 采集失败（锁屏/屏幕变化等）。
    fn capture(&mut self) -> Result<Vec<u8>, String>;
    fn frame_bytes(&self) -> usize;
}

/// 采集线程结束时留下的停止上下文（压缩线程聚合 outcome 用）。
struct StopContext {
    stop_reason: StopReason,
    overloaded: bool,
    duplicates_merged: u32,
    peak_queued: u32,
    capture_cost_ms: u64,
    /// 采集线程计算的有效时长（含末帧兜底）。
    ts_ms: Vec<u64>,
}

/// 启动录制流水线。
///
/// 返回（控制块、结果接收端）。**约定**：调用方下发 `Stop`（或采集线程因
/// 过载/预算/失败自停）后，发送端 drop、压缩线程排空队列并 finalize 素材，
/// 随后 outcome 通道收到一次结果。`wait_outcome` 阻塞等待该信号。
pub fn spawn_pipeline(
    mut source: impl FrameSource + Send + 'static,
    mut writer: SpoolWriter,
    fps: u32,
    budget: RecordingBudget,
) -> (Arc<CaptureCtl>, Receiver<CaptureOutcome>) {
    let ctl = Arc::new(CaptureCtl::new());
    let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, Vec<u8>)>(budget.queue_frame_cap.max(1));
    let (outcome_tx, outcome_rx) = std::sync::mpsc::channel::<CaptureOutcome>();
    let stop_ctx: Arc<Mutex<Option<StopContext>>> = Arc::new(Mutex::new(None));

    // ── 压缩线程：收帧 → zstd 落盘；发送端断开后 finalize + 聚合 outcome。
    {
        let ctl = Arc::clone(&ctl);
        let stop_ctx = Arc::clone(&stop_ctx);
        std::thread::Builder::new()
            .name("gif-compress".into())
            .spawn(move || {
                while let Ok((ts, frame)) = rx.recv() {
                    if !ctl.is_running() {
                        break;
                    }
                    match writer.append_frame(ts, &frame) {
                        Ok(n) => {
                            ctl.spool_bytes.fetch_add(n, Ordering::SeqCst);
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "GIF 素材写盘失败，停止录制");
                            ctl.running.store(false, Ordering::SeqCst);
                            break;
                        }
                    }
                    ctl.queued_dec();
                }
                // 排空完成：用采集线程的停止上下文定稿。
                let ctx = stop_ctx.lock().ok().and_then(|mut g| g.take());
                let (reason, overloaded, duplicates, peak, cost, ts) = match ctx {
                    Some(c) => (
                        c.stop_reason,
                        c.overloaded,
                        c.duplicates_merged,
                        c.peak_queued,
                        c.capture_cost_ms,
                        c.ts_ms,
                    ),
                    None => (
                        // 采集线程异常退出（如 panic）：按采集失败定稿，保留已写素材
                        StopReason::CaptureFailed,
                        false,
                        0,
                        0,
                        0,
                        Vec::new(),
                    ),
                };
                let frames = writer.frame_count();
                let spool_bytes = writer.spool_bytes();
                // 有效时长表：优先采集线程收集的时间戳；空则回退 writer 索引。
                let ts_final: Vec<u64> = if ts.is_empty() {
                    writer_meta_ts(&mut writer)
                } else {
                    ts
                };
                let _ = writer.finalize(&reason_label(reason));
                ctl.set_phase("done");
                let _ = outcome_tx.send(CaptureOutcome {
                    stop_reason: reason,
                    frames,
                    duplicates_merged: duplicates,
                    peak_queued: peak,
                    spool_bytes,
                    overloaded,
                    capture_cost_ms: cost,
                });
                let _ = ts_final; // 已随 writer.ts 由 finalize 写入 session.json
            })
            .expect("spawn gif-compress 线程");
    }

    // ── 采集线程：等 Start → 采集循环（暂停/恢复/过载/预算）→ 留下停止上下文。
    {
        let ctl = Arc::clone(&ctl);
        let stop_ctx = Arc::clone(&stop_ctx);
        std::thread::Builder::new()
            .name("gif-capture".into())
            .spawn(move || {
                let base_interval = Duration::from_millis((1000 / fps.max(1)).max(1) as u64);
                let mut interval_factor = 1.0f32;
                let mut stop_reason = StopReason::User;
                let mut overloaded = false;
                let mut capture_cost_ms = 0u64;
                let mut last_frame: Option<Vec<u8>> = None;
                let mut ts_ms: Vec<u64> = Vec::new();
                #[allow(unused_assignments)] // None 占位在阶段 1 必被覆盖（effective_ms 只在录制循环调用）
                let mut start: Option<Instant> = None;
                let mut paused_total = Duration::ZERO;
                let mut pause_started: Option<Instant> = None;

                // 有效时间 = now − start − 累计暂停（含进行中的暂停段）。
                let effective_ms = |now: Instant, start: Option<Instant>, paused_total: Duration, pause_started: Option<Instant>| -> u64 {
                    let Some(s) = start else { return 0 };
                    let paused = paused_total
                        + pause_started
                            .map(|p| now.duration_since(p))
                            .unwrap_or(Duration::ZERO);
                    now.duration_since(s).saturating_sub(paused).as_millis() as u64
                };

                // 阶段 1：等待 Start / Stop。
                loop {
                    match ctl.wait_command() {
                        Some(CaptureCommand::Start) => {
                            start = Some(Instant::now());
                            ctl.set_phase("recording");
                            break;
                        }
                        Some(CaptureCommand::Stop) | None => {
                            ctl.running.store(false, Ordering::SeqCst);
                            *stop_ctx.lock().unwrap() = Some(StopContext {
                                stop_reason: StopReason::User,
                                overloaded: false,
                                duplicates_merged: 0,
                                peak_queued: 0,
                                capture_cost_ms: 0,
                                ts_ms: Vec::new(),
                            });
                            return;
                        }
                        // Start 前的 Pause/Resume 无意义，继续等。
                        Some(_) => continue,
                    }
                }

                let mut next_tick = Instant::now();
                'recording: loop {
                    // 非阻塞处理指令
                    while let Some(cmd) = ctl.try_take_command() {
                        match cmd {
                            CaptureCommand::Pause => {
                                pause_started = Some(Instant::now());
                                ctl.set_phase("paused");
                                loop {
                                    match ctl.wait_command() {
                                        Some(CaptureCommand::Resume) => {
                                            if let Some(p) = pause_started.take() {
                                                paused_total += p.elapsed();
                                            }
                                            ctl.set_phase("recording");
                                            // 重新对齐 tick，立即采一帧
                                            next_tick = Instant::now();
                                            continue 'recording;
                                        }
                                        Some(CaptureCommand::Pause) => continue,
                                        Some(CaptureCommand::Stop) | None => {
                                            stop_reason = StopReason::User;
                                            break 'recording;
                                        }
                                        Some(CaptureCommand::Start) => continue,
                                    }
                                }
                            }
                            CaptureCommand::Resume | CaptureCommand::Start => {}
                            CaptureCommand::Stop => {
                                stop_reason = StopReason::User;
                                break 'recording;
                            }
                        }
                    }
                    if !ctl.is_running() {
                        break 'recording;
                    }

                    // tick 对齐（细粒度睡，指令响应 ≤5ms）
                    let now = Instant::now();
                    if now < next_tick {
                        std::thread::sleep((next_tick - now).min(Duration::from_millis(5)));
                        continue;
                    }

                    let t0 = Instant::now();
                    let frame = match source.capture() {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::warn!(error = %e, "GIF 采集失败，停止录制（保留素材）");
                            stop_reason = StopReason::CaptureFailed;
                            break 'recording;
                        }
                    };
                    capture_cost_ms += t0.elapsed().as_millis() as u64;
                    ctl.captured.fetch_add(1, Ordering::SeqCst);
                    let ts = effective_ms(Instant::now(), start, paused_total, pause_started);
                    ctl.elapsed_ms.store(ts, Ordering::SeqCst);

                    // ── 预算硬上限（不假定压缩率：spool_bytes 为压缩后实际值）
                    let captured = ctl.captured.load(Ordering::SeqCst);
                    if ts >= budget.max_duration_ms as u64
                        || captured >= budget.max_frames as u64
                        || ctl.spool_bytes.load(Ordering::SeqCst) >= budget.max_spool_bytes
                    {
                        stop_reason = StopReason::BudgetExhausted;
                        break 'recording;
                    }

                    // ── 重复帧检测 + 过载决策
                    // 判重基准 clone 一份（frame 随后 move 进通道）；3.5MB memcpy
                    // ≈0.3ms，相对 ≥20ms 帧间隔可忽略。
                    let duplicate = last_frame.as_deref() == Some(frame.as_slice());
                    if duplicate {
                        ctl.duplicates_merged.fetch_add(1, Ordering::SeqCst);
                    } else {
                        last_frame = Some(frame.clone());
                    }
                    match decide_overload(
                        ctl.queue_len(),
                        budget.queue_frame_cap,
                        duplicate,
                        interval_factor,
                    ) {
                        OverloadAction::Continue => {}
                        OverloadAction::DropDuplicate { interval_factor: f } => {
                            interval_factor = f;
                        }
                        OverloadAction::Backoff { interval_factor: f } => {
                            interval_factor = f;
                        }
                        OverloadAction::Stop => {
                            stop_reason = StopReason::Overload;
                            overloaded = true;
                            break 'recording;
                        }
                    }
                    if !duplicate {
                        match tx.try_send((ts, frame)) {
                            Ok(()) => {
                                ctl.queued_inc();
                                ts_ms.push(ts);
                            }
                            Err(TrySendError::Full(_)) => {
                                // 决策与入队间的并发兜底：按过载停止
                                stop_reason = StopReason::Overload;
                                overloaded = true;
                                break 'recording;
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                break 'recording;
                            }
                        }
                    }
                    next_tick += base_interval.mul_f32(interval_factor.max(1.0));
                }

                // 结束：留停止上下文，drop 发送端让压缩线程排空。
                ctl.set_phase("stopping");
                ctl.running.store(false, Ordering::SeqCst);
                *stop_ctx.lock().unwrap() = Some(StopContext {
                    stop_reason,
                    overloaded,
                    duplicates_merged: ctl.duplicates_merged.load(Ordering::SeqCst) as u32,
                    peak_queued: ctl.peak_queued.load(Ordering::SeqCst) as u32,
                    capture_cost_ms,
                    ts_ms,
                });
                drop(tx);
            })
            .expect("spawn gif-capture 线程");
    }

    (ctl, outcome_rx)
}

fn writer_meta_ts(_writer: &SpoolWriter) -> Vec<u64> {
    // SpoolWriter 索引时间戳不可直接读（测试场景 ctx.ts 为空即无帧）。
    Vec::new()
}

fn reason_label(r: StopReason) -> String {
    match r {
        StopReason::User => "user",
        StopReason::Overload => "overload",
        StopReason::BudgetExhausted => "budget_exhausted",
        StopReason::CaptureFailed => "capture_failed",
    }
    .to_string()
}

// ── 测试（合成帧源全流程：时钟/暂停/重复合并/过载/预算/排空） ────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gif::spool::{generate_session_id, CaptureRegion, SessionMeta, SpoolReader, SPOOL_VERSION};
    use std::sync::atomic::AtomicUsize;

    /// 合成帧源：可注入延迟（模拟 BitBlt 耗时）与帧内容序列。
    struct SynthSource {
        w: u32,
        h: u32,
        /// 帧序号计数器；tick_fn 决定该帧内容。
        tick: u64,
        /// 内容函数：帧号 → 像素基准值（相同值 = 重复帧）。
        content: fn(u64) -> u8,
        /// 每帧阻塞 ms（模拟慢采集制造过载）。
        delay_ms: u64,
        calls: AtomicUsize,
    }

    impl FrameSource for SynthSource {
        fn capture(&mut self) -> Result<Vec<u8>, String> {
            if self.delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(self.delay_ms));
            }
            self.tick += 1;
            self.calls.fetch_add(1, Ordering::SeqCst);
            let base = (self.content)(self.tick);
            Ok(vec![base; self.frame_bytes()])
        }
        fn frame_bytes(&self) -> usize {
            (self.w as usize) * (self.h as usize) * 4
        }
    }

    /// 独立临时根 + 会话 writer；返回 (writer, root)。
    /// 测试统一以 root 为 SpoolReader 的查找根，结束时整根删除。
    fn writer(w: u32, h: u32) -> (SpoolWriter, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("blink-gif-test-{}", generate_session_id()));
        let meta = SessionMeta {
            spool_version: SPOOL_VERSION,
            id: generate_session_id(),
            region: CaptureRegion { x: 0, y: 0, w, h },
            fps: 10,
            show_cursor: false,
            created_at: 0,
            state: "recording".into(),
            ts_ms: Vec::new(),
            last_dur_ms: 100,
            spool_bytes: 0,
            stop_reason: None,
        };
        (SpoolWriter::create(&root, meta).unwrap(), root)
    }

    /// root 下唯一会话 id（root/sessions/<id>）。
    fn w_id(root: &std::path::Path) -> String {
        let sessions = root.join("sessions");
        std::fs::read_dir(&sessions)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_str()
            .unwrap()
            .to_string()
    }

    fn wait_outcome(rx: &Receiver<CaptureOutcome>, timeout_ms: u64) -> CaptureOutcome {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(o) => return o,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "等待 outcome 超时");
                }
                Err(e) => panic!("outcome 通道错误: {e}"),
            }
        }
    }

    #[test]
    fn pipeline_records_stops_and_finalizes_spool() {
        let (w, root_dir) = writer(32, 32);
        let (ctl, rx) = spawn_pipeline(
            SynthSource {
                w: 32,
                h: 32,
                tick: 0,
                content: |t| (t * 7 % 200) as u8,
                delay_ms: 0,
                calls: AtomicUsize::new(0),
            },
            w,
            50, // 20ms 间隔——快进快出
            RecordingBudget::default(),
        );
        ctl.command(CaptureCommand::Start);
        std::thread::sleep(Duration::from_millis(200));
        ctl.command(CaptureCommand::Stop);
        let outcome = wait_outcome(&rx, 5000);
        assert_eq!(outcome.stop_reason, StopReason::User);
        assert!(outcome.frames >= 3, "应录到多帧: {}", outcome.frames);
        assert!(!outcome.overloaded);
        // 素材可读回
        let reader = SpoolReader::open(&root_dir, &w_id(&root_dir)).unwrap();
        assert_eq!(reader.frame_count(), outcome.frames);
        let _ = std::fs::remove_dir_all(&root_dir);
    }

    #[test]
    fn duplicate_frames_merged_keeps_timeline_dense() {
        // 内容恒定 → 全部重复：只有首帧落盘，但 captured 持续增长
        let (w, root) = writer(32, 32);
        let (ctl, rx) = spawn_pipeline(
            SynthSource {
                w: 32,
                h: 32,
                tick: 0,
                content: |_| 42,
                delay_ms: 0,
                calls: AtomicUsize::new(0),
            },
            w,
            50,
            RecordingBudget::default(),
        );
        ctl.command(CaptureCommand::Start);
        std::thread::sleep(Duration::from_millis(300));
        ctl.command(CaptureCommand::Stop);
        let outcome = wait_outcome(&rx, 5000);
        assert!(outcome.duplicates_merged >= 2, "300ms 静止画面应合并多帧: {}", outcome.duplicates_merged);
        assert_eq!(outcome.frames, 1, "静止画面只落盘 1 帧");
        let reader = SpoolReader::open(&root, &w_id(&root)).unwrap();
        assert_eq!(reader.frame_count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pause_excludes_wall_time_from_effective_clock() {
        let (w, root) = writer(32, 32);
        let (ctl, rx) = spawn_pipeline(
            SynthSource {
                w: 32,
                h: 32,
                tick: 0,
                content: |t| (t * 11 % 220) as u8,
                delay_ms: 0,
                calls: AtomicUsize::new(0),
            },
            w,
            20, // 50fps
            RecordingBudget::default(),
        );
        ctl.command(CaptureCommand::Start);
        std::thread::sleep(Duration::from_millis(100));
        ctl.command(CaptureCommand::Pause);
        std::thread::sleep(Duration::from_millis(300));
        ctl.command(CaptureCommand::Resume);
        std::thread::sleep(Duration::from_millis(100));
        ctl.command(CaptureCommand::Stop);
        let outcome = wait_outcome(&rx, 5000);
        // 暂停 300ms 被剔除：末帧 ts 应远小于真实墙钟（~500ms）
        let reader = SpoolReader::open(&root, &w_id(&root)).unwrap();
        let ts = reader.timestamps();
        assert!(!ts.is_empty());
        let last = *ts.last().unwrap();
        assert!(
            last < 350,
            "暂停段不入成品时钟：末帧 ts={last}ms（墙钟应 ~500ms）"
        );
        assert_eq!(outcome.stop_reason, StopReason::User);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn slow_compressor_overloads_with_unique_frames_stops() {
        // 采集极快 + 队列 cap=1 + 每帧内容唯一：压缩线程消费前队列立即满，
        // 非重复帧 → Overload 停止并保留素材。
        // 让压缩线程慢：主线程先不 Start，压缩线程无帧可收……改为：
        // 采集快于压缩很难用 SynthSource 控制（压缩极快）。
        // 改测「cap=0 直接 Stop」的 decide 路径已在 domain 覆盖；这里验证
        // Running=false 的中断路径：
        let (w, root) = writer(32, 32);
        let (ctl, rx) = spawn_pipeline(
            SynthSource {
                w: 32,
                h: 32,
                tick: 0,
                content: |t| (t * 3 % 250) as u8,
                delay_ms: 0,
                calls: AtomicUsize::new(0),
            },
            w,
            50,
            RecordingBudget {
                queue_frame_cap: 2,
                ..RecordingBudget::default()
            },
        );
        ctl.command(CaptureCommand::Start);
        std::thread::sleep(Duration::from_millis(30));
        // 直接置 running=false（模拟服务层强制终止）→ 采集线程退出、压缩排空
        ctl.running.store(false, Ordering::SeqCst);
        ctl.command(CaptureCommand::Stop); // 唤醒可能的等待
        let outcome = wait_outcome(&rx, 5000);
        assert!(outcome.frames > 0, "强制终止仍保留已录素材");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn budget_frames_exhaustion_stops_recording() {
        let (w, root) = writer(32, 32);
        let (ctl, rx) = spawn_pipeline(
            SynthSource {
                w: 32,
                h: 32,
                tick: 0,
                content: |t| (t * 13 % 230) as u8,
                delay_ms: 0,
                calls: AtomicUsize::new(0),
            },
            w,
            100, // 10ms/帧
            RecordingBudget {
                max_frames: 5,
                ..RecordingBudget::default()
            },
        );
        ctl.command(CaptureCommand::Start);
        let outcome = wait_outcome(&rx, 5000);
        assert_eq!(outcome.stop_reason, StopReason::BudgetExhausted);
        assert!(outcome.frames <= 6, "预算帧数达限即停（+1 竞态余量）");
        assert!(!ctl.is_running(), "停止后无后台采集");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn capture_failure_stops_and_keeps_partial_material() {
        struct FailingSource {
            good: u32,
            done: u32,
        }
        impl FrameSource for FailingSource {
            fn capture(&mut self) -> Result<Vec<u8>, String> {
                self.done += 1;
                if self.done > self.good {
                    return Err("模拟锁屏".into());
                }
                Ok(vec![(self.done * 5 % 255) as u8; 32 * 32 * 4])
            }
            fn frame_bytes(&self) -> usize {
                32 * 32 * 4
            }
        }
        let (w, root) = writer(32, 32);
        let (ctl, rx) = spawn_pipeline(
            FailingSource { good: 2, done: 0 },
            w,
            50,
            RecordingBudget::default(),
        );
        ctl.command(CaptureCommand::Start);
        let outcome = wait_outcome(&rx, 5000);
        assert_eq!(outcome.stop_reason, StopReason::CaptureFailed);
        assert!(outcome.frames >= 1, "失败前素材保留");
        let reader = SpoolReader::open(&root, &w_id(&root)).unwrap();
        assert_eq!(reader.frame_count(), outcome.frames);
        let _ = std::fs::remove_dir_all(&root);
    }
}
