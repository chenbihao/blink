//! UI Automation (UIA) 公共原语。
//!
//! 从 `selection/windows.rs` 抽取，供 selection（划词抓取）和 inject（G2 焦点恢复）共用。
//!
//! 核心能力：
//! - `get_focused_element()`：跨进程获取前台焦点 UIA 元素
//! - `set_focused_element()`：跨进程恢复焦点到指定元素
//! - `focused_control_type()`：获取焦点控件类型（判断是否文本输入框）
//! - `collect_control_hints()`：0.18.2 逐层 BFS 收集控件矩形（截图控件级智能吸附）
//!
//! COM 公寓用 MTA（UIA 官方建议）。所有函数在后台线程调用（UIA 是跨进程 COM 调用，单次几十 ms）。

use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationElementArray,
    TreeScope_Children,
};

// ── COM 初始化 RAII ──────────────────────────────────────────────────────

/// COM MTA 初始化 RAII guard。
///
/// 构造时 `CoInitializeEx(MTA)`，析构时 `CoUninitialize`。
/// 线程已是其他公寓（如 STA）时不 uninit（避免破坏调用方的公寓状态）。
pub(crate) struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    pub fn init_mta() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            ComGuard {
                should_uninit: true,
            }
        } else {
            tracing::debug!(hr = hr.0, "CoInit MTA 失败（线程已是其他公寓），继续尝试");
            ComGuard {
                should_uninit: false,
            }
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe { CoUninitialize() };
        }
    }
}

/// 创建 UIA 实例。调用方需已初始化 COM（MTA）。
fn create_automation() -> Option<IUIAutomation> {
    match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL) } {
        Ok(a) => Some(a),
        Err(e) => {
            tracing::debug!(error = %e, "CoCreateInstance(CUIAutomation) 失败");
            None
        }
    }
}

// ── 公共 API ──────────────────────────────────────────────────────────────

/// 获取前台应用的焦点 UIA 元素（跨进程）。
///
/// 在后台线程调用。返回 None 表示获取失败或无前台窗口。
/// COM MTA 自动初始化/释放（内部用 ComGuard）。
pub fn get_focused_element() -> Option<IUIAutomationElement> {
    let _com = ComGuard::init_mta();
    let automation = create_automation()?;
    unsafe { automation.GetFocusedElement() }.ok()
}

/// 恢复焦点到指定 UIA 元素（跨进程）。
///
/// 返回 true 表示成功。调用方需已初始化 COM（MTA）。
pub fn set_focused_element(elem: &IUIAutomationElement) -> bool {
    unsafe { elem.SetFocus() }.is_ok()
}

/// 获取前台焦点元素的控件类型 ID。
///
/// 用于判断焦点是否在文本输入控件上（如 `UIA_EditControlTypeId`、
/// `UIA_DocumentControlTypeId`）。返回 None 表示获取失败。
#[allow(dead_code)]
pub fn focused_control_type() -> Option<i32> {
    let elem = get_focused_element()?;
    unsafe { elem.CurrentControlType() }.map(|t| t.0).ok()
}

/// 获取前台焦点元素的类名（诊断用）。
#[allow(dead_code)]
pub fn focused_class_name() -> Option<String> {
    let elem = get_focused_element()?;
    unsafe { elem.CurrentClassName() }
        .ok()
        .map(|s| s.to_string())
}

// ── 文本输入控件类型判断 ────────────────────────────────────────────────

/// UIA 控件类型 ID 常量（来自 windows crate 的 CONTROLTYPE_ID）。
///
/// 见 https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-controltype-ids
const UIA_EDIT_CONTROL_TYPE_ID: i32 = 50004;
const UIA_DOCUMENT_CONTROL_TYPE_ID: i32 = 50036;
const UIA_EDIT2_CONTROL_TYPE_ID: i32 = 50089; // Chromium 内部 "Edit" 变体

/// 判断指定控件类型 ID 是否属于文本输入控件。
///
/// 覆盖原生 Win32 Edit、UWP/WinUI TextBox（Document）、Chromium 输入框（Edit2）。
pub fn is_text_input_control(control_type_id: i32) -> bool {
    control_type_id == UIA_EDIT_CONTROL_TYPE_ID
        || control_type_id == UIA_DOCUMENT_CONTROL_TYPE_ID
        || control_type_id == UIA_EDIT2_CONTROL_TYPE_ID
}

