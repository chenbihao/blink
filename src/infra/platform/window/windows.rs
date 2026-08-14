//! Windows 平台特定的窗口控制实现：Win32 API。

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ── 0.18.3：便签 N+1 预热机制 ──────────────────────────
//
// 后台始终保留一个已加载 Tiptap bundle 的 WebView2 备用窗口。
// 被借用后立即创建新的；借出的窗口独立运行，关闭时正常 trash + 回收/销毁。
//
// 三个全局状态：
// - SPARE_SEQ：自增序号，为每个 spare 生成唯一 label（sticky-spare-{N}）
// - AVAILABLE_SPARE：当前空闲 spare 的 label（None = 无可用，需等待创建）
// - SPARE_BORROW：已借出 spare 的 label → sticky_id 映射

static SPARE_SEQ: AtomicU64 = AtomicU64::new(0);

static AVAILABLE_SPARE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn available_spare() -> &'static Mutex<Option<String>> {
    AVAILABLE_SPARE.get_or_init(|| Mutex::new(None))
}

static SPARE_BORROW: OnceLock<Mutex<std::collections::HashMap<String, String>>> = OnceLock::new();

fn spare_borrow() -> &'static Mutex<std::collections::HashMap<String, String>> {
    SPARE_BORROW.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

// ── 多 Pin N+1 预热机制（镜像便签 N+1 架构）──────────────────
//
// 支持同时 pin 多张图片。后台始终保留一个已加载 pin.html 的 WebView2 备用窗口。
// 被借用后立即创建新的；借出的窗口独立运行，关闭时回收/销毁。
//
// - PIN_SEQ：自增序号，为每个 spare 生成唯一 label（pin-spare-{N}）
// - AVAILABLE_PIN_SPARE：当前空闲 spare 的 label（None = 无可用）
// - PIN_SPARE_BORROW：已借出 spare 的 label → "pin" 固定值映射（pin 无持久化 id）
// - LAST_PIN_LABEL：最近一次 pin 的窗口 label，供 refresh_pin_image 定位目标

static PIN_SEQ: AtomicU64 = AtomicU64::new(0);

static AVAILABLE_PIN_SPARE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn available_pin_spare() -> &'static Mutex<Option<String>> {
    AVAILABLE_PIN_SPARE.get_or_init(|| Mutex::new(None))
}

static PIN_SPARE_BORROW: OnceLock<Mutex<std::collections::HashMap<String, String>>> =
    OnceLock::new();

fn pin_spare_borrow() -> &'static Mutex<std::collections::HashMap<String, String>> {
    PIN_SPARE_BORROW.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

static LAST_PIN_LABEL: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn last_pin_label() -> &'static Mutex<Option<String>> {
    LAST_PIN_LABEL.get_or_init(|| Mutex::new(None))
}

// ── 0.19.14：pin 图片进程内 registry ──────────────────────
//
// pin 窗口通过 `blink-pin:///{seq}` 自定义协议拉取 PNG bytes，
// 替代 base64 data URL（7.2MB PNG → 9.6MB base64 → WebView 解析阻塞）。
// 每次 store 返回递增 seq，URL 不同 → 浏览器不缓存，refresh 也能刷新。
//
// 0.20.4：增加 PIN_LABEL_TO_SEQ 映射，使 pin 窗口 label → image seq
// 可查（编辑器从 pin 窗口进入时需要按 label 找到对应图片）。
static PIN_IMAGE_SEQ: AtomicU64 = AtomicU64::new(0);
static PIN_IMAGE_REGISTRY: OnceLock<Mutex<std::collections::HashMap<u64, PinImage>>> =
    OnceLock::new();

/// 0.20.4：pin 窗口 label → image seq 映射。
/// pin 窗口创建/刷新时写入，关闭时清除。
static PIN_LABEL_TO_SEQ: OnceLock<Mutex<std::collections::HashMap<String, u64>>> =
    OnceLock::new();

fn pin_label_to_seq() -> &'static Mutex<std::collections::HashMap<String, u64>> {
    PIN_LABEL_TO_SEQ.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// P6：pin 图片存储格式——PNG（有标注路径）或 raw BGRA（快路径）。
/// 快路径存 raw BGRA，协议 handler 按需 lazy 编码 PNG，
/// `screenshot_pin_region` 不再阻塞等 encode_png 完成。
#[derive(Clone)]
pub enum PinImage {
    Png(Arc<Vec<u8>>),
    Bgra(Arc<Vec<u8>>, u32, u32),
}

fn pin_image_registry() -> &'static Mutex<std::collections::HashMap<u64, PinImage>> {
    PIN_IMAGE_REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// 把图片存入进程内 registry，返回递增 seq 供 `blink-pin:///{seq}` URL 使用。
///
/// registry 保留最近 8 条，超出时删除最旧的——pin 窗口 fetch 是同步的
/// （协议 handler 直接读内存 Arc），store 后立即 eval，不存在旧条目被删前未 fetch 的竞态。
pub fn store_pin_image(image: PinImage) -> u64 {
    let seq = PIN_IMAGE_SEQ.fetch_add(1, Ordering::SeqCst);
    let mut reg = pin_image_registry().lock().unwrap();
    reg.insert(seq, image);
    while reg.len() > 8 {
        let oldest = *reg.keys().min().unwrap();
        reg.remove(&oldest);
    }
    seq
}

/// 按 seq 取 pin 图片（供 `blink-pin://` 协议 handler 调用）。
pub fn get_pin_image(seq: u64) -> Option<PinImage> {
    pin_image_registry().lock().unwrap().get(&seq).cloned()
}

/// 0.20.4：按 pin 窗口 label 取对应的图片。
///
/// 用于编辑器从 pin 窗口进入：前端传 window label，后端查 label → seq → PinImage。
/// pin 窗口关闭后映射被清除，此函数返回 None。
/// 返回的 PinImage 供编辑器 session 使用，不受原 pin 生命周期影响（Arc clone）。
pub fn get_pin_image_by_label(label: &str) -> Option<PinImage> {
    let seq = pin_label_to_seq().lock().unwrap().get(label).copied()?;
    get_pin_image(seq)
}

use crate::domain::event_names::EventNames;
// 0.20.0：便签兜底关闭需要调用 trash_sticky_and_notify
use crate::domain::event::CapabilityEnv;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow};
use tokio::time::sleep;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAK, DWMWA_WINDOW_CORNER_PREFERENCE, DwmExtendFrameIntoClientArea, DwmFlush,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWL_STYLE, GWLP_WNDPROC, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, GetWindowThreadProcessId, HWND_TOP, IsIconic, SET_WINDOW_POS_FLAGS,
    SW_RESTORE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WNDPROC, WS_CAPTION, WS_THICKFRAME,
};

const ST_HIDDEN: u8 = 0;
const ST_VISIBLE: u8 = 1;

/// 默认 grace period。
const DEFAULT_GRACE_MS: u64 = 500;

// ── 0.18.3：窗口尺寸单一数据源 ──────────────────────────
//
// 每个窗口的默认尺寸 / 最小尺寸集中定义在此处，show 函数和 preheat 函数
// 统一引用，消除"散弹式修改"——改一处即同步生效。
//
// 便签窗口尺寸另有 `sticky::DEFAULT_WIDTH` / `DEFAULT_HEIGHT`（用于 DB
// 新建便签的初始 width/height），`create_sticky_spare` 应引用这两个常量
// 而非硬编码。

/// Chat 窗口
const CHAT_W: f64 = 900.0;
const CHAT_H: f64 = 680.0;
const CHAT_MIN_W: f64 = 560.0;
const CHAT_MIN_H: f64 = 420.0;

/// 内容编辑器窗口
const EDITOR_W: f64 = 720.0;
const EDITOR_H: f64 = 560.0;
const EDITOR_MIN_W: f64 = 400.0;
const EDITOR_MIN_H: f64 = 300.0;

/// 便签管理窗口
const MANAGER_W: f64 = 560.0;
const MANAGER_H: f64 = 640.0;
const MANAGER_MIN_W: f64 = 360.0;
const MANAGER_MIN_H: f64 = 400.0;

/// 设置窗口
const SETTINGS_W: f64 = 960.0;
const SETTINGS_H: f64 = 680.0;
const SETTINGS_MIN_W: f64 = 760.0;
const SETTINGS_MIN_H: f64 = 520.0;

/// 便签窗口最小尺寸
const STICKY_MIN_W: f64 = 120.0;
const STICKY_MIN_H: f64 = 80.0;

/// 语音浮层窗口
const VOICE_W: f64 = 260.0;
const VOICE_H: f64 = 140.0;

/// 唤起时的基准逻辑尺寸——用来在跨 DPI 屏定位时算出目标屏上的物理尺寸。
/// 与前端 `syncWindowSize()` 首帧一致（宽 700 / 高 65 含 CSS padding），
/// 避免"定位算 60、前端 resize 到 65"导致的 5px 视觉抖动。
const BASE_W_LOGICAL: f64 = 700.0;
const BASE_H_LOGICAL: f64 = 65.0;

static STATE: AtomicU8 = AtomicU8::new(ST_HIDDEN);
static START: OnceLock<Instant> = OnceLock::new();
static INVOKE_AT: AtomicU64 = AtomicU64::new(0);
static GRACE_MS: AtomicU64 = AtomicU64::new(DEFAULT_GRACE_MS);
/// 主窗口 visibility transition 计数器（window 模块拥有，每次成功 transition 递增）。
/// 通知 hotkey 输入状态机时携带，旧 revision 被 reduce_window_changed 丢弃。
static WINDOW_REVISION: AtomicU64 = AtomicU64::new(0);
/// Blink 主窗口抢焦点前的外部前台窗口，截图/chord 后续需要恢复或驱动原应用时使用。
static LAST_EXTERNAL_HWND: AtomicIsize = AtomicIsize::new(0);

/// 0.16.11：应用退出标志。
///
/// 在 `RunEvent::Exit` 时设为 true，便签窗口的 `CloseRequested` handler 据此区分
/// 「用户关闭单条便签」与「应用整体退出」——退出时不把 visible 改成 false，
/// 只隐藏窗口，保证下次启动按原 visible 状态恢复。
static IS_APP_EXITING: AtomicBool = AtomicBool::new(false);

/// 0.17.6: 主窗口 AI 活跃标志。
///
/// 主窗口 AI（AiMode）激活时设为 true，watchdog 据此跳过失焦隐藏，
/// 防止 AI 生成过程中窗口被意外隐藏。Done/Error/abort 时设回 false。
static MAIN_WINDOW_AI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 0.20.4: 图片编辑器活跃标志。
///
/// 图片编辑器（chord-screenshot 窗口 image-editor-mode）激活时设为 true，
/// watchdog 据此跳过 screenshot overlay 失焦隐藏——防止主窗口关闭导致
/// 编辑器被误关。编辑器关闭（cancel/copy/pin/save）时设回 false。
static IMAGE_EDITOR_ACTIVE: AtomicBool = AtomicBool::new(false);

// ── 0.19：chat prefill 暂存（infra 层，带 revision） ──────────────────────
//
// show_chat_window 先 set_chat_prefill(text) 拿到 revision R，再 emit {R, text}。
// 前端两条路径：
//   冷启动：await listen → take_chat_prefill() 拉取 {R, text}（take 清空 pending）
//   热窗口：listener 已在线 → 收到事件 → ack_chat_prefill(R) 清空 pending
// revision 防止旧事件的 ack 误删较新的 pending。
// build/show 失败时 clear_chat_prefill(R) 回滚。

struct ChatPrefill {
    revision: u64,
    text: String,
}

static CHAT_PREFILL_STATE: OnceLock<Mutex<Option<ChatPrefill>>> = OnceLock::new();
static CHAT_PREFILL_REV: AtomicU64 = AtomicU64::new(0);

fn chat_prefill_state() -> &'static Mutex<Option<ChatPrefill>> {
    CHAT_PREFILL_STATE.get_or_init(|| Mutex::new(None))
}

/// 写入 prefill，返回本次 revision（用于后续 ack/clear/rollback）。
pub fn set_chat_prefill(text: &str) -> u64 {
    let revision = CHAT_PREFILL_REV.fetch_add(1, Ordering::SeqCst) + 1;
    let mut guard = chat_prefill_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *guard = Some(ChatPrefill {
        revision,
        text: text.to_string(),
    });
    revision
}

/// 拉取并清空 pending（冷启动路径）。
/// 返回 (revision, text) 或 None。
pub fn take_chat_prefill() -> Option<(u64, String)> {
    let mut guard = chat_prefill_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guard.take().map(|p| (p.revision, p.text))
}

/// 按 revision 清除 pending（热窗口 event 路径）。
/// 仅当当前 pending 的 revision 匹配时才清空，防止旧事件误删新 pending。
pub fn ack_chat_prefill(revision: u64) {
    let mut guard = chat_prefill_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(ref p) = *guard {
        if p.revision == revision {
            *guard = None;
        }
    }
}

/// 按 revision 回滚 pending（build/show 失败时调用）。
/// 与 ack 相同语义：仅清除匹配 revision 的 pending。
pub fn clear_chat_prefill(revision: u64) {
    ack_chat_prefill(revision);
}

// ── 0.19：按 label 串行化窗口创建（single-flight） ──────────────────────────
//
// 预热和用户唤起可能并发创建同一 label 的窗口（如 chat），Tauri 不容忍
// duplicate label。用 per-label Mutex 串行化"检查 + build"，配合二次检查
// 消除竞态。锁只覆盖创建，不覆盖 show/focus/emit。
//
// 设计要点：
// - 无竞争快速路径：先不加锁检查 get_webview_window，命中直接返回。
// - 加锁后二次检查：等待期间可能已被并发路径创建。
// - build 失败后再查一次：防御 Tauri 注册与 build 返回的边界竞态。
// - 锁中毒时恢复（into_inner），避免一次 panic 永久破坏入口。

type WindowCreateLock = std::sync::Arc<std::sync::Mutex<()>>;

static WINDOW_CREATE_LOCKS: OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, WindowCreateLock>>,
> = OnceLock::new();

static PENDING_CONTEXT_MENU: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();

fn creation_lock(label: &str) -> WindowCreateLock {
    let locks = WINDOW_CREATE_LOCKS.get_or_init(|| std::sync::Mutex::new(Default::default()));
    let mut locks = locks.lock().unwrap_or_else(|e| e.into_inner());
    locks
        .entry(label.to_string())
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
        .clone()
}

/// 保存首次建窗期间可能尚未有前端 listener 的右键菜单载荷。
pub fn set_context_menu_payload(items: String, theme: String) {
    let pending = PENDING_CONTEXT_MENU.get_or_init(|| Mutex::new(None));
    *pending.lock().unwrap_or_else(|error| error.into_inner()) = Some((items, theme));
}

/// 前端 ready 后主动拉取，兜底预热与用户首次唤起并发时丢失 eval 的窗口。
pub fn take_context_menu_payload() -> Option<(String, String)> {
    PENDING_CONTEXT_MENU
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
}

/// 按 label 串行化创建窗口（single-flight）。
///
/// 返回 `(window, created)`：`created=true` 表示本次调用真正创建了窗口，
/// `false` 表示复用了已存在的窗口（无论来自快速路径还是并发创建）。
///
/// `build` 闭包只覆盖窗口创建，**不**包含 show/focus/emit——这些在锁外执行，
/// 避免持有全局锁做 IO 密集操作。
///
/// 日志：等待耗时 + created 标记，便于排查热键被冷建窗堵住的场景。
fn get_or_create_window<F>(
    app: &AppHandle,
    label: &str,
    build: F,
) -> Result<(WebviewWindow, bool), String>
where
    F: FnOnce() -> Result<WebviewWindow, tauri::Error>,
{
    use tauri::Manager;

    // 无竞争快速路径
    if let Some(win) = app.get_webview_window(label) {
        return Ok((win, false));
    }

    let lock = creation_lock(label);
    let t_wait = Instant::now();
    tracing::debug!(label, "window get_or_create: waiting");
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let waited_ms = t_wait.elapsed().as_millis();

    // 二次检查：等待期间可能已由并发路径创建
    if let Some(win) = app.get_webview_window(label) {
        tracing::debug!(
            label,
            waited_ms,
            created = false,
            "window get_or_create: ready (race)"
        );
        return Ok((win, false));
    }

    match build() {
        Ok(win) => {
            tracing::debug!(
                label,
                waited_ms,
                created = true,
                "window get_or_create: ready"
            );
            Ok((win, true))
        }
        Err(error) => {
            // 防御 Tauri 注册窗口与 build 返回之间的边界竞态
            if let Some(win) = app.get_webview_window(label) {
                tracing::debug!(
                    label,
                    waited_ms,
                    created = false,
                    "window get_or_create: build 失败但窗口已由并发路径创建，复用"
                );
                Ok((win, false))
            } else {
                Err(format!("创建窗口 {label} 失败: {error}"))
            }
        }
    }
}

