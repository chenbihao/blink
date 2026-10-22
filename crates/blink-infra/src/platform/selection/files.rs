//! Shell COM 文件选中采集（0.27.3，phase §3.2 / §8.8）。
//!
//! 路线（定案）：`IShellWindows` 定位冻结的目标视图 → `QueryActiveShellView` →
//! `GetItemObject(SVGIO_SELECTION)` → `IShellItemArray` → 逐项投影为文件系统对象。
//! 桌面走 `FindWindowSW(SWC_DESKTOP)`；不把找到 HWND 当作完成 COM 视图桥接，
//! 不模拟 Ctrl+C。
//!
//! **时序契约**（调用方 window_orchestrator 负责）：
//! - show 前冻结来源 HWND/PID（AwarenessSnapshot.foreground_app）；
//! - show 后在专用 STA 线程异步采集（本模块自管 `CoInitializeEx`）；
//! - 回写前核对 generation / 来源 PID / 开关 / 敏感门控。
//!
//! **边界**：
//! - 只接受文件系统对象（`SIGDN_FILESYSPATH` 解析失败即虚拟项，跳过不伪造路径）；
//! - 目录参与本地动作（`is_dir=true`），后续版本不做文本附件；
//! - 数量默认上限 [`MAX_FILE_SELECTION_ITEMS`]，超出置 `truncated`；
//! - 采集不读文件内容；日志只记数量/来源 PID，不打印完整路径（§8.8）。
//!
//! **COM 公寓**：Shell 视图接口从 STA 使用最稳（与 UIA 的 MTA 假设不同，
//! 本模块不照搬）；每请求专用线程 + `ComStaGuard`。

use std::time::Instant;

/// 单次采集选中项数量默认上限（§5.3.2；超出置 truncated 并截断）。
pub const MAX_FILE_SELECTION_ITEMS: usize = 100;

/// 单个选中项——仅文件系统对象（虚拟项不进）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSelectionItem {
    /// 文件系统绝对路径（`SIGDN_FILESYSPATH`）。虚拟项解析失败已被跳过，
    /// 此字段保证非空。
    pub path: String,
    /// 是否目录。目录可参与复制路径/定位等本地动作。
    pub is_dir: bool,
    /// 显示名（`SIGDN_NORMALDISPLAY`，失败回退路径末段）。
    pub display_name: String,
}

/// 一次采集的选中状态（§8.8 `FileSelectionState`）。
///
/// 快照仅驻内存（沿用选区缓存纪律：不持久化）；`source_generation` 供回写前
/// 代次核验——采集是异步的，唤起 A→关闭→B 场景旧结果不得覆盖新快照。
#[derive(Debug, Clone)]
pub struct FileSelectionState {
    pub items: Vec<FileSelectionItem>,
    /// 来源窗口进程 PID（explorer.exe——Explorer 窗口/Win11 标签页/桌面同宿主）。
    pub source_pid: u32,
    /// 冻结时的 invoke 代次（SearchService::snapshot_generation）。
    pub source_generation: u64,
    pub collected_at: Instant,
    /// 选中数超上限被截断。
    pub truncated: bool,
}