/// 判断当前前台焦点是否在文本输入控件上。
///
/// 在后台线程调用。返回 false 表示获取失败或焦点不在文本输入框。
#[allow(dead_code)]
pub fn is_focused_on_text_input() -> bool {
    focused_control_type()
        .map(|ct| is_text_input_control(ct))
        .unwrap_or(false)
}

// ── 0.18.2 控件级智能吸附 ──────────────────────────────────────────────────

/// 读取窗口的 DWM 扩展边框（物理像素，虚拟屏幕坐标系）。
///
/// 与 `list.rs` 中 `enumerate_pickable_windows` 使用相同的 API，确保坐标系一致。
/// 用于控件矩形 clamp——Chromium/Electron 的 UIA 树会暴露网页 DOM 元素，
/// 这些元素的 BoundingRectangle 可能超出窗口可视区域（滚动内容、整页文档等），
/// 需要约束到窗口边框内。
///
/// 返回 None 表示 DWM 不可用，调用方应跳过 clamp（降级为不裁剪）。
fn get_window_dwm_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if hr.is_ok() {
        Some(rect)
    } else {
        None
    }
}

/// 截图控件吸附提示——一个 UIA 控件的矩形 + 类型信息。
///
/// 坐标为**虚拟屏幕物理像素**（与截图 SESSION 同坐标系，与 `PickableWindow` 一致）。
/// 前端 `rectScreenToCss` 转 CSS 后做 hit-test。
///
/// `control_type` 仅诊断/未来筛选用，前端 hit-test 不依赖。
/// `name` 仅后端 trace 级日志诊断用，**不下发前端展示**（界面文本可能含敏感信息）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ControlHint {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// UIA 控件类型 ID（如 50004=Edit / 50000=Button）
    pub control_type: i32,
    /// UIA CurrentName（仅诊断日志用，前端不展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// 控件吸附收集的默认超时（1s deadline）。
///
/// 异步收集不阻塞 overlay 显示和拖拽，宽松超时让更多应用在 budget 内到达有用控件层。
/// 实际运行时由 `ScreenshotConfig.control_snap_deadline_ms` 配置覆盖。
#[allow(dead_code)]
pub const CONTROL_HINT_DEADLINE: Duration = Duration::from_millis(1000);

/// 记事本 1 层够（编辑框）、计算器 2 层（分组→按钮）、WPF/Office 一般 3-4 层到有用控件、
/// Electron 第 5 层仍是 chrome 容器（被超时挡掉，无害）。
/// 死端控件类型（ScrollBar/Thumb/TitleBar 等）不展开子树，节省 COM 调用预算。
/// 实际运行时由 `ScreenshotConfig.control_snap_depth` 配置覆盖。
#[allow(dead_code)]
pub const CONTROL_HINT_MAX_DEPTH: usize = 15;

/// 控件吸附最小展开尺寸（物理像素，0=禁用）。
///
/// 控件宽或高低于此值则不展开子树（控件自身仍作为 hint 被收集）。
/// 跳过微型控件的子树以节省 COM 调用预算。
/// 实际运行时由 `ScreenshotConfig.control_snap_min_size` 配置覆盖。
#[allow(dead_code)]
pub const CONTROL_HINT_MIN_SIZE: i32 = 50;

// ── 死端控件类型剪枝 ─────────────────────────────────────────────────────

/// UIA 控件类型 ID 常量——死端控件（不展开子树）。
///
/// 这些控件类型的子元素对截图吸附无意义（如 ScrollBar 的 Thumb、TitleBar 的
/// min/max/close 按钮），展开它们浪费 COM 调用预算。
const UIA_SCROLLBAR_CONTROL_TYPE_ID: i32 = 50014;
const UIA_THUMB_CONTROL_TYPE_ID: i32 = 50027;
const UIA_TITLEBAR_CONTROL_TYPE_ID: i32 = 50036;
const UIA_SEPARATOR_CONTROL_TYPE_ID: i32 = 50037;
const UIA_TOOLTIP_CONTROL_TYPE_ID: i32 = 50022;
const UIA_PROGRESSBAR_CONTROL_TYPE_ID: i32 = 50012;

