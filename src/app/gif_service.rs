//! GIF 录制服务编排（0.28.7，bin 侧）。
//!
//! 职责：
//! - **录制会话**：`start`（素材目录 + 采集流水线 + 控制条窗口 + 倒计时）、
//!   `pause/resume`、`stop`（排空 → finalize → 构建 Timeline → 打开编辑器）、
//!   `cancel`（丢弃素材）；同一时刻仅一个活跃录制。
//! - **状态广播**：`blink://gif-recorder-state`（250ms 节流 ticker + 阶段变化
//!   即时推送），控制条窗口消费。
//! - **编辑会话**：`session_id → Timeline`（domain 编辑模型）内存态；素材/元数据
//!   真源在磁盘（infra spool），崩溃后重开编辑器可重建。
//! - **导出**：`spawn_blocking` 流式编码 + 进度事件 + 取消标志；导出用冻结的
//!   revision 快照，编码期间编辑不影响本次导出。
//! - **素材 TTL**：启动清理 72h 前的会话目录（含崩溃残留）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use blink_domain::gif::Timeline;
use blink_infra::event_names::EventNames;
use blink_infra::gif::{
    self as infra_gif, spawn_pipeline, CaptureCommand, CaptureCtl, CaptureOutcome,
    EncodeFrame, EncodePlan, SessionMeta, SpoolReader, SpoolWriter,
};

/// 录制倒计时（ms）。0 = 无倒计时、点「开始录制」立即采集（2026-10-27
/// 用户定案；保留倒计时机制，改回 >0 即恢复——控制条已适配两种形态）。
const COUNTDOWN_MS: u64 = 0;
/// 状态广播周期。
const STATE_TICK_MS: u64 = 250;
/// 素材 TTL。
const SESSION_TTL: Duration = Duration::from_secs(72 * 3600);
/// 控制条窗口 label。
const CONTROL_LABEL: &str = "gif-control";
/// 录制范围指示框窗口 label。
const REGION_LABEL: &str = "gif-region";
/// 录制区域拖动柄窗口 label。
const REGION_HANDLE_LABEL: &str = "gif-region-handle";
/// 范围框边框宽度（px，物理像素）；窗口比录制区域四周外扩该值。
const REGION_BORDER: i32 = 3;
/// 编辑器窗口 label。
const EDITOR_LABEL: &str = "gif-editor";

pub(crate) struct ActiveRecording {
    pub(crate) session_id: String,
    ctl: Arc<CaptureCtl>,
    /// 倒计时剩余（phase=countdown 时 Some）。
    countdown_deadline: Option<std::time::Instant>,
    /// 取消标志：true 时停止后丢弃素材。
    cancel_flag: Arc<AtomicBool>,
    /// 共享采集区域位置（拖动柄热更新；与 GdiFrameSource 同一 Arc）。
    region_pos: std::sync::Arc<std::sync::Mutex<(i32, i32)>>,
    /// 区域尺寸（物理像素）。拖动只改位置；尺寸恒定——移动区域时直接取用，
    /// 不再从磁盘会话元数据回读（录制中索引还在增长，回读既贵又可能失败）。
    region_w: u32,
    region_h: u32,
}

/// GIF 服务（managed state）。
pub struct GifService {
    pub(crate) recording: Mutex<Option<ActiveRecording>>,
    pub(crate) timelines: Mutex<HashMap<String, Timeline>>,
    pub(crate) exports: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// 编辑器当前会话（复用窗口 + ready/pull）。
    pub(crate) editor_session: Mutex<Option<String>>,
}

impl Default for GifService {
    fn default() -> Self {
        Self::new()
    }
}

impl GifService {
    pub fn new() -> Self {
        Self {
            recording: Mutex::new(None),
            timelines: Mutex::new(HashMap::new()),
            exports: Mutex::new(HashMap::new()),
            editor_session: Mutex::new(None),
        }
    }

    pub fn root() -> PathBuf {
        blink_infra::utils::paths::app_data_dir().join("gif")
    }

    /// 启动清理：过期素材回收（main.rs spawn_blocking 调用）。
    pub fn startup_cleanup() {
        let removed = infra_gif::cleanup_expired(&Self::root(), SESSION_TTL);
        if removed > 0 {
            tracing::info!(removed, "GIF 素材 TTL 清理完成");
        }
    }
}

