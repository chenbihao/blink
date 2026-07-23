//! Windows 平台选区抓取：UIA TextPattern。
//!
//! API 路径（三段式，逐级降级）：
//! 1. **GetFocusedElement（主）**：直取焦点元素 → TextPattern → GetSelection。O(1)。
//! 2. **祖先链**：焦点元素无 TextPattern 时，`FindFirst(Ancestors)` 向上找。
//!    场景：焦点是 `<span>` 子节点，TextPattern 在容器祖先上。O(深度)。
//! 3. **焦点子树后代**：`FindFirst(Descendants)` 在焦点元素子树内向下找。
//!    场景：焦点是 WebView2 宿主 Pane，TextPattern 在其子 Document 上。O(焦点子树)。
//!
//! **不再使用 FindAll(Subtree) 全树回退**。原因：
//! - FindAll 遍历整个窗口 UIA 树（含标题栏/工具栏等无关区域），大子树应用 1-12 秒
//! - 三段式已覆盖选区所有可能位置：焦点元素本身 / 其祖先 / 其后代
//! - FindAll 耗时期间选区退化——实测候选虽在但选区已空，1-12 秒纯浪费
//! - UIA COM 是同步阻塞调用，无 async API；加超时需额外线程且被放弃的线程仍在跑
//!
//! 局限：Scintilla(Notepad3)/Java Swing 等控件不暴露 UIA TextPattern，三段式均不命中，
//! 这类 UIA 无解，只能靠 Ctrl+C 兜底（文档 §1.1 初期不做，属「明确不支持」）。
//!
//! COM 公寓用 MTA（UIA 官方建议），与图标提取那条 STA 路径互不影响。
//!
//! 日志策略：
//! - 命中（正常路径）：trace，含阶段名 + 耗时
//! - 未命中（常见：用户在非文本区域拖拽/点击）：trace，含诊断字段
//! - 耗时超阈值(>200ms)：升级为 warn（性能退化信号）
//! - COM 初始化/创建失败：debug（罕见，诊断用）

use std::time::Instant;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomationCondition, IUIAutomationElement, IUIAutomationTextPattern,
    PropertyConditionFlags_None, TreeScope_Ancestors, TreeScope_Descendants,
    UIA_IsTextPatternAvailablePropertyId, UIA_TextPatternId,
};
use windows::core::Interface;

use crate::infra::platform::uia;

// 用于 hwnd → 进程名 的 Win32 调用（隐私门控：见 listener.rs on_selection）
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use windows::Win32::Foundation::{HANDLE, MAX_PATH};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

/// 耗时告警阈值：超过此值打 warn。
const SLOW_THRESHOLD_MS: u128 = 200;

/// 抓取指定窗口当前的鼠标选区文本。
///
/// 三段式策略（逐级降级），任意环节失败均返回 None，绝不抛错。
pub(crate) fn get_selected_text(hwnd_raw: isize) -> Option<String> {
    if hwnd_raw == 0 {
        return None;
    }
    // COM 初始化 + UIA 实例创建（从公共 uia 模块）
    let _com = uia::ComGuard::init_mta();
    let automation: IUIAutomation = match unsafe {
        windows::Win32::System::Com::CoCreateInstance(
            &windows::Win32::UI::Accessibility::CUIAutomation,
            None,
            windows::Win32::System::Com::CLSCTX_ALL,
        )
    } {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(error = %e, "选区抓取：CoCreateInstance(CUIAutomation) 失败");
            return None;
        }
    };

    let start = Instant::now();

    // ── Phase 1: GetFocusedElement — 焦点元素直取 ──────────────
    // 选词瞬间焦点就在用户操作的控件上，选区也在它上面。O(1) 跨进程调用。
    if let Ok(focused) = unsafe { automation.GetFocusedElement() } {
        // 1a. 焦点元素自身是否支持 TextPattern 且有选区
        if let Some(text) = extract_selection(&focused) {
            log_hit(&start, "GetFocusedElement");
            return Some(text);
        }

        let cond = create_text_pattern_condition(&automation);

        // ── Phase 2: 祖先链查找 ─────────────────────────────────
        // 焦点元素自身无 TextPattern 时，向上找最近的 TextPattern 祖先。
        // 场景：焦点是 <span> 等子节点，TextPattern 在容器祖先上。O(深度)。
        if let Some(ref cond) = cond {
            if let Ok(ancestor) = unsafe { focused.FindFirst(TreeScope_Ancestors, cond) } {
                if let Some(text) = extract_selection(&ancestor) {
                    log_hit(&start, "祖先链");
                    return Some(text);
                }
            }
        }

        // ── Phase 3: 焦点子树后代查找 ───────────────────────────
        // 祖先链也未命中时，在焦点元素的子树内向下找 TextPattern 元素。
        // 场景：焦点是 WebView2 宿主 Pane（无 TextPattern），其子 Document 有 TextPattern。
        // O(焦点子树)，远小于 FindAll(Subtree) 的 O(全窗口子树)。
        let mut descendant_ct: i32 = 0; // 若找到后代，记录其控件类型供诊断
        if let Some(ref cond) = cond {
            if let Ok(descendant) = unsafe { focused.FindFirst(TreeScope_Descendants, cond) } {
                descendant_ct = unsafe { descendant.CurrentControlType() }
                    .map(|t| t.0)
                    .unwrap_or(0);
                if let Some(text) = extract_selection(&descendant) {
                    log_hit(&start, "焦点子树后代");
                    return Some(text);
                }
                // 后代有 TextPattern 但无选区——选区可能已退化
            }
        }

        // ── 三段式均未命中：合并为单条日志 ─────────────────────
        // 这是常见情况（用户在非文本区域拖拽/点击，或不支持 UIA 的应用），
        // 用 trace 而非 debug 避免日志噪音。诊断信息合并到一条。
        let elapsed_ms = start.elapsed().as_millis();
        let (ct, class) = element_info(&focused);
        if elapsed_ms > SLOW_THRESHOLD_MS {
            tracing::warn!(
                elapsed_ms,
                control_type = ct,
                class = %class,
                descendant_control_type = descendant_ct,
                "选区抓取：三段式未命中（耗时超阈值）"
            );
        } else {
            // tracing::trace!(
            //     elapsed_ms,
            //     control_type = ct,
            //     class = %class,
            //     descendant_control_type = descendant_ct,
            //     "选区抓取：三段式未命中"
            // );
        }
    }

    None
}