/// 判断控件类型是否为"死端"——即其子元素对截图吸附无意义，不应展开。
///
/// 注意：死端控件**仍会作为 hint 被收集**（它自身有矩形可以吸附），
/// 只是不展开它的子树以节省 COM 调用预算。
fn is_dead_end_control_type(control_type_id: i32) -> bool {
    matches!(
        control_type_id,
        UIA_SCROLLBAR_CONTROL_TYPE_ID
            | UIA_THUMB_CONTROL_TYPE_ID
            | UIA_TITLEBAR_CONTROL_TYPE_ID
            | UIA_SEPARATOR_CONTROL_TYPE_ID
            | UIA_TOOLTIP_CONTROL_TYPE_ID
            | UIA_PROGRESSBAR_CONTROL_TYPE_ID
    )
}

/// 从 HWND 获取 UIA 根元素。
///
/// 用 `ElementFromHandle` 而非 `ElementFromPoint`——截图 overlay 是 always_on_top
/// 全屏透明窗，`ElementFromPoint` 会返回 overlay 自己。我们只读控件的 BoundingRectangle，
/// 不 hit-test 坐标，所以不关心 overlay 盖没盖。
#[allow(dead_code)]
pub fn element_from_handle(hwnd: HWND) -> Option<IUIAutomationElement> {
    let _com = ComGuard::init_mta();
    let automation = create_automation()?;
    unsafe { automation.ElementFromHandle(hwnd) }.ok()
}

/// 逐层 BFS 收集控件矩形（0.18.2 截图控件级智能吸附）。
///
/// 使用编译期默认参数。实际运行时应通过 `collect_control_hints_with` 传配置值。
///
/// **调用方**需在 `spawn_blocking` 中调用（UIA 是同步 COM 调用）。
/// COM MTA 自动初始化/释放。
#[allow(dead_code)]
pub fn collect_control_hints(hwnd: HWND) -> Vec<ControlHint> {
    collect_control_hints_with(hwnd, CONTROL_HINT_DEADLINE, CONTROL_HINT_MAX_DEPTH, CONTROL_HINT_MIN_SIZE)
}

/// 带自定义 deadline、深度和最小展开尺寸的 `collect_control_hints`。
///
/// - `deadline`：BFS 超时，超时后返回已收集的部分结果
/// - `max_depth`：往下遍历几层子元素（不含 root）
/// - `min_size`：物理像素，控件宽或高低于此值则不展开子树（0=禁用）
pub fn collect_control_hints_with(
    hwnd: HWND,
    deadline: Duration,
    max_depth: usize,
    min_size: i32,
) -> Vec<ControlHint> {
    let started = Instant::now();
    let _com = ComGuard::init_mta();
    let automation = match create_automation() {
        Some(a) => a,
        None => {
            tracing::debug!("collect_control_hints: create_automation 失败");
            return Vec::new();
        }
    };

    let root = match unsafe { automation.ElementFromHandle(hwnd) } {
        Ok(elem) => elem,
        Err(e) => {
            tracing::debug!(error = %e, "collect_control_hints: ElementFromHandle 失败");
            return Vec::new();
        }
    };

    // 读取窗口 DWM 扩展边框，用于 clamp 控件矩形到窗口可视区域内。
    // Chromium/Electron 的 UIA DOM 元素可能返回超出窗口的矩形（滚动内容等）。
    let win_rect = get_window_dwm_rect(hwnd);
    if win_rect.is_none() {
        tracing::debug!("collect_control_hints: DWM 扩展边框不可用，跳过 clamp");
    }

    let deadline_instant = started + deadline;
    let hints = bfs_collect(
        root,
        max_depth,
        || Instant::now() >= deadline_instant,
        |elem| fetch_uia_children(&automation, elem),
        |elem| {
            let ct = unsafe { elem.CurrentControlType() }
                .map(|t| t.0)
                .unwrap_or(0);
            if is_dead_end_control_type(ct) {
                tracing::trace!(control_type = ct, "跳过展开（死端控件类型）");
                return false;
            }
            if min_size > 0 {
                if let Ok(rect) = unsafe { elem.CurrentBoundingRectangle() } {
                    let w = rect.right - rect.left;
                    let h = rect.bottom - rect.top;
                    if w < min_size || h < min_size {
                        tracing::trace!(
                            w, h, min_size,
                            "跳过展开（控件尺寸低于阈值）"
                        );
                        return false;
                    }
                }
            }
            true
        },
        // 提取 hint 后 clamp 到窗口边框内：Chromium DOM 元素可能超出窗口
        |elem| {
            let hint = extract_uia_hint(elem)?;
            if let Some(wr) = win_rect {
                clamp_hint_to_rect(hint, wr)
            } else {
                Some(hint)
            }
        },
    );

    let elapsed = started.elapsed();
    tracing::debug!(
        hints_count = hints.len(),
        elapsed_ms = elapsed.as_millis() as u64,
        max_depth,
        min_size,
        "collect_control_hints 完成"
    );
    if elapsed >= deadline {
        tracing::warn!(
            elapsed_ms = elapsed.as_millis() as u64,
            deadline_ms = deadline.as_millis() as u64,
            "collect_control_hints 超时降级（部分结果）"
        );
    }
    hints
}