/// 右键菜单唯一建窗入口；预热与用户首次唤起必须共用同一 label single-flight。
pub fn get_or_create_context_menu_window(
    app: &AppHandle,
    initial_url: String,
    width: f64,
    height: f64,
) -> Result<(WebviewWindow, bool), String> {
    get_or_create_window(app, "context-menu", || {
        use tauri::{WebviewUrl, WebviewWindowBuilder};
        WebviewWindowBuilder::new(app, "context-menu", WebviewUrl::App(initial_url.into()))
            .title("")
            .inner_size(width, height)
            .position(0.0, 0.0)
            .decorations(false)
            .transparent(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .focused(false)
            .resizable(false)
            .visible(false)
            .build()
    })
}

/// 程序启动以来的毫秒数（单调时钟，用于 grace period 计算）。
fn elapsed_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 统一的 visibility transition：写 STATE + 递增 revision + 通知 hotkey 输入状态机。
///
/// 所有主窗口 visibility 写入收敛到此 helper（§3.5）。每次成功 transition 递增
/// revision 并通知 InputController，使输入状态机的 `state.window.visible` 与真实
/// 窗口同步——native chord session 据此建立/退出。
fn transition_visibility(visible: bool) {
    let st = if visible { ST_VISIBLE } else { ST_HIDDEN };
    STATE.store(st, Ordering::SeqCst);
    let rev = WINDOW_REVISION.fetch_add(1, Ordering::SeqCst) + 1;
    crate::infra::platform::hotkey::InputController::update_window(visible, rev);
}

/// 唤起：采集上下文快照 -> 定位 -> show -> set_focus -> 通知前端。
///
/// **采集时机很重要**：必须在 show() 之前调用，否则拿到的前台是 Blink 自己。
pub fn invoke(app: &AppHandle) {
    let t0 = std::time::Instant::now();

    // 1. 先采集上下文快照（show 之前！）
    //    读内存 ContextConfig（零 IO，热键回调不能 await），按配置过滤采集
    let context_cfg = app
        .try_state::<std::sync::Arc<std::sync::RwLock<crate::domain::config::ContextConfig>>>()
        .map(|c| c.read().unwrap().clone())
        .unwrap_or_default();
    let snapshot = crate::infra::platform::context::collect(&context_cfg);
    if let Some(hwnd) = snapshot
        .foreground_app
        .as_ref()
        .map(|foreground| foreground.hwnd)
        .filter(|hwnd| *hwnd != 0)
    {
        LAST_EXTERNAL_HWND.store(hwnd, Ordering::SeqCst);
    }
    tracing::debug!(
        foreground_app = ?snapshot.foreground_app.as_ref().map(|f| &f.process_name),
        window_title = ?snapshot.foreground_app.as_ref().map(|f| &f.window_title),
        "invoke: captured context"
    );

    // 2. 更新 SearchService 中的快照
    //
    // 选区抓取采用「快速捕获 + 慢速异步提取」模式：
    // - show() 之前：capture_focused_element() 仅做 GetFocusedElement()（O(1)，<5ms）
    // - show() 之后：spawn 线程做三段式 TextPattern 提取（可能 100-500ms）
    // 这样窗口显示不被 UIA 阻塞——慢应用上用户不再感到"卡一下才出来"。
    //
    // 提取完成后通过 update_selected_text 回填 + emit awareness-updated 触发前端 retrigger。
    let focused_element = if context_cfg.selection_enabled {
        let t_capture = std::time::Instant::now();
        let focused = snapshot
            .foreground_app
            .as_ref()
            .filter(|fg| fg.hwnd != 0)
            .and_then(|_| crate::infra::platform::selection::capture_focused_element());
        tracing::debug!(
            target: "perf",
            capture_ms = t_capture.elapsed().as_millis(),
            has_element = focused.is_some(),
            "[perf] invoke: capture_focused_element (before show)"
        );
        focused
    } else {
        None
    };

    if let Some(search_service) =
        app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
    {
        search_service.update_snapshot(snapshot.clone());
    }

    let Some(win) = app.get_webview_window("main") else {
        return;
    };

    let t_show = std::time::Instant::now();
    let _ = win.set_size(tauri::LogicalSize::new(BASE_W_LOGICAL, BASE_H_LOGICAL));

    if let Some(pos) = launcher_position(&win) {
        let _ = win.set_position(pos);
    }
    let now = elapsed_ms();
    let grace_ms = GRACE_MS.load(Ordering::SeqCst);
    INVOKE_AT.store(now, Ordering::SeqCst);
    tracing::trace!(grace_ms, "invoke: show + set_focus");
    // 0.19 修正：show 失败时不能写 Visible——内部状态不能在窗口根本没显示时
    // 被写成 Visible，否则输入状态机误判窗口可见、watchdog 不隐藏。
    if let Err(error) = win.show() {
        tracing::warn!(%error, "invoke: 主窗口 show 失败，保持 Hidden");
        return;
    }
    // show 成功后建立 Visible（§3.5）：通知输入状态机 window 已可见，
    // native chord session 据此建立。必须在 emit SHOWN 之前。
    transition_visibility(true);
    // set_focus 失败不回滚 Visible——窗口已确实可见，只是焦点没拿到。
    // 记录 warn 便于诊断；前端仍会收到 SHOWN 事件触发输入 focus。
    if let Err(error) = win.set_focus() {
        tracing::warn!(%error, "invoke: 主窗口显示成功但聚焦失败");
    }
    let _ = app.emit(EventNames::SHOWN, ());
    tracing::debug!(
        target: "perf",
        show_ms = t_show.elapsed().as_millis(),
        total_ms = t0.elapsed().as_millis(),
        "[perf] invoke: show+focus+emit (TOTAL)"
    );

    // 3. show 之后：异步提取选区（不阻塞窗口显示）
    //
    // focused_element 在 show() 之前通过 GetFocusedElement() 捕获，
    // 此时焦点还在原应用上。show() 之后焦点已移到 Blink，但捕获的 COM 元素
    // 仍然指向原应用的焦点控件——MTA 公寓下 COM 接口跨线程安全。
    //
    // 提取完成后回填 SearchService 快照 + emit awareness-updated 触发前端 retrigger，
    // 让翻译 Ghost 等依赖选区的建议在选区就绪后自动出现。
    if let Some(focused) = focused_element {
        let search_service = app
            .try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            .map(|s| s.inner().clone());
        let app_clone = app.clone();
        std::thread::spawn(move || {
            let t_extract = std::time::Instant::now();
            let grabbed =
                crate::infra::platform::selection::extract_selection_from_element(&focused)
                    .or_else(|| {
                        // UIA 未命中，回退鼠标钩子缓存
                        let cached = crate::infra::platform::selection::get_last_selection();
                        if cached.is_some() {
                            tracing::trace!("invoke: 回退到鼠标钩子选区缓存");
                        }
                        cached.map(|(text, _)| text)
                    });
            let hit = grabbed.is_some();
            if let Some(ref text) = grabbed {
                tracing::debug!(len = text.chars().count(), "invoke: UIA 异步抓取选区成功");
            }
            if let Some(ss) = search_service {
                ss.update_selected_text(grabbed, None);
                // 通知前端重跑搜索——选区可能刚到，翻译 Ghost 等建议需要更新
                // 仅在窗口仍可见时 emit（用户可能已 ESC 关闭）
                if crate::infra::platform::window::is_visible() {
                    let _ = app_clone.emit(EventNames::AWARENESS_UPDATED, ());
                }
            }
            tracing::debug!(
                target: "perf",
                extract_ms = t_extract.elapsed().as_millis(),
                hit,
                "[perf] invoke: async UIA extraction (after show)"
            );
        });
    }
}

/// 隐藏：ESC / 看门狗 / 单实例重复启动。
/// 同时隐藏右键菜单窗口（保留窗口供下次复用）。
pub fn hide(app: &AppHandle, reason: &str) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
        tracing::debug!(reason, "hide: state -> HIDDEN");
        transition_visibility(false);
        let _ = app.emit(EventNames::HIDDEN, ());
    }
    // 主窗口隐藏时联动隐藏右键菜单（保留窗口供下次复用）
    if let Some(menu_win) = app.get_webview_window("context-menu") {
        let _ = menu_win.hide();
    }
}

/// 窗口焦点事件：仅记录诊断，不写 Visible（§3.5）。
///
/// Focus 是观察量，不是状态写入口。窗口可见性只由 `invoke()`/`hide()` 经
/// `transition_visibility` 建立。看门狗只依据后者产生的权威 visible state。
pub fn on_focused(focused: bool) {
    let st = STATE.load(Ordering::SeqCst);
    tracing::trace!(focused, st, "on_focused");
}

/// 启用系统级圆角（Windows 11+）。Win10 不支持此 API，静默忽略。
///
/// DWMWCP_ROUND = 2，让系统 DWM 绘制圆角，与 CSS border-radius 同步，
/// 避免窗口四角露出不透明背景。
pub fn enable_rounded_corners(hwnd: HWND) {
    // DWMWA_WINDOW_CORNER_PREFERENCE = 33, DWMWCP_ROUND = 2
    let pref: u32 = 2; // DWMWCP_ROUND
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

/// 强制窗口置顶（HWND_TOPMOST）。Tauri 的 `show()` / `set_always_on_top()` 在
/// WebView2 窗口上不一定可靠恢复 z-order，直接走 Win32 更稳妥。
pub fn force_topmost(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND(-1isize as *mut _)), // HWND_TOPMOST
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 0.20.4-fix：取消窗口的 topmost 属性，允许其他窗口覆盖。
///
/// 图片编辑器不需要像截图 overlay 那样强制置顶——用户可能需要在编辑图片时
/// 参考其他窗口内容。`HWND_NOTOPMOST` = `HWND(-2)`。
pub fn cancel_topmost(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND(-2isize as *mut _)), // HWND_NOTOPMOST
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 彻底移除窗口边框和标题栏（DWM 在 transparent + decorations:false 时仍会画）。
///
/// 双重手段：① 去掉 WS_CAPTION + WS_THICKFRAME 窗口样式；
/// ② DwmExtendFrameIntoClientArea 设负 margin 把 DWM 帧完全推出可视区域。
pub fn strip_window_border(hwnd: HWND) {
    unsafe {
        // 1. 去掉窗口样式中的标题栏和可拖拽边框
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let new_style = style & !(WS_CAPTION.0 as isize) & !(WS_THICKFRAME.0 as isize);
        SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE | SET_WINDOW_POS_FLAGS(0x0003),
        );

        // 2. 负 margin 把 DWM 帧完全推出可视区域
        let margins = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
}

/// 常驻看门狗：窗口 Visible 时每 150ms 检查前台窗口是否仍为自身，否则隐藏。
/// invoke 后 GRACE_MS 内不触发隐藏，覆盖 show → 获焦 → 立即丢焦 的焦点抖动。
///
/// **0.15 hotfix**：扩展为同时监视 `chord-screenshot` overlay 窗口。
/// 截图 overlay 是 `always_on_top` 全屏透明窗，前端 JS 失败/卡住时用户无法 ESC 退出、
/// 无法唤起任务管理器（被 overlay 遮挡）。watchdog 在 overlay 可见且前台非本进程时
/// 自动隐藏 overlay，提供后端兜底逃生通道。
pub fn start_watchdog(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            sleep(Duration::from_millis(150)).await;

            // ── 主窗口失焦检测（原有逻辑）──────────────────────────
            if STATE.load(Ordering::SeqCst) == ST_VISIBLE {
                // 0.17.6: AI 活跃时跳过失焦隐藏——主窗口 AI 生成过程中不应被隐藏
                if !MAIN_WINDOW_AI_ACTIVE.load(Ordering::Relaxed) {
                    let grace_ms = GRACE_MS.load(Ordering::SeqCst);
                    let since_invoke = elapsed_ms() - INVOKE_AT.load(Ordering::SeqCst);
                    if since_invoke >= grace_ms {
                        let fg = unsafe { GetForegroundWindow() };
                        // fg == NULL:焦点真空(系统正在切换前台窗口的瞬态,如刚拉起子进程时)。
                        // 这不代表用户切到了别的窗口,据此隐藏会误伤——跳过本轮,等下次轮询。
                        if !fg.0.is_null() && !is_self_foreground(&app, fg) {
                            tracing::info!(
                                since_invoke,
                                "watchdog: hide! fg=0x{:x}",
                                fg.0 as isize
                            );
                            hide(&app, "watchdog");
                        }
                    }
                }
            }

            // ── 截图 overlay 失焦检测（0.15 hotfix）─────────────────
            // overlay 可见 + 前台非本进程 → 自动隐藏。
            // 覆盖场景：用户 Ctrl+Shift+Esc 唤起任务管理器、Alt+Tab 切窗口、
            // 或前端 JS 模块加载失败导致 blur handler 未注册。
            //
            // 0.20.4：图片编辑器活跃时跳过——编辑器复用 chord-screenshot 窗口，
            // 主窗口关闭导致的焦点瞬态切换不应触发编辑器关闭。
            if !IMAGE_EDITOR_ACTIVE.load(Ordering::Relaxed) {
                if let Some(ss_win) = app.get_webview_window("chord-screenshot") {
                    if ss_win.is_visible().unwrap_or(false) {
                        let fg = unsafe { GetForegroundWindow() };
                        if !fg.0.is_null() && !is_self_foreground(&app, fg) {
                            tracing::info!(
                                "watchdog: screenshot overlay hide! fg=0x{:x}",
                                fg.0 as isize
                            );
                            // 用 hide_screenshot_overlay 而非 win.hide()，
                            // 确保同时清空 SESSION 释放位图内存
                            hide_screenshot_overlay(&app);
                        }
                    }
                }
            }
        }
    });
}

/// 更新 grace period（线程安全）。
pub fn update_grace_period(period: u64) {
    GRACE_MS.store(period, Ordering::SeqCst);
}

/// 主窗口当前是否处于可见态（供快捷键 toggle 判断）。
pub fn is_visible() -> bool {
    STATE.load(Ordering::SeqCst) == ST_VISIBLE
}

const WM_SYSCOMMAND: u32 = 0x0112;
const SC_KEYMENU: usize = 0xF100;

/// 各窗口的原始窗口过程（0.12.4 §6.6：从 OnceLock 改为 HashMap 支持多窗口）。
/// key = HWND 指针值（isize），value = 原始 WndProc 地址。
static ORIGINAL_WNDPROCS: std::sync::Mutex<Option<std::collections::HashMap<isize, isize>>> =
    std::sync::Mutex::new(None);

/// 拦截 Alt+Space 系统菜单（替换窗口过程，吞掉 SC_KEYMENU）。
/// 主窗口和 chat 窗口虽无边框仍响应 Alt+Space 弹出移动/最大化菜单，
/// 前端 preventDefault 与去 WS_SYSMENU 都无效，
/// 只能在窗口过程层拦截 WM_SYSCOMMAND。
/// 0.12.4 §6.6：支持多窗口安装（HashMap 按 HWND 存储 original wndproc）。
pub fn install_sysmenu_blocker(hwnd: HWND) {
    unsafe {
        // 检查是否已安装——避免重复 SetWindowLongPtrW 返回 sysmenu_block_proc 自身，
        // 导致 CallWindowProcW(sysmenu_block_proc, ...) 无限递归 → stack overflow。
        // （0.12.5 修复：此前注释称"重复安装安全"是错误的）
        let already_installed = ORIGINAL_WNDPROCS
            .lock()
            .unwrap()
            .as_ref()
            .map(|m| m.contains_key(&(hwnd.0 as isize)))
            .unwrap_or(false);
        if already_installed {
            return;
        }

        let original = SetWindowLongPtrW(
            hwnd,
            GWLP_WNDPROC,
            sysmenu_block_proc as *const () as usize as isize,
        );
        let mut map = ORIGINAL_WNDPROCS.lock().unwrap();
        map.get_or_insert_with(std::collections::HashMap::new)
            .insert(hwnd.0 as isize, original);
        tracing::debug!(
            hwnd = hwnd.0 as isize,
            original_wndproc = original,
            "install_sysmenu_blocker: 已安装系统菜单拦截器"
        );
    }
}

unsafe extern "system" fn sysmenu_block_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_SYSCOMMAND && (wparam.0 as usize & 0xFFF0) == SC_KEYMENU {
        return LRESULT(0);
    }
    let original = ORIGINAL_WNDPROCS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(&(hwnd.0 as isize)))
        .copied()
        .unwrap_or(0);
    if original != 0 {
        // edition 2024：unsafe fn 内的 unsafe 操作需显式 unsafe block
        unsafe {
            let proc: WNDPROC = std::mem::transmute::<isize, WNDPROC>(original);
            CallWindowProcW(proc, hwnd, msg, wparam, lparam)
        }
    } else {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }
}

/// 前台窗口是否为我们的主窗口（拿不到窗口/句柄时保守返回 true，避免误隐藏）。
/// 前台窗口是否属于本应用(按进程 ID 判定)。
///
/// 不再死比单个主窗口 HWND——那样会把「同属本进程的其它窗口」(debug 下 cargo run 的
/// 控制台、子进程交互产生的瞬时窗口等)误判为「别人」而隐藏。只要前台窗口的进程 ==
/// 本进程,就算焦点仍在自己,不隐藏。
fn is_self_foreground(_app: &AppHandle, fg: windows::Win32::Foundation::HWND) -> bool {
    let mut fg_pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(fg, Some(&mut fg_pid)) };
    if fg_pid == 0 {
        return true; // 拿不到 PID:保守不隐藏
    }
    let self_pid = unsafe { GetCurrentProcessId() };
    fg_pid == self_pid
}