/// 命中时记录单条 trace（超阈值升级 warn）。
fn log_hit(start: &Instant, phase: &str) {
    let elapsed_ms = start.elapsed().as_millis();
    if elapsed_ms > SLOW_THRESHOLD_MS {
        tracing::warn!(phase, elapsed_ms, "选区抓取：命中但耗时超阈值");
    } else {
        tracing::trace!(phase, elapsed_ms, "选区抓取：命中");
    }
}

/// 获取元素的控件类型和类名（诊断用）。
fn element_info(elem: &IUIAutomationElement) -> (i32, String) {
    let ct = unsafe { elem.CurrentControlType() }
        .map(|t| t.0)
        .unwrap_or(0);
    let class = unsafe { elem.CurrentClassName() }
        .map(|s| s.to_string())
        .unwrap_or_default();
    (ct, class)
}

/// 创建「IsTextPatternAvailable == true」属性条件。失败返回 None。
fn create_text_pattern_condition(automation: &IUIAutomation) -> Option<IUIAutomationCondition> {
    unsafe {
        automation
            .CreatePropertyConditionEx(
                UIA_IsTextPatternAvailablePropertyId,
                &VARIANT::from(true),
                PropertyConditionFlags_None,
            )
            .ok()
    }
}

/// 从单个 UIA 元素提取选区文本。
///
/// 元素必须支持 TextPattern 且当前有非空选区，否则返回 None。
/// 三段式各阶段共用此函数。
fn extract_selection(elem: &IUIAutomationElement) -> Option<String> {
    let pattern: IUIAutomationTextPattern = unsafe { elem.GetCurrentPattern(UIA_TextPatternId) }
        .ok()?
        .cast::<IUIAutomationTextPattern>()
        .ok()?;

    let sels = unsafe { pattern.GetSelection() }.ok()?;
    let count = unsafe { sels.Length() }.unwrap_or(0);
    if count == 0 {
        return None;
    }

    // 取该元素首个非空 TextRange
    for j in 0..count {
        let range = match unsafe { sels.GetElement(j) } {
            Ok(r) => r,
            Err(_) => continue,
        };
        // -1 = 无限制；选区文本是用户选中的内容，长度可控。
        if let Ok(text) = unsafe { range.GetText(-1) } {
            let text = text.to_string();
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// 由 HWND 查前台窗口所属进程名（如 "Bitwarden.exe"）。抓不到返回 None。
/// 用于划词感知的隐私门控：`on_selection` 调它决定是否跳过抓取。
///
/// 独立于 `infra::platform::context` 的同名 helper——避免 selection 反向依赖 context 平台层。
/// TODO(0.9 awareness 重构)：把这类 Win32 helper 统一挪进 `infra::platform::awareness::foreground`
/// 供各通道复用。
pub(crate) fn process_name_of_window(hwnd_raw: isize) -> Option<String> {
    if hwnd_raw == 0 {
        return None;
    }
    unsafe {
        let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let Ok(hprocess) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return None;
        };
        let mut path_buf = vec![0u16; MAX_PATH as usize];
        let mut path_len = path_buf.len() as u32;
        if QueryFullProcessImageNameW(
            HANDLE(hprocess.0),
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(path_buf.as_mut_ptr()),
            &mut path_len,
        )
        .is_err()
        {
            return None;
        }
        let path = OsString::from_wide(&path_buf[..path_len as usize])
            .to_string_lossy()
            .into_owned();
        Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }
}
