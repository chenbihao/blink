//! GIF 录制服务编排（0.28.7，bin 侧；0.28.23 统一浮栏）。
//!
//! 职责：
//! - **录制会话**：`start`（素材目录 + 采集流水线 + overlay 进入录制叠层模式）、
//!   `pause/resume`、`stop`（排空 → finalize → 构建 Timeline → 打开编辑器）、
//!   `cancel`（丢弃素材）；同一时刻仅一个活跃录制。
//! - **统一浮栏（0.28.23 §5.7.1）**：截图 overlay（`chord-screenshot`）在录制
//!   期间**不隐藏**——前端隐藏静态截图层、保留同一 `#toolbar` 宿主与
//!   `#annot-canvas` 实时涂画层；本服务负责窗口属性：不可聚焦（不抢被录应用
//!   焦点）、「操作桌面」模式下鼠标穿透 + 命中矩形轮询（指针在工具栏/弹层
//!   矩形内时临时取消穿透）。原独立控制条 / 范围框 / 拖动柄三窗已废弃。
//! - **状态广播**：`blink://gif-recorder-state`（250ms 节流 ticker + 阶段变化
//!   即时推送），overlay 录制面消费。
//! - **编辑会话**：`session_id → Timeline`（domain 编辑模型）内存态；素材/元数据
//!   真源在磁盘（infra spool），崩溃后重开编辑器可重建。
//! - **导出**：`spawn_blocking` 流式编码 + 进度事件 + 取消标志；导出用冻结的
//!   revision 快照，编码期间编辑不影响本次导出。
//! - **素材 TTL**：启动清理 72h 前的会话目录（含崩溃残留）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use blink_domain::gif::Timeline;
use blink_infra::event_names::EventNames;
use blink_infra::gif::{
    self as infra_gif, CaptureCommand, CaptureCtl, CaptureOutcome, EncodeFrame, EncodePlan,
    SessionMeta, SpoolReader, SpoolWriter, spawn_pipeline,
};

/// 录制倒计时（ms）。0 = 无倒计时、点「开始录制」立即采集（2026-10-27
/// 用户定案；保留倒计时机制，改回 >0 即恢复——控制条已适配两种形态）。
const COUNTDOWN_MS: u64 = 0;
/// 状态广播周期。
const STATE_TICK_MS: u64 = 250;
static SNAPSHOT_REVISION: AtomicU64 = AtomicU64::new(1);
/// 素材 TTL。
const SESSION_TTL: Duration = Duration::from_secs(72 * 3600);
/// 截图 overlay 窗口 label（录制期间作为统一浮栏 + 涂画层宿主）。
const OVERLAY_LABEL: &str = "chord-screenshot";
/// 「操作桌面」模式命中矩形轮询周期（ms）——指针移入工具栏到可点击的最大延迟。
const OVERLAY_HIT_POLL_MS: u64 = 16;
/// 编辑器窗口 label。
const EDITOR_LABEL: &str = "gif-editor";

pub(crate) struct ActiveRecording {
    pub(crate) session_id: String,
    pub(crate) ctl: Arc<CaptureCtl>,
    /// 倒计时剩余（phase=countdown 时 Some）。
    countdown_deadline: Option<std::time::Instant>,
    /// 取消标志：true 时停止后丢弃素材。
    cancel_flag: Arc<AtomicBool>,
    /// 共享采集区域位置（拖动柄热更新；与 GdiFrameSource 同一 Arc）。
    region_pos: std::sync::Arc<std::sync::Mutex<(i32, i32)>>,
    /// 区域尺寸（物理像素）。拖动只改位置；尺寸恒定——移动区域时直接取用，
    /// 不再从磁盘会话元数据回读（录制中索引还在增长，回读既贵又可能失败）。
    pub(crate) region_w: u32,
    pub(crate) region_h: u32,
    target_fps: u32,
    pub(crate) annotation: blink_infra::gif::annotation::SharedAnnotation,
    pub(crate) annotation_updates: Arc<tokio::sync::Semaphore>,
    pub(crate) audio: blink_infra::gif::audio::SharedAudio,
}

/// 录制叠层模式的 overlay 输入态（0.28.23）。
#[derive(Debug, Default)]
pub(crate) struct OverlayInputState {
    /// 「操作桌面」模式：整窗鼠标穿透，指针落在 `hit_rects` 内临时取消穿透。
    desktop_mode: bool,
    /// 命中矩形（物理屏幕坐标 `[x0, y0, x1, y1]`，右下开区间）：工具栏与打开的弹层。
    hit_rects: Vec<[i32; 4]>,
    /// 当前已应用到窗口的穿透状态（避免重复 set）。
    pass_through: bool,
}

/// GIF 服务（managed state）。
pub struct GifService {
    start_lock: Mutex<()>,
    pub(crate) recording: Mutex<Option<ActiveRecording>>,
    /// overlay 录制叠层输入态（穿透/命中矩形）。
    pub(crate) overlay_input: Mutex<OverlayInputState>,
    pub(crate) timelines: Mutex<HashMap<String, Timeline>>,
    pub(crate) readers: Mutex<HashMap<String, Arc<SpoolReader>>>,
    /// 首建时间轴与草稿恢复必须整体发布，避免并发命令看到半初始化状态。
    pub(crate) timeline_init: Mutex<()>,
    /// 编辑器当前会话（复用窗口 + ready/pull）。
    pub(crate) editor_session: Mutex<Option<String>>,
    /// 编辑进度草稿状态（0.28.23；播放位置 + 防抖代次，见 gif_draft.rs）。
    pub(crate) drafts: Mutex<HashMap<String, crate::app::gif_draft::DraftState>>,
    /// 会话素材身份（创建时间, 索引帧数）——草稿保存/恢复共用。
    pub(crate) session_identity: Mutex<HashMap<String, (u64, u32)>>,
    pub(crate) audio_preview: Mutex<Option<blink_infra::platform::audio::recording::Session>>,
}