/// 计算窗口在鼠标所在显示器上的位置：工作区中心居中（物理像素）。
///
/// 跟随鼠标所在屏（业界主流：Alfred / PowerToys Run 都这么做）——
/// 用户按热键前手在哪、窗口就在哪，无需感知"前台窗口在哪块屏"。
/// 天然规避 `GetForegroundWindow` 返回 NULL（切桌面 / 前台切换瞬态）时
/// `MonitorFromWindow(NULL, …)` 会误落到主屏的问题。
///
/// 用 `rcWork`（工作区，排除任务栏）而非 `rcMonitor`，与
/// `clamp_to_work_area` 行为一致：任务栏放屏顶部/侧边时也不会视觉偏移。
///
/// **跨 DPI 屏关键**：物理尺寸 **不能读 `outer_size()`**——它反映的是
/// 「窗口当前所在屏」的 DPI 换算结果，而我们要去的可能是另一块 DPI 不同的屏。
/// 一旦 `set_position` 把窗口移过去，Windows 发 `WM_DPICHANGED` 让 winit
/// 按目标屏 DPI **rescale 尺寸但不动位置**，就会视觉偏移。
/// 正确做法：`GetDpiForMonitor(目标屏) × 基准逻辑尺寸` 直接算目标屏物理尺寸，
/// 位置随之对齐——首次跨屏也一步到位。
fn launcher_position(_win: &WebviewWindow) -> Option<PhysicalPosition<i32>> {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        let hmon = if GetCursorPos(&mut pt).is_ok() {
            MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
        } else {
            // 极端 fallback：拿不到光标就落主屏
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
        };

        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            let rc = mi.rcWork; // 工作区（排除任务栏），与 clamp_to_work_area 一致

            // 0.11.9：走公共 DPI helper
            let dpi_x = crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon);
            let w = crate::infra::platform::dpi::logical_to_physical(BASE_W_LOGICAL, dpi_x);
            let h = crate::infra::platform::dpi::logical_to_physical(BASE_H_LOGICAL, dpi_x);

            let cx = rc.left + (rc.right - rc.left) / 2;
            let cy = rc.top + (rc.bottom - rc.top) / 2;
            tracing::trace!(
                cursor_x = pt.x,
                cursor_y = pt.y,
                mon_left = rc.left,
                mon_top = rc.top,
                mon_right = rc.right,
                mon_bottom = rc.bottom,
                dpi_x,
                w,
                h,
                "launcher_position: located on monitor under cursor"
            );
            return Some(PhysicalPosition::new(cx - w / 2, cy - h / 2));
        }
    }
    None
}

/// 计算指定尺寸窗口在鼠标所在屏工作区中心的物理位置（0.17.7）。
///
/// 与 `launcher_position` 同逻辑，但接受任意宽高（供便签窗口使用）。
/// `launcher_position` 硬编码主窗口尺寸（BASE_W/BASE_H_LOGICAL），此函数通用化。
fn compute_center_position(phys_w: i32, phys_h: i32) -> Option<(i32, i32)> {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        let hmon = if GetCursorPos(&mut pt).is_ok() {
            MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
        } else {
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
        };

        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            let rc = mi.rcWork;
            let cx = rc.left + (rc.right - rc.left) / 2;
            let cy = rc.top + (rc.bottom - rc.top) / 2;
            return Some((cx - phys_w / 2, cy - phys_h / 2));
        }
    }
    None
}

/// 便签标题栏高度（CSS 像素），与 `sticky.css` 中 `.sticky-titlebar { height: 32px }` 对齐。
const STICKY_TITLEBAR_H_CSS: f64 = 32.0;

/// 计算便签窗口位置，使标题栏中心对准鼠标光标（0.18.4）。
///
/// 主窗口唤起新便签时使用：窗口水平居中于鼠标 X，标题栏竖直中心在鼠标 Y。
/// 这样用户从主窗口钉文本为便签时，便签标题栏正好出现在鼠标处，方便立即拖动。
///
/// 返回物理坐标。标题栏高度 32 CSS px 按显示器 DPI 换算为物理像素。
pub fn compute_cursor_titlebar_position(phys_w: i32) -> Option<(i32, i32)> {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt).is_err() {
            return None;
        }
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let scale = crate::infra::platform::dpi::scale_factor(
            crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon),
        );
        let titlebar_phys = (STICKY_TITLEBAR_H_CSS * scale) as i32;
        // 水平居中于鼠标，标题栏竖直中心对准鼠标
        let x = pt.x - phys_w / 2;
        let y = pt.y - titlebar_phys / 2;
        Some((x, y))
    }
}

/// resize 后若窗口底部超出显示器工作区，向上移动使其完整可见。
pub fn clamp_to_work_area(win: &WebviewWindow) {
    let Ok(pos) = win.outer_position() else {
        return;
    };
    let Ok(size) = win.outer_size() else { return };
    let Ok(hwnd_raw) = win.hwnd() else { return };
    let hwnd = HWND(hwnd_raw.0 as _);

    unsafe {
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return;
        }
        let work = mi.rcWork; // 工作区(排除任务栏)
        let bottom = pos.y + size.height as i32;
        if bottom > work.bottom {
            let new_y = (work.bottom - size.height as i32).max(work.top);
            let _ = win.set_position(PhysicalPosition::new(pos.x, new_y));
            tracing::debug!(
                old_y = pos.y,
                new_y,
                work_bottom = work.bottom,
                height = size.height,
                "窗口超出屏幕底部,上移"
            );
        }
    }
}

/// 右键菜单多屏感知定位：直接用 Win32 `GetCursorPos` 拿光标**物理坐标**，
/// 找到目标显示器，按其 DPI 把 CSS 尺寸换算成物理尺寸 + 工作区 clamp。
///
/// **不接受前端的 x/y**：`MouseEvent.screenX/Y` 在 WebView2 里是 **CSS 像素**，
/// 高 DPI 屏（如 150%）直接当物理像素用会偏 1/3 位置；多屏跨 DPI 更乱。
/// 光标物理坐标由 Win32 直接给，绕过所有浏览器坐标系猜谜。
///
/// 返回值 `(x, y, width, height)` 均为**物理像素**，可直接传给 `PhysicalSize` / `PhysicalPosition`。
///
/// - `css_w/h`：菜单的 CSS 像素尺寸（前端估算值，会按目标屏 DPI 缩放）
pub fn clamp_context_menu(css_w: f64, css_h: f64) -> (i32, i32, u32, u32) {
    unsafe {
        // 光标物理坐标（进程需 DPI-aware，Tauri 默认 PerMonitorV2 已满足）
        let mut pt = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut pt);
        let screen_x = pt.x;
        let screen_y = pt.y;
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);

        // 获取目标显示器工作区（排除任务栏）
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let work = if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            // fallback：拿不到就用主屏
            let hmon_primary = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut mi2: MONITORINFO = std::mem::zeroed();
            mi2.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(hmon_primary, &mut mi2).as_bool() {
                mi2.rcWork
            } else {
                // 极端兜底：返回原坐标原尺寸
                return (screen_x, screen_y, css_w as u32, css_h as u32);
            }
        };

        // 0.11.9：走公共 DPI helper
        let dpi_x = crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon);
        let scale = crate::infra::platform::dpi::scale_factor(dpi_x);

        // CSS 像寸 → 物理像素
        let phys_w = (css_w * scale).round() as i32;
        let phys_h = (css_h * scale).round() as i32;

        // 智能翻转：右/下空间不够时，菜单显示在光标左/上方（老 0.5.3+ 前端行为）
        //   贴边 clamp 会让菜单紧贴屏幕右/下边缘，视觉上像是"卡住"了。
        let margin = 4;
        let prefer_x = if screen_x + phys_w + margin > work.right {
            (screen_x - phys_w).max(work.left + margin)
        } else {
            screen_x
        };
        let prefer_y = if screen_y + phys_h + margin > work.bottom {
            (screen_y - phys_h).max(work.top + margin)
        } else {
            screen_y
        };
        // 再做一次工作区 clamp（防单块屏幕比菜单还小的极端情况）
        let max_x = work.right - phys_w - margin;
        let max_y = work.bottom - phys_h - margin;
        let x = prefer_x.clamp(work.left + margin, max_x.max(work.left + margin));
        let y = prefer_y.clamp(work.top + margin, max_y.max(work.top + margin));

        tracing::trace!(
            screen_x,
            screen_y,
            css_w,
            css_h,
            dpi = dpi_x,
            scale,
            phys_w,
            phys_h,
            work_left = work.left,
            work_top = work.top,
            work_right = work.right,
            work_bottom = work.bottom,
            final_x = x,
            final_y = y,
            "clamp_context_menu: 多屏定位"
        );

        (x, y, phys_w as u32, phys_h as u32)
    }
}

/// 给窗口加 WS_EX_NOACTIVATE——点击不激活，用户能回原应用选文本。
/// 被 voice-overlay / chord-screenshot 等次级窗口复用（划词场景已移除，但
/// WS_EX_NOACTIVATE 对不抢焦点的 overlay 窗口通用）。
fn apply_no_activate(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{GWL_EXSTYLE, WS_EX_NOACTIVATE};
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize);
    }
}

/// 获取当前前台窗口的 HWND（供 G2 注入前恢复焦点用）。
pub fn get_foreground_hwnd() -> Option<isize> {
    unsafe {
        let hwnd = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
        if hwnd.is_invalid() {
            None
        } else {
            Some(hwnd.0 as isize)
        }
    }
}

/// 返回 Blink 最近一次唤起、尚未抢焦点时记录的外部前台窗口。
pub fn last_external_foreground_hwnd() -> Option<isize> {
    let hwnd = LAST_EXTERNAL_HWND.load(Ordering::SeqCst);
    (hwnd != 0).then_some(hwnd)
}

/// 恢复前台窗口焦点（G2 注入文本前调用）。
///
/// Alt+Space 唤起 Blink 时，组合键到达前台应用会弹出系统菜单（Alt+Space 的系统行为），
/// 导致焦点从文本输入框漂移到系统菜单。本函数负责在注入前修复焦点：
///
/// 1. **WM_CANCELMODE**：关闭 Alt+Space 弹出的系统菜单（DefWindowProc → EndMenu），
///    无副作用（不像 ESC 会关对话框/清输入）。
/// 2. **AttachThreadInput + SetForegroundWindow**：恢复前台窗口，绕过 Windows 前台锁定。
///    不使用 Alt 欺骗——合成 Alt keydown 会被目标应用接收，在 Electron/Chromium 上
///    可能激活菜单栏，反而干扰焦点。
/// 3. **UIA SetFocus**：关闭系统菜单后 Windows 自动恢复焦点到弹出前的控件，
///    但不保证可靠。用 UIA `GetFocusedElement` + `SetFocus` 保险——如果焦点恢复后
///    的控件是文本输入框（Edit/Document），主动 SetFocus 确保焦点到位。
///
/// > **不吞键时 Alt+Space 只触发系统菜单，不触发 Alt tap 菜单栏激活**——
/// > 因为 Alt keydown→keyup 之间有 Space 到达应用，Windows 不判定为 Alt tap。
/// > 所以只需关闭系统菜单，不需要处理 Chromium 菜单栏。
pub fn restore_foreground(hwnd: isize) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowThreadProcessId, PostMessageW, SetForegroundWindow, WM_CANCELMODE,
    };

    let target_hwnd = HWND(hwnd as *mut _);
    if target_hwnd.is_invalid() {
        return;
    }

    unsafe {
        // 1. 关闭 Alt+Space 弹出的系统菜单（异步投递，无副作用）
        let _ = PostMessageW(Some(target_hwnd), WM_CANCELMODE, WPARAM(0), LPARAM(0));

        // 2. AttachThreadInput + SetForegroundWindow（恢复前台窗口，不使用 Alt 欺骗）
        let current_tid = GetCurrentThreadId();
        let mut target_pid: u32 = 0;
        let target_tid = GetWindowThreadProcessId(target_hwnd, Some(&mut target_pid));

        if target_tid != 0 && target_tid != current_tid {
            let attached = AttachThreadInput(current_tid, target_tid, true);
            let _ = SetForegroundWindow(target_hwnd);
            if attached.as_bool() {
                let _ = AttachThreadInput(current_tid, target_tid, false);
            }
        } else {
            let _ = SetForegroundWindow(target_hwnd);
        }
    }

    // 3. UIA 焦点恢复（保险）：关闭系统菜单后 Windows 自动恢复焦点，
    //    但不保证可靠。用 UIA GetFocusedElement + SetFocus 主动恢复。
    //    等 50ms 让菜单关闭 + 焦点自动恢复完成，再检查。
    std::thread::sleep(std::time::Duration::from_millis(50));

    if let Some(elem) = crate::infra::platform::uia::get_focused_element() {
        // 焦点已恢复到某个元素——如果它是文本输入控件，主动 SetFocus 确保到位
        let ct = unsafe { elem.CurrentControlType() }
            .map(|t| t.0)
            .unwrap_or(0);
        if crate::infra::platform::uia::is_text_input_control(ct) {
            tracing::debug!(
                control_type = ct,
                "restore_foreground: 焦点在文本输入控件，SetFocus"
            );
            let _ = crate::infra::platform::uia::set_focused_element(&elem);
        } else {
            tracing::debug!(
                control_type = ct,
                "restore_foreground: 焦点不在文本输入控件，不强制 SetFocus"
            );
        }
    } else {
        tracing::debug!(
            "restore_foreground: GetFocusedElement 返回 None（UIA 不可用或无前台窗口）"
        );
    }
}

/// 显示独立 AI 对话窗口（0.12.1）。
///
/// 与 voice-overlay 不同：对话窗口需要接收键盘输入，因此不加 `WS_EX_NOACTIVATE`；
/// 首次运行时创建，后续复用同一 WebView，避免重复窗口和状态分裂。
///
/// **生命周期**（Phase 3A）：点击关闭→隐藏不销毁；隐藏先 abort active request。
/// CloseRequested handler 只注册一次的标记。
static CHAT_CLOSE_HANDLER_REGISTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn show_chat_window(app: &AppHandle, initial_text: Option<&str>) -> Result<(), String> {
    use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

    const LABEL: &str = "chat";

    // 0.19：带初始文本时先写入 prefill（infra 层），拿到 revision 用于后续 ack/rollback。
    let prefill_text: Option<&str> = initial_text.filter(|s| !s.is_empty());
    let prefill_rev: Option<u64> = prefill_text.map(|t| set_chat_prefill(t));

    // 0.19：经 get_or_create_window 串行化创建，消除预热与用户唤起的 duplicate label 竞态。
    // 统一创建配置：visible(false) + focused(false)，show/focus 在锁外统一执行。
    let (win, _is_new) = match get_or_create_window(app, LABEL, || {
        WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("chat.html".into()))
            .title("Blink AI")
            .inner_size(CHAT_W, CHAT_H)
            .min_inner_size(CHAT_MIN_W, CHAT_MIN_H)
            .decorations(false)
            .transparent(false)
            .always_on_top(false)
            .skip_taskbar(false)
            .resizable(true)
            .focused(false)
            .visible(false)
            .build()
    }) {
        Ok(v) => v,
        Err(e) => {
            // build 失败：回滚本次 pending
            if let Some(rev) = prefill_rev {
                clear_chat_prefill(rev);
            }
            return Err(e);
        }
    };

    // 0.12.4 §6.6：安装系统菜单拦截器 + 圆角（与主窗口一致）
    // install_sysmenu_blocker 内部按 HWND 去重，重复调用安全（0.12.5 修复递归 BUG）
    if let Ok(hwnd) = win.hwnd() {
        let hwnd = windows::Win32::Foundation::HWND(hwnd.0 as _);
        install_sysmenu_blocker(hwnd);
        enable_rounded_corners(hwnd);
    }

    // CloseRequested handler：只注册一次（预热窗口复用时不会重复注册）
    if !CHAT_CLOSE_HANDLER_REGISTERED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        let app_clone = app.clone();
        win.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Some(cs) = app_clone
                    .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
                {
                    cs.abort_active();
                }
                if let Some(w) = app_clone.get_webview_window("chat") {
                    let _ = w.hide();
                }
                tracing::debug!("chat window: CloseRequested → prevent_close + hide");
            }
        });
    }

    // 每次显示时居中到当前屏幕（与主窗口行为一致）
    let _ = win.center();
    // show 失败：回滚 pending 后返回错误
    if let Err(e) = win.show() {
        if let Some(rev) = prefill_rev {
            clear_chat_prefill(rev);
        }
        return Err(format!("显示 chat 窗口失败: {e}"));
    }
    let _ = win.unminimize();

    // 0.19：交付 prefill（emit 加速 + pending 兜底）。
    // 顺序：show → unminimize → 交付 prefill → set_focus
    // set_focus 失败不阻断 prefill 投递——窗口已可见，文本必须送达。
    if let (Some(rev), Some(text)) = (prefill_rev, prefill_text) {
        // emit {revision, text} —— 热窗口 listener 立即收到，冷窗口走 take 兜底
        let payload = serde_json::json!({ "revision": rev, "text": text });
        if let Err(e) = app.emit_to(
            LABEL,
            crate::domain::event_names::EventNames::CHAT_PREFILL,
            payload,
        ) {
            tracing::warn!(error = %e, "chat-prefill emit 失败");
        }
    }

    // set_focus 放最后：失败只 warn，不返回 Err——窗口已显示、prefill 已投递。
    if let Err(e) = win.set_focus() {
        tracing::warn!(error = %e, "chat window: 显示成功但聚焦失败");
    }

    tracing::info!("chat window: 已显示");
    Ok(())
}