/// 带 `on_batch` 回调的 BFS（流式推送用）。
///
/// 每完成一层，`on_batch(&hints[batch_start..], depth)` 被调用一次，
/// 传入该层新增的 hints 切片和层号（0-based）。
///
/// 其余参数同 `bfs_collect`。
fn bfs_collect_with_batch<E>(
    root: E,
    max_depth: usize,
    is_expired: impl Fn() -> bool,
    fetch_children: impl Fn(&E) -> Vec<E>,
    should_expand: impl Fn(&E) -> bool,
    extract_hint: impl Fn(&E) -> Option<ControlHint>,
    mut on_batch: impl FnMut(&[ControlHint], usize),
) -> Vec<ControlHint>
where
    E: Clone,
{
    let mut hints = Vec::new();
    let mut current_layer = vec![root];

    for depth in 0..max_depth {
        if is_expired() {
            tracing::trace!(depth, "bfs 截断（deadline 到达）");
            break;
        }

        let batch_start = hints.len();

        tracing::trace!(
            depth,
            layer_size = current_layer.len(),
            hints_so_far = hints.len(),
            "bfs 开始遍历层"
        );

        let mut next_layer = Vec::new();
        let mut children_found = 0usize;
        let mut expanded = 0usize;

        for elem in &current_layer {
            if is_expired() {
                break;
            }

            let children = fetch_children(elem);
            children_found += children.len();
            for child in children {
                if is_expired() {
                    break;
                }

                if let Some(hint) = extract_hint(&child) {
                    hints.push(hint);
                }

                if should_expand(&child) {
                    next_layer.push(child);
                    expanded += 1;
                }
            }
        }

        tracing::trace!(
            depth,
            children_found,
            expanded,
            skipped = children_found.saturating_sub(expanded),
            next_layer_size = next_layer.len(),
            hints_so_far = hints.len(),
            "bfs 层完成"
        );

        on_batch(&hints[batch_start..], depth);

        current_layer = next_layer;
        if current_layer.is_empty() {
            break;
        }
    }

    hints
}

/// 旧签名保留（6 个测试 + 向后兼容），内部转发，on_batch = noop。
fn bfs_collect<E>(
    root: E,
    max_depth: usize,
    is_expired: impl Fn() -> bool,
    fetch_children: impl Fn(&E) -> Vec<E>,
    should_expand: impl Fn(&E) -> bool,
    extract_hint: impl Fn(&E) -> Option<ControlHint>,
) -> Vec<ControlHint>
where
    E: Clone,
{
    bfs_collect_with_batch(
        root,
        max_depth,
        is_expired,
        fetch_children,
        should_expand,
        extract_hint,
        |_, _| {},
    )
}