impl Default for GifService {
    fn default() -> Self {
        Self::new()
    }
}

impl GifService {
    /// 录制启动/收尾与截图、图片编辑共享的 overlay 所有权门。
    /// 调用者必须持有到窗口会话提交完成，不能只保护状态检查。
    pub(crate) fn lock_overlay_session(&self) -> std::sync::MutexGuard<'_, ()> {
        self.start_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn new() -> Self {
        Self {
            start_lock: Mutex::new(()),
            recording: Mutex::new(None),
            overlay_input: Mutex::new(OverlayInputState::default()),
            timelines: Mutex::new(HashMap::new()),
            readers: Mutex::new(HashMap::new()),
            timeline_init: Mutex::new(()),
            editor_session: Mutex::new(None),
            drafts: Mutex::new(HashMap::new()),
            session_identity: Mutex::new(HashMap::new()),
            audio_preview: Mutex::new(None),
        }
    }

    pub fn root() -> PathBuf {
        if blink_infra::runtime_mode::is_production() {
            blink_infra::utils::paths::app_data_dir().join("gif")
        } else {
            blink_infra::local_engine::runtime::runtimes_root().join("test-gif")
        }
    }

    /// 定期清理保留编辑会话；读写器租约保护在途录制/缩略图/导出。
    pub fn cleanup_expired(&self) {
        let recording = self
            .recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| r.session_id.clone());
        let current = self
            .editor_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut protected: Vec<String> = current.iter().cloned().collect();
        if let Some(recording) = recording {
            protected.push(recording);
        }
        let removed =
            infra_gif::spool::cleanup_expired_except(&Self::root(), SESSION_TTL, &protected);
        if removed > 0 {
            tracing::info!(removed, "录制素材 TTL 清理完成");
        }
    }
}

// ── 状态广播 ────────────────────────────────────────────────────────────────

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RecorderStatePayload {
    pub revision: u64,
    pub region: Option<infra_gif::CaptureRegion>,
    pub audio_options: Option<blink_infra::gif::audio::AudioOptions>,
    pub audio: blink_infra::gif::audio::AudioStatus,
    pub storage: blink_infra::gif::space::Snapshot,
    pub phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub countdown_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<u32>,
    pub paused: bool,
    pub target_fps: Option<u32>,
    pub actual_fps: Option<f64>,
}

fn emit_state(app: &AppHandle, payload: RecorderStatePayload) {
    let _ = app.emit(EventNames::GIF_RECORDER_STATE, payload);
}

/// 命令层暴露的状态快照（控制条 ready 拉取）。
pub fn snapshot_state_public(service: &Arc<GifService>) -> RecorderStatePayload {
    snapshot_state(service)
}

/// 调用方持有 overlay session gate：活跃录制只恢复控制，不初始化截图或停止采集。
pub(crate) fn restore_active_overlay(app: &AppHandle) -> Result<bool, String> {
    let service = app.state::<Arc<GifService>>();
    let session = service.recording.lock().unwrap_or_else(|e| e.into_inner())
        .as_ref().map(|r| r.session_id.clone());
    let Some(session_id) = session else { return Ok(false); };
    blink_infra::platform::window::restore_recording_overlay(app)?;
    if let Some(win) = overlay_window(app) {
        win.eval("window.__blinkRestoreRecording && window.__blinkRestoreRecording(true);")
            .map_err(|e| e.to_string())?;
    }
    tracing::info!(session_id, "已有录制占用截图窗口，恢复原控制栏");
    Ok(true)
}

/// 仅恢复 UI 时枚举显示器；高频录制 ticker 不做平台几何查询。
pub(crate) fn overlay_restore_meta() -> serde_json::Value {
    let (vx, vy, w, h) = virtual_screen_rect();
    serde_json::json!({"vx": vx, "vy": vy, "w": w, "h": h,
        "physicalDisplays": blink_infra::platform::screenshot::list_displays()})
}

fn snapshot_state(service: &GifService) -> RecorderStatePayload {
    let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
    let revision = SNAPSHOT_REVISION.fetch_add(1, Ordering::Relaxed);
    match rec.as_ref() {
        None => RecorderStatePayload {
            revision,
            region: None,
            audio_options: None,
            audio: Default::default(),
            storage: Default::default(),
            phase: "idle",
            session_id: None,
            countdown_ms: None,
            elapsed_ms: None,
            frames: None,
            paused: false,
            target_fps: None,
            actual_fps: None,
        },
        Some(r) => {
            // 倒计时是服务层语义（采集线程仍在 waiting Start）——对外统一
            // 报 "countdown"，控制条按该 phase 渲染大数字。
            let in_countdown = r
                .countdown_deadline
                .map(|d| d > std::time::Instant::now())
                .unwrap_or(false);
            let phase = if in_countdown {
                "countdown"
            } else {
                r.ctl.phase()
            };
            let countdown_ms = r.countdown_deadline.map(|d| {
                d.saturating_duration_since(std::time::Instant::now())
                    .as_millis() as u64
            });
            let (audio, audio_options) = {
                let mut gate = r.audio.lock().unwrap_or_else(|e| e.into_inner());
                (gate.snapshot(), gate.options.clone())
            };
            RecorderStatePayload {
                revision,
                region: {
                    let (x, y) = *r.region_pos.lock().unwrap_or_else(|e| e.into_inner());
                    Some(infra_gif::CaptureRegion { x, y, w: r.region_w, h: r.region_h })
                },
                audio_options: Some(audio_options),
                storage: r
                    .ctl
                    .space
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                audio,
                phase,
                session_id: Some(r.session_id.clone()),
                countdown_ms,
                elapsed_ms: Some(r.ctl.elapsed_ms()),
                frames: Some(r.ctl.captured_frames()),
                paused: phase == "paused",
                target_fps: Some(r.target_fps),
                actual_fps: (r.ctl.elapsed_ms() > 0)
                    .then(|| r.ctl.captured_frames() as f64 * 1000.0 / r.ctl.elapsed_ms() as f64),
            }
        }
    }
}