/// 隐藏 chat 窗口（Phase 3A）。
///
/// 先中止 active request，再隐藏窗口。若窗口不存在则 no-op。
pub fn hide_chat_window(app: &AppHandle) {
    // 先 abort active request
    if let Some(cs) =
        app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
    {
        cs.abort_active();
    }
    // 再隐藏窗口
    if let Some(win) = app.get_webview_window("chat") {
        let _ = win.hide();
        tracing::debug!("chat window: 已隐藏");
    }
}

/// 显示内容编辑器窗口（0.16.3）。
///
/// 独立 Tauri 窗口，按需创建（不预热）。窗口关闭即销毁，不 prevent_close。
/// 看门狗按 PID 判定，前台切到编辑器时主窗不会被误隐藏。
/// payload 经 PendingEditorPayload State 中转，前端 init 时调 get_content_editor_payload 拉取。
pub fn show_content_editor_window(app: &AppHandle) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder, window::Color};

    const LABEL: &str = "content-editor";
    let is_new = app.get_webview_window(LABEL).is_none();

    let win = if is_new {
        // 0.16.13 fix：改回 .visible(true) + background_color 消除白屏闪烁。
        // 之前的 .visible(false) + 前端 init 调 win.show() 方案在首次点击时
        // 因 WebView2 冷启动加载 JS 模块耗时，窗口长时间不可见，用户感知为「没反应」。
        // background_color 设为 dark 主题底色 #1e1e2e，CSS 加载前不闪白。
        WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("content-editor.html".into()))
            .title("编辑内容")
            .inner_size(EDITOR_W, EDITOR_H)
            .min_inner_size(EDITOR_MIN_W, EDITOR_MIN_H)
            .decorations(false)
            .transparent(false)
            .always_on_top(false)
            .skip_taskbar(false)
            .resizable(true)
            .focused(true)
            .visible(true)
            .background_color(Color(30, 30, 46, 255))
            .center()
            .build()
            .map_err(|e| {
                tracing::warn!(error = %e, "content-editor window: 创建失败");
                format!("创建编辑器窗口失败: {e}")
            })?
    } else {
        // 复用已有窗口——前端需重新拉取 payload
        let win = app.get_webview_window(LABEL).unwrap();
        let _ = win.eval("window.__contentEditorReload && window.__contentEditorReload()");
        win
    };

    // 系统菜单拦截 + 圆角（与 chat 窗口一致）
    if let Ok(hwnd) = win.hwnd() {
        let hwnd = HWND(hwnd.0 as _);
        install_sysmenu_blocker(hwnd);
        enable_rounded_corners(hwnd);
    }

    // 复用窗口可能被 hide 了，需要重新 show；新窗口已 visible(true) 创建
    if !is_new {
        win.show().map_err(|e| format!("显示编辑器窗口失败: {e}"))?;
    }
    let _ = win.unminimize();
    win.set_focus()
        .map_err(|e| format!("聚焦编辑器窗口失败: {e}"))?;

    tracing::info!("content-editor window: 已显示");
    Ok(())
}

/// 显示便签管理窗口（0.16.10）。
///
/// 独立 Tauri 窗口，label 为 `sticky-manager`。按需创建（不预热）。
/// 窗口关闭即销毁，不 prevent_close。
/// 看门狗按 PID 判定，前台切到管理窗口时主窗不会被误隐藏。
pub fn show_sticky_manager_window(app: &AppHandle) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder, window::Color};

    const LABEL: &str = "sticky-manager";
    let is_new = app.get_webview_window(LABEL).is_none();

    let win = if is_new {
        // 0.16.13 fix：改回 .visible(true) + background_color 消除白屏闪烁。
        // 0.17.7：background_color 从硬编码 #1e1e2e（dark only）改为中性灰 #333333，
        // 在 light / dark 主题下都不会产生突兀的色差（CSS 加载后由 .manager-root 覆盖）。
        let w =
            WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("sticky-manager.html".into()))
                .title("便签管理")
                .inner_size(MANAGER_W, MANAGER_H)
                .min_inner_size(MANAGER_MIN_W, MANAGER_MIN_H)
                .decorations(false)
                .transparent(false)
                .always_on_top(false)
                .skip_taskbar(false)
                .resizable(true)
                .focused(true)
                .visible(true)
                .background_color(Color(51, 51, 51, 255))
                .center()
                .build()
                .map_err(|e| {
                    tracing::warn!(error = %e, "sticky-manager window: 创建失败");
                    format!("创建便签管理窗口失败: {e}")
                })?;

        // prevent_close + hide——与 chat/content-editor 一致的复用模式
        let app_clone = app.clone();
        w.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if IS_APP_EXITING.load(Ordering::SeqCst) {
                    return; // 应用退出：不 prevent_close
                }
                api.prevent_close();
                if let Some(w) = app_clone.get_webview_window(LABEL) {
                    let _ = w.hide();
                }
                tracing::debug!("sticky-manager window: CloseRequested → prevent_close + hide");
            }
        });
        w
    } else {
        let win = app.get_webview_window(LABEL).unwrap();
        let _ = win.eval("window.__stickyManagerReload && window.__stickyManagerReload()");
        win
    };

    if let Ok(hwnd) = win.hwnd() {
        let hwnd = HWND(hwnd.0 as _);
        install_sysmenu_blocker(hwnd);
        enable_rounded_corners(hwnd);
    }

    // 复用窗口可能被 hide 了，需要重新 show；新窗口已 visible(true) 创建
    if !is_new {
        win.show()
            .map_err(|e| format!("显示便签管理窗口失败: {e}"))?;
    }
    let _ = win.unminimize();
    win.set_focus()
        .map_err(|e| format!("聚焦便签管理窗口失败: {e}"))?;

    tracing::info!("sticky-manager window: 已显示");
    Ok(())
}

/// 0.17.3：显示首次启动引导窗口。
///
/// 独立窗口（label "welcome"），480×500 居中，有标题栏（decorations: true），
/// 不可调整大小。关闭时自动标记 `first_run = false`（防止用户点 X 不点"开始使用"）。
/// 与主窗口独立——watchdog 只 hide "main" 窗口，不影响引导窗口。
pub fn show_welcome_window(app: &AppHandle) {
    const LABEL: &str = "welcome";

    // 已存在则直接 show + focus（安全兜底，正常不会走到——first_run=false 后不再弹）
    if let Some(win) = app.get_webview_window(LABEL) {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }

    use tauri::{WebviewUrl, WebviewWindowBuilder, window::Color};

    let win = match WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("welcome.html".into()))
        .title("Blink")
        .inner_size(480.0, 500.0)
        .resizable(false)
        .decorations(true)
        .transparent(false)
        .always_on_top(false)
        .skip_taskbar(false)
        .focused(true)
        .visible(true)
        .background_color(Color(30, 30, 46, 255))
        .center()
        .build()
    {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "welcome window: 创建失败");
            return;
        }
    };

    // 关闭时标记 first_run = false（防用户点 X 不点"开始使用"按钮）
    let app_clone = app.clone();
    win.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { .. } = event {
            let app = app_clone.clone();
            tauri::async_runtime::spawn(async move {
                let pools = app.state::<crate::infra::data::DbPools>();
                let _ = crate::app::config::update_first_run(&pools.config, false).await;
                tracing::info!("welcome window: CloseRequested -> first_run = false");
            });
        }
    });

    tracing::info!("welcome window: 已显示");
}

/// 更新便签窗口的任务栏可见性（置顶→跳过任务栏，非置顶→显示任务栏）。
///
/// 在 `set_sticky_always_on_top` 命令中调用，使 toggle 立即生效于已打开的窗口。
/// 窗口可能尚未创建（便签在管理器中 toggle 但桌面窗口未打开）——此时 no-op，
/// 下次 `show_sticky_window` 会按 DB 中的 `always_on_top` 正确设置。
pub fn update_sticky_taskbar(app: &AppHandle, sticky_id: &str, always_on_top: bool) {
    let truncated_id: String = sticky_id.chars().take(64).collect();
    let label = format!("sticky-{truncated_id}");
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.set_skip_taskbar(always_on_top);
        return;
    }
    // 尝试已借出的 spare 窗口
    if let Some(bl) = spare_borrow()
        .lock()
        .unwrap()
        .iter()
        .find(|(_, sid)| sid.as_str() == sticky_id)
        .map(|(l, _)| l.clone())
    {
        if let Some(win) = app.get_webview_window(&bl) {
            let _ = win.set_skip_taskbar(always_on_top);
        }
    }
}

/// 显示便签窗口（0.16.8）。
///
/// 每条便签一个独立 Tauri 窗口，label 为 `sticky-{id}`（id 截断到 60 字符防止超长）。
/// 窗口位置、尺寸、置顶状态从 StickyNote 数据恢复。
/// 关闭按钮 = 隐藏（prevent_close），不销毁窗口——下次显示复用同一 webview。
///
/// **看门狗安全**：看门狗按 PID 判定，前台切到便签时 `fg_pid == self_pid`，主窗不会被误隐藏。
pub fn show_sticky_window(
    app: &AppHandle,
    sticky_id: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    always_on_top: bool,
    focus: bool,
) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    // 0.16.11：安全截断——按字符而非字节切片，避免非 ASCII ID 截断 panic。
    // sticky_id 实际都是 ASCII（generate_id 产 sticky_{nanos}），但做防御性编程。
    let truncated_id: String = sticky_id.chars().take(64).collect();
    let label = format!("sticky-{truncated_id}");

    // 0.18.4：新建便签（x=0 && y=0）定位到鼠标所在屏的工作区中心。
    // 此逻辑原先只在路径4（全新建窗口）中，但 N+1 预热机制下 spare 几乎总可用，
    // 新便签走路径3（借用 spare），导致跳过居中 → 定位到 (0,0) 角落。
    // 提到分支之前，确保所有路径一致居中。
    let (x, y) = if x == 0 && y == 0 {
        compute_center_position(width, height).unwrap_or((x, y))
    } else {
        (x, y)
    };

    let is_new = app.get_webview_window(&label).is_none();

    // 0.18.3 N+1：检查此 sticky 是否已借出在某个 spare 窗口中
    let borrowed_label = spare_borrow()
        .lock()
        .unwrap()
        .iter()
        .find(|(_, sid)| sid.as_str() == sticky_id)
        .map(|(l, _)| l.clone());

    let win = if !is_new {
        // 复用已有 sticky-{id} 窗口
        let (cx, cy, cw, ch) = clamp_sticky_geometry(x, y, width, height);
        let win = app.get_webview_window(&label).ok_or_else(|| {
            tracing::warn!(label = %label, "复用便签窗口时发现窗口已不存在");
            "便签窗口在复用时已不存在".to_string()
        })?;
        let scale = unsafe {
            let pt = POINT { x: cx, y: cy };
            let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            crate::infra::platform::dpi::scale_factor(
                crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon),
            )
        };
        let _ = win.set_size(tauri::LogicalSize::new(
            cw as f64 / scale,
            ch as f64 / scale,
        ));
        let _ = win.set_position(tauri::PhysicalPosition::new(cx, cy));
        let _ = win.set_always_on_top(always_on_top);
        // 非置顶时显示在任务栏，让用户能找回便签；置顶时跳过任务栏
        let _ = win.set_skip_taskbar(always_on_top);
        let escaped_id = sticky_id
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        let _ = win.eval(&format!(
            "if (window.__stickyReload) window.__stickyReload('{escaped_id}')"
        ));
        win
    } else if let Some(bl) = borrowed_label {
        // 复用已借出的 spare 窗口（同一便签再次唤起）
        tracing::debug!(sticky_id, spare_label = %bl, "sticky window: 复用已借出 spare");
        let (cx, cy, cw, ch) = clamp_sticky_geometry(x, y, width, height);
        let win = app
            .get_webview_window(&bl)
            .ok_or_else(|| "便签窗口在复用时已不存在".to_string())?;
        let scale = unsafe {
            let pt = POINT { x: cx, y: cy };
            let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            crate::infra::platform::dpi::scale_factor(
                crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon),
            )
        };
        let _ = win.set_size(tauri::LogicalSize::new(
            cw as f64 / scale,
            ch as f64 / scale,
        ));
        let _ = win.set_position(tauri::PhysicalPosition::new(cx, cy));
        let _ = win.set_always_on_top(always_on_top);
        // 非置顶时显示在任务栏，让用户能找回便签；置顶时跳过任务栏
        let _ = win.set_skip_taskbar(always_on_top);
        let escaped_id = sticky_id
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        let _ = win.eval(&format!(
            "if (window.__stickyReload) window.__stickyReload('{escaped_id}')"
        ));
        win
    } else {
        // 尝试借用空闲 spare
        let available_label = available_spare().lock().unwrap().take();
        if let Some(al) = available_label {
            // 借用预热窗口
            tracing::debug!(sticky_id, spare_label = %al, "sticky window: 借用预热 spare");
            spare_borrow()
                .lock()
                .unwrap()
                .insert(al.clone(), sticky_id.to_string());

            let (cx, cy, cw, ch) = clamp_sticky_geometry(x, y, width, height);
            let scale = unsafe {
                let pt = POINT { x: cx, y: cy };
                let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
                crate::infra::platform::dpi::scale_factor(
                    crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon),
                )
            };
            let spare_win = app
                .get_webview_window(&al)
                .ok_or_else(|| "预热窗口不存在".to_string())?;
            let _ = spare_win.set_size(tauri::LogicalSize::new(
                cw as f64 / scale,
                ch as f64 / scale,
            ));
            let _ = spare_win.set_position(tauri::PhysicalPosition::new(cx, cy));
            let _ = spare_win.set_always_on_top(always_on_top);
            // 非置顶时显示在任务栏，让用户能找回便签；置顶时跳过任务栏
            let _ = spare_win.set_skip_taskbar(always_on_top);
            let escaped_id = sticky_id
                .replace('\\', "\\\\")
                .replace('\'', "\\'")
                .replace('\n', "\\n")
                .replace('\r', "\\r");
            let _ = spare_win.eval(&format!(
                "if (window.__stickyReload) window.__stickyReload('{escaped_id}')"
            ));

            if let Ok(hwnd) = spare_win.hwnd() {
                let hwnd = HWND(hwnd.0 as _);
                install_sysmenu_blocker(hwnd);
                enable_rounded_corners(hwnd);
            }
            spare_win
                .show()
                .map_err(|e| format!("显示便签窗口失败: {e}"))?;
            let _ = spare_win.unminimize();
            if focus {
                spare_win
                    .set_focus()
                    .map_err(|e| format!("聚焦便签窗口失败: {e}"))?;
            }

            tracing::info!(sticky_id, focus, "sticky window: 已显示（预热借用）");

            // N+1：spare 被借用后，后台延迟创建新的备用窗口
            let app_clone = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                create_sticky_spare(&app_clone);
                tracing::debug!("sticky-spare: N+1 补充完成");
            });

            return Ok(());
        }

        // 无可用 spare，创建新窗口（URL 带 sticky_id 参数）
        // P3-#22 fix: URL 编码防注入——sticky_id 来自前端任意字符串
        let encoded_id = sticky_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c.to_string()
                } else {
                    format!("%{:02X}", c as u32)
                }
            })
            .collect::<String>();
        let url = format!("sticky.html?id={encoded_id}");

        // 0.16.11：几何钳制——显示器拔插/分辨率变化后保证窗口至少部分可见
        let (cx, cy, cw, ch) = clamp_sticky_geometry(x, y, width, height);

        // 0.16.10 fix P0-#7: inner_size 接受逻辑像素，需将物理像素转换为逻辑
        let scale = unsafe {
            let pt = POINT { x: cx, y: cy };
            let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            crate::infra::platform::dpi::scale_factor(
                crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon),
            )
        };
        let logical_w = cw as f64 / scale;
        let logical_h = ch as f64 / scale;

        let w = WebviewWindowBuilder::new(app, &label, WebviewUrl::App(url.into()))
            .title("便签")
            .inner_size(logical_w, logical_h)
            .min_inner_size(STICKY_MIN_W, STICKY_MIN_H)
            .position(cx as f64, cy as f64)
            .decorations(false)
            .transparent(false)
            .always_on_top(always_on_top)
            .skip_taskbar(always_on_top)
            .resizable(true)
            .focused(focus)
            .visible(true)
            // 0.17.7：background_color 设为默认黄色（与 --sticky-bg: #fff9c4 对齐），
            // 避免 CSS 加载前闪白 / 分数 DPI 下透明背景产生 tile seam。
            .background_color(tauri::window::Color(255, 249, 196, 255))
            .build()
            .map_err(|e| {
                tracing::warn!(error = %e, "sticky window: 创建失败");
                format!("创建便签窗口失败: {e}")
            })?;

        // 注册 CloseRequested handler：仅新窗口注册一次，避免复用时重复绑定
        //
        // 0.20.0：关闭 = 原子关闭工作流（空→删除，非空→保存+回收站）。
        // - 用户关闭（前端按钮/ESC）：前端直接调 closeStickyNote API，不走 CloseRequested。
        // - 此 handler 是兜底路径（系统 Alt+F4 等）：prevent_close + flush + close_sticky_and_notify + hide
        // - 应用退出：不 prevent_close，不修改 trashed
        // - spare 窗口回收不走此路径（spare 有独立的 CloseRequested handler）
        let label_owned = label.clone();
        let app_clone = app.clone();
        let sid = sticky_id.to_string();
        w.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if IS_APP_EXITING.load(Ordering::SeqCst) {
                    // 应用退出：不 prevent_close，不修改 trashed
                    tracing::debug!(
                        sticky_id = %sid,
                        "sticky window: CloseRequested during app exit → 不修改 trashed"
                    );
                    return;
                }
                api.prevent_close();
                // P1-#12 fix: 关闭前 flush 未保存内容（前端有 500ms 防抖）
                if let Some(w) = app_clone.get_webview_window(&label_owned) {
                    let _ = w.eval("if (window.__stickyFlush) window.__stickyFlush();");
                }
                // 0.20.0：兜底路径用 trash（flush 已保存最新内容到 DB）
                // 前端按钮/ESC 走 closeStickyNote API 实现原子关闭（空→删除）
                let app_c = app_clone.clone();
                let sid_owned = sid.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(env) = app_c
                        .try_state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
                    {
                        // flush 已把最新内容写入 DB → trash 保存最终状态
                        if let Err(e) = env.trash_sticky_and_notify(&sid_owned).await {
                            tracing::warn!(error = %e, sticky_id = %sid_owned, "便签兜底关闭失败");
                        }
                    } else {
                        tracing::warn!("便签关闭时 TauriDomainEnv 不可用，跳过");
                    }
                });
                if let Some(w) = app_clone.get_webview_window(&label_owned) {
                    let _ = w.hide();
                }
                tracing::debug!(sticky_id = %sid, "sticky window: CloseRequested → prevent_close + flush + close + hide");
            }
        });
        w
    };

    // 系统菜单拦截 + 圆角
    if let Ok(hwnd) = win.hwnd() {
        let hwnd = HWND(hwnd.0 as _);
        install_sysmenu_blocker(hwnd);
        enable_rounded_corners(hwnd);
    }

    win.show().map_err(|e| format!("显示便签窗口失败: {e}"))?;
    let _ = win.unminimize();
    // 0.16.11：恢复路径（focus=false）不抢焦点，不影响主窗口 Alt+Space
    if focus {
        win.set_focus()
            .map_err(|e| format!("聚焦便签窗口失败: {e}"))?;
    }

    tracing::info!(sticky_id, focus, "sticky window: 已显示");
    Ok(())
}