// ── 状态广播 ────────────────────────────────────────────────────────────────

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RecorderStatePayload {
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
}

fn emit_state(app: &AppHandle, payload: RecorderStatePayload) {
    let _ = app.emit(EventNames::GIF_RECORDER_STATE, payload);
}

/// 命令层暴露的状态快照（控制条 ready 拉取）。
pub fn snapshot_state_public(service: &Arc<GifService>) -> RecorderStatePayload {
    snapshot_state(service)
}

fn snapshot_state(service: &GifService) -> RecorderStatePayload {
    let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
    match rec.as_ref() {
        None => RecorderStatePayload {
            phase: "idle",
            session_id: None,
            countdown_ms: None,
            elapsed_ms: None,
            frames: None,
            paused: false,
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
            let countdown_ms = r
                .countdown_deadline
                .map(|d| d.saturating_duration_since(std::time::Instant::now()).as_millis() as u64);
            RecorderStatePayload {
                phase,
                session_id: Some(r.session_id.clone()),
                countdown_ms,
                elapsed_ms: Some(r.ctl.elapsed_ms()),
                frames: Some(r.ctl.captured_frames()),
                paused: phase == "paused",
            }
        }
    }
}

/// 启动状态 ticker（录制期间 250ms 广播；录制结束自动停）。
fn spawn_state_ticker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let service = app.state::<Arc<GifService>>();
            let (phase, keep) = {
                let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
                match rec.as_ref() {
                    Some(r) => (Some(r.ctl.phase()), r.ctl.is_running() || r.ctl.phase() != "done"),
                    None => (None, false),
                }
            };
            if !keep && phase.is_none() {
                break;
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
    #[error("区域尺寸超限（宽 64–2560 / 高 64–1600）")]
    InvalidRegion,
    #[error("FPS 需在 5–30 之间")]
    InvalidFps,
    #[error("会话不存在: {0}")]
    SessionNotFound(String),
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

/// 开始录制：素材目录 + 流水线 + 控制条 + 倒计时。
pub fn start_recording(
    app: &AppHandle,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    fps: u32,
    show_cursor: bool,
    control_x: Option<i32>,
    control_y: Option<i32>,
) -> GifResult<String> {
    if !(64..=2560).contains(&w) || !(64..=1600).contains(&h) {
        return Err(GifServiceError::InvalidRegion);
    }
    if !(5..=30).contains(&fps) {
        return Err(GifServiceError::InvalidFps);
    }
    let service = app.state::<Arc<GifService>>();
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
    #[cfg(target_os = "windows")]
    let source = blink_infra::gif::GdiFrameSource::with_shared_pos(
        x, y, w, h, show_cursor, Some(Arc::clone(&region_pos)),
    );

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

    #[cfg(target_os = "windows")]
    let (ctl, outcome_rx) = spawn_pipeline(
        source,
        writer,
        fps,
        // 帧上限按 fps 折算（fps × 120s），否则 30fps 只有 60s 有效录制
        infra_gif::RecordingBudget::for_fps(fps),
    );

    let cancel_flag = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "windows")]
    let region_for_rec = Arc::clone(&region_pos);
    #[cfg(not(target_os = "windows"))]
    let region_for_rec = Arc::new(Mutex::new((x, y)));

    // 控制条窗口（区域下方，避让选区——控制条不入成品）；范围指示框
    // （边框落在区域外，鼠标穿透）；窗口就绪后再收截图 overlay（时序确定：
    // overlay 的灰罩不会盖住倒计时，录制画面干净）。
    // 窗口预热（准备模式进入时创建）后这里全部为复用路径——set/show
    // 毫秒级，「点开始到边框出现」无冷启动间隔。
    show_control_window(app, x, y, w, h, control_x, control_y);
    show_region_frame(app, x, y, w, h);
    show_region_handle(app, x, y, w);
    emit_region_updated(app, x, y, w, h);
    crate::infra::platform::window::hide_screenshot_overlay(app);

    // 倒计时基准在窗口就绪后才起算（COUNTDOWN_MS>0 时可见倒计时恒为
    // 完整时长；当前 0 = 点击即录，此行仅为恢复倒计时时保序）。
    *service.recording.lock().unwrap_or_else(|e| e.into_inner()) = Some(ActiveRecording {
        session_id: session_id.clone(),
        ctl: Arc::clone(&ctl),
        countdown_deadline: Some(std::time::Instant::now() + Duration::from_millis(COUNTDOWN_MS)),
        cancel_flag: Arc::clone(&cancel_flag),
        region_pos: region_for_rec,
        region_w: w,
        region_h: h,
    });

    // 倒计时 → Start
    let app2 = app.clone();
    let ctl2 = Arc::clone(&ctl);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(COUNTDOWN_MS)).await;
        ctl2.command(CaptureCommand::Start);
        // 倒计时结束可能已被取消
        let service = app2.state::<Arc<GifService>>();
        let still = {
            let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
            matches!(rec.as_ref(), Some(r) if Arc::ptr_eq(&r.ctl, &ctl2))
        };
        if still {
            emit_state(&app2, snapshot_state(&service));
        }
    });

    // outcome 等待任务：排空 finalize 后按停止方式分派
    let app3 = app.clone();
    let session_for_wait = session_id.clone();
    let ctl_for_wait = Arc::clone(&ctl);
    tauri::async_runtime::spawn_blocking(move || {
        let outcome = outcome_rx.recv().ok();
        finalize_after_stop(&app3, &session_for_wait, &ctl_for_wait, cancel_flag.load(Ordering::SeqCst), outcome);
    });

    spawn_state_ticker(app.clone());
    emit_state(app, snapshot_state(&service));
    tracing::info!(session_id, region = ?(x, y, w, h), fps, "GIF 录制会话已创建（倒计时）");
    Ok(session_id)
}