/// 启动状态 ticker（录制期间 250ms 广播；录制结束自动停）。
fn spawn_state_ticker(app: AppHandle, session_id: String) {
    tauri::async_runtime::spawn(async move {
        loop {
            let service = app.state::<Arc<GifService>>();
            let (phase, keep) = {
                let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
                match rec.as_ref().filter(|r| r.session_id == session_id) {
                    Some(r) => (
                        Some(r.ctl.phase()),
                        r.ctl.is_running() || r.ctl.phase() != "done",
                    ),
                    None => (None, false),
                }
            };
            if !keep && phase.is_none() {
                break;
            }
            if keep
                && app
                    .get_webview_window(OVERLAY_LABEL)
                    .is_none_or(|window| !window.is_visible().unwrap_or(false))
            {
                stop_lost_overlay(&app, Some(&session_id));
            }
            emit_state(&app, snapshot_state(&service));
            if !keep {
                break;
            }
            tokio::time::sleep(Duration::from_millis(STATE_TICK_MS)).await;
        }
    });
}

// ── 录制编排 ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum GifServiceError {
    #[error("已有录制进行中")]
    AlreadyRecording,
    #[error("没有进行中的录制")]
    NoActiveRecording,
    #[error("区域尺寸无效（至少 64×64，单帧不超过 64MiB）")]
    InvalidRegion,
    #[error("FPS 需在 5–30 之间")]
    InvalidFps,
    #[error("会话不存在: {0}")]
    SessionNotFound(String),
    #[allow(dead_code)] // 仅 #[cfg(not(windows))] 分支构造，Windows 构建下不产生
    #[error("当前平台不支持 GIF 采集")]
    UnsupportedPlatform,
    #[error("素材读写失败: {0}")]
    Spool(String),
}

impl From<blink_infra::gif::SpoolError> for GifServiceError {
    fn from(e: blink_infra::gif::SpoolError) -> Self {
        GifServiceError::Spool(e.to_string())
    }
}

pub type GifResult<T> = Result<T, GifServiceError>;

/// 开始录制：素材目录 + 流水线 + overlay 进入录制叠层模式 + 倒计时。
pub fn start_recording(
    app: &AppHandle,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    fps: u32,
    show_cursor: bool,
    audio_options: blink_infra::gif::audio::AudioOptions,
) -> GifResult<String> {
    if w < 64 || h < 64 || (w as u64).saturating_mul(h as u64).saturating_mul(4) > (64 << 20) {
        return Err(GifServiceError::InvalidRegion);
    }
    if ![10, 15, 24, 30, 60].contains(&fps) {
        return Err(GifServiceError::InvalidFps);
    }
    let service = app.state::<Arc<GifService>>();
    let _starting = service
        .start_lock
        .try_lock()
        .map_err(|_| GifServiceError::AlreadyRecording)?;
    {
        let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
        if rec.is_some() {
            return Err(GifServiceError::AlreadyRecording);
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        return Err(GifServiceError::UnsupportedPlatform);
    }
    #[cfg(target_os = "windows")]
    let region_pos: blink_infra::gif::SharedRegionPos = Arc::new(Mutex::new((x, y)));
    let annotation = Arc::new(Mutex::new(
        blink_infra::gif::annotation::AnnotationState::default(),
    ));
    #[cfg(target_os = "windows")]
    let source = blink_infra::gif::GdiFrameSource::with_shared_pos(
        x,
        y,
        w,
        h,
        show_cursor,
        Some(Arc::clone(&region_pos)),
    )
    .with_fps(fps)
    .with_annotation(annotation.clone());

    let root = GifService::root();
    let session_id = infra_gif::generate_session_id();
    let meta = SessionMeta {
        spool_version: infra_gif::SPOOL_VERSION,
        id: session_id.clone(),
        region: infra_gif::CaptureRegion { x, y, w, h },
        fps,
        show_cursor,
        created_at: now_ms(),
        state: "recording".into(),
        ts_ms: Vec::new(),
        last_dur_ms: (1000 / fps).max(1),
        spool_bytes: 0,
        stop_reason: None,
    };
    let writer = SpoolWriter::create(&root, meta).map_err(GifServiceError::from)?;

    // 准备态试音先释放，采集资源在 worker 内重新解析/核验，不继承旧默认设备。
    service
        .audio_preview
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let audio = blink_infra::gif::audio::shared(audio_options.clone());
    let source = RecordingSource {
        storage: root.join("sessions").join(&session_id),
        video: source,
        audio: blink_infra::platform::audio::recording::Session::new(
            audio.clone(),
            (audio_options.mode != blink_infra::gif::audio::AudioMode::Silent)
                .then(|| root.join("sessions").join(&session_id)),
        ),
        enabled: audio_options.mode != blink_infra::gif::audio::AudioMode::Silent,
    };

    #[cfg(target_os = "windows")]
    let (ctl, outcome_rx) = spawn_pipeline(
        source,
        writer,
        fps,
        infra_gif::RecordingBudget::for_fps(fps),
    );

    let cancel_flag = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "windows")]
    let region_for_rec = Arc::clone(&region_pos);
    #[cfg(not(target_os = "windows"))]
    let region_for_rec = Arc::new(Mutex::new((x, y)));

    // 0.28.23 统一浮栏：overlay 不隐藏，转为录制叠层模式（前端在 invoke 成功
    // 后切换 UI：隐藏静态截图层、工具栏切录制面、#annot-canvas 变实时涂画层）。
    // 后端只管窗口属性与看门狗豁免；静态截图位图此时可释放（底图不再需要）。

    // 倒计时基准在窗口就绪后才起算（COUNTDOWN_MS>0 时可见倒计时恒为
    // 完整时长；当前 0 = 点击即录，此行仅为恢复倒计时时保序）。
    *service.recording.lock().unwrap_or_else(|e| e.into_inner()) = Some(ActiveRecording {
        session_id: session_id.clone(),
        ctl: Arc::clone(&ctl),
        countdown_deadline: None,
        cancel_flag: Arc::clone(&cancel_flag),
        region_pos: region_for_rec,
        region_w: w,
        region_h: h,
        target_fps: fps,
        annotation,
        annotation_updates: Arc::new(tokio::sync::Semaphore::new(1)),
        audio,
    });

    enter_overlay_record_mode(app);
    blink_infra::platform::screenshot::end_session();
    emit_region_updated(app, x, y, w, h);

    // 前端消失/初始化失败时不能永久占住唯一录制槽。
    let app2 = app.clone();
    let ctl2 = Arc::clone(&ctl);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let service = app2.state::<Arc<GifService>>();
        let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = rec
            .as_ref()
            .filter(|r| Arc::ptr_eq(&r.ctl, &ctl2) && r.countdown_deadline.is_none())
        {
            tracing::warn!(session_id = %r.session_id, "录制叠层未就绪，取消未开始的录制");
            r.cancel_flag.store(true, Ordering::SeqCst);
            r.ctl.command(CaptureCommand::Stop);
        }
    });

    // outcome 等待任务：排空 finalize 后按停止方式分派
    let app3 = app.clone();
    let session_for_wait = session_id.clone();
    let ctl_for_wait = Arc::clone(&ctl);
    tauri::async_runtime::spawn_blocking(move || {
        let outcome = outcome_rx.recv().ok();
        finalize_after_stop(
            &app3,
            &session_for_wait,
            &ctl_for_wait,
            cancel_flag.load(Ordering::SeqCst),
            outcome,
        );
    });

    spawn_state_ticker(app.clone(), session_id.clone());
    emit_state(app, snapshot_state(&service));
    tracing::info!(session_id, region = ?(x, y, w, h), fps, "GIF 录制会话已创建（倒计时）");
    Ok(session_id)
}