/// 流式版 `collect_control_hints_with`：每层完成后调 `on_batch`。
///
/// `on_batch(batch_hints, depth)` 收到的是**该层新增**的 hints（已 clamp）。
/// 调用方负责 emit 给前端。
///
/// 返回 `(全部 hints, 是否因 deadline 截断)`。
/// 不改动现有 `collect_control_hints_with` 签名（旧路径可能还有调用/测试）。
pub fn collect_control_hints_streaming<F>(
    hwnd: HWND,
    deadline: Duration,
    max_depth: usize,
    min_size: i32,
    mut on_batch: F,
) -> (Vec<ControlHint>, bool)
where
    F: FnMut(&[ControlHint], usize),
{
    let started = Instant::now();
    let _com = ComGuard::init_mta();
    let automation = match create_automation() {
        Some(a) => a,
        None => {
            tracing::debug!("collect_control_hints_streaming: create_automation 失败");
            return (Vec::new(), false);
        }
    };

    let root = match unsafe { automation.ElementFromHandle(hwnd) } {
        Ok(elem) => elem,
        Err(e) => {
            tracing::debug!(error = %e, "collect_control_hints_streaming: ElementFromHandle 失败");
            return (Vec::new(), false);
        }
    };

    let win_rect = get_window_dwm_rect(hwnd);
    if win_rect.is_none() {
        tracing::debug!("collect_control_hints_streaming: DWM 扩展边框不可用，跳过 clamp");
    }

    let deadline_instant = started + deadline;
    let truncated = std::cell::Cell::new(false);
    let hints = bfs_collect_with_batch(
        root,
        max_depth,
        || {
            let expired = Instant::now() >= deadline_instant;
            if expired {
                truncated.set(true);
            }
            expired
        },
        |elem| fetch_uia_children(&automation, elem),
        |elem| {
            let ct = unsafe { elem.CurrentControlType() }
                .map(|t| t.0)
                .unwrap_or(0);
            if is_dead_end_control_type(ct) {
                tracing::trace!(control_type = ct, "跳过展开（死端控件类型）");
                return false;
            }
            if min_size > 0 {
                if let Ok(rect) = unsafe { elem.CurrentBoundingRectangle() } {
                    let w = rect.right - rect.left;
                    let h = rect.bottom - rect.top;
                    if w < min_size || h < min_size {
                        tracing::trace!(
                            w, h, min_size,
                            "跳过展开（控件尺寸低于阈值）"
                        );
                        return false;
                    }
                }
            }
            true
        },
        |elem| {
            let hint = extract_uia_hint(elem)?;
            if let Some(wr) = win_rect {
                clamp_hint_to_rect(hint, wr)
            } else {
                Some(hint)
            }
        },
        |batch, depth| on_batch(batch, depth),
    );

    let elapsed = started.elapsed();
    tracing::debug!(
        hints_count = hints.len(),
        elapsed_ms = elapsed.as_millis() as u64,
        max_depth,
        min_size,
        truncated = truncated.get(),
        "collect_control_hints_streaming 完成"
    );
    if elapsed >= deadline {
        tracing::warn!(
            elapsed_ms = elapsed.as_millis() as u64,
            deadline_ms = deadline.as_millis() as u64,
            "collect_control_hints_streaming 超时降级（部分结果）"
        );
    }
    (hints, truncated.get())
}

/// UIA `FindAll(TreeScope_Children, TrueCondition)` 获取直接子元素。
fn fetch_uia_children(
    automation: &IUIAutomation,
    elem: &IUIAutomationElement,
) -> Vec<IUIAutomationElement> {
    let condition = match unsafe { automation.CreateTrueCondition() } {
        Ok(c) => c,
        Err(e) => {
            tracing::trace!(error = %e, "CreateTrueCondition 失败");
            return Vec::new();
        }
    };

    let array: IUIAutomationElementArray =
        match unsafe { elem.FindAll(TreeScope_Children, &condition) } {
            Ok(arr) => arr,
            Err(e) => {
                tracing::trace!(error = %e, "FindAll(Children) 失败");
                return Vec::new();
            }
        };

    let len = unsafe { array.Length() }.unwrap_or(0);
    if len == 0 {
        return Vec::new();
    }

    let mut children = Vec::with_capacity(len as usize);
    for i in 0..len {
        if let Ok(child) = unsafe { array.GetElement(i) } {
            children.push(child);
        }
    }
    children
}

/// 从 UIA 元素提取 `ControlHint`（读 BoundingRectangle / ControlType / Name）。
///
/// 返回 `None` 表示元素无有效矩形（零尺寸或读取失败），应跳过。
fn extract_uia_hint(elem: &IUIAutomationElement) -> Option<ControlHint> {
    let rect = unsafe { elem.CurrentBoundingRectangle() }.ok()?;
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 {
        return None;
    }

    let control_type = unsafe { elem.CurrentControlType() }
        .map(|t| t.0)
        .unwrap_or(0);

    let name = unsafe { elem.CurrentName() }
        .ok()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());

    // trace 级记录控件 name（诊断用），不记 info/debug 避免泄露界面文本
    if let Some(ref n) = name {
        tracing::trace!(control_type, name = %n, "UIA 控件命中");
    }

    Some(ControlHint {
        x: rect.left,
        y: rect.top,
        w,
        h,
        control_type,
        name,
    })
}