/// 暂停 / 恢复 / 停止 / 取消（指令下发；停止结果由 finalize_after_stop 分派）。
pub fn command_recording(app: &AppHandle, cmd: CaptureCommand, cancel: bool) -> GifResult<()> {
    let service = app.state::<Arc<GifService>>();
    let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
    let Some(r) = rec.as_ref() else {
        return Err(GifServiceError::NoActiveRecording);
    };
    if matches!(cmd, CaptureCommand::Stop) && cancel {
        r.cancel_flag.store(true, Ordering::SeqCst);
    }
    r.ctl.command(cmd);
    Ok(())
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
    // 清活跃录制（同一会话才清）
    {
        let mut rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(rec.as_ref(), Some(r) if Arc::ptr_eq(&r.ctl, ctl)) {
            *rec = None;
        }
    }
    close_control_window(app);
    close_region_frame(app);
    close_region_handle(app);
    let root = GifService::root();
    if cancelled {
        let _ = std::fs::remove_dir_all(root.join("sessions").join(session_id));
        service.timelines.lock().unwrap_or_else(|e| e.into_inner()).remove(session_id);
        tracing::info!(session_id, "GIF 录制已取消，素材已丢弃");
        emit_state(app, snapshot_state(&service));
        return;
    }

    // 正常停止：读取定稿素材，构建时间轴
    match SpoolReader::open(&root, session_id) {
        Ok(reader) => {
            let fps = reader.meta.fps;
            let durs = infra_gif::finalize_durations(&reader.timestamps(), fps);
            let timeline = Timeline::from_durations(durs);
            let frames = timeline.frame_count();
            service
                .timelines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(session_id.to_string(), timeline);
            tracing::info!(
                session_id,
                frames,
                stop_reason = ?outcome.as_ref().map(|o| o.stop_reason),
                duplicates = outcome.as_ref().map(|o| o.duplicates_merged).unwrap_or(0),
                "GIF 录制完成，素材已定稿"
            );
            emit_state(app, snapshot_state(&service));
            open_editor(app, session_id);
        }
        Err(e) => {
            tracing::error!(session_id, error = %e, "GIF 素材定稿读取失败");
            emit_state(app, snapshot_state(&service));
        }
    }
}