/// 前端透明叠层完成绘制后才开采集，避免首帧捕获静态截图/准备栏。
pub fn overlay_ready(app: &AppHandle, session_id: &str) -> GifResult<()> {
    let service = app.state::<Arc<GifService>>();
    let ctl = {
        let mut rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
        let r = rec
            .as_mut()
            .filter(|r| r.session_id == session_id)
            .ok_or(GifServiceError::NoActiveRecording)?;
        if r.countdown_deadline.is_some() || r.cancel_flag.load(Ordering::SeqCst) {
            return Ok(());
        }
        r.countdown_deadline =
            Some(std::time::Instant::now() + Duration::from_millis(COUNTDOWN_MS));
        Arc::clone(&r.ctl)
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(COUNTDOWN_MS)).await;
        let service = app.state::<Arc<GifService>>();
        let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
        if rec
            .as_ref()
            .is_some_and(|r| Arc::ptr_eq(&r.ctl, &ctl) && !r.cancel_flag.load(Ordering::SeqCst))
        {
            ctl.command(CaptureCommand::Start);
        }
        drop(rec);
        emit_state(&app, snapshot_state(&service));
    });
    Ok(())
}

/// 暂停 / 恢复 / 停止 / 取消（指令下发；停止结果由 finalize_after_stop 分派）。
pub fn command_recording(
    app: &AppHandle,
    cmd: CaptureCommand,
    cancel: bool,
    session_id: Option<&str>,
) -> GifResult<()> {
    let service = app.state::<Arc<GifService>>();
    let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
    let Some(r) = rec
        .as_ref()
        .filter(|r| session_id.is_none_or(|id| id == r.session_id))
    else {
        return Err(GifServiceError::NoActiveRecording);
    };
    if matches!(cmd, CaptureCommand::Stop) && cancel {
        r.cancel_flag.store(true, Ordering::SeqCst);
    }
    r.ctl.command(cmd);
    Ok(())
}

/// 非预期关窗/隐藏按停止保留素材处理，不能留下一条失去控制面的后台录制。
/// 正常失焦不走这里；用户操作桌面时透明控制层仍保持可见。
pub(crate) fn stop_lost_overlay(app: &AppHandle, session_id: Option<&str>) {
    let Some(service) = app.try_state::<Arc<GifService>>() else {
        return;
    };
    let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(recording) = rec
        .as_ref()
        .filter(|r| session_id.is_none_or(|id| id == r.session_id))
        && !matches!(recording.ctl.phase(), "stopping" | "done")
    {
        tracing::warn!(session_id = %recording.session_id, "录制控制窗口丢失，停止并保留已录素材");
        recording.ctl.command(CaptureCommand::Stop);
    }
}

