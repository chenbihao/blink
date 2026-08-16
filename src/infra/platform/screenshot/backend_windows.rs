//! `WindowsScreenshotBackend`：Windows 平台 GDI 截屏实现（0.11.7-f）。
//!
//! 基于 `BitBlt(SRCCOPY)` + `EnumDisplayMonitors`。快（<50ms）、兼容性最好、
//! 不引入 D3D11 依赖；对 hardware-accelerated 窗口（游戏、部分视频播放器）会截
//! 黑屏，属已知限制（见 phases 0.8 §九）。
//!
//! **迁移自 0.8.7 的 `windows.rs`**：把 `capture_virtual_screen()` 从裸函数
//! 改为 trait method；新增 `list_displays()` / `capture_display()`。

use std::cell::RefCell;

use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDCW,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDIBits, GetMonitorInfoW,
    HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITORINFO, MONITORINFOEXW, RGBQUAD, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};
use windows::core::BOOL;

use super::ScreenCaptureMeta;
use super::backend::{DisplayGeometry, ScreenshotBackend};

/// Windows GDI 截屏后端。零状态，`Default::default()` 可用。
#[derive(Debug, Default)]
pub struct WindowsScreenshotBackend;

impl WindowsScreenshotBackend {
    pub fn new() -> Self {
        Self
    }
}

impl ScreenshotBackend for WindowsScreenshotBackend {
    fn list_displays(&self) -> Vec<DisplayGeometry> {
        enumerate_displays()
    }

    fn capture_virtual_screen(&self) -> Result<(Vec<u8>, ScreenCaptureMeta), String> {
        capture_virtual_screen_impl()
    }

    fn capture_display(&self, display_id: u32) -> Result<(Vec<u8>, DisplayGeometry), String> {
        let displays = enumerate_displays();
        let target = displays
            .iter()
            .find(|d| d.id == display_id)
            .ok_or_else(|| format!("display_id={display_id} 不存在"))?
            .clone();
        let pixels = capture_region_bgra(target.x, target.y, target.w, target.h)?;
        Ok((pixels, target))
    }

    fn capture_region(&self, x: i32, y: i32, w: u32, h: u32) -> Result<Vec<u8>, String> {
        capture_region_bgra(x, y, w, h)
    }
}

// ── 显示器枚举 ────────────────────────────────────────────────────────────────

thread_local! {
    /// EnumDisplayMonitors 回调收集器（thread_local 因 Win32 callback 是 unsafe C 接口）。
    static ENUM_BUF: RefCell<Vec<DisplayGeometry>> = const { RefCell::new(Vec::new()) };
}

/// 枚举所有显示器。主屏在第一个。
fn enumerate_displays() -> Vec<DisplayGeometry> {
    ENUM_BUF.with(|buf| buf.borrow_mut().clear());

    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(monitor_enum_proc), LPARAM(0));
    }

    let mut list = ENUM_BUF.with(|buf| buf.borrow_mut().drain(..).collect::<Vec<_>>());
    // 主屏排第一
    list.sort_by(|a, b| b.primary.cmp(&a.primary).then(a.id.cmp(&b.id)));
    list
}

unsafe extern "system" fn monitor_enum_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    _lparam: LPARAM,
) -> BOOL {
    let mut mi = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        szDevice: [0; 32],
    };

    let ok = unsafe { GetMonitorInfoW(hmon, &mut mi.monitorInfo as *mut _) };
    if !ok.as_bool() {
        return BOOL(1); // 继续枚举
    }

    let rc = mi.monitorInfo.rcMonitor;
    let w = (rc.right - rc.left).max(0) as u32;
    let h = (rc.bottom - rc.top).max(0) as u32;

    // 0.11.9：走公共 DPI helper（消除 4 处复制的 GetDpiForMonitor 块）
    let dpi_x = crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon);

    // szDevice 是 UTF-16，转 String
    let name = String::from_utf16_lossy(
        &mi.szDevice[..mi.szDevice.iter().position(|&c| c == 0).unwrap_or(32)],
    );

    let primary = (mi.monitorInfo.dwFlags & 1) != 0; // MONITORINFOF_PRIMARY

    ENUM_BUF.with(|buf| {
        let mut b = buf.borrow_mut();
        let id = b.len() as u32;
        b.push(DisplayGeometry {
            id,
            name: if name.is_empty() {
                format!("Display #{id}")
            } else {
                name
            },
            x: rc.left,
            y: rc.top,
            w,
            h,
            dpi: dpi_x,
            primary,
        });
    });

    BOOL(1) // 继续枚举
}