/// 控制条窗口：优先定位到截图工具栏原位（「工具栏原地切换成录制栏」，
/// overlay 前端传入物理坐标）；无位置信息时区域下方 12px → 上方。
///
/// focusable(false)（WS_EX_NOACTIVATE）：show() 不从被录应用抢前台焦点，
/// 鼠标点击照常可用——控制条不需要键盘输入。
fn show_control_window(
    app: &AppHandle,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    control_x: Option<i32>,
    control_y: Option<i32>,
) {
    let win = match show_control_window_builder(app) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "GIF 控制条窗口创建失败（录制仍继续，用托盘不可停）");
            return;
        }
    };
    // 初始位置：工具栏原位优先；否则区域下方。
    let (mut pos_x, mut pos_y) = match (control_x, control_y) {
        (Some(cx), Some(cy)) => (cx, cy),
        _ => (x, y + h as i32 + 12),
    };
    let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
        pos_x, pos_y,
    )));
    // 物理尺寸读回（逻辑 400×52 随所在屏 DPI 缩放；移动后按目标屏实际值）。
    let (cw, ch) = win
        .outer_size()
        .map(|s| (s.width as i32, s.height as i32))
        .unwrap_or((400, 52));
    // 工具栏原位可能被拖进过选区 → 控制条会入成品，挪到区域下方，放不下放上方。
    let overlaps = pos_x < x + w as i32
        && pos_x + cw > x
        && pos_y < y + h as i32
        && pos_y + ch > y;
    if overlaps {
        pos_y = y + h as i32 + 12;
        let (_, vy, _, vh) = virtual_screen_rect();
        if pos_y + ch > vy + vh {
            pos_y = y - 12 - ch;
        }
    }
    // 虚拟屏幕边界钳制（多屏负坐标合法，按虚拟屏矩形而非主屏判断）。
    let (vx, vy, vw, vh) = virtual_screen_rect();
    pos_x = pos_x.clamp(vx, (vx + vw - cw).max(vx));
    pos_y = pos_y.clamp(vy, (vy + vh - ch).max(vy));
    let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
        pos_x, pos_y,
    )));
    let _ = win.show();
}

/// 结束录制：控制条隐藏保活（不销毁）。复用路径 `show()` 毫秒级——
/// 若每次 close/rebuild，WebView2 冷启动会让「点开始到控制条出现」
/// 有数百毫秒到一秒的空窗。
fn close_control_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(CONTROL_LABEL) {
        let _ = win.hide();
    }
}

/// 录制范围指示框：透明置顶 + 鼠标穿透 + 不抢焦点；窗口比区域外扩
/// REGION_BORDER，边框画在窗口最外圈（内边缘 = 区域边界，不入画面）。
fn show_region_frame(app: &AppHandle, x: i32, y: i32, w: u32, h: u32) {
    match show_region_frame_builder(app) {
        Ok(win) => {
            let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
                x - REGION_BORDER,
                y - REGION_BORDER,
            )));
            let _ = win.set_size(tauri::PhysicalSize::new(
                w + (REGION_BORDER as u32) * 2,
                h + (REGION_BORDER as u32) * 2,
            ));
            // 鼠标穿透：被录区域内的点击/拖拽完全不受影响
            let _ = win.set_ignore_cursor_events(true);
            let _ = win.show();
        }
        Err(e) => {
            tracing::warn!(error = %e, "GIF 范围框窗口创建失败（录制继续）");
        }
    }
}

fn close_region_frame(app: &AppHandle) {
    // 隐藏保活（复用秒显，同 close_control_window 注释）
    if let Some(win) = app.get_webview_window(REGION_LABEL) {
        let _ = win.hide();
    }
}

/// 播放期间区域变化的统一广播（拖动柄窗口复用时靠此同步，无 URL 重载）。
fn emit_region_updated(app: &AppHandle, x: i32, y: i32, w: u32, h: u32) {
    let (vx, vy, vw, vh) = virtual_screen_rect();
    let _ = app.emit(
        EventNames::GIF_REGION_UPDATED,
        serde_json::json!({"x": x, "y": y, "w": w, "h": h, "vx": vx, "vy": vy, "vw": vw, "vh": vh}),
    );
}

/// 预热录制辅助窗口（准备模式进入时调用）：三个窗口以隐藏态建好，
/// 点「开始录制」走复用路径（set/show 毫秒级，无 WebView2 冷启动延迟）。
/// 已存在则无操作。代价是三个常驻隐藏 WebView（各约十几 MB）——只在
/// 用户明确表达录制意图后才付出。
pub fn prewarm_windows(app: &AppHandle) {
    // 三个 builder 与正式 show 路径一致（含 focusable 等），确保复用时
    // 窗口行为相同；预热不定位不显示。
    let _ = show_control_window_builder(app);
    let _ = show_region_frame_builder(app);
    let _ = show_region_handle_builder(app);
}

