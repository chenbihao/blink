//! GIF 录制服务编排（0.28.7，bin 侧）。
//!
//! 职责：
//! - **录制会话**：`start`（素材目录 + 采集流水线 + 控制条窗口 + 3s 倒计时）、
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

/// 录制倒计时（ms）。
const COUNTDOWN_MS: u64 = 3000;
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
        infra_gif::RecordingBudget::default(),
    );

    let cancel_flag = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "windows")]
    let region_for_rec = Arc::clone(&region_pos);
    #[cfg(not(target_os = "windows"))]
    let region_for_rec = Arc::new(Mutex::new((x, y)));
    *service.recording.lock().unwrap_or_else(|e| e.into_inner()) = Some(ActiveRecording {
        session_id: session_id.clone(),
        ctl: Arc::clone(&ctl),
        countdown_deadline: Some(std::time::Instant::now() + Duration::from_millis(COUNTDOWN_MS)),
        cancel_flag: Arc::clone(&cancel_flag),
        region_pos: region_for_rec,
    });

    // 控制条窗口（区域下方，避让选区——控制条不入成品）；范围指示框
    // （边框落在区域外，鼠标穿透）；窗口就绪后再收截图 overlay（时序确定：
    // overlay 的灰罩不会盖住倒计时，录制画面干净）。
    show_control_window(app, x, y, w, h, control_x, control_y);
    show_region_frame(app, x, y, w, h);
    show_region_handle(app, x, y, w);
    crate::infra::platform::window::hide_screenshot_overlay(app);

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
/// overlay 前端传入物理坐标）；无位置信息时区域下方 12px → 上方 → 主屏底部。
fn show_control_window(
    app: &AppHandle,
    x: i32,
    y: i32,
    _w: u32,
    h: u32,
    control_x: Option<i32>,
    control_y: Option<i32>,
) {
    use blink_infra::platform::window::get_or_create_window;
    let (win, _created) = match get_or_create_window(app, CONTROL_LABEL, || {
        WebviewWindowBuilder::new(
            app,
            CONTROL_LABEL,
            WebviewUrl::App("gif-recorder.html".into()),
        )
        .title("Blink GIF")
        .inner_size(400.0, 52.0)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .visible(false)
        .build()
    }) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "GIF 控制条窗口创建失败（录制仍继续，用托盘不可停）");
            return;
        }
    };
    // 位置：工具栏原位优先；否则区域下方 → 上方 → 主屏底部中央。
    // （物理像素；set_position Physical 直接使用）
    let (mut pos_x, mut pos_y) = match (control_x, control_y) {
        (Some(cx), Some(cy)) => (cx as f64, cy as f64),
        _ => {
            let (px, py) = (x as f64, y as f64 + h as f64 + 12.0);
            if py as i64 + 52 > virtual_screen_bottom() {
                (px, (y as f64 - 12.0 - 52.0).max(0.0))
            } else {
                (px, py)
            }
        }
    };
    if pos_y < 0.0 {
        // 主屏底部中央兜底
        pos_x = 0.0;
        pos_y = primary_work_area_bottom() as f64 - 96.0;
    }
    let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
        pos_x as i32, pos_y as i32,
    )));
    // 不抢焦点（focused(false) 建窗 + 不 set_focus）：被录应用保持前台，
    // 光标/键盘上下文不变。
    let _ = win.show();
}

fn close_control_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(CONTROL_LABEL) {
        let _ = win.close();
    }
}

/// 录制范围指示框：透明置顶 + 鼠标穿透 + 不抢焦点；窗口比区域外扩
/// REGION_BORDER，边框画在窗口最外圈（内边缘 = 区域边界，不入画面）。
fn show_region_frame(app: &AppHandle, x: i32, y: i32, w: u32, h: u32) {
    use blink_infra::platform::window::get_or_create_window;
    let built = get_or_create_window(app, REGION_LABEL, || {
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
        .visible(false)
        .build()
    });
    match built {
        Ok((win, _)) => {
            let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
                x - REGION_BORDER,
                y - REGION_BORDER,
            )));
            let _ = win.set_size(tauri::PhysicalSize::new(
                w as u32 + (REGION_BORDER as u32) * 2,
                h as u32 + (REGION_BORDER as u32) * 2,
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
    if let Some(win) = app.get_webview_window(REGION_LABEL) {
        let _ = win.close();
    }
}

/// 录制区域拖动柄：范围框上边缘中央的小置顶窗（区域外，不挡被录内容）。
fn show_region_handle(app: &AppHandle, x: i32, y: i32, w: u32) {
    use blink_infra::platform::window::get_or_create_window;
    let url = format!("gif-region-handle.html?x={x}&y={y}");
    let built = get_or_create_window(app, REGION_HANDLE_LABEL, || {
        WebviewWindowBuilder::new(
            app,
            REGION_HANDLE_LABEL,
            WebviewUrl::App(url.clone().into()),
        )
        .title("")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(false)
        .inner_size(34.0, 22.0)
        .visible(false)
        .build()
    });
    match built {
        Ok((win, _)) => {
            let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
                x + w as i32 / 2 - 17,
                y - REGION_BORDER - 22,
            )));
            let _ = win.show();
        }
        Err(e) => tracing::warn!(error = %e, "GIF 拖动柄窗口创建失败（录制继续）"),
    }
}

fn close_region_handle(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(REGION_HANDLE_LABEL) {
        let _ = win.close();
    }
}

/// 移动录制区域（拖动柄上报目标坐标）：热更新采集位置 + 同步范围框/柄窗口。
pub fn move_recording_region(app: &AppHandle, x: i32, y: i32) -> GifResult<()> {
    let service = app.state::<Arc<GifService>>();
    let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
    let Some(r) = rec.as_ref() else {
        return Err(GifServiceError::NoActiveRecording);
    };
    if let Ok(mut g) = r.region_pos.lock() {
        *g = (x, y);
    }
    let (w, h) = {
        let root = GifService::root();
        // 尺寸从会话元数据读（ActiveRecording 不存尺寸）
        let meta = SpoolReader::open(&root, &r.session_id).ok().map(|m| (m.meta.region.w, m.meta.region.h));
        meta.unwrap_or((0, 0))
    };
    if let Some(win) = app.get_webview_window(REGION_LABEL) {
        let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
            x - REGION_BORDER,
            y - REGION_BORDER,
        )));
        let _ = win.set_size(tauri::PhysicalSize::new(
            w as u32 + (REGION_BORDER as u32) * 2,
            h as u32 + (REGION_BORDER as u32) * 2,
        ));
    }
    if let Some(win) = app.get_webview_window(REGION_HANDLE_LABEL) {
        let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
            x + w as i32 / 2 - 17,
            y - REGION_BORDER - 22,
        )));
    }
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

fn virtual_screen_bottom() -> i64 {
    #[cfg(target_os = "windows")]
    unsafe {
        let top = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_YVIRTUALSCREEN,
        );
        let h = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CYVIRTUALSCREEN,
        );
        top as i64 + h as i64
    }
    #[cfg(not(target_os = "windows"))]
    4096
}

fn primary_work_area_bottom() -> i32 {
    #[cfg(target_os = "windows")]
    unsafe {
        let mut rc = windows::Win32::Foundation::RECT::default();
        let ok = windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW(
            windows::Win32::UI::WindowsAndMessaging::SPI_GETWORKAREA,
            0,
            Some(&mut rc as *mut _ as *mut _),
            windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        if ok.is_ok() { rc.bottom } else { 1080 }
    }
    #[cfg(not(target_os = "windows"))]
    1080
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