/// 停止后的分派：cancel 删素材；正常停止构建 Timeline 并打开编辑器。
fn finalize_after_stop(
    app: &AppHandle,
    session_id: &str,
    ctl: &Arc<CaptureCtl>,
    cancelled: bool,
    outcome: Option<CaptureOutcome>,
) {
    let service = app.state::<Arc<GifService>>();
    let _finalizing = service.start_lock.lock().unwrap_or_else(|e| e.into_inner());
    // 清活跃录制（同一会话才清）
    {
        let mut rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(rec.as_ref(), Some(r) if Arc::ptr_eq(&r.ctl, ctl)) {
            *rec = None;
        }
    }
    exit_overlay_record_mode(app);
    // 携带结束会话身份，前端仅清理所属叠层/定时器，不接受跨会话 idle。
    let mut finished = snapshot_state(&service);
    finished.session_id = Some(session_id.to_string());
    emit_state(app, finished);
    let root = GifService::root();
    if cancelled {
        if let Err(error) = infra_gif::spool::discard_session(&root, session_id) {
            tracing::error!(session_id, %error, "录制已停止，但临时素材删除失败，可从最近录制重试删除");
            if !open_editor(app, session_id) {
                open_editor_entry(app);
            }
            let current = service
                .editor_session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let _ = app.emit_to(
                EDITOR_LABEL,
                EventNames::GIF_EDITOR_SESSION,
                serde_json::json!({"sessionId":current,"discardFailed":true}),
            );
            return;
        }
        service
            .timelines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        tracing::info!(session_id, "GIF 录制已取消，素材已丢弃");
        return;
    }

    // 正常停止：读取定稿素材，构建时间轴
    match SpoolReader::open(&root, session_id) {
        Ok(reader) => {
            // 与历史会话共用初始化：同时记录素材身份，确保新录制也能写草稿。
            if let Err(error) =
                crate::app::gif_draft::ensure_timeline(&service, &reader, session_id)
            {
                tracing::error!(session_id,%error,"录制时间轴初始化失败，素材保留");
                return;
            }
            let frames = reader.frame_count();
            tracing::info!(
                session_id,
                frames,
                stop_reason = ?outcome.as_ref().map(|o| o.stop_reason),
                duplicates = outcome.as_ref().map(|o| o.duplicates_merged).unwrap_or(0),
                "GIF 录制完成，素材已定稿"
            );
            if !open_editor(app, session_id) {
                crate::app::gif_draft::release_if_saved(&service, session_id);
                // 录制完成不能静默取消另一段素材的导出；新素材保留在最近录制。
                open_editor_entry(app);
                let current = service
                    .editor_session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let _ = app.emit_to(
                    EDITOR_LABEL,
                    EventNames::GIF_EDITOR_SESSION,
                    serde_json::json!({"sessionId": current, "recordingSaved": true}),
                );
            }
        }
        Err(e) => {
            tracing::error!(session_id, error = %e, "GIF 素材定稿读取失败");
        }
    }
}

// ── 录制叠层模式（0.28.23 §5.7.1/§5.7.2：overlay 作为统一浮栏与涂画层宿主）──

fn overlay_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window(OVERLAY_LABEL)
}

/// 进入录制叠层模式：看门狗豁免 + 不可聚焦（点击浮栏不抢被录应用焦点；
/// 文字输入期间前端显式申请聚焦）+ 初始非穿透（前端随后按默认「操作桌面」
/// 模式上报命中矩形并切换）。启动命中轮询任务。
fn enter_overlay_record_mode(app: &AppHandle) {
    crate::infra::platform::window::set_overlay_record_active(true);
    // 启动前的迟到失焦可能已隐藏宿主；先找回窗口，再等前端 ready 开始采集。
    if let Err(error) = blink_infra::platform::window::restore_recording_overlay(app) {
        tracing::warn!(%error, "录制宿主恢复失败，等待前端就绪超时兜底");
    }
    {
        let service = app.state::<Arc<GifService>>();
        *service
            .overlay_input
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = OverlayInputState::default();
    }
    if let Some(win) = overlay_window(app) {
        let _ = win.set_ignore_cursor_events(false);
        if let Err(e) = win.set_focusable(false) {
            tracing::warn!(error = %e, "overlay 设为不可聚焦失败（录制继续，点击浮栏可能抢焦点）");
        }
    }
    spawn_overlay_hit_poller(app.clone());
}

/// 退出录制叠层模式：复位窗口属性 → 清标志 → 走既有隐藏链（含 cloak 与会话清理）。
fn exit_overlay_record_mode(app: &AppHandle) {
    if let Some(win) = overlay_window(app) {
        let _ = win.set_ignore_cursor_events(false);
        let _ = win.set_focusable(true);
    }
    {
        let service = app.state::<Arc<GifService>>();
        *service
            .overlay_input
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = OverlayInputState::default();
    }
    crate::infra::platform::window::set_overlay_record_active(false);
    crate::infra::platform::window::hide_screenshot_overlay(app);
}

/// 前端上报 overlay 输入模式与命中矩形（物理屏幕坐标）。
/// - `draw`：涂画模式，整窗接收指针；
/// - `desktop`：操作桌面模式，整窗穿透，指针在 `rects` 内临时取消穿透。
pub fn set_overlay_input(
    app: &AppHandle,
    desktop_mode: bool,
    rects: Vec<[i32; 4]>,
) -> GifResult<()> {
    let service = app.state::<Arc<GifService>>();
    if service
        .recording
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_none()
    {
        return Err(GifServiceError::NoActiveRecording);
    }
    let want_pass = {
        let mut st = service
            .overlay_input
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        st.desktop_mode = desktop_mode;
        st.hit_rects = rects;
        // 涂画模式立即取消穿透；桌面模式按当前指针位置先算一次，
        // 避免切换瞬间的轮询空档
        if desktop_mode {
            let (cx, cy) = cursor_pos();
            !st.hit_rects.iter().any(|r| rect_contains(r, cx, cy))
        } else {
            false
        }
    };
    apply_pass_through(app, &service, want_pass);
    Ok(())
}

/// 仅更新命中矩形（工具栏拖动/弹层开合后的周期刷新）。
pub fn set_overlay_hit_rects(app: &AppHandle, rects: Vec<[i32; 4]>) -> GifResult<()> {
    let service = app.state::<Arc<GifService>>();
    if service
        .recording
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_none()
    {
        return Err(GifServiceError::NoActiveRecording);
    }
    service
        .overlay_input
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .hit_rects = rects;
    Ok(())
}

/// 文字输入期间 overlay 需要键盘焦点（前端在文字编辑开始/结束时申请/归还）。
pub fn set_overlay_focusable(app: &AppHandle, focusable: bool) -> GifResult<()> {
    let service = app.state::<Arc<GifService>>();
    if service
        .recording
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_none()
    {
        return Err(GifServiceError::NoActiveRecording);
    }
    if let Some(win) = overlay_window(app) {
        let _ = win.set_focusable(focusable);
        if focusable {
            let _ = win.set_focus();
        }
    }
    Ok(())
}