/// 判定窗口是否属于 explorer.exe（Explorer 窗口与桌面同宿主进程）。
///
/// 冻结与回写两处调用：冻结时决定是否提交采集任务，回写前复核对来源
/// 未漂移。非 Windows 恒 false。
pub fn window_belongs_to_explorer(hwnd_raw: isize) -> bool {
    #[cfg(target_os = "windows")]
    {
        if hwnd_raw == 0 {
            return false;
        }
        crate::platform::context::process_name_of_pid(window_pid(hwnd_raw))
            .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// 由 HWND 查窗口线程进程 PID（0 = 拿不到）。
pub fn window_pid(hwnd_raw: isize) -> u32 {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        if hwnd_raw == 0 {
            return 0;
        }
        let mut pid: u32 = 0;
        unsafe {
            GetWindowThreadProcessId(
                HWND(hwnd_raw as *mut std::ffi::c_void),
                Some(&mut pid),
            );
        }
        pid
    }
    #[cfg(not(target_os = "windows"))]
    {
        0
    }
}

/// 采集冻结窗口的选中项（0.27.3 主入口）。
///
/// `source_hwnd` 是 show 前冻结的前台 Explorer/桌面窗口；`generation` 为冻结时的
/// invoke 代次（原样写进结果供回写核验）。内部自管 STA COM 初始化——每调用
/// 需处于可安全 `CoInitializeEx(APARTMENTTHREADED)` 的线程（专用 worker）。
///
/// 返回 `None`：无匹配视图 / 无选中 / 全部为虚拟项 / COM 失败。绝不 panic。
pub fn collect_selection(
    source_hwnd: isize,
    source_pid: u32,
    generation: u64,
) -> Option<FileSelectionState> {
    let t0 = Instant::now();
    let state = collect_selection_inner(source_hwnd, source_pid, generation);
    tracing::debug!(
        target: "perf",
        hit = state.is_some(),
        count = state.as_ref().map(|s| s.items.len()).unwrap_or(0),
        elapsed_ms = t0.elapsed().as_millis(),
        "[perf] selection::files 采集完成"
    );
    state
}

#[cfg(target_os = "windows")]
fn collect_selection_inner(
    source_hwnd: isize,
    source_pid: u32,
    generation: u64,
) -> Option<FileSelectionState> {
    use windows::core::Interface;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_LOCAL_SERVER};
    use windows::Win32::UI::Shell::{IShellWindows, ShellWindows};

    let _com = ComStaGuard::new();
    let shell_windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) }
            .map_err(|e| tracing::debug!(error = %e, "files: CoCreateInstance(ShellWindows) 失败"))
            .ok()?;

    // 1) Explorer 窗口/Win11 标签页：遍历条目匹配冻结 HWND。
    //    多条目同 HWND（多标签页）时逐个取活动视图，首个有选中即用。
    let count = unsafe { shell_windows.Count() }.unwrap_or(0);
    for index in 0..count {
        let Ok(dispatch) = (unsafe {
            shell_windows.Item(&windows::Win32::System::Variant::VARIANT::from(index))
        }) else {
            continue;
        };
        let Ok(browser) = dispatch.cast::<windows::Win32::UI::Shell::IWebBrowser2>() else {
            continue;
        };
        let Ok(hwnd) = (unsafe { browser.HWND() }) else {
            continue;
        };
        if hwnd.0 != source_hwnd {
            continue;
        }
        if let Some(state) = selection_from_dispatch(&dispatch, source_pid, generation) {
            return Some(state);
        }
    }

    // 2) 桌面：冻结 HWND 无窗口条目匹配（Progman/WorkerW 不在 IShellWindows
    //    普通枚举里），走 FindWindowSW(SWC_DESKTOP) → 活动桌面视图。
    let missing = missing_param_variant();
    let mut desktop_hwnd: i32 = 0;
    let dispatch = unsafe {
        shell_windows.FindWindowSW(
            &missing,
            &missing,
            windows::Win32::UI::Shell::SWC_DESKTOP,
            &mut desktop_hwnd,
            windows::Win32::UI::Shell::SWFO_NEEDDISPATCH,
        )
    }
    .map_err(|e| tracing::trace!(error = %e, "files: FindWindowSW(SWC_DESKTOP) 未命中"))
    .ok()?;
    selection_from_dispatch(&dispatch, source_pid, generation)
}

/// 从浏览器条目（IDispatch）取活动视图选中项。HWND 匹配由调用方完成。
#[cfg(target_os = "windows")]
fn selection_from_dispatch(
    dispatch: &windows::Win32::System::Com::IDispatch,
    source_pid: u32,
    generation: u64,
) -> Option<FileSelectionState> {
    use windows::core::Interface;
    use windows::Win32::UI::Shell::{IShellBrowser, IShellView, SID_STopLevelBrowser};

    let provider: windows::Win32::System::Com::IServiceProvider = dispatch
        .cast()
        .map_err(|e| tracing::trace!(error = %e, "files: 条目不支持 IServiceProvider"))
        .ok()?;
    let browser: IShellBrowser = unsafe { provider.QueryService(&SID_STopLevelBrowser) }
        .map_err(|e| tracing::trace!(error = %e, "files: QueryService(SID_STopLevelBrowser) 失败"))
        .ok()?;
    let view: IShellView = unsafe { browser.QueryActiveShellView() }
        .map_err(|e| tracing::trace!(error = %e, "files: QueryActiveShellView 失败"))
        .ok()?;
    selection_from_view(&view, source_pid, generation)
}