/// 将控件矩形 clamp 到窗口边框内。
///
/// Chromium/Electron 的 UIA DOM 元素可能返回超出窗口可视区域的矩形
///（滚动内容、整页文档高度等）。本方法将矩形与窗口边框求交：
/// - 部分超出 → 裁剪到窗口内
/// - 完全超出 → 返回 None（该控件不可见，不应作为吸附提示）
///
/// 返回 `None` 表示裁剪后面积为零或负值，该 hint 应被丢弃。
fn clamp_hint_to_rect(mut hint: ControlHint, win: RECT) -> Option<ControlHint> {
    let left = hint.x.max(win.left);
    let top = hint.y.max(win.top);
    let right = (hint.x + hint.w).min(win.right);
    let bottom = (hint.y + hint.h).min(win.bottom);
    let w = right - left;
    let h = bottom - top;
    if w <= 0 || h <= 0 {
        return None;
    }
    hint.x = left;
    hint.y = top;
    hint.w = w;
    hint.h = h;
    Some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// 测试用 mock 元素：每层 3 个子元素，共 4 层深度。
    /// 每个元素有一个唯一的 id 用于 hint。
    #[derive(Clone)]
    struct MockElem {
        id: usize,
    }

    fn make_tree(depth: usize, branching: usize) -> Vec<(usize, Vec<MockElem>)> {
        let id_counter = Cell::new(0usize);
        let mut nodes: Vec<(usize, Vec<MockElem>)> = Vec::new();

        fn build(
            depth: usize,
            branching: usize,
            id_counter: &Cell<usize>,
            nodes: &mut Vec<(usize, Vec<MockElem>)>,
        ) -> MockElem {
            let id = id_counter.get();
            id_counter.set(id + 1);
            let mut children = Vec::new();
            if depth > 0 {
                for _ in 0..branching {
                    children.push(build(depth - 1, branching, id_counter, nodes));
                }
            }
            nodes.push((id, children.clone()));
            MockElem { id }
        }

        let _root = build(depth, branching, &id_counter, &mut nodes);
        nodes
    }

    #[test]
    fn bfs_depth_truncation_stops_at_max_depth() {
        // 4 层深、每层 2 个子元素
        let nodes = make_tree(4, 2);
        let nodes_ref = &nodes;

        let root = MockElem { id: 0 };

        let hints = bfs_collect(
            root,
            3, // max_depth = 3
            || false, // never expire
            |elem| {
                nodes_ref
                    .iter()
                    .find(|(id, _)| *id == elem.id)
                    .map(|(_, children)| children.clone())
                    .unwrap_or_default()
            },
            |_| true, // expand all
            |elem| {
                Some(ControlHint {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 10,
                    control_type: 50000,
                    name: Some(format!("elem_{}", elem.id)),
                })
            },
        );

        // 树结构：root(id=0) → 2 子(id=1,2) → 4 孙(id=3,4,5,6) → 8 曾孙(id=7..14) → 16
        // max_depth=3: 收集 depth 1(2) + depth 2(4) + depth 3(8) = 14 个
        // 不含 root，不含 depth 4(16 个)
        assert_eq!(
            hints.len(),
            14,
            "max_depth=3 应收集 3 层子元素（2+4+8=14），不含第 4 层"
        );
    }

    #[test]
    fn bfs_deadline_stops_early_returns_partial_results() {
        let nodes = make_tree(3, 3);
        let nodes_ref = &nodes;
        let root = MockElem { id: 0 };

        let call_count = Cell::new(0usize);

        let hints = bfs_collect(
            root,
            10, // high max_depth, rely on deadline
            || {
                let n = call_count.get();
                call_count.set(n + 1);
                n >= 5 // expire after 5 is_expired() checks
            },
            |elem| {
                nodes_ref
                    .iter()
                    .find(|(id, _)| *id == elem.id)
                    .map(|(_, children)| children.clone())
                    .unwrap_or_default()
            },
            |_| true, // expand all
            |elem| {
                Some(ControlHint {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 10,
                    control_type: 50000,
                    name: Some(format!("elem_{}", elem.id)),
                })
            },
        );

        // deadline 在第 5 次 is_expired 检查后触发，应该收集到部分结果
        assert!(
            !hints.is_empty(),
            "deadline 截断应返回部分结果，不应为空"
        );
        // 且不应收集到全部结果（树有 3+9+27=39 个非 root 元素）
        assert!(
            hints.len() < 39,
            "deadline 截断应返回部分结果，不应收集全部 39 个"
        );
    }

    #[test]
    fn bfs_empty_tree_returns_empty() {
        let root = MockElem { id: 0 };
        let hints = bfs_collect(
            root,
            3,
            || false,
            |_| Vec::new(), // no children
            |_| true,       // expand all
            |_| {
                Some(ControlHint {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 10,
                    control_type: 50000,
                    name: None,
                })
            },
        );
        // root has no children → 0 hints
        assert_eq!(hints.len(), 0, "空树应返回空");
    }

    #[test]
    fn bfs_skips_none_hints() {
        let children = vec![MockElem { id: 1 }, MockElem { id: 2 }, MockElem { id: 3 }];
        let root = MockElem { id: 0 };

        let hints = bfs_collect(
            root,
            1,
            || false,
            |_| children.clone(),
            |_| true, // expand all
            |elem| {
                // 只有 id=2 产生有效 hint
                if elem.id == 2 {
                    Some(ControlHint {
                        x: 10,
                        y: 10,
                        w: 50,
                        h: 30,
                        control_type: 50004,
                        name: None,
                    })
                } else {
                    None
                }
            },
        );
        assert_eq!(hints.len(), 1, "只有 1 个元素产生有效 hint");
        assert_eq!(hints[0].control_type, 50004);
    }

    #[test]
    fn bfs_pruning_skips_dead_end_subtrees() {
        // make_tree(2,2) 深度优先编号：root(id=0) → [id=1, id=4]
        //   id=1 → [id=2, id=3]（叶子）
        //   id=4 → [id=5, id=6]（叶子）
        // should_expand 对 id=1 返回 false（模拟死端控件），其子 id=2,3 不被遍历
        let nodes = make_tree(2, 2);
        let nodes_ref = &nodes;
        let root = MockElem { id: 0 };

        let hints = bfs_collect(
            root,
            5, // enough depth
            || false,
            |elem| {
                nodes_ref
                    .iter()
                    .find(|(id, _)| *id == elem.id)
                    .map(|(_, children)| children.clone())
                    .unwrap_or_default()
            },
            |elem| elem.id != 1, // skip expanding id=1 (dead-end)
            |_| {
                Some(ControlHint {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 10,
                    control_type: 50000,
                    name: None,
                })
            },
        );

        // 层 1（root 的子）：id=1, id=4 → 2 hints，但 id=1 不展开
        // 层 2（id=4 的子）：id=5, id=6 → 2 hints
        // id=2, id=3（id=1 的子）不被遍历
        // 总计 4 hints
        assert_eq!(
            hints.len(),
            4,
            "id=1 不展开，其子(id=2,3)不被遍历，总计 4 个 hint"
        );
    }

    // ── clamp_hint_to_rect 单测 ──────────────────────────────────

    fn make_hint(x: i32, y: i32, w: i32, h: i32) -> ControlHint {
        ControlHint {
            x,
            y,
            w,
            h,
            control_type: 50000,
            name: None,
        }
    }

    fn make_rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn clamp_hint_fully_inside_unchanged() {
        // 控件完全在窗口内 → 矩形不变
        let hint = make_hint(100, 100, 200, 150);
        let win = make_rect(0, 0, 1920, 1080);
        let clamped = clamp_hint_to_rect(hint, win).unwrap();
        assert_eq!(clamped.x, 100);
        assert_eq!(clamped.y, 100);
        assert_eq!(clamped.w, 200);
        assert_eq!(clamped.h, 150);
    }

    #[test]
    fn clamp_hint_partially_outside_right_bottom() {
        // 控件右下角超出窗口 → 裁剪到窗口边界
        let hint = make_hint(1800, 1000, 300, 200);
        let win = make_rect(0, 0, 1920, 1080);
        let clamped = clamp_hint_to_rect(hint, win).unwrap();
        assert_eq!(clamped.x, 1800);
        assert_eq!(clamped.y, 1000);
        assert_eq!(clamped.w, 120); // 1920 - 1800
        assert_eq!(clamped.h, 80);  // 1080 - 1000
    }

    #[test]
    fn clamp_hint_partially_outside_left_top() {
        // 控件左上角超出窗口（负坐标） → 裁剪到窗口边界
        let hint = make_hint(-50, -30, 200, 150);
        let win = make_rect(0, 0, 1920, 1080);
        let clamped = clamp_hint_to_rect(hint, win).unwrap();
        assert_eq!(clamped.x, 0);
        assert_eq!(clamped.y, 0);
        assert_eq!(clamped.w, 150); // 200 - 50
        assert_eq!(clamped.h, 120); // 150 - 30
    }

    #[test]
    fn clamp_hint_completely_outside_returns_none() {
        // 控件完全在窗口外 → 返回 None
        let hint = make_hint(2000, 2000, 100, 100);
        let win = make_rect(0, 0, 1920, 1080);
        assert!(clamp_hint_to_rect(hint, win).is_none());
    }

    #[test]
    fn clamp_hint_completely_outside_negative_returns_none() {
        // 控件完全在窗口左上方（负坐标区域） → 返回 None
        let hint = make_hint(-200, -200, 100, 100);
        let win = make_rect(0, 0, 1920, 1080);
        assert!(clamp_hint_to_rect(hint, win).is_none());
    }

    #[test]
    fn clamp_hint_chromium_dom_scenario() {
        // 模拟 Chromium DOM Document 元素：矩形高度远超窗口
        //（网页内容总高度 5000px，但窗口只有 1080px 高）
        let hint = make_hint(0, 0, 1920, 5000);
        let win = make_rect(0, 0, 1920, 1080);
        let clamped = clamp_hint_to_rect(hint, win).unwrap();
        assert_eq!(clamped.x, 0);
        assert_eq!(clamped.y, 0);
        assert_eq!(clamped.w, 1920);
        assert_eq!(clamped.h, 1080, "DOM Document 应被裁剪到窗口高度");
    }

    #[test]
    fn clamp_hint_edge_touching_returns_none() {
        // 控件恰好贴在窗口边界外侧（右边界 = 窗口左边界）
        let hint = make_hint(1920, 0, 100, 100);
        let win = make_rect(0, 0, 1920, 1080);
        assert!(clamp_hint_to_rect(hint, win).is_none());
    }

    #[test]
    fn bfs_with_batch_calls_on_batch_per_layer() {
        // make_tree(3, 2)：root(id=0) → [id=1, id=8] → [id=2,5,9,12] → [id=3,4,6,7,10,11,13,14]
        let nodes = make_tree(3, 2);
        let nodes_ref = &nodes;
        let root = MockElem { id: 0 };

        let mut calls = Vec::new();
        let hints = bfs_collect_with_batch(
            root,
            3, // max_depth=3
            || false,
            |e| {
                nodes_ref
                    .iter()
                    .find(|(id, _)| *id == e.id)
                    .map(|(_, children)| children.clone())
                    .unwrap_or_default()
            },
            |_| true, // expand all
            |e| {
                Some(ControlHint {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 10,
                    control_type: 50000,
                    name: Some(format!("e{}", e.id)),
                })
            },
            |batch, depth| calls.push((depth, batch.len())),
        );

        // 3 层各调一次 on_batch
        assert_eq!(calls.len(), 3, "3 层各调一次 on_batch");
        // depth 0: root 的 2 个子 → batch 2
        assert_eq!(calls[0], (0, 2));
        // depth 1: 4 个子 → batch 4
        assert_eq!(calls[1], (1, 4));
        // depth 2: 8 个子 → batch 8
        assert_eq!(calls[2], (2, 8));
        // 总计 2+4+8=14
        assert_eq!(hints.len(), 14);
    }

    #[test]
    fn bfs_with_clamp_filters_out_of_bounds_hints() {
        // 验证 BFS + clamp 集成：3 个子元素，其中 1 个完全在窗口外
        let children = vec![MockElem { id: 1 }, MockElem { id: 2 }, MockElem { id: 3 }];
        let root = MockElem { id: 0 };
        let win = make_rect(0, 0, 1000, 1000);

        let hints = bfs_collect(
            root,
            1,
            || false,
            |_| children.clone(),
            |_| true,
            |elem| {
                let hint = match elem.id {
                    1 => make_hint(100, 100, 200, 200),      // 在窗口内
                    2 => make_hint(1100, 100, 200, 200),     // 完全在窗口外
                    3 => make_hint(900, 900, 200, 200),       // 部分超出
                    _ => return None,
                };
                clamp_hint_to_rect(hint, win)
            },
        );

        assert_eq!(hints.len(), 2, "id=2 完全在窗口外应被过滤");
        // id=3 部分超出，裁剪后应为 (900, 900, 100, 100)
        let partial = hints.iter().find(|h| h.x == 900).unwrap();
        assert_eq!(partial.w, 100);
        assert_eq!(partial.h, 100);
    }
}