fn apply_pass_through(app: &AppHandle, service: &GifService, want: bool) {
    let changed = {
        let mut st = service
            .overlay_input
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if st.pass_through == want {
            false
        } else {
            st.pass_through = want;
            true
        }
    };
    if changed && let Some(win) = overlay_window(app) {
        let _ = win.set_ignore_cursor_events(want);
    }
}

fn rect_contains(r: &[i32; 4], x: i32, y: i32) -> bool {
    x >= r[0] && x < r[2] && y >= r[1] && y < r[3]
}

/// 当前指针物理屏幕坐标。
fn cursor_pos() -> (i32, i32) {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut pt = POINT::default();
        // SAFETY: 纯查询，输出参数为本地栈变量
        if unsafe { GetCursorPos(&mut pt) }.is_ok() {
            return (pt.x, pt.y);
        }
        (i32::MIN, i32::MIN)
    }
    #[cfg(not(target_os = "windows"))]
    (i32::MIN, i32::MIN)
}

/// 命中轮询：录制期间每 16ms 读指针；「操作桌面」模式下指针在命中矩形内
/// 取消穿透、否则恢复穿透。录制结束自动退出（按活跃录制判活，单实例）。
fn spawn_overlay_hit_poller(app: AppHandle) {
    let session_id = app
        .state::<Arc<GifService>>()
        .recording
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|r| r.session_id.clone());
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(OVERLAY_HIT_POLL_MS)).await;
            let service = app.state::<Arc<GifService>>();
            let current = service
                .recording
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|r| r.session_id.clone());
            if current.is_none() || current != session_id {
                break;
            }
            let want = {
                let st = service
                    .overlay_input
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !st.desktop_mode {
                    false
                } else {
                    let (cx, cy) = cursor_pos();
                    !st.hit_rects.iter().any(|r| rect_contains(r, cx, cy))
                }
            };
            apply_pass_through(&app, &service, want);
        }
    });
}

/// 录制区域变化的统一广播（overlay 前端据此同步范围框与涂画层位置）。
fn emit_region_updated(app: &AppHandle, x: i32, y: i32, w: u32, h: u32) {
    let (vx, vy, vw, vh) = virtual_screen_rect();
    let session_id = app
        .state::<Arc<GifService>>()
        .recording
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|r| r.session_id.clone());
    let _ = app.emit(
        EventNames::GIF_REGION_UPDATED,
        serde_json::json!({"sessionId": session_id, "x": x, "y": y, "w": w, "h": h, "vx": vx, "vy": vy, "vw": vw, "vh": vh}),
    );
}

/// 移动录制区域（浮栏「移动区域」按钮拖动上报目标坐标）：热更新采集位置。
///
/// 坐标钳制到虚拟屏幕矩形——BitBlt 越界部分是黑边。尺寸取 `ActiveRecording`
/// 内存值（录制中索引仍在增长，不回读元数据）。范围框/涂画层由 overlay
/// 前端按 `GIF_REGION_UPDATED` 对账同步（拖动端就地钳制为主）。
pub fn move_recording_region(
    app: &AppHandle,
    x: i32,
    y: i32,
    session_id: Option<&str>,
) -> GifResult<()> {
    let service = app.state::<Arc<GifService>>();
    let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
    let Some(r) = rec
        .as_ref()
        .filter(|r| session_id.is_none_or(|id| id == r.session_id))
    else {
        return Err(GifServiceError::NoActiveRecording);
    };
    let (w, h) = (r.region_w as i32, r.region_h as i32);
    let (vx, vy, vw, vh) = virtual_screen_rect();
    let x = x.clamp(vx, (vx + vw - w).max(vx));
    let y = y.clamp(vy, (vy + vh - h).max(vy));
    if let Ok(mut g) = r.region_pos.lock() {
        *g = (x, y);
    }
    let (w, h) = (r.region_w, r.region_h);
    drop(rec);
    emit_region_updated(app, x, y, w, h);
    Ok(())
}

/// 打开编辑器窗口（复用 + 会话事件；初始会话由页面 ready 后拉取）。
/// 换会话前 flush 草稿；在途导出拒绝切换并返回 false，所有入口统一保护。
pub fn open_editor(app: &AppHandle, session_id: &str) -> bool {
    use blink_infra::platform::window::get_or_create_window;
    {
        let service = app.state::<Arc<GifService>>();
        let mut current = service
            .editor_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = current.as_deref().filter(|id| *id != session_id) {
            if crate::app::media_service::shared().has_active_job(previous) {
                tracing::info!(session_id, "编辑器保留在途导出会话，新素材可从最近录制打开");
                return false;
            }
            crate::app::media_service::shared().cancel_estimate(previous);
            crate::app::gif_draft::flush_draft(app, previous);
            crate::app::gif_draft::release_if_saved(&service, previous);
        }
        *current = Some(session_id.to_string());
    }
    let built = get_or_create_window(app, EDITOR_LABEL, || {
        WebviewWindowBuilder::new(app, EDITOR_LABEL, WebviewUrl::App("gif-editor.html".into()))
            // 0.28.23 改名「录制编辑器」；窗口 label / IPC 保持 gif-* 不迁移，
            // 前端启动后按语言经 set-title 覆盖为双语标题。
            .title("Blink 录制编辑器")
            .inner_size(980.0, 720.0)
            .min_inner_size(760.0, 540.0)
            .visible(false)
            .build()
    });
    match built {
        Ok((win, created)) => {
            if created {
                bind_editor_lifecycle(app, &win);
            }
            let _ = win.show();
            let _ = win.set_focus();
            let _ = app.emit_to(
                EDITOR_LABEL,
                EventNames::GIF_EDITOR_SESSION,
                serde_json::json!({ "sessionId": session_id }),
            );
        }
        Err(e) => tracing::error!(error = %e, "GIF 编辑器窗口打开失败"),
    }
    true
}