/// 0.16.11：标记应用正在退出。
///
/// 在 `RunEvent::Exit` 时调用，让便签窗口的 CloseRequested handler 知道
/// 这是应用整体退出而非用户关闭单条便签。
pub fn set_app_exiting() {
    IS_APP_EXITING.store(true, Ordering::SeqCst);
    tracing::debug!("set_app_exiting: IS_APP_EXITING → true");
}

/// 0.17.6：设置主窗口 AI 活跃标志。
///
/// `active = true`：watchdog 跳过失焦隐藏，主窗口 AI 生成过程中不会被意外隐藏。
/// `active = false`：恢复正常 watchdog 行为。
///
/// 调用时机：`chat_prompt(target="main")` 成功后设 true；
/// Done/Error/abort/hide_window 时设 false。
pub fn set_main_ai_active(active: bool) {
    MAIN_WINDOW_AI_ACTIVE.store(active, Ordering::SeqCst);
    tracing::debug!(active, "set_main_ai_active: MAIN_WINDOW_AI_ACTIVE");
}

/// 0.17.6：查询主窗口 AI 是否活跃。
pub fn is_main_ai_active() -> bool {
    MAIN_WINDOW_AI_ACTIVE.load(Ordering::Relaxed)
}

/// 0.20.4：设置图片编辑器活跃标志。
///
/// `active = true`：watchdog 跳过 screenshot overlay 失焦隐藏，
/// 防止主窗口关闭或焦点切换导致编辑器被误关。
/// `active = false`：恢复正常 watchdog 行为。
///
/// 调用时机：`show_image_editor_window` 设 true；
/// `hide_image_editor_window` / `hide_screenshot_overlay` 设 false。
pub fn set_image_editor_active(active: bool) {
    IMAGE_EDITOR_ACTIVE.store(active, Ordering::SeqCst);
    tracing::debug!(active, "set_image_editor_active: IMAGE_EDITOR_ACTIVE");
}

/// 0.16.11：退出前 flush 所有便签窗口的未保存内容。
///
/// 前端有 500ms 内容防抖和 300ms 几何防抖。退出时 eval flush JS，
/// 让前端立即写入后端，避免丢失最近 500ms 的编辑。
/// 返回 flush 的窗口数量。
pub fn flush_all_sticky_windows(app: &AppHandle) -> usize {
    let mut count = 0usize;
    for (label, win) in app.webview_windows() {
        if !label.starts_with("sticky-") || label == "sticky-manager" {
            continue;
        }
        // eval flush——前端 __stickyFlush 立即调用后端保存
        let _ = win.eval("if (window.__stickyFlush) window.__stickyFlush();");
        count += 1;
    }
    if count > 0 {
        tracing::debug!(
            count,
            "flush_all_sticky_windows: 已向 {} 个便签窗口发送 flush",
            count
        );
    }
    count
}

/// 计算便签在当前前台窗口所在显示器工作区的居中坐标（0.16.11）。
///
/// 新建便签时调用，让便签出现在用户当前关注的屏幕中心而非 (0,0) 角落。
/// 返回 (x, y) 物理像素。
pub fn center_of_active_monitor(width: i32, height: i32) -> (i32, i32) {
    unsafe {
        let hwnd = GetForegroundWindow();
        let hmon = if hwnd.is_invalid() {
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
        } else {
            MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)
        };
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return (0, 0);
        }
        let work = mi.rcWork;
        let x = work.left + (work.right - work.left - width) / 2;
        let y = work.top + (work.bottom - work.top - height) / 2;
        (x, y)
    }
}

/// 0.16.11：钳制便签窗口几何到可见工作区。
///
/// 显示器拔插、分辨率/DPI 变化后，存储的 (x, y) 可能指向不存在的显示器。
/// 使用 `MonitorFromPoint` 查找位置所在显示器，找不到时 fallback 到主屏，
/// 然后钳制到工作区内，确保窗口至少部分可见。
///
/// 返回值 `(x, y, width, height)` 为钳制后的物理像素。
fn clamp_sticky_geometry(x: i32, y: i32, width: i32, height: i32) -> (i32, i32, i32, i32) {
    // 保证尺寸合理
    let w = width.max(120).min(4096);
    let h = height.max(80).min(4096);

    unsafe {
        let pt = POINT { x, y };
        // 先尝试指定位置所在显示器，找不到则取主屏
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            // 极端 fallback：拿不到显示器信息，原样返回（尺寸已 clamp）
            return (x, y, w, h);
        }
        let work = mi.rcWork; // 工作区（排除任务栏）

        // 钳制到工作区：确保窗口至少 80x60 像素可见
        let min_visible_w = 80i32;
        let min_visible_h = 60i32;

        // X：如果窗口完全在 Work 区左侧，移到 work.left；
        //     完全在右侧，移到 work.right - min_visible_w；
        //     部分可见且可见部分 >= min_visible_w，保持不动；
        //     部分可见但可见部分 < min_visible_w，调整使其至少 min_visible_w 可见
        let cx = if x + w <= work.left + min_visible_w {
            // 窗口在左边界外或几乎不可见
            work.left
        } else if x >= work.right - min_visible_w {
            // 窗口在右边界外或几乎不可见
            (work.right - w).max(work.left)
        } else {
            // 至少部分可见，保持
            x
        };

        let cy = if y + h <= work.top + min_visible_h {
            work.top
        } else if y >= work.bottom - min_visible_h {
            (work.bottom - h).max(work.top)
        } else {
            y
        };

        tracing::trace!(
            orig_x = x,
            orig_y = y,
            orig_w = width,
            orig_h = height,
            clamped_x = cx,
            clamped_y = cy,
            clamped_w = w,
            clamped_h = h,
            work_left = work.left,
            work_top = work.top,
            work_right = work.right,
            work_bottom = work.bottom,
            "clamp_sticky_geometry: 钳制完成"
        );

        (cx, cy, w, h)
    }
}

/// 隐藏便签窗口（不删除数据）。
pub fn hide_sticky_window(app: &AppHandle, sticky_id: &str) -> Result<(), String> {
    let truncated_id: String = sticky_id.chars().take(64).collect();
    let label = format!("sticky-{truncated_id}");
    if let Some(win) = app.get_webview_window(&label) {
        win.hide().map_err(|e| e.to_string())?;
        tracing::debug!(sticky_id, "sticky window: 已隐藏");
        return Ok(());
    }

    // 0.18.3 N+1：显示中的便签可能借用了 sticky-spare-* 窗口。
    let borrowed_label = spare_borrow()
        .lock()
        .unwrap()
        .iter()
        .find(|(_, sid)| sid.as_str() == sticky_id)
        .map(|(label, _)| label.clone());
    if let Some(label) = borrowed_label
        && let Some(win) = app.get_webview_window(&label)
    {
        win.hide().map_err(|e| e.to_string())?;
        tracing::debug!(sticky_id, spare_label = %label, "sticky spare: 已隐藏借出窗口");
    }
    Ok(())
}

/// 销毁便签窗口（删除数据后调用）。
pub fn destroy_sticky_window(app: &AppHandle, sticky_id: &str) {
    let truncated_id: String = sticky_id.chars().take(64).collect();
    let label = format!("sticky-{truncated_id}");
    if let Some(win) = app.get_webview_window(&label) {
        // 用 destroy() 而非 close()——close() 会触发 CloseRequested 被 prevent_close 拦截
        let _ = win.destroy();
        tracing::debug!(sticky_id, "sticky window: 已销毁");
    }
    // 0.18.3 N+1：也检查借出的 spare 窗口
    let borrowed_label = spare_borrow()
        .lock()
        .unwrap()
        .iter()
        .find(|(_, sid)| sid.as_str() == sticky_id)
        .map(|(l, _)| l.clone());
    if let Some(bl) = borrowed_label {
        spare_borrow().lock().unwrap().remove(&bl);
        if let Some(win) = app.get_webview_window(&bl) {
            let _ = win.destroy();
        }
        tracing::debug!(sticky_id, spare_label = %bl, "sticky spare: 已销毁借出窗口");
    }
}

/// 显示语音录音 mini overlay（0.10 G2）。
/// 独立 webview 窗口，不抢焦点（WS_EX_NOACTIVATE），显示在光标附近。
/// 录音结束后由 voice::VoiceService::stop_recording 发 voice-recording-end → 前端隐藏。
pub fn show_voice_overlay(app: &AppHandle) {
    const LABEL: &str = "voice-overlay";
    let (mx, my) = unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut pt);
        (pt.x, pt.y)
    };

    if let Some(win) = app.get_webview_window(LABEL) {
        // 0.10.6: 复用时重置尺寸为默认值（上次可能被 autoResize 撑高）
        let _ = win.set_size(tauri::LogicalSize::new(260.0, 140.0));
        let _ = win.set_position(tauri::PhysicalPosition::new(mx + 16, my + 16));
        let _ = win.show();
        return;
    }

    use tauri::{WebviewUrl, WebviewWindowBuilder};
    match WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("voice-overlay.html".into()))
        .title("")
        .inner_size(VOICE_W, VOICE_H)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .focused(false)
        .visible(true)
        .build()
    {
        Ok(win) => {
            let _ = win.set_position(tauri::PhysicalPosition::new(mx + 16, my + 16));
            if let Ok(hwnd) = win.hwnd() {
                apply_no_activate(HWND(hwnd.0 as _));
            }
            tracing::debug!("voice-overlay: 已显示");
        }
        Err(e) => tracing::warn!(error = %e, "voice-overlay: 创建失败"),
    }
}

/// 隐藏语音录音 mini overlay。
pub fn hide_voice_overlay(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("voice-overlay") {
        let _ = win.hide();
    }
}

/// 显示截图覆盖窗（0.8.7 §九）。
///
/// **前置条件**：调用方已通过 `screenshot::begin_session()` 完成截屏，SESSION 中
/// 已有位图；`meta` 是该 session 的元数据（物理像素坐标 + 尺寸）。
///
/// 流程：构建 overlay → SetWindowPos 按物理像素强制定位（绕开 Tauri 逻辑像素接口）
/// → 前端通过 `blink-screenshot://capture` 协议只读 SESSION 拿 PNG。
/// 前端拿到图后先铺暗色蒙版，用户拖选才显示亮区；ESC / 失焦 / 确认走 command 层。
pub fn show_screenshot_overlay(
    app: &AppHandle,
    meta: crate::infra::platform::screenshot::ScreenCaptureMeta,
) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    const LABEL: &str = "chord-screenshot";

    // ⚠️ 临时打桩日志（0.19.14 性能排查用），收尾时清理
    let t0 = std::time::Instant::now();

    // 注入原始物理显示器矩形（physicalDisplays），前端用 canvas 实测 renderScale 转换 CSS。
    // overlayDpi 仅诊断用，不参与坐标变换。
    // **复用窗口时序**：clear → place → inject meta → show → focus → 双 rAF 后 reload
    // 先清屏防止旧选区闪现，place 后注入物理 meta，show+focus 后等布局稳定再 reload。
    if let Some(win) = app.get_webview_window(LABEL) {
        // 0. 注入光标所在显示器索引——必须在 clearScreenshotVisual 之前，
        //    因为 clearScreenshotVisual 会启动 per-monitor 预取 fetch
        let active_display = crate::infra::platform::screenshot::active_display_index();
        let _ = win.eval(&format!(
            "window.__blinkActiveDisplay = {};",
            active_display
        ));
        // 1. 清屏——只清旧画面，不触发截图加载
        let _ = win
            .eval("window.__blinkClearScreenshotVisual && window.__blinkClearScreenshotVisual()");
        let t_clear = t0.elapsed();
        let mut overlay_dpi = 96u32;
        if let Ok(hwnd) = win.hwnd() {
            // 0.19.14：撤销 hide_screenshot_overlay 设的 cloak，否则 show 后窗口不可见
            apply_cloak(HWND(hwnd.0 as _), false);
            // 2. place
            place_at_physical(
                HWND(hwnd.0 as _),
                meta.virtual_x,
                meta.virtual_y,
                meta.width,
                meta.height,
            );
            // 0.20.4-fix：图片编辑器可能用 cancel_topmost 取消了置顶，
            // 截图 overlay 必须恢复 topmost 以覆盖全屏。
            force_topmost(HWND(hwnd.0 as _));
            overlay_dpi = crate::infra::platform::dpi::get_dpi_for_hwnd(HWND(hwnd.0 as _));
        }
        let t_place = t0.elapsed();
        let displays_json = build_physical_displays_json();
        tracing::debug!(
            overlay_dpi,
            physical_displays = %displays_json,
            "show_screenshot_overlay (reuse): physical displays injected"
        );
        let fg_hwnd = crate::infra::platform::screenshot::session_fg_hwnd().unwrap_or(0);
        // 3. 注入物理 meta
        let meta_js = format!(
            "window.__blinkScreenMeta = {{ vx: {}, vy: {}, w: {}, h: {}, overlayDpi: {}, fgHwnd: {}, activeDisplay: {}, physicalDisplays: {} }};",
            meta.virtual_x,
            meta.virtual_y,
            meta.width,
            meta.height,
            overlay_dpi,
            fg_hwnd,
            active_display,
            displays_json
        );
        let _ = win.eval(&meta_js);
        // 4. show + 5. focus
        let _ = win.show();
        let _ = win.set_focus();
        let t_show = t0.elapsed();
        // 6. 双 rAF 后 reload——等布局稳定再加载截图，确保 canvas 已有正确尺寸
        let _ = win.eval(
            "requestAnimationFrame(()=>{requestAnimationFrame(()=>{window.__blinkReloadScreenshot&&window.__blinkReloadScreenshot()})})"
        );
        tracing::info!(
            total_ms = t0.elapsed().as_millis() as u64,
            clear_ms = t_clear.as_millis() as u64,
            place_ms = (t_place - t_clear).as_millis() as u64,
            show_focus_ms = (t_show - t_place).as_millis() as u64,
            path = "reuse",
            "show_screenshot_overlay 完成"
        );
        return Ok(());
    }

    // 首次构建：inner_size / position 会被后续 SetWindowPos 覆盖，这里只是让 Tauri 别报参数错。
    let t_build_start = t0.elapsed();
    let win =
        WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("chord-screenshot.html".into()))
            .title("")
            .inner_size(meta.width as f64, meta.height as f64)
            .position(meta.virtual_x as f64, meta.virtual_y as f64)
            .decorations(false)
            .resizable(false) // 禁用原生 resize 边框，防止屏幕边缘出现 resize 双箭头并误触 blur
            .transparent(true) // 透明背景，让 canvas 上的桌面截图独占视觉
            .always_on_top(true)
            .skip_taskbar(true)
            .shadow(false)
            .focused(true)
            .build()
            .map_err(|e| e.to_string())?;
    let t_build = t0.elapsed();

    if let Ok(hwnd) = win.hwnd() {
        place_at_physical(
            HWND(hwnd.0 as _),
            meta.virtual_x,
            meta.virtual_y,
            meta.width,
            meta.height,
        );
    }
    let t_place = t0.elapsed();
    // place 后读窗口实际 DPI（仅诊断用）
    let overlay_dpi = win
        .hwnd()
        .ok()
        .map(|h| crate::infra::platform::dpi::get_dpi_for_hwnd(HWND(h.0 as _)))
        .unwrap_or(96);
    let displays_json = build_physical_displays_json();
    tracing::debug!(
        overlay_dpi,
        physical_displays = %displays_json,
        "show_screenshot_overlay (first build): physical displays injected"
    );
    let fg_hwnd = crate::infra::platform::screenshot::session_fg_hwnd().unwrap_or(0);
    let active_display = crate::infra::platform::screenshot::active_display_index();
    let meta_js = format!(
        "window.__blinkScreenMeta = {{ vx: {}, vy: {}, w: {}, h: {}, overlayDpi: {}, fgHwnd: {}, activeDisplay: {}, physicalDisplays: {} }};",
        meta.virtual_x,
        meta.virtual_y,
        meta.width,
        meta.height,
        overlay_dpi,
        fg_hwnd,
        active_display,
        displays_json
    );
    let _ = win.eval(&meta_js);
    let _ = win.set_focus();

    tracing::info!(
        total_ms = t0.elapsed().as_millis() as u64,
        pre_ms = t_build_start.as_millis() as u64,
        build_ms = (t_build - t_build_start).as_millis() as u64,
        place_ms = (t_place - t_build).as_millis() as u64,
        path = "first_build",
        "show_screenshot_overlay 完成"
    );

    Ok(())
}

