//! 显式粘贴的目标窗口校验。UIA 控件类型不能代表应用是否支持 Ctrl+V。

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowThreadProcessId, IsWindow,
};

/// 固定窗口身份，防止焦点恢复失败时把粘贴键送入另一个窗口。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasteTarget {
    hwnd: isize,
    pid: u32,
    tid: u32,
}

impl PasteTarget {
    /// 在异步复制/隐藏之前快照目标；不接受 Blink 自己的顶层窗口。
    pub fn capture(hwnd: isize) -> Option<Self> {
        Self::identity(hwnd).filter(|target| target.pid != std::process::id())
    }

    pub fn hwnd(self) -> isize {
        self.hwnd
    }

    /// 只恢复原窗口的前台状态，让应用保留自己的光标和选区。
    pub fn restore(self) -> bool {
        if Self::identity(self.hwnd) != Some(self) {
            return false;
        }
        super::restore_foreground_g2(self.hwnd);
        self.is_foreground()
    }

    /// 发键前再次核对 HWND、进程和线程。不同窗口即使同进程也不能放行。
    pub fn is_foreground(self) -> bool {
        let foreground = unsafe { GetForegroundWindow() };
        Self::identity(foreground.0 as isize) == Some(self)
    }

    fn identity(hwnd: isize) -> Option<Self> {
        let hwnd = HWND(hwnd as *mut _);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return None;
            }
            // WebView2 的前台 HWND 可能属于 renderer 子进程，按顶层窗口归属校验。
            let root = GetAncestor(hwnd, GA_ROOT);
            if root.is_invalid() {
                return None;
            }
            let mut pid = 0;
            let tid = GetWindowThreadProcessId(root, Some(&mut pid));
            (pid != 0 && tid != 0).then_some(Self {
                hwnd: root.0 as isize,
                pid,
                tid,
            })
        }
    }
}