/// 主窗口/功能目录入口（0.28.23 §5.7.4）：有编辑会话则原地激活保留工作，
/// 无会话打开编辑器首页（前端进入「最近录制」列表）。
pub fn open_editor_entry(app: &AppHandle) {
    let current = {
        let service = app.state::<Arc<GifService>>();
        service
            .editor_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    };
    match current {
        Some(session_id) => {
            if let Some(win) = app.get_webview_window(EDITOR_LABEL) {
                let _ = win.show();
                let _ = win.set_focus();
                let _ = app.emit_to(
                    EDITOR_LABEL,
                    EventNames::GIF_EDITOR_SESSION,
                    serde_json::json!({ "sessionId": session_id }),
                );
                return;
            }
            // 窗口丢失但会话仍在：走 open_editor 重建窗口
            open_editor(app, &session_id);
        }
        None => open_editor_home(app),
    }
}

/// 编辑器首页（无会话）：复用窗口 + 发空会话事件（前端显示最近录制）。
pub fn open_editor_home(app: &AppHandle) {
    use blink_infra::platform::window::get_or_create_window;
    {
        let service = app.state::<Arc<GifService>>();
        let mut current = service
            .editor_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = current.take() {
            crate::app::gif_draft::flush_draft(app, &previous);
            crate::app::gif_draft::release_if_saved(&service, &previous);
            crate::app::media_service::shared().cancel(Some(&previous), None);
        }
    }
    let built = get_or_create_window(app, EDITOR_LABEL, || {
        WebviewWindowBuilder::new(app, EDITOR_LABEL, WebviewUrl::App("gif-editor.html".into()))
            .title("Blink 录制编辑器")
            .inner_size(980.0, 720.0)
            .min_inner_size(760.0, 540.0)
            .visible(false)
            .build()
    });
    match built {
        Ok((win, created)) => {
            if created {
                bind_editor_lifecycle(app, &win);
            }
            let _ = win.show();
            let _ = win.set_focus();
            let _ = app.emit_to(
                EDITOR_LABEL,
                EventNames::GIF_EDITOR_SESSION,
                serde_json::json!({ "sessionId": null }),
            );
        }
        Err(e) => tracing::error!(error = %e, "GIF 编辑器窗口打开失败"),
    }
}

/// 所有窗口创建入口共用收尾钩子，首页先创建的窗口也必须 flush/取消导出。
fn bind_editor_lifecycle(app: &AppHandle, win: &tauri::WebviewWindow) {
    let service = app.state::<Arc<GifService>>().inner().clone();
    let app = app.clone();
    win.on_window_event(move |event| {
        if matches!(
            event,
            tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed
        ) {
            let id = service
                .editor_session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(id) = id {
                crate::app::gif_draft::flush_draft(&app, &id);
                crate::app::gif_draft::release_if_saved(&service, &id);
                crate::app::media_service::shared().cancel(Some(&id), None);
            }
        }
    });
}

// ── 简单屏幕几何（Win32 直查；避免引入窗口管理依赖） ──────────────────────────

/// 虚拟屏幕矩形（物理像素，多屏时左上角可为负坐标）。
fn virtual_screen_rect() -> (i32, i32, i32, i32) {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };
        // SAFETY: GetSystemMetrics 为纯查询，无副作用
        let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        (x, y, w, h)
    }
    #[cfg(not(target_os = "windows"))]
    (0, 0, 4096, 4096)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── 编辑/查询/导出辅助 ──────────────────────────────────────────────────────

pub fn timeline_view(tl: &Timeline) -> serde_json::Value {
    serde_json::json!({
        "frameCount":tl.frame_count(), "effectiveCount":tl.effective_count(),
        "revision":tl.revision(), "trimStart":tl.trim_range().0, "trimEnd":tl.trim_range().1,
        "speed":tl.speed(), "loopCount":tl.loop_count(), "totalMs":tl.effective_total_ms(),
        "canUndo":tl.can_undo(), "canRedo":tl.can_redo(), "mix":tl.mix(),
    })
}

/// 音画生命周期由采集线程串行驱动；打开两路成功后才启动共同单调时钟。
struct RecordingSource {
    storage: PathBuf,
    video: blink_infra::gif::GdiFrameSource,
    audio: blink_infra::platform::audio::recording::Session,
    enabled: bool,
}
impl blink_infra::gif::capture::FrameSource for RecordingSource {
    fn storage(&self) -> Option<PathBuf> {
        Some(self.storage.clone())
    }
    fn capture(&mut self) -> Result<Vec<u8>, String> {
        if let Some(error) = self.audio.fault() {
            return Err(error);
        }
        self.video.capture().inspect_err(|_| {
            self.audio
                .shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .fail("画面采集失效，全部来源已暂停，请手动继续或停止保留素材");
        })
    }
    fn frame_bytes(&self) -> usize {
        self.video.frame_bytes()
    }
    fn timestamp_ms(&self, fallback: u64) -> u64 {
        self.video.timestamp_ms(fallback)
    }
    fn resume(&mut self, offset: u64) -> Result<std::time::Instant, String> {
        let now = if self.enabled {
            self.audio.resume(offset)?
        } else {
            std::time::Instant::now()
        };
        self.video.resume_at(now, offset);
        Ok(now)
    }
    fn pause(&mut self, end: u64) {
        if self.enabled {
            self.audio.pause(end);
        }
        self.video.pause(end);
    }
    fn recoverable(&self) -> bool {
        self.enabled
    }
}

/// 把 domain 导出计划转换为 infra 编码计划（分层边界桥接）。
pub fn to_encode_plan(plan: &blink_domain::gif::ExportPlan) -> EncodePlan {
    EncodePlan {
        src_w: plan.src_w,
        src_h: plan.src_h,
        loop_count: plan.loop_count,
        frames: plan.frames.map(|f| EncodeFrame {
            src_idx: f.src_idx,
            delay_cs: f.delay_cs,
        }),
    }
}

