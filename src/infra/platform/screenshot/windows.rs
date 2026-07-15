//! Windows 平台：GDI 截屏实现。
//!
//! 使用 `BitBlt(SRCCOPY)` 截取整个虚拟屏幕（所有显示器合并）。
//! 快（&lt;50ms）、兼容性最好、不引入 D3D11 依赖；对 hardware-accelerated 窗口
//! （游戏、部分视频播放器）会截黑屏，属已知限制（见 phases 0.8 §九 已知复杂点）。

use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDCW,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, RGBQUAD, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::ScreenCaptureMeta;

/// 截取整个虚拟屏幕。返回 `(BGRA 像素, 元数据)`。
///
/// 像素格式：**BGRA**（BitBlt 原生输出，避免全屏 R↔B swap 的耗时——3.7M 像素 x 3.5MB
/// 的 memory shuffle 在低配机上 ~30ms）、top-down、每行 `width * 4` 字节。
/// 消费方（PNG 编码 / 剪贴板 CF_DIB）自行处理字节序。
pub fn capture_virtual_screen() -> Result<(Vec<u8>, ScreenCaptureMeta), String> {
    unsafe {
        // BitBlt 前再 DwmFlush 一次——确保 GDI 拿到的屏幕是 DWM 最新合成的一帧，
        // 而不是"主窗 hide 之前"的旧合成（wait_frame_after_hide 已经等过 IsVisible=false，
        // 但 DWM 合成流水线相对窗口状态是异步的，这里再兜一次最保险）。
        let _ = DwmFlush();

        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);

        if vw <= 0 || vh <= 0 {
            return Err(format!("虚拟屏幕尺寸异常: {vw}x{vh}"));
        }

        let width = vw as u32;
        let height = vh as u32;

        // 创建屏幕 DC 和兼容 DC
        let hdc_screen = CreateDCW(windows::core::w!("DISPLAY"), None, None, None);
        if hdc_screen.is_invalid() {
            return Err("CreateDC(DISPLAY) 失败".into());
        }

        let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
        if hdc_mem.is_invalid() {
            let _ = DeleteDC(hdc_screen);
            return Err("CreateCompatibleDC 失败".into());
        }

        // 创建兼容位图
        let hbitmap = CreateCompatibleBitmap(hdc_screen, vw, vh);
        if hbitmap.is_invalid() {
            let _ = DeleteDC(hdc_mem);
            let _ = DeleteDC(hdc_screen);
            return Err("CreateCompatibleBitmap 失败".into());
        }

        // 选到位图到内存 DC
        let old_bmp = SelectObject(hdc_mem, hbitmap.into());

        // BitBlt 拷贝屏幕像素
        let ok = BitBlt(hdc_mem, 0, 0, vw, vh, Some(hdc_screen), vx, vy, SRCCOPY);
        if !ok.is_ok() {
            let _ = SelectObject(hdc_mem, old_bmp);
            let _ = DeleteObject(hbitmap.into());
            let _ = DeleteDC(hdc_mem);
            let _ = DeleteDC(hdc_screen);
            return Err("BitBlt 失败".into());
        }

        // GetDIBits 读取像素到 buffer
        let pixel_count = (width * height) as usize;
        let mut pixels: Vec<u8> = vec![0u8; pixel_count * 4];

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: vw,
                biHeight: -(vh as i32), // 负值 = top-down（与 RGBA 行序一致）
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
            hdc_mem,
            hbitmap,
            0,
            height,
            Some(pixels.as_mut_ptr() as *mut _),
            &bmi as *const _ as *mut _,
            DIB_RGB_COLORS,
        );
        if lines == 0 {
            let _ = SelectObject(hdc_mem, old_bmp);
            let _ = DeleteObject(hbitmap.into());
            let _ = DeleteDC(hdc_mem);
            let _ = DeleteDC(hdc_screen);
            return Err("GetDIBits 返回 0 行".into());
        }

        // BitBlt 产出 BGRA——直接返回，不做 swap。消费方（PNG 编码 / CF_DIB 剪贴板）
        // 各自按需求转字节序，避免全屏两次 shuffle。

        // 清理 GDI 对象
        let _ = SelectObject(hdc_mem, old_bmp);
        let _ = DeleteObject(hbitmap.into());
        let _ = DeleteDC(hdc_mem);
        let _ = DeleteDC(hdc_screen);

        Ok((
            pixels,
            ScreenCaptureMeta {
                virtual_x: vx,
                virtual_y: vy,
                width,
                height,
            },
        ))
    }
}