/// 显示来源无关的用户图片编辑窗口。
///
/// 复用截图 overlay 的预热 WebView，但不创建或读取截图 SESSION。图片字节由独立的
/// `image_editor` 会话提供，前端从 `/editor` 协议路径初始化完整图片画布。
///
/// `source_kind` 传递给前端 `window.__blinkEditorSource.kind`，用于区分
/// 来源（clipboard / history / pin），前端据此走不同初始化路径。
pub fn show_image_editor_window(
    app: &AppHandle,
    image: crate::infra::platform::image_editor::ImageEditorMeta,
    source_kind: &str,
) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    const LABEL: &str = "chord-screenshot";

    let displays = crate::infra::platform::screenshot::list_displays();
    let display = displays
        .iter()
        .find(|display| display.primary)
        .or_else(|| displays.first());
    let meta = display
        .map(
            |display| crate::infra::platform::screenshot::ScreenCaptureMeta {
                virtual_x: display.x,
                virtual_y: display.y,
                width: display.w,
                height: display.h,
            },
        )
        .unwrap_or(crate::infra::platform::screenshot::ScreenCaptureMeta {
            virtual_x: 0,
            virtual_y: 0,
            width: image.width.max(640),
            height: image.height.max(480),
        });

    let (win, created) = get_or_create_window(app, LABEL, || {
        WebviewWindowBuilder::new(
            app,
            LABEL,
            WebviewUrl::App("chord-screenshot.html?source=clipboard".into()),
        )
        .title("")
        .inner_size(meta.width as f64, meta.height as f64)
        .position(meta.virtual_x as f64, meta.virtual_y as f64)
        .decorations(false)
        .resizable(false) // 与截图 overlay 保持一致：禁用原生 resize 边框
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .focused(true)
        .build()
    })?;

    let mut overlay_dpi = 96u32;
    if let Ok(hwnd) = win.hwnd() {
        let hwnd = HWND(hwnd.0 as _);
        // 0.20.4：撤销 hide_screenshot_overlay 可能设置的 DWM cloak，
        // 否则窗口 show 后在 DWM 合成器层面仍不可见（症状：窗口"打开"了但看不到画面）。
        if !created {
            apply_cloak(hwnd, false);
        }
        place_at_physical(
            hwnd,
            meta.virtual_x,
            meta.virtual_y,
            meta.width,
            meta.height,
        );
        overlay_dpi = crate::infra::platform::dpi::get_dpi_for_hwnd(hwnd);
    }
    let displays_json = build_physical_displays_json();
    let init_js = format!(
        "window.__blinkScreenMeta = {{ vx: {}, vy: {}, w: {}, h: {}, overlayDpi: {}, fgHwnd: 0, physicalDisplays: {} }}; window.__blinkEditorSource = {{ kind: '{kind}' }}; window.__blinkOpenImageEditor && window.__blinkOpenImageEditor();",
        meta.virtual_x, meta.virtual_y, meta.width, meta.height, overlay_dpi, displays_json,
        kind = source_kind,
    );
    win.eval(&init_js).map_err(|e| e.to_string())?;
    win.show().map_err(|e| e.to_string())?;
    if let Ok(hwnd) = win.hwnd() {
        // 0.20.4-fix：图片编辑器不强制置顶——用户可能需要参考其他窗口内容。
        // 窗口创建时设了 always_on_top(true)（与截图 overlay 共用窗口），
        // 这里用 HWND_NOTOPMOST 取消置顶，允许其他窗口覆盖编辑器。
        cancel_topmost(HWND(hwnd.0 as _));
    }
    if let Err(error) = win.set_focus() {
        tracing::warn!(%error, "图片编辑窗口 focus 失败");
    }
    // 0.20.4：标记编辑器活跃，watchdog 据此跳过 overlay 失焦隐藏
    set_image_editor_active(true);
    tracing::info!(
        created,
        width = image.width,
        height = image.height,
        "用户图片编辑窗口已显示"
    );
    Ok(())
}

/// 构造 `__blinkScreenMeta.physicalDisplays` 字段的 JS 数组字面量。
///
/// 注入原始物理显示器矩形（虚拟屏幕坐标系），不做任何 DPI 折算。
/// 前端用 canvas 实测的 renderScale 负责物理↔CSS 转换。
///
/// 失败时返回空数组 `[]`，前端按"无 displays 信息"降级到虚拟屏幕 clamp。
fn build_physical_displays_json() -> String {
    let displays = crate::infra::platform::screenshot::list_displays();
    if displays.is_empty() {
        return "[]".to_string();
    }
    let items: Vec<String> = displays
        .iter()
        .map(|d| {
            format!(
                "{{ x: {}, y: {}, w: {}, h: {}, primary: {}, dpi: {} }}",
                d.x, d.y, d.w, d.h, d.primary, d.dpi
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// 单测：验证 `build_physical_displays_json` 格式和物理几何注入。
#[cfg(test)]
mod tests_display_geometry {
    /// scale_factor 基础数学验证（仍用于诊断日志的正确性保证）。
    #[test]
    fn test_scale_factor_math() {
        assert_eq!(crate::infra::platform::dpi::scale_factor(96), 1.0);
        assert!((crate::infra::platform::dpi::scale_factor(144) - 1.5).abs() < 1e-9);
        assert!((crate::infra::platform::dpi::scale_factor(192) - 2.0).abs() < 1e-9);
        assert_eq!(crate::infra::platform::dpi::scale_factor(0), 1.0);
    }

    /// build_physical_displays_json 格式验证：返回合法 JS 数组，每项含物理坐标 + dpi 字段。
    #[test]
    fn test_build_physical_displays_json_format() {
        let json = super::build_physical_displays_json();
        assert!(
            json.starts_with('[') && json.ends_with(']'),
            "应为 JS 数组字面量"
        );
        if json != "[]" {
            assert!(json.contains("dpi:"), "每项应含 dpi 字段");
            assert!(json.contains("primary:"), "每项应含 primary 字段");
            assert!(json.contains("x:"), "每项应含物理 x 字段");
        }
    }
}

/// 按物理像素强制定位窗口，覆盖 Tauri 逻辑像素接口的 DPI 缩放。
///
/// **为何要走 Win32 而非 Tauri 的 `set_size` + `set_position`**：
/// 当窗口跨过一块 DPI 不同的显示器时（如从主屏 150% 移到副屏 100%），
/// `set_position` 会触发 `WM_DPICHANGED`，tao 的窗口过程据此**按 DPI 比例
/// 重设窗口尺寸但不动位置**——与刚排队的 `set_size` 竞态，导致最终尺寸/位置
/// 不可预测（Tauri issue #3610 / #10263，无边框窗口尤甚）。`SetWindowPos` 一次
/// 原子地设定位置+尺寸，绕开 tao 的 WM_DPICHANGED 重设尺寸逻辑，所见即所得。
///
/// 用途：
/// - 截图 overlay 必须精确对齐虚拟屏幕物理像素（canvas.width 与窗口 CSS 尺寸比
///   值需与 DPR 匹配，否则选区坐标全歪）
/// - 右键菜单复用路径（窗口在主屏预热，需移到任意屏的物理坐标）
pub fn place_at_physical(hwnd: HWND, x: i32, y: i32, w: u32, h: u32) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            x,
            y,
            w as i32,
            h as i32,
            SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0),
        );
    }
}

/// 隐藏截图覆盖窗 + 清空 SESSION（释放位图内存）。
pub fn hide_screenshot_overlay(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("chord-screenshot") {
        if let Ok(hwnd) = win.hwnd() {
            // 0.19.14：cloak 先于 hide——DWM 瞬时从合成中剔除 overlay，
            // 避免 fullscreen 透明窗口 hide 触发的 DWM 全屏重组闪烁（视频场景尤为明显）。
            apply_cloak(HWND(hwnd.0 as _), true);
        }
        let _ = win.hide();
    }
    crate::infra::platform::screenshot::end_session();
    // 同一 WebView 也承载通用图片编辑；看门狗/脚本兜底退出需释放两类短期载荷。
    crate::infra::platform::image_editor::end_session();
    // 0.20.4：清除编辑器活跃标志，恢复 watchdog 正常行为
    set_image_editor_active(false);
}

/// 隐藏通用图片编辑窗口并释放用户图片载荷；不触碰截图 SESSION。
pub fn hide_image_editor_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("chord-screenshot") {
        let _ = win.hide();
    }
    crate::infra::platform::image_editor::end_session();
    // 0.20.4：清除编辑器活跃标志，恢复 watchdog 正常行为
    set_image_editor_active(false);
}

/// 钉图窗口的物理像素 padding（窗口比图片大一圈，给发光留空间）。
/// 20px 足够 box-shadow 的 12px 模糊半径扩散。
pub const PIN_PAD: i32 = 20;

/// 显示钉图窗口（0.11.7-d；多 Pin N+1 改造）。
///
/// **多 Pin 策略**：每次 pin 创建/借用一个独立窗口（label `pin-spare-{N}` 或
/// `pin-{N}`），支持同时 pin 多张图片。N+1 预热机制：后台始终保留一个备用窗口，
/// 被借用后立即创建新的；关闭时回收/销毁。
///
/// **纯图片贴桌面效果**（0.11.8）：
/// - 窗口 `.transparent(true)` 让背景完全透明，只有图片本身可见
/// - 窗口尺寸 = 图片显示尺寸 + 2×PIN_PAD（预留发光空间，否则 box-shadow 被裁）
/// - 窗口左上 = `(screen_x - PAD, screen_y - PAD)`，使图片左上落在选区原位
/// - 缩放时窗口尺寸跟随变化（`screenshot_pin_transform`），图片用 width/height 不用 scale
///
/// 返回窗口 label，供 `refresh_pin_image` 定位目标窗口。
pub fn show_pin_window(
    app: &AppHandle,
    image: PinImage,
    screen_x: i32,
    screen_y: i32,
    show_translating: bool,
) -> Result<String, String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    const FALLBACK_W: f64 = 400.0;
    const FALLBACK_H: f64 = 300.0;

    // 解析图片像素尺寸用于开窗
    let (png_w, png_h) = match &image {
        PinImage::Png(data) => crate::infra::platform::screenshot::parse_png_size(data)
            .map(|(w, h)| (w as f64, h as f64))
            .unwrap_or((FALLBACK_W, FALLBACK_H)),
        PinImage::Bgra(_, w, h) => (*w as f64, *h as f64),
    };
    let png_len = match &image {
        PinImage::Png(data) => data.len(),
        PinImage::Bgra(data, _, _) => data.len(),
    };

    // 0.19.14：存入进程内 registry，用 blink-pin:// 协议替代 base64 data URL。
    // P6：快路径存 raw BGRA，协议 handler lazy 编码 PNG，不阻塞 screenshot_pin_region。
    let pin_seq = store_pin_image(image);
    let img_url = format!("http://blink-pin.localhost/{pin_seq}");

    // 0.18.8+：取图片初始落点显示器的 DPR，作为前端视觉尺寸的权威基准。
    // 不依赖 WebView 尚未稳定的 window.devicePixelRatio（跨屏后会变化）。
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint};
    let source_dpi = {
        let pt = windows::Win32::Foundation::POINT {
            x: screen_x,
            y: screen_y,
        };
        let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
        crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon)
    };
    let source_dpr = crate::infra::platform::dpi::scale_factor(source_dpi);

    // 窗口左上 = 图片左上 - PAD（让图片左上对齐选区原位，窗口外圈留 PAD 给发光）
    let win_x = screen_x - PIN_PAD;
    let win_y = screen_y - PIN_PAD;
    let win_w = png_w as u32 + 2 * PIN_PAD as u32;
    let win_h = png_h as u32 + 2 * PIN_PAD as u32;

    // 构造注入 JS（复用窗口与首次创建共用）。sourceDpr 传给前端作为视觉尺寸基准。
    let js = format!(
        "if (window.__blinkResetPin) window.__blinkResetPin('{url}', {w}, {h}, {sx}, {sy}, {st}, {sdpr}); else document.getElementById('pin-img').src = '{url}';",
        url = img_url,
        w = png_w,
        h = png_h,
        sx = screen_x,
        sy = screen_y,
        st = if show_translating { "true" } else { "false" },
        sdpr = source_dpr,
    );

    // 尝试借用空闲 spare
    let available_label = available_pin_spare().lock().unwrap().take();
    if let Some(al) = available_label {
        tracing::debug!(spare_label = %al, "pin window: 借用预热 spare");
        pin_spare_borrow()
            .lock()
            .unwrap()
            .insert(al.clone(), "pin".to_string());

        let spare_win = app
            .get_webview_window(&al)
            .ok_or_else(|| "预热 pin 窗口不存在".to_string())?;
        if let Ok(hwnd) = spare_win.hwnd() {
            place_at_physical(HWND(hwnd.0 as _), win_x, win_y, win_w, win_h);
        }
        spare_win
            .eval(&js)
            .map_err(|e| format!("eval 注入 PNG 失败: {e}"))?;
        let _ = spare_win.show();
        let _ = spare_win.set_focus();

        // 记录最近 pin 的 label
        *last_pin_label().lock().unwrap() = Some(al.clone());
        // 0.20.4：注册 label → seq 映射，供编辑器按 label 查找 pin 图片
        pin_label_to_seq().lock().unwrap().insert(al.clone(), pin_seq);

        tracing::info!(
            png_w,
            png_h,
            screen_x,
            screen_y,
            show_translating,
            png_bytes = png_len,
            "钉图窗口已借用预热 spare"
        );

        // N+1：spare 被借用后，后台延迟创建新的备用窗口
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            create_pin_spare(&app_clone);
            tracing::debug!("pin-spare: N+1 补充完成");
        });

        return Ok(al);
    }

    // 无可用 spare，创建新窗口
    let seq = PIN_SEQ.fetch_add(1, Ordering::SeqCst);
    let label = format!("pin-{seq}");

    match WebviewWindowBuilder::new(app, &label, WebviewUrl::App("pin.html".into()))
        .title("")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .resizable(false)
        .inner_size(win_w as f64, win_h as f64)
        .position(win_x as f64, win_y as f64)
        .build()
    {
        Ok(win) => {
            // 再用 SetWindowPos 精确对齐物理像素（首次 build 后 Tauri 可能因 DPI 偏移）
            if let Ok(hwnd) = win.hwnd() {
                place_at_physical(HWND(hwnd.0 as _), win_x, win_y, win_w, win_h);
            }
            win.eval(&js)
                .map_err(|e| format!("eval 注入 PNG 失败: {e}"))?;
            let _ = win.show();

            // 注册关闭处理：prevent_close + hide + 回收/销毁
            let label_owned = label.clone();
            let app_clone = app.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    if IS_APP_EXITING.load(Ordering::SeqCst) {
                        return;
                    }
                    api.prevent_close();
                    handle_pin_close(&app_clone, &label_owned);
                }
            });

            // 记录最近 pin 的 label
            *last_pin_label().lock().unwrap() = Some(label.clone());
            // 0.20.4：注册 label → seq 映射
            pin_label_to_seq().lock().unwrap().insert(label.clone(), pin_seq);

            tracing::info!(
                png_w,
                png_h,
                screen_x,
                screen_y,
                show_translating,
                png_bytes = png_len,
                "钉图窗口已创建"
            );
            Ok(label)
        }
        Err(e) => {
            tracing::warn!(error = %e, "钉图窗口创建失败");
            Err(format!("钉图窗口创建失败: {e}"))
        }
    }
}

/// 获取光标所在显示器工作区中心，让图片居中放置（0.19.3 从 clipboard.rs 提升到 window 模块）。
///
/// 0.18.8：从 `GetSystemMetrics(SM_CXSCREEN)`（仅主屏）改为
/// `MonitorFromPoint` + `GetMonitorInfoW` 取光标所在屏的工作区，
/// 副屏右键钉图时图片不再出现在主屏。
///
/// 供 `pin_image` Capability 位置兜底和 `pin_clipboard_image` command 共用。
pub fn get_primary_monitor_center(img_w: i32, img_h: i32) -> (i32, i32) {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    unsafe {
        let mut pt = std::mem::zeroed();
        let _ = GetCursorPos(&mut pt);
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let _ = GetMonitorInfoW(hmon, &mut mi);
        // 工作区（rcWork）排除任务栏，居中放置
        let wa = &mi.rcWork;
        let wa_w = wa.right - wa.left;
        let wa_h = wa.bottom - wa.top;
        let x = wa.left + (wa_w - img_w) / 2;
        let y = wa.top + (wa_h - img_h) / 2;
        (x.max(wa.left), y.max(wa.top))
    }
}