fn show_control_window_builder(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    use blink_infra::platform::window::get_or_create_window;
    get_or_create_window(app, CONTROL_LABEL, || {
        WebviewWindowBuilder::new(
            app,
            CONTROL_LABEL,
            WebviewUrl::App("gif-recorder.html".into()),
        )
        .title("Blink GIF")
        .inner_size(340.0, 52.0)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .focusable(false)
        .focused(false)
        .visible(false)
        .build()
    })
    .map(|(win, _)| win)
}

fn show_region_frame_builder(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    use blink_infra::platform::window::get_or_create_window;
    get_or_create_window(app, REGION_LABEL, || {
        WebviewWindowBuilder::new(
            app,
            REGION_LABEL,
            WebviewUrl::App("gif-region.html".into()),
        )
        .title("")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(false)
        .focusable(false)
        .focused(false)
        .visible(false)
        .build()
    })
    .map(|(win, _)| win)
}

fn show_region_handle_builder(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    use blink_infra::platform::window::get_or_create_window;
    get_or_create_window(app, REGION_HANDLE_LABEL, || {
        WebviewWindowBuilder::new(
            app,
            REGION_HANDLE_LABEL,
            WebviewUrl::App("gif-region-handle.html".into()),
        )
        .title("")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(false)
        .focusable(false)
        .focused(false)
        .inner_size(34.0, 22.0)
        .visible(false)
        .build()
    })
    .map(|(win, _)| win)
}

/// 录制区域拖动柄：范围框上边缘中央的小置顶窗（区域外，不挡被录内容）。
/// focusable(false)：show() 不抢被录应用焦点（拖拽只需要鼠标）。
/// 窗口保活复用——初始区域/边界经 GIF_REGION_UPDATED 事件同步（预热线
/// 无 URL 参数；事件为准）。
fn show_region_handle(app: &AppHandle, x: i32, y: i32, w: u32) {
    match show_region_handle_builder(app) {
        Ok(win) => {
            // 先粗定位到目标屏（让 DPI 变化生效），再按实际物理尺寸精确定位。
            let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
                x,
                y,
            )));
            place_region_handle(app, x, y, w);
            let _ = win.show();
        }
        Err(e) => tracing::warn!(error = %e, "GIF 拖动柄窗口创建失败（录制继续）"),
    }
}

/// 按实际物理尺寸把柄居中到范围框上边缘中央，并钳制到虚拟屏顶。
/// 柄窗口以逻辑 34×22 创建，落在不同 DPI 屏上物理宽度不同——用固定
/// `-17` 物理偏移会在高缩放屏偏心，因此读回 outer_size 后再算。
fn place_region_handle(app: &AppHandle, x: i32, y: i32, w: u32) {
    let Some(win) = app.get_webview_window(REGION_HANDLE_LABEL) else {
        return;
    };
    let (phys_w, phys_h) = win
        .outer_size()
        .map(|s| (s.width as i32, s.height as i32))
        .unwrap_or((34, 22));
    let hx = x + w as i32 / 2 - phys_w / 2;
    let hy = (y - REGION_BORDER - phys_h).max(virtual_screen_rect().1);
    let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
        hx, hy,
    )));
}

fn close_region_handle(app: &AppHandle) {
    // 隐藏保活（复用秒显，同 close_control_window 注释）；区域/边界由
    // GIF_REGION_UPDATED 事件在下次开始时重新同步（复用无 URL 重载）
    if let Some(win) = app.get_webview_window(REGION_HANDLE_LABEL) {
        let _ = win.hide();
    }
}