#[cfg(test)]
mod media_storage_tests {
    #[test]
    #[ignore = "真实桌面 30 分钟录制/剪辑/导出；隔离临时目录，显式运行"]
    fn native_long_recording_edit_export() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();
        use blink_domain::gif::{EditCommand, Timeline};
        use blink_infra::gif::*;
        let root = tempfile::tempdir().unwrap();
        let display = blink_infra::platform::screenshot::list_displays().remove(0);
        let duration = std::env::var("BLINK_RECORDING_LONG_TEST_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1_800_000);
        let meta = SessionMeta {
            spool_version: SPOOL_VERSION,
            id: "native-long".into(),
            region: CaptureRegion {
                x: display.x,
                y: display.y,
                w: 64,
                h: 64,
            },
            fps: 10,
            show_cursor: false,
            created_at: 0,
            state: "recording".into(),
            ts_ms: vec![],
            last_dur_ms: 100,
            spool_bytes: 0,
            stop_reason: None,
        };
        let writer = SpoolWriter::create(root.path(), meta).unwrap();
        let mut source = GdiFrameSource::new(display.x, display.y, 64, 64, false);
        use blink_infra::gif::capture::FrameSource;
        source.capture().expect("原生桌面捕获前置核验");
        let (ctl, rx) = spawn_pipeline(
            source,
            writer,
            10,
            RecordingBudget {
                max_duration_ms: duration,
                max_frames: u32::MAX,
                max_spool_bytes: 512 << 20,
                queue_frame_cap: 8,
            },
        );
        ctl.command(CaptureCommand::Start);
        let began = std::time::Instant::now();
        let outcome = loop {
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(outcome) => break outcome,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    tracing::info!(elapsed_ms=ctl.elapsed_ms(),captured=ctl.captured_frames(),
                        peak_process_bytes=?blink_infra::platform::process::peak_working_set_bytes(std::process::id()),"native long recording");
                    assert!(
                        (began.elapsed().as_millis() as u64) < duration + 60_000,
                        "长会话超时"
                    );
                }
                Err(error) => panic!("采集线程异常: {error}"),
            }
        };
        assert_eq!(outcome.stop_reason, StopReason::BudgetExhausted);
        let reader = SpoolReader::open(root.path(), "native-long").unwrap();
        let mut timeline = Timeline::from_timings(reader.timings().unwrap());
        assert!(timeline.effective_total_ms() >= duration);
        timeline
            .apply(&EditCommand::SetSpeed { factor: 1.5 })
            .unwrap();
        let total = timeline.effective_total_ms();
        let located = timeline.window(None, Some(total - 1), None);
        assert!(!located["frames"].as_array().unwrap().is_empty());
        assert!(timeline.undo());
        assert!(timeline.redo());
        let plan = super::to_encode_plan(&timeline.export_plan(64, 64));
        let out = root.path().join("long.gif");
        let stats = export_gif(
            &reader,
            &plan,
            &ExportOptions::default(),
            &out,
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
        assert!(stats.bytes > 0);
        tracing::info!(source_ms=duration,edited_ms=total,retained=outcome.frames,exported=stats.written_frames,
            source_bytes=outcome.spool_bytes,output_bytes=stats.bytes,
            peak_process_bytes=?blink_infra::platform::process::peak_working_set_bytes(std::process::id()),"native long recording done");
    }

    #[test]
    #[ignore = "重放已保留的真实 30 分钟夹具；不能代替真实采集，需 BLINK_RECORDING_REPLAY_ROOT"]
    fn long_recording_fixture_replay_with_current_editor() {
        use blink_domain::gif::{EditCommand, Timeline, timeline::RangeOperation};
        use blink_infra::gif::*;
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();
        let root = std::path::PathBuf::from(
            std::env::var_os("BLINK_RECORDING_REPLAY_ROOT").expect("需要真实长素材夹具目录"),
        );
        let reader = SpoolReader::open(&root, "native-long").unwrap();
        let mut timeline = Timeline::from_timings(reader.timings().unwrap());
        let source_ms = timeline.effective_total_ms();
        assert!((1_800_000..1_801_000).contains(&source_ms));
        let count = timeline.effective_count();
        let at = reader.frame_count() / 2;
        timeline
            .apply(&EditCommand::EditRanges {
                operation: RangeOperation::Delete,
                ranges: vec![(at, at + 1)],
                ms: 0,
            })
            .unwrap();
        assert_eq!(timeline.effective_count(), count - 2);
        assert!(timeline.undo());
        assert!(timeline.redo());
        assert!(timeline.undo());
        timeline
            .apply(&EditCommand::SetSpeed { factor: 1.5 })
            .unwrap();
        let edited_ms = timeline.effective_total_ms();
        assert_eq!(edited_ms, (source_ms as f64 / 1.5).round() as u64);
        let window = timeline.window(None, Some(edited_ms - 1), None);
        assert_eq!(window["frames"][0]["srcIdx"], reader.frame_count() - 1);
        let draft = timeline.to_draft(edited_ms - 1);
        let mut restored = Timeline::from_timings(reader.timings().unwrap());
        restored.restore_draft(draft).unwrap();
        assert_eq!(restored.effective_total_ms(), edited_ms);
        let output = tempfile::tempdir().unwrap();
        let plan = super::to_encode_plan(&restored.export_plan(64, 64));
        let stats = export_gif(
            &reader,
            &plan,
            &ExportOptions::default(),
            &output.path().join("replayed.gif"),
            &std::sync::atomic::AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
        tracing::info!(
            source_ms,
            edited_ms,
            frames = reader.frame_count(),
            output_bytes = stats.bytes,
            "current editor replayed native long recording"
        );
    }

    #[test]
    fn gif_test_storage_uses_the_isolated_runtime_root() {
        assert!(!blink_infra::runtime_mode::is_production());
        assert_eq!(
            super::GifService::root(),
            blink_infra::local_engine::runtime::runtimes_root().join("test-gif")
        );
    }
}