// ── 截屏实现 ──────────────────────────────────────────────────────────────────

// H4 优化：thread_local GDI 资源缓存——避免每次 capture_region_bgra 全建全毁 DC+bitmap。
// bitmap 在尺寸变化时才重建；DC 全程复用直到 thread 退出（Drop 自动清理）。
struct GdiCache {
    hdc_screen: HDC,
    hdc_mem: HDC,
    hbitmap: HBITMAP,
    bitmap_w: i32,
    bitmap_h: i32,
    old_bmp: HGDIOBJ, // SelectObject 保存的原对象，销毁 bitmap 前需恢复
}

impl Drop for GdiCache {
    fn drop(&mut self) {
        unsafe {
            if !self.hbitmap.is_invalid() {
                let _ = SelectObject(self.hdc_mem, self.old_bmp);
                let _ = DeleteObject(self.hbitmap.into());
            }
            let _ = DeleteDC(self.hdc_mem);
            let _ = DeleteDC(self.hdc_screen);
        }
    }
}

thread_local! {
    static GDI_CACHE: RefCell<Option<GdiCache>> = const { RefCell::new(None) };
}

/// 截取整个虚拟屏幕（等价于旧 `windows::capture_virtual_screen()`）。
fn capture_virtual_screen_impl() -> Result<(Vec<u8>, ScreenCaptureMeta), String> {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);

        if vw <= 0 || vh <= 0 {
            return Err(format!("虚拟屏幕尺寸异常: {vw}x{vh}"));
        }

        let pixels = capture_region_bgra(vx, vy, vw as u32, vh as u32)?;
        Ok((
            pixels,
            ScreenCaptureMeta {
                virtual_x: vx,
                virtual_y: vy,
                width: vw as u32,
                height: vh as u32,
            },
        ))
    }
}