/// 视图 → `SVGIO_SELECTION` → `IShellItemArray` → 文件系统项投影。
#[cfg(target_os = "windows")]
fn selection_from_view(
    view: &windows::Win32::UI::Shell::IShellView,
    source_pid: u32,
    generation: u64,
) -> Option<FileSelectionState> {
    use windows::Win32::UI::Shell::{IShellItemArray, SVGIO_SELECTION};

    let array: IShellItemArray = unsafe { view.GetItemObject(SVGIO_SELECTION) }
        .map_err(|e| tracing::trace!(error = %e, "files: GetItemObject(SVGIO_SELECTION) 失败"))
        .ok()?;
    let total = unsafe { array.GetCount() }.unwrap_or(0);
    if total == 0 {
        return None;
    }
    let take = total.min(MAX_FILE_SELECTION_ITEMS as u32);
    let mut items = Vec::with_capacity(take as usize);
    for index in 0..take {
        let Ok(item) = (unsafe { array.GetItemAt(index) }) else {
            continue;
        };
        if let Some(projected) = project_item(&item) {
            items.push(projected);
        }
    }
    if items.is_empty() {
        // 选中项全是虚拟对象（此电脑/回收站等）——明确无文件选中
        return None;
    }
    Some(FileSelectionState {
        items,
        source_pid,
        source_generation: generation,
        collected_at: Instant::now(),
        truncated: total > take,
    })
}

/// 单项投影：只接受文件系统对象。路径解析失败（虚拟项）返回 None。
#[cfg(target_os = "windows")]
fn project_item(item: &windows::Win32::UI::Shell::IShellItem) -> Option<FileSelectionItem> {
    use windows::Win32::System::SystemServices::SFGAO_FOLDER;
    use windows::Win32::UI::Shell::{SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY};

    let path_pw = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok()?;
    let path = take_pwstr(path_pw);
    if path.is_empty() {
        return None;
    }
    let display_name = match unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) } {
        Ok(pw) => take_pwstr(pw),
        Err(_) => String::new(),
    };
    let display_name = if display_name.is_empty() {
        std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone())
    } else {
        display_name
    };
    let attrs = unsafe { item.GetAttributes(SFGAO_FOLDER) }.unwrap_or_default();
    Some(FileSelectionItem {
        path,
        is_dir: attrs.contains(SFGAO_FOLDER),
        display_name,
    })
}

/// 取走 PWSTR 内容并释放 CoTaskMem 分配（GetDisplayName 契约）。
#[cfg(target_os = "windows")]
fn take_pwstr(pw: windows::core::PWSTR) -> String {
    let text = unsafe { pw.to_string() }.unwrap_or_default();
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(pw.as_ptr().cast())) };
    text
}

/// FindWindowSW 的 missing 参数（VT_ERROR + DISP_E_PARAMNOTFOUND）。
///
/// 构造形态对齐 windows crate extensions 的 `variant_from_value!` 宏展开
/// （VARIANT Drop 走 VariantClear，VT_ERROR 无内嵌资源，安全）。
#[cfg(target_os = "windows")]
fn missing_param_variant() -> windows::Win32::System::Variant::VARIANT {
    use windows::Win32::Foundation::DISP_E_PARAMNOTFOUND;
    use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_ERROR};
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_ERROR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    scode: DISP_E_PARAMNOTFOUND.0 as i32,
                },
            }),
        },
    }
}

/// STA COM 初始化 guard——与 UIA 的 MTA `ComGuard` 分离（Shell 视图接口从 STA 使用）。
#[cfg(target_os = "windows")]
struct ComStaGuard {
    should_uninit: bool,
}

#[cfg(target_os = "windows")]
impl ComStaGuard {
    fn new() -> Self {
        let hr = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
        };
        if hr.is_ok() {
            ComStaGuard {
                should_uninit: true,
            }
        } else {
            // S_FALSE（已初始化）等其他情形：不配对 CoUninitialize
            tracing::debug!(hr = hr.0, "files: CoInit STA 非成功码，跳过 uninit 配对");
            ComStaGuard {
                should_uninit: false,
            }
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for ComStaGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn collect_selection_inner(
    _source_hwnd: isize,
    _source_pid: u32,
    _generation: u64,
) -> Option<FileSelectionState> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 选中项结构投影纯逻辑（构造断言；COM 路径属 GUI/Shell 集成层，手动验证）。
    #[test]
    fn file_selection_item_shape() {
        let item = FileSelectionItem {
            path: "C:\\tmp\\报告.md".into(),
            is_dir: false,
            display_name: "报告.md".into(),
        };
        assert_eq!(item.path, "C:\\tmp\\报告.md");
        assert!(!item.is_dir);
    }

    /// 非法 HWND 恒判非 Explorer（0 值守卫）。
    #[test]
    fn zero_hwnd_is_not_explorer() {
        assert!(!window_belongs_to_explorer(0));
        assert_eq!(window_pid(0), 0);
    }

    /// 上限常量受控（§5.3.2 默认 100，调整需过 phase 文档）。
    #[test]
    fn selection_cap_is_100() {
        assert_eq!(MAX_FILE_SELECTION_ITEMS, 100);
    }
}