/// 移动录制区域（拖动柄上报目标坐标）：热更新采集位置 + 同步范围框/柄窗口。
///
/// 坐标钳制到虚拟屏幕矩形——BitBlt 越界部分是黑边，范围框也不该悬出屏。
/// 尺寸取 `ActiveRecording` 内存值（会话元数据回读每帧拖动都要解析整个
/// frames.jsonl，且录制中索引仍在增长）。
pub fn move_recording_region(app: &AppHandle, x: i32, y: i32) -> GifResult<()> {
    let service = app.state::<Arc<GifService>>();
    let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
    let Some(r) = rec.as_ref() else {
        return Err(GifServiceError::NoActiveRecording);
    };
    let (w, h) = (r.region_w as i32, r.region_h as i32);
    let (vx, vy, vw, vh) = virtual_screen_rect();
    let x = x.clamp(vx, (vx + vw - w).max(vx));
    let y = y.clamp(vy, (vy + vh - h).max(vy));
    if let Ok(mut g) = r.region_pos.lock() {
        *g = (x, y);
    }
    if let Some(win) = app.get_webview_window(REGION_LABEL) {
        let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
            x - REGION_BORDER,
            y - REGION_BORDER,
        )));
        let _ = win.set_size(tauri::PhysicalSize::new(
            r.region_w + (REGION_BORDER as u32) * 2,
            r.region_h + (REGION_BORDER as u32) * 2,
        ));
    }
    place_region_handle(app, x, y, r.region_w);
    // 后端钳制结果回发（拖动端就地钳制为主，此为对账同步）
    emit_region_updated(app, x, y, r.region_w, r.region_h);
    Ok(())
}

/// 打开编辑器窗口（复用 + 会话事件；初始会话由页面 ready 后拉取）。
pub fn open_editor(app: &AppHandle, session_id: &str) {
    use blink_infra::platform::window::get_or_create_window;
    {
        let service = app.state::<Arc<GifService>>();
        *service
            .editor_session
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(session_id.to_string());
    }
    let built = get_or_create_window(app, EDITOR_LABEL, || {
        WebviewWindowBuilder::new(
            app,
            EDITOR_LABEL,
            WebviewUrl::App("gif-editor.html".into()),
        )
        .title("Blink GIF 编辑器")
        .inner_size(980.0, 720.0)
        .min_inner_size(760.0, 540.0)
        .visible(false)
        .build()
    });
    match built {
        Ok((win, _created)) => {
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
}

// ── 简单屏幕几何（Win32 直查；避免引入窗口管理依赖） ──────────────────────────

/// 当前录制区域快照（拖动柄窗口加载时拉取；无活跃录制返回 None）。
pub fn recording_region_snapshot(service: &Arc<GifService>) -> Option<serde_json::Value> {
    let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
    let r = rec.as_ref()?;
    let (x, y) = r.region_pos.lock().map(|g| *g).unwrap_or((0, 0));
    let (vx, vy, vw, vh) = virtual_screen_rect();
    Some(serde_json::json!({
        "x": x, "y": y, "w": r.region_w, "h": r.region_h,
        "vx": vx, "vy": vy, "vw": vw, "vh": vh,
    }))
}

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
    let frames: Vec<serde_json::Value> = tl
        .effective_frames()
        .iter()
        .map(|f| {
            serde_json::json!({
                "srcIdx": f.src_idx,
                "durMs": f.dur_ms,
                "held": f.held,
            })
        })
        .collect();
    serde_json::json!({
        "frameCount": tl.frame_count(),
        "revision": tl.revision(),
        "trimStart": tl.trim_range().0,
        "trimEnd": tl.trim_range().1,
        "deleted": tl.deleted_frames(),
        "speed": tl.speed(),
        "holds": tl.holds().iter().map(|(k, v)| (k.to_string(), *v)).collect::<HashMap<String, u32>>(),
        "loopCount": tl.loop_count(),
        "frames": frames,
        "totalMs": tl.effective_total_ms(),
        "canUndo": tl.can_undo(),
        "canRedo": tl.can_redo(),
    })
}

/// 把 domain 导出计划转换为 infra 编码计划（分层边界桥接）。
pub fn to_encode_plan(plan: &blink_domain::gif::ExportPlan) -> EncodePlan {
    EncodePlan {
        src_w: plan.src_w,
        src_h: plan.src_h,
        loop_count: plan.loop_count,
        frames: plan
            .frames
            .iter()
            .map(|f| EncodeFrame {
                src_idx: f.src_idx,
                delay_cs: f.delay_cs,
            })
            .collect(),
    }
}