/// 截取虚拟屏幕坐标系下的矩形区域为 BGRA。
///
/// `src_x/src_y` 是虚拟屏幕坐标；`w/h` 是目标像素尺寸。
/// **像素格式**：BGRA、top-down、每行 `w*4` 字节（BitBlt 原生输出，不做 swap）。
///
/// H4 优化：使用 thread_local GDI 缓存——DC 全程复用，bitmap 仅在尺寸变化时重建。
/// 避免每次调用都 CreateDC + CreateCompatibleDC + CreateCompatibleBitmap + Delete* 6 次 syscall。
fn capture_region_bgra(src_x: i32, src_y: i32, w: u32, h: u32) -> Result<Vec<u8>, String> {
    unsafe {
        // BitBlt 前再 DwmFlush 一次——确保 GDI 拿到的屏幕是 DWM 最新合成的一帧
        let _ = DwmFlush();

        GDI_CACHE.with(|cache| {
            let mut gdi_opt = cache.borrow_mut();

            // 初始化 DC（仅首次或 thread 首次调用时）
            if gdi_opt.is_none() {
                let hdc_screen = CreateDCW(windows::core::w!("DISPLAY"), None, None, None);
                if hdc_screen.is_invalid() {
                    return Err("CreateDC(DISPLAY) 失败".into());
                }
                let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
                if hdc_mem.is_invalid() {
                    let _ = DeleteDC(hdc_screen);
                    return Err("CreateCompatibleDC 失败".into());
                }
                *gdi_opt = Some(GdiCache {
                    hdc_screen,
                    hdc_mem,
                    hbitmap: HBITMAP::default(),
                    bitmap_w: 0,
                    bitmap_h: 0,
                    old_bmp: HGDIOBJ::default(),
                });
            }

            let gdi = gdi_opt.as_mut().unwrap();

            // 尺寸变化时重建 bitmap（长截图采集带尺寸通常不变，仅首次重建）
            if gdi.bitmap_w != w as i32 || gdi.bitmap_h != h as i32 {
                // 先恢复旧对象再删除旧 bitmap
                if !gdi.hbitmap.is_invalid() {
                    let _ = SelectObject(gdi.hdc_mem, gdi.old_bmp);
                    let _ = DeleteObject(gdi.hbitmap.into());
                }
                gdi.hbitmap = CreateCompatibleBitmap(gdi.hdc_screen, w as i32, h as i32);
                if gdi.hbitmap.is_invalid() {
                    return Err("CreateCompatibleBitmap 失败".into());
                }
                gdi.old_bmp = SelectObject(gdi.hdc_mem, gdi.hbitmap.into());
                gdi.bitmap_w = w as i32;
                gdi.bitmap_h = h as i32;
            }

            let ok = BitBlt(
                gdi.hdc_mem,
                0,
                0,
                w as i32,
                h as i32,
                Some(gdi.hdc_screen),
                src_x,
                src_y,
                SRCCOPY,
            );
            if ok.is_err() {
                return Err("BitBlt 失败".into());
            }

            let pixel_count = (w * h) as usize;
            let mut pixels: Vec<u8> = vec![0u8; pixel_count * 4];

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32), // 负值 = top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0, // BI_RGB
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [RGBQUAD::default(); 1],
            };

            let lines = GetDIBits(
                gdi.hdc_mem,
                gdi.hbitmap,
                0,
                h,
                Some(pixels.as_mut_ptr() as *mut _),
                &bmi as *const _ as *mut _,
                DIB_RGB_COLORS,
            );

            if lines == 0 {
                return Err("GetDIBits 返回 0 行".into());
            }
            Ok(pixels)
        })
    }
}

// ── 兼容层：保留旧函数签名，内部委托到 backend ────────────────────────────────
//
// 供 `screenshot/mod.rs::begin_session` 短期使用；Step 2 完全走 backend 后可删除。

/// **兼容层**（0.11.7-f 迁移用）：调用默认 backend 的 `capture_virtual_screen`。
pub fn capture_virtual_screen() -> Result<(Vec<u8>, ScreenCaptureMeta), String> {
    capture_virtual_screen_impl()
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 枚举显示器不 panic，至少返回 1 个（测试机总有屏）。
    /// **依赖桌面环境**：`Path::exists` 守卫无意义，只能靠 GetSystemMetrics 判断。
    #[test]
    fn enumerate_displays_returns_at_least_one() {
        // 有桌面时才跑（CI headless 环境跳过）
        let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        if vw <= 0 {
            eprintln!("无桌面环境，跳过测试");
            return;
        }

        let list = enumerate_displays();
        assert!(!list.is_empty(), "至少应返回 1 个显示器");
        assert!(list[0].primary, "第一个应是主屏");
        assert!(list[0].w > 0 && list[0].h > 0, "主屏尺寸应非零");
    }

    /// backend trait 契约：list_displays 与直接调 enumerate_displays 一致。
    #[test]
    fn backend_list_displays_matches_direct_call() {
        let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        if vw <= 0 {
            return;
        }

        let backend = WindowsScreenshotBackend::new();
        let a = backend.list_displays();
        let b = enumerate_displays();
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.id, y.id);
            assert_eq!((x.x, x.y, x.w, x.h), (y.x, y.y, y.w, y.h));
        }
    }
}
