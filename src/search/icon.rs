//! 图标提取：从 .lnk / .exe 提取应用图标，编码为 PNG 字节。
//!
//! 用 `IShellItemImageFactory::GetImage` 取图标（原生支持高 DPI、lnk 目标失效兜底），
//! 再经 GDI `GetDIBits` 拿 32 位 BGRA 像素，转 RGBA 后用 `png` crate 编码成标准 PNG。
//!
//! 由自定义协议（Windows 下 `http://blink-icon.localhost/<path>`）按需懒加载调用
//! （见 main.rs），不进搜索扫描热路径。

use std::collections::HashMap;
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
};

/// 默认提取尺寸（物理像素）。32 足够列表项显示，高 DPI 下 GetImage 会按需给更大位图。
const ICON_SIZE: i32 = 32;

/// 图标缓存：lnk_path -> PNG 字节。
/// 值为 `None` 表示「提取过但无图标/失败」，避免对失效项反复重试。
static ICON_CACHE: Mutex<Option<HashMap<String, Option<Vec<u8>>>>> = Mutex::new(None);

/// 获取图标 PNG 字节（带缓存）。供协议 handler 调用，可能阻塞（Shell 调用），应在 blocking 线程跑。
pub fn get_icon_png(path: &str) -> Option<Vec<u8>> {
    // 查缓存
    {
        if let Ok(cache) = ICON_CACHE.lock() {
            if let Some(map) = cache.as_ref() {
                if let Some(cached) = map.get(path) {
                    return cached.clone();
                }
            }
        }
    }

    let result = extract_icon_png(path, ICON_SIZE);

    if let Ok(mut cache) = ICON_CACHE.lock() {
        let map = cache.get_or_insert_with(HashMap::new);
        map.insert(path.to_string(), result.clone());
    }

    result
}

/// COM 初始化 RAII guard：仅在本次确实初始化成功时负责 `CoUninitialize`。
struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    fn init() -> Self {
        // 在已是其他模型的线程上会返回 RPC_E_CHANGED_MODE，此时不应 uninit（不是我们初始化的）。
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        ComGuard {
            should_uninit: hr.is_ok(),
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

/// 实际提取：path -> PNG 字节。失败返回 None。
fn extract_icon_png(path: &str, size: i32) -> Option<Vec<u8>> {
    let _com = ComGuard::init();

    unsafe {
        // SHCreateItemFromParsingName 是 Shell 名称解析 API，要求规范 Windows 路径（全反斜杠）。
        // 扫描得到的路径可能混用 '/'（开始菜单子目录字面量用了正斜杠），需归一化，否则报 0x80070057。
        let normalized = path.replace('/', "\\");
        let wide: Vec<u16> = normalized.encode_utf16().chain(std::iter::once(0)).collect();

        // 直接请求 IShellItemImageFactory，省去额外 cast。
        let factory: IShellItemImageFactory =
            match SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) {
                Ok(f) => f,
                Err(e) => {
                    tracing::debug!(%path, error = %e, "SHCreateItemFromParsingName 失败");
                    return None;
                }
            };

        let hbitmap = match factory.GetImage(
            SIZE { cx: size, cy: size },
            SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(%path, error = %e, "IShellItemImageFactory::GetImage 失败");
                return None;
            }
        };

        // 读位图实际尺寸（GetImage 可能返回比请求更大的位图）。
        let mut bmp = BITMAP::default();
        let got = GetObjectW(
            hbitmap.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut BITMAP as *mut _),
        );
        if got == 0 {
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(hbitmap.into());
            tracing::debug!(%path, "GetObjectW 失败");
            return None;
        }

        let width = bmp.bmWidth;
        let height = bmp.bmHeight;
        if width <= 0 || height <= 0 {
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(hbitmap.into());
            tracing::debug!(%path, width, height, "位图尺寸非法");
            return None;
        }

        // 用 GetDIBits 取 top-down 32 位 BGRA 像素。
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // 负 = top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };

        let pixel_count = (width * height) as usize;
        let mut buf = vec![0u8; pixel_count * 4];

        let hdc = GetDC(None);
        let scanlines = GetDIBits(
            hdc,
            hbitmap,
            0,
            height as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(hbitmap.into());

        if scanlines != height as i32 {
            // 0 = 失败；不足 height = 只取到部分扫描线，剩余是零初始化像素，
            // 编码出来是半截/损坏图标，按失败降级（缓存 None 避免反复重试）。
            tracing::debug!(%path, scanlines, height, "GetDIBits 未取到完整位图");
            return None;
        }

        // BGRA -> RGBA
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }

        match encode_rgba_to_png(&buf, width as u32, height as u32) {
            Some(png) => {
                // tracing::debug!(%path, width, height, bytes = png.len(), "图标提取成功");
                Some(png)
            }
            None => {
                tracing::debug!(%path, width, height, "PNG 编码失败");
                None
            }
        }
    }
}

/// 把 RGBA 像素编码为 PNG 字节。纯函数，便于单测。
fn encode_rgba_to_png(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    if rgba.len() != (width as usize) * (height as usize) * 4 {
        return None;
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

/// 清空图标缓存。
#[allow(dead_code)]
pub fn clear_cache() {
    if let Ok(mut cache) = ICON_CACHE.lock() {
        if let Some(map) = cache.as_mut() {
            map.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_MAGIC: [u8; 4] = [0x89, 0x50, 0x4E, 0x47];

    #[test]
    fn encode_rgba_roundtrips() {
        // 2x2 全不透明像素：红、绿、蓝、白
        let rgba: Vec<u8> = vec![
            255, 0, 0, 255, // R
            0, 255, 0, 255, // G
            0, 0, 255, 255, // B
            255, 255, 255, 255, // W
        ];
        let png = encode_rgba_to_png(&rgba, 2, 2).expect("应能编码");
        assert_eq!(&png[..4], &PNG_MAGIC, "应以 PNG 魔数开头");

        // 用 decoder 读回，验证尺寸与通道
        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = decoder.read_info().expect("应能解析头");
        let info = reader.info();
        assert_eq!(info.width, 2);
        assert_eq!(info.height, 2);
        assert_eq!(info.color_type, png::ColorType::Rgba);

        let mut out = vec![0u8; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut out).expect("应能解码像素");
        assert_eq!(&out[..frame.buffer_size()], &rgba[..]);
    }

    #[test]
    fn encode_rejects_mismatched_len() {
        // 长度与 w*h*4 不符应返回 None
        assert!(encode_rgba_to_png(&[0u8; 7], 2, 2).is_none());
    }

    #[test]
    fn extract_from_system_exe_is_valid_png() {
        // 用稳定存在的系统 exe；不存在则跳过（不依赖 CI 桌面环境）
        let path = "C:\\Windows\\explorer.exe";
        if !std::path::Path::new(path).exists() {
            eprintln!("跳过：{} 不存在", path);
            return;
        }
        match extract_icon_png(path, 32) {
            Some(png) => {
                assert_eq!(&png[..4], &PNG_MAGIC, "提取结果必须是合法 PNG（魔数）");
                // 能被 decoder 读回头部
                let decoder = png::Decoder::new(std::io::Cursor::new(&png));
                assert!(decoder.read_info().is_ok(), "PNG 应可解析");
            }
            None => eprintln!("explorer.exe 未提取到图标（环境相关），跳过断言"),
        }
    }

    #[test]
    fn extract_from_nonexistent_returns_none() {
        assert!(extract_icon_png("C:\\definitely\\nope\\nonexistent.exe", 32).is_none());
    }
}