/// 0.18.3：原地刷新钉图窗口的图片（不重定位、不重置缩放）。
///
/// 用于「翻译并 pin」流程：后台翻译完成后合成含译文的 PNG，
/// 调本函数只换 `img.src`，不动窗口位置和 scale。
///
/// 多 Pin 改造：通过 `LAST_PIN_LABEL` 定位最近 pin 的窗口。
///
/// - `show_translating=false` 时隐藏 pin 窗口的「翻译中」指示器。
/// - pin 窗口不存在或已 hide 时静默返回 Ok（用户已关 pin，丢弃译文）。
pub fn refresh_pin_image(
    app: &AppHandle,
    png_data: Vec<u8>,
    show_translating: bool,
) -> Result<(), String> {
    let label = last_pin_label().lock().unwrap().clone();
    let label = match label {
        Some(l) => l,
        None => {
            tracing::debug!("refresh_pin_image: 无 LAST_PIN_LABEL，静默丢弃");
            return Ok(());
        }
    };

    let win = match app.get_webview_window(&label) {
        Some(w) => w,
        None => {
            tracing::debug!(label = %label, "refresh_pin_image: pin 窗口不存在，静默丢弃");
            return Ok(());
        }
    };

    // 窗口已 hide 时静默返回（用户已关 pin）
    if !win.is_visible().unwrap_or(false) {
        tracing::debug!("refresh_pin_image: pin 窗口已隐藏，静默丢弃");
        return Ok(());
    }

    let (png_w, png_h) = crate::infra::platform::screenshot::parse_png_size(&png_data)
        .map(|(w, h)| (w as f64, h as f64))
        .unwrap_or((0.0, 0.0));

    let pin_seq = store_pin_image(PinImage::Png(Arc::new(png_data)));
    let img_url = format!("http://blink-pin.localhost/{pin_seq}");

    // 0.20.4：更新 label → seq 映射（刷新后旧 seq 对应的图片已过时）
    pin_label_to_seq().lock().unwrap().insert(label.clone(), pin_seq);

    // 只换 img.src + 控制指示器，不调 place_at_physical，不调 __blinkResetPin
    let js = format!(
        "if (window.__blinkRefreshPinImage) window.__blinkRefreshPinImage('{url}', {w}, {h}, {st});",
        url = img_url,
        w = png_w,
        h = png_h,
        st = if show_translating { "true" } else { "false" }
    );
    win.eval(&js)
        .map_err(|e| format!("eval 刷新 pin 图片失败: {e}"))?;
    Ok(())
}

/// 多 Pin N+1：处理 pin 窗口关闭（回收或销毁）。
///
/// 立即 hide（用户感知"点击即关闭"），后台 500ms 后回收/销毁：
/// - 无可用 spare → 回收：eval `__blinkClearPin` 清空状态，标记为可用 spare
/// - 已有可用 spare → 销毁窗口
fn handle_pin_close(app: &AppHandle, label: &str) {
    // 立即隐藏
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.hide();
    }

    // 从借出映射移除
    pin_spare_borrow().lock().unwrap().remove(label);
    // 0.20.4：从 label → seq 映射移除（pin 已关闭，图片不再可编辑）
    pin_label_to_seq().lock().unwrap().remove(label);

    let label_owned = label.to_string();
    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 回收或销毁
        let available = available_pin_spare().lock().unwrap();
        if available.is_none() {
            // 回收：清空图片状态，标记为可用 spare
            drop(available);
            if let Some(w) = app_clone.get_webview_window(&label_owned) {
                let _ = w.eval("if (window.__blinkClearPin) window.__blinkClearPin();");
            }
            tracing::debug!(spare_label = %label_owned, "pin-spare: 回收中，等待前端 __blinkClearPin 完成");
        } else {
            // 已有可用 spare，销毁此窗口
            drop(available);
            if let Some(w) = app_clone.get_webview_window(&label_owned) {
                let _ = w.destroy();
            }
            tracing::debug!(spare_label = %label_owned, "pin-spare: 销毁多余备用窗口");
        }
    });
}

/// 多 Pin N+1：创建 pin 预热窗口。
///
/// 后台始终保留一个已加载 pin.html 的 WebView2 备用窗口。
/// 被借用后由调用方 spawn 新的 spare 创建（500ms 延迟避抢资源）。
/// 借出的窗口关闭时：hide + 回收（若无可用 spare）或销毁（已有可用 spare）。
fn create_pin_spare(app: &AppHandle) {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    // 已有可用 spare 则不重复创建
    if available_pin_spare().lock().unwrap().is_some() {
        return;
    }

    let seq = PIN_SEQ.fetch_add(1, Ordering::SeqCst);
    let label = format!("pin-spare-{seq}");

    match WebviewWindowBuilder::new(app, &label, WebviewUrl::App("pin.html?preheat=1".into()))
        .title("")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .resizable(false)
        .focused(false)
        .visible(false)
        .inner_size(400.0, 300.0)
        .build()
    {
        Ok(w) => {
            let label_owned = label.clone();
            let app_clone = app.clone();
            w.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    if IS_APP_EXITING.load(Ordering::SeqCst) {
                        return;
                    }
                    api.prevent_close();
                    handle_pin_close(&app_clone, &label_owned);
                }
            });
            tracing::debug!(spare_label = %label, "pin-spare: 窗口已创建，等待前端 init 就绪");
        }
        Err(e) => tracing::warn!(error = %e, "pin-spare: 创建失败"),
    }
}

/// 多 Pin N+1：前端 init 完成后调用，将 spare 注册为可用。
///
/// `create_pin_spare` 只 build 窗口，不立即注册 available——
/// 因为 WebView2 的 HTML/JS 加载是异步的，在 init 完成前 eval 会静默失败。
/// 前端 preheat init 完成后通过 IPC 命令调用此函数，标记 spare 就绪。
pub fn mark_pin_spare_ready(label: &str) {
    let mut available = available_pin_spare().lock().unwrap();
    if available.is_none() {
        *available = Some(label.to_string());
        tracing::debug!(spare_label = %label, "pin-spare: 前端已就绪，注册为可用备用窗口");
    }
}

/// 获取 Pin 窗口的当前物理矩形和目标屏 DPR。
///
/// 用于 DPI reconcile：`onScaleChanged` 回调后或拖动跨 DPI 边界后，
/// 前端调用此命令回读窗口实际物理位置（Windows 可能因 WM_DPICHANGED 改变了矩形），
/// 再用 `pin-geometry.js::reconcileDpi` 重算状态。
///
/// 返回 `None` 如果窗口不存在。
pub fn get_pin_window_rect(app: &AppHandle, label: &str) -> Option<(i32, i32, u32, u32, f64)> {
    use windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST;
    use windows::Win32::Graphics::Gdi::MonitorFromWindow;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let win = app.get_webview_window(label)?;
    let hwnd = win.hwnd().ok()?;
    let hwnd_raw = windows::Win32::Foundation::HWND(hwnd.0 as _);

    let mut rect = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd_raw, &mut rect) }.is_err() {
        return None;
    }

    let hmon = unsafe { MonitorFromWindow(hwnd_raw, MONITOR_DEFAULTTONEAREST) };
    let dpi = crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon);
    let dpr = crate::infra::platform::dpi::scale_factor(dpi);

    let x = rect.left;
    let y = rect.top;
    let w = (rect.right - rect.left) as u32;
    let h = (rect.bottom - rect.top) as u32;

    Some((x, y, w, h, dpr))
}

/// Apply or remove DWM Cloak on a window.
///
/// Cloak = true: DWM 层瞬间"雾化"窗口（无 fade 动画），WS_VISIBLE 仍为 on。
/// Cloak = false: 恢复正常可见性。
///
/// 调用方负责确保 cloak 状态对称——cloak 后必须在下次 show 前 uncloak，
/// 否则窗口 show 出来仍不可见。
pub fn apply_cloak(hwnd: HWND, on: bool) {
    unsafe {
        let cloak: i32 = if on { 1 } else { 0 };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CLOAK,
            &cloak as *const _ as *const _,
            std::mem::size_of::<i32>() as u32,
        );
    }
}

/// 截图专用：**瞬间**隐藏主窗（DWM Cloak + hide），零 fade 动画。
///
/// **和 `hide()` 的区别**：
/// - `hide()` 走 `ShowWindow(SW_HIDE)`，触发 Windows 11 系统级 fade-out（~200ms 视觉延迟）
/// - `hide_for_screenshot()` 先 `DwmSetWindowAttribute(DWMWA_CLOAK, TRUE)` 让 DWM
///   **立即**从合成里剔除窗口（无动画），再调 `ShowWindow(SW_HIDE)` 落 Win32 状态
///
/// Cloak 是任务视图/Alt-Tab 预览用的机制，DWM 层瞬间"雾化"窗口——远快于走 fade。
///
/// 调用侧应在截图完成后（成功或取消）调 `unhide_after_screenshot` 撤销 cloak，
/// 否则下次 `show()` 出来的窗口是不可见的。
pub fn hide_for_screenshot(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if let Ok(hwnd) = win.hwnd() {
            apply_cloak(HWND(hwnd.0 as _), true);
        }
        let _ = win.hide();
        transition_visibility(false);
        let _ = app.emit(EventNames::HIDDEN, ());
    }
    // 联动隐藏右键菜单（保留窗口供下次复用）
    if let Some(menu_win) = app.get_webview_window("context-menu") {
        let _ = menu_win.hide();
    }
}

/// 撤销 `hide_for_screenshot` 的 cloak 标志。
///
/// 只清 cloak，不 `show`——主窗此时仍应保持 hidden 状态（截图完成后主窗不该出来）。
/// 下次 `invoke()` 时 `show()` 会正常工作。
pub fn unhide_after_screenshot(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if let Ok(hwnd) = win.hwnd() {
            apply_cloak(HWND(hwnd.0 as _), false);
        }
    }
}

/// 等主窗真正从桌面上消失（截图前调用，防"BitBlt 拍到主窗"）。
///
/// 配 `hide_for_screenshot()` 使用时无需等 fade 动画——cloak 是瞬时的，只需要一次
/// DwmFlush 保证 DWM 完成一帧新合成（不含主窗）即可。
///
/// 调用侧应保证跑在 blocking 线程（tokio `spawn_blocking`），DwmFlush 是同步阻塞。
pub fn wait_frame_after_hide(app: &AppHandle) {
    use std::time::Instant;
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

    let t0 = Instant::now();

    // DwmFlush x 1：cloak 后瞬时生效，一次 flush 保证 DWM 完成不含主窗的新合成。
    // 0.8.8 优化：从 2 次减到 1 次，实测截图无残影，省 ~10ms。
    unsafe {
        let _ = DwmFlush();
    }
    let t_flush = t0.elapsed();

    // 轮询 IsWindowVisible —— cloak + hide 后立刻就是 false，这里主要作日志用
    let hwnd = app
        .get_webview_window("main")
        .and_then(|w| w.hwnd().ok())
        .map(|h| HWND(h.0 as _));
    let mut polled_ms = 0u64;
    let mut visible_final = None;
    if let Some(hwnd) = hwnd {
        loop {
            let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
            visible_final = Some(visible);
            if !visible || polled_ms >= 100 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
            polled_ms += 8;
        }
    }

    // ⚠️ 临时打桩日志（0.19.14 性能排查用），收尾时降回 debug!
    tracing::info!(
        flush_ms = t_flush.as_millis() as u64,
        poll_ms = polled_ms,
        total_ms = t0.elapsed().as_millis() as u64,
        visible_final = ?visible_final,
        "wait_frame_after_hide 完成"
    );
}

/// 0.18.3：创建便签预热窗口。
///
/// N+1 预热机制：后台始终保留一个已加载 Tiptap bundle 的 WebView2 备用窗口。
/// 被借用后由调用方 spawn 新的 spare 创建（500ms 延迟避抢资源）。
/// 借出的窗口关闭时：flush + trash + 回收（若无可用 spare）或销毁（已有可用 spare）。
fn create_sticky_spare(app: &AppHandle) {
    use tauri::{WebviewUrl, WebviewWindowBuilder, window::Color};

    // 已有可用 spare 则不重复创建
    if available_spare().lock().unwrap().is_some() {
        return;
    }

    let seq = SPARE_SEQ.fetch_add(1, Ordering::SeqCst);
    let label = format!("sticky-spare-{seq}");

    match WebviewWindowBuilder::new(app, &label, WebviewUrl::App("sticky.html?preheat=1".into()))
        .title("便签")
        .inner_size(
            crate::infra::data::sticky::DEFAULT_WIDTH as f64,
            crate::infra::data::sticky::DEFAULT_HEIGHT as f64,
        )
        .min_inner_size(STICKY_MIN_W, STICKY_MIN_H)
        .decorations(false)
        .transparent(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(true)
        .focused(false)
        .visible(false)
        .background_color(Color(255, 249, 196, 255))
        .build()
    {
        Ok(w) => {
            let label_owned = label.clone();
            let app_clone = app.clone();
            w.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    if IS_APP_EXITING.load(Ordering::SeqCst) {
                        return; // 应用退出：不 prevent_close
                    }
                    api.prevent_close();

                    // 检查此 spare 是否已借出
                    let borrowed_id = spare_borrow().lock().unwrap().remove(&label_owned);

                    if let Some(sid) = borrowed_id {
                        // 借出的 spare 关闭 → 立即 flush + hide，后台再 trash + 回收/销毁
                        if let Some(w) = app_clone.get_webview_window(&label_owned) {
                            let _ = w.eval("if (window.__stickyFlush) window.__stickyFlush();");
                            // 立即隐藏——用户感知为"点击即关闭"，清理在后台不可见时进行
                            let _ = w.hide();
                        }

                        let app_c = app_clone.clone();
                        let lbl = label_owned.clone();
                        tauri::async_runtime::spawn(async move {
                            // 等 flush 完成（前端防抖 500ms）
                            tokio::time::sleep(Duration::from_millis(500)).await;

                            // trash 便签
                            if let Some(svc) = app_c
                                .try_state::<std::sync::Arc<crate::domain::sticky::StickyService>>()
                            {
                                if let Err(e) = svc.trash_note(&sid).await {
                                    tracing::warn!(error = %e, "预热便签关闭时移入回收站失败");
                                } else {
                                    let _ = app_c.emit(
                                        EventNames::STICKY_TRASHED,
                                        serde_json::json!({ "stickyId": sid }),
                                    );
                                }
                            }

                            // 回收或销毁
                            let available = available_spare().lock().unwrap();
                            if available.is_none() {
                                // 回收：eval __stickyReset，前端完成后会 invoke sticky_spare_ready
                                // 不在此处直接设 AVAILABLE_SPARE——避免 __stickyReset 未执行完就被借用
                                drop(available);
                                if let Some(w) = app_c.get_webview_window(&lbl) {
                                    let _ = w.eval("if (window.__stickyReset) window.__stickyReset();");
                                }
                                tracing::debug!(spare_label = %lbl, "sticky-spare: 回收中，等待前端 __stickyReset 完成后注册");
                            } else {
                                // 已有可用 spare，销毁此窗口
                                drop(available);
                                if let Some(w) = app_c.get_webview_window(&lbl) {
                                    let _ = w.destroy();
                                }
                                tracing::debug!(spare_label = %lbl, "sticky-spare: 销毁多余备用窗口");
                            }
                        });
                    } else {
                        // 空闲 spare 被关闭 → 仅 hide
                        if let Some(w) = app_clone.get_webview_window(&label_owned) {
                            let _ = w.hide();
                        }
                        tracing::debug!(spare_label = %label_owned, "sticky-spare: 空闲关闭 → hide");
                    }
                }
            });

            // 注册为可用 spare——延迟到前端 init 完成后由 mark_spare_ready 设置
            // 否则 spare 可能在 JS 未加载完时被借用，eval __stickyReload 静默失败
            tracing::debug!(spare_label = %label, "sticky-spare: 窗口已创建，等待前端 init 就绪");
        }
        Err(e) => tracing::warn!(error = %e, "sticky-spare: 创建失败"),
    }
}

/// 0.18.3 N+1：前端 init 完成后调用，将 spare 注册为可用。
///
/// `create_sticky_spare` 只 build 窗口，不立即注册 available——
/// 因为 WebView2 的 HTML/JS 加载是异步的，在 init 完成前 eval 会静默失败。
/// 前端 preheat init 完成后通过 IPC 命令调用此函数，标记 spare 就绪。
pub fn mark_spare_ready(label: &str) {
    // 仅当该 label 对应的窗口存在且当前无可用 spare 时才注册
    let mut available = available_spare().lock().unwrap();
    if available.is_none() {
        *available = Some(label.to_string());
        tracing::debug!(spare_label = %label, "sticky-spare: 前端已就绪，注册为可用备用窗口");
    }
}


/// 后台预热次级窗口：延迟创建 chord-screenshot / context-menu / voice-overlay /
/// chord-pin / chat / settings / content-editor / sticky-manager 并立即隐藏。
///
/// WebView2 首次建实例 300~400ms，预热后 show 只是切可见性 (<50ms)。
/// 代价：常驻内存 +10~20MB × N（8 窗口 + 动态便签，实测 < 300MB 预算内）；
/// 收益：所有次级窗口首次触发无感。
///
/// 0.17.2：追加 settings / content-editor / sticky-manager 三个窗口预热。
/// sticky-manager 预热时注册 prevent_close + hide（show 函数复用路径不注册）。
///
/// chord-ball 悬浮球预热已随划词翻译 chord 移除而删除。
pub fn preheat_secondary_windows(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // 等主窗稳定 + 前端加载完毕，不与启动路径抢资源
        tokio::time::sleep(Duration::from_secs(1)).await;
        tracing::debug!("preheat: 开始预热次级窗口");

        // --- chord-screenshot（截图 overlay，透明全屏层） ---
        // 0.19：经 get_or_create_window 串行化创建。
        match get_or_create_window(&app, "chord-screenshot", || {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            WebviewWindowBuilder::new(
                &app,
                "chord-screenshot",
                // 0.11.7-f：URL 加 ?preheat=1，前端识别参数跳过 loadScreenshot，
                // 避免 SESSION 空时 img.onerror 遗留 error-hint 到用户实际唤起的 overlay
                WebviewUrl::App("chord-screenshot.html?preheat=1".into()),
            )
            .title("")
            .inner_size(1920.0, 1080.0)
            .decorations(false)
            .resizable(false) // 与截图 overlay 首次创建路径保持一致：禁用原生 resize 边框
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .shadow(false)
            .focused(false)
            .visible(false)
            .build()
        }) {
            Ok((_, created)) => {
                if created {
                    tracing::debug!("preheat: chord-screenshot ✓");
                } else {
                    tracing::debug!("preheat: chord-screenshot 复用已有窗口");
                }
            }
            Err(e) => tracing::warn!(error = %e, "preheat: chord-screenshot 失败"),
        }

        // --- context-menu（右键菜单，非透明小窗） ---
        // 0.19：经 get_or_create_window 串行化创建。
        match get_or_create_context_menu_window(
            &app,
            "contextmenu-popup.html".to_string(),
            200.0,
            200.0,
        ) {
            Ok((win, created)) => {
                if created {
                    if let Ok(hwnd) = win.hwnd() {
                        force_topmost(HWND(hwnd.0 as _));
                    }
                    tracing::debug!("preheat: context-menu ✓");
                } else {
                    tracing::debug!("preheat: context-menu 复用已有窗口");
                }
            }
            Err(e) => tracing::warn!(error = %e, "preheat: context-menu 失败"),
        }

        // --- voice-overlay（语音录音 mini overlay，0.10 G2） ---
        // 0.19：经 get_or_create_window 串行化创建。
        match get_or_create_window(&app, "voice-overlay", || {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            WebviewWindowBuilder::new(
                &app,
                "voice-overlay",
                WebviewUrl::App("voice-overlay.html".into()),
            )
            .title("")
            .inner_size(VOICE_W, VOICE_H)
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .shadow(false)
            .focused(false)
            .visible(false)
            .build()
        }) {
            Ok((win, created)) => {
                if created {
                    if let Ok(hwnd) = win.hwnd() {
                        apply_no_activate(HWND(hwnd.0 as _));
                    }
                    tracing::debug!("preheat: voice-overlay ✓");
                } else {
                    tracing::debug!("preheat: voice-overlay 复用已有窗口");
                }
            }
            Err(e) => tracing::warn!(error = %e, "preheat: voice-overlay 失败"),
        }

        // --- pin-spare（钉图预热窗口，多 Pin N+1） ---
        // 后台始终保留一个备用钉图窗口，被借用后立即创建新的
        create_pin_spare(&app);

        // --- chat（对话窗口，0.12.2 加入预热） ---
        // 0.19：经 get_or_create_window 串行化创建，消除预热与用户 Alt+Q 的 duplicate label 竞态。
        // 创建配置与 show_chat_window 完全一致（visible=false + focused=false），保证
        // 无论谁创建，后续 show/focus 路径都统一。
        match get_or_create_window(&app, "chat", || {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            WebviewWindowBuilder::new(&app, "chat", WebviewUrl::App("chat.html".into()))
                .title("Blink AI")
                .inner_size(CHAT_W, CHAT_H)
                .min_inner_size(CHAT_MIN_W, CHAT_MIN_H)
                .decorations(false)
                .transparent(false)
                .always_on_top(false)
                .skip_taskbar(false)
                .resizable(true)
                .focused(false)
                .visible(false)
                .build()
        }) {
            Ok((_, created)) => {
                if created {
                    tracing::debug!("preheat: chat ✓");
                } else {
                    tracing::debug!("preheat: chat 复用已有窗口");
                }
            }
            Err(e) => tracing::warn!(error = %e, "preheat: chat 失败"),
        }

        // --- settings（设置窗口，0.17.2 加入预热） ---
        // 0.19：经 get_or_create_window 串行化创建。
        // 预热时补 strip_window_border + enable_rounded_corners（幂等，重复调用安全）。
        match get_or_create_window(&app, "settings", || {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            WebviewWindowBuilder::new(&app, "settings", WebviewUrl::App("settings.html".into()))
                .title("Blink Settings")
                .inner_size(SETTINGS_W, SETTINGS_H)
                .min_inner_size(SETTINGS_MIN_W, SETTINGS_MIN_H)
                .position(0.0, 0.0)
                .visible(false)
                .decorations(false)
                .transparent(true)
                .shadow(false)
                .background_color(tauri::window::Color(0, 0, 0, 0))
                .build()
        }) {
            Ok((win, created)) => {
                if created {
                    if let Ok(hwnd) = win.hwnd() {
                        let hwnd = HWND(hwnd.0 as _);
                        strip_window_border(hwnd);
                        enable_rounded_corners(hwnd);
                    }
                    tracing::debug!("preheat: settings ✓");
                } else {
                    tracing::debug!("preheat: settings 复用已有窗口");
                }
            }
            Err(e) => tracing::warn!(error = %e, "preheat: settings 失败"),
        }

        // --- content-editor（内容编辑器，0.17.2 加入预热） ---
        // 0.19：经 get_or_create_window 串行化创建。
        match get_or_create_window(&app, "content-editor", || {
            use tauri::{WebviewUrl, WebviewWindowBuilder, window::Color};
            WebviewWindowBuilder::new(
                &app,
                "content-editor",
                WebviewUrl::App("content-editor.html".into()),
            )
            .title("编辑内容")
            .inner_size(EDITOR_W, EDITOR_H)
            .min_inner_size(EDITOR_MIN_W, EDITOR_MIN_H)
            .decorations(false)
            .transparent(false)
            .always_on_top(false)
            .skip_taskbar(false)
            .resizable(true)
            .focused(false)
            .visible(false)
            .background_color(Color(30, 30, 46, 255))
            .center()
            .build()
        }) {
            Ok((_, created)) => {
                if created {
                    tracing::debug!("preheat: content-editor ✓");
                } else {
                    tracing::debug!("preheat: content-editor 复用已有窗口");
                }
            }
            Err(e) => tracing::warn!(error = %e, "preheat: content-editor 失败"),
        }

        // --- sticky-manager（便签管理，0.17.2 加入预热） ---
        // 0.19：经 get_or_create_window 串行化创建。
        // 预热时注册 prevent_close + hide（与 show_sticky_manager_window 创建路径一致），
        // 因为 show 函数的复用路径（is_new=false）不注册 on_window_event。
        match get_or_create_window(&app, "sticky-manager", || {
            use tauri::{WebviewUrl, WebviewWindowBuilder, window::Color};
            WebviewWindowBuilder::new(
                &app,
                "sticky-manager",
                WebviewUrl::App("sticky-manager.html".into()),
            )
            .title("便签管理")
            .inner_size(MANAGER_W, MANAGER_H)
            .min_inner_size(MANAGER_MIN_W, MANAGER_MIN_H)
            .decorations(false)
            .transparent(false)
            .always_on_top(false)
            .skip_taskbar(false)
            .resizable(true)
            .focused(false)
            .visible(false)
            .background_color(Color(30, 30, 46, 255))
            .center()
            .build()
        }) {
            Ok((w, created)) => {
                if created {
                    // 注册 prevent_close + hide（复用模式）
                    let app_clone = app.clone();
                    w.on_window_event(move |event| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                            if IS_APP_EXITING.load(Ordering::SeqCst) {
                                return; // 应用退出：不 prevent_close
                            }
                            api.prevent_close();
                            if let Some(w) = app_clone.get_webview_window("sticky-manager") {
                                let _ = w.hide();
                            }
                            tracing::debug!(
                                "preheat sticky-manager: CloseRequested → prevent_close + hide"
                            );
                        }
                    });
                    tracing::debug!("preheat: sticky-manager ✓");
                } else {
                    tracing::debug!("preheat: sticky-manager 复用已有窗口");
                }
            }
            Err(e) => tracing::warn!(error = %e, "preheat: sticky-manager 失败"),
        }

        // --- sticky-spare（便签预热窗口，0.18.3 N+1） ---
        // 后台始终保留一个备用便签窗口，被借用后立即创建新的
        create_sticky_spare(&app);

        tracing::debug!("preheat: 预热完成");
    });
}

/// 打开设置窗口：**每次都定位到光标所在屏的工作区中心**。
///
/// - 已存在：从 iconic 恢复 → 读当前 outer_size 保留用户 resize 过的尺寸 →
///   `place_at_physical` 一次原子挪到光标屏中心（避开 WM_DPICHANGED 抢跑）。
/// - 首次创建：build 完立刻按目标屏 DPI 把 960×680 CSS → 物理尺寸，挪过去。
///
/// 语义：用户在哪块屏发起动作（右键 → 打开设置 / 托盘 → 设置），设置就出现在
/// 那块屏。跟 Universal Action Layer 的直觉一致，也省了跨屏找窗口的动作。
pub fn open_settings(app: &AppHandle) {
    // 光标所在屏工作区 + DPI（一次读，两条路径复用）
    let (work, target_dpi) = unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut pt);
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let work = if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        };
        // 0.11.9：走公共 DPI helper（get_dpi_for_hmonitor 内部已 .max(96) 兜底）
        let target_dpi = crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon);
        (work, target_dpi)
    };
    let work_w = work.right - work.left;
    let work_h = work.bottom - work.top;

    if let Some(w) = app.get_webview_window("settings") {
        // 从最小化恢复
        let hwnd_raw = w.hwnd().ok();
        if let Some(h) = hwnd_raw {
            let hwnd = HWND(h.0 as _);
            unsafe {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
            }
        }
        // 保留 **CSS 尺寸**（不是物理）——跨 DPI 屏保留物理尺寸会越挪越离谱：
        //   主屏 150% 首次 1440 phys(=960 CSS)
        //   → 挪副屏 100%,tao 处理 WM_DPICHANGED 按 100/150 缩到 960 phys
        //   → 回主屏读 outer_size=960 phys,若直接用作物理 → 主屏 150% 视觉 640 CSS,变小 1/3
        // 用当前 scale_factor 折算 CSS,再按目标屏 DPI 换回物理。scale_factor 和 outer_size
        // 都反映"窗口当前所在屏",配对读一致快照,比值稳定 = CSS 尺寸恒定。
        let cur_scale = w.scale_factor().unwrap_or(1.0).max(1.0);
        let cur_phys = w.outer_size().unwrap_or_else(|_| {
            tauri::PhysicalSize::new(
                (960.0 * cur_scale).round() as u32,
                (680.0 * cur_scale).round() as u32,
            )
        });
        let css_w = (cur_phys.width as f64) / cur_scale;
        let css_h = (cur_phys.height as f64) / cur_scale;
        let target_scale = crate::infra::platform::dpi::scale_factor(target_dpi);
        let phys_w = (css_w * target_scale).round() as i32;
        let phys_h = (css_h * target_scale).round() as i32;
        // clamp 到目标屏工作区
        let win_w = phys_w.min(work_w).max(1);
        let win_h = phys_h.min(work_h).max(1);
        let fx = work.left + (work_w - win_w) / 2;
        let fy = work.top + (work_h - win_h) / 2;
        if let Some(h) = hwnd_raw {
            let hwnd = HWND(h.0 as _);
            place_at_physical(hwnd, fx, fy, win_w as u32, win_h as u32);
            let _ = w.show();
            // 跨 DPI 屏时 WM_DPICHANGED 会抢跑改尺寸,补一次覆盖回来
            place_at_physical(hwnd, fx, fy, win_w as u32, win_h as u32);
        } else {
            let _ = w.set_position(PhysicalPosition::new(fx, fy));
            let _ = w.show();
        }
        let _ = w.set_focus();
        return;
    }
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    // 首次创建：先 hidden build（避免主屏闪一下），然后按目标屏 DPI 把默认
    // CSS 尺寸 折算成物理尺寸，place_at_physical 挪到光标屏中心。
    // 位置给 (0,0) 占位，builder 的 .center() 只会居中主屏——用不上。
    let win = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("Blink Settings")
        .inner_size(SETTINGS_W, SETTINGS_H)
        .min_inner_size(SETTINGS_MIN_W, SETTINGS_MIN_H)
        .position(0.0, 0.0)
        .visible(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .background_color(tauri::window::Color(0, 0, 0, 0))
        .build()
        .expect("创建设置窗口失败");
    let scale = crate::infra::platform::dpi::scale_factor(target_dpi);
    let phys_w = (SETTINGS_W * scale).round() as i32;
    let phys_h = (SETTINGS_H * scale).round() as i32;
    let win_w = phys_w.min(work_w);
    let win_h = phys_h.min(work_h);
    let fx = work.left + (work_w - win_w) / 2;
    let fy = work.top + (work_h - win_h) / 2;
    if let Ok(h) = win.hwnd() {
        let hwnd = HWND(h.0 as _);
        strip_window_border(hwnd);
        enable_rounded_corners(hwnd);
        place_at_physical(hwnd, fx, fy, win_w as u32, win_h as u32);
        let _ = win.show();
        // 补一次：show 触发 WM_DPICHANGED 时 tao 会改尺寸，覆盖回来
        place_at_physical(hwnd, fx, fy, win_w as u32, win_h as u32);
        let _ = win.set_focus();
    } else {
        let _ = win.show();
    }
}

// ── 0.19 单元测试：chat prefill revision 机制 ──────────────────────────────

#[cfg(test)]
mod tests_0_19_prefill {
    use super::*;

    /// take 后清空。
    #[test]
    fn test_take_clears_pending() {
        // 重置状态：先 take 清空任何残留
        let _ = take_chat_prefill();

        let rev = set_chat_prefill("hello");
        let taken = take_chat_prefill();
        assert_eq!(taken, Some((rev, "hello".to_string())));

        // 再次 take 返回 None
        assert_eq!(take_chat_prefill(), None);
    }

    /// 旧 revision 的 ack 不能清除新值。
    #[test]
    fn test_old_revision_ack_does_not_clear_new() {
        let _ = take_chat_prefill();

        let rev1 = set_chat_prefill("first");
        let rev2 = set_chat_prefill("second");
        assert_ne!(rev1, rev2);

        // ack 旧 revision → 不应清除当前 pending（rev2）
        ack_chat_prefill(rev1);
        let taken = take_chat_prefill();
        assert_eq!(taken, Some((rev2, "second".to_string())));
    }

    /// 匹配 revision 的 ack 清除 pending。
    #[test]
    fn test_matching_revision_ack_clears() {
        let _ = take_chat_prefill();

        let rev = set_chat_prefill("data");
        ack_chat_prefill(rev);
        // ack 后 take 应返回 None
        assert_eq!(take_chat_prefill(), None);
    }

    /// 失败回滚（clear）不能清除更新后的值。
    #[test]
    fn test_rollback_does_not_clear_newer_value() {
        let _ = take_chat_prefill();

        let rev1 = set_chat_prefill("old");
        let rev2 = set_chat_prefill("new");

        // 回滚 rev1（模拟 build 失败）→ 不应清除 rev2
        clear_chat_prefill(rev1);
        let taken = take_chat_prefill();
        assert_eq!(taken, Some((rev2, "new".to_string())));
    }

    /// 失败回滚匹配 revision 时清除 pending。
    #[test]
    fn test_rollback_matching_revision_clears() {
        let _ = take_chat_prefill();

        let rev = set_chat_prefill("temp");
        clear_chat_prefill(rev);
        assert_eq!(take_chat_prefill(), None);
    }

    /// revision 单调递增。
    #[test]
    fn test_revision_monotonic() {
        let _ = take_chat_prefill();

        let r1 = set_chat_prefill("a");
        let r2 = set_chat_prefill("b");
        let r3 = set_chat_prefill("c");
        assert!(r1 < r2);
        assert!(r2 < r3);

        // 清理
        let _ = take_chat_prefill();
    }

    /// warm event 成功后 ack 无残留。
    #[test]
    fn test_warm_event_ack_no_residue() {
        let _ = take_chat_prefill();

        let rev = set_chat_prefill("warm-text");
        // 模拟热窗口：event 收到后 ack
        ack_chat_prefill(rev);
        // take 应返回 None（已被 ack 清空）
        assert_eq!(take_chat_prefill(), None);
    }

    /// 冷启动路径：take 拉取后，后续 ack 同 revision 是 no-op。
    #[test]
    fn test_cold_take_then_ack_same_revision_is_noop() {
        let _ = take_chat_prefill();

        let rev = set_chat_prefill("cold-text");
        // 冷启动：take 拉取
        let taken = take_chat_prefill();
        assert_eq!(taken, Some((rev, "cold-text".to_string())));
        // 后续 ack 同 revision → pending 已空，no-op（不 panic）
        ack_chat_prefill(rev);
        assert_eq!(take_chat_prefill(), None);
    }
}
