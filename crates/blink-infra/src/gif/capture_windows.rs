//! Windows GDI 帧源（0.28.7）：连续 BitBlt + 光标叠加。
//!
//! **采集**：复用 `platform::screenshot::capture_region_continuous`
//! （thread_local GDI 缓存 + 不带 CAPTUREBLT 的连续读取）——采集线程是
//! 专用 OS 线程，thread_local 资源自然绑定该线程。
//!
//! **光标**（BitBlt 不含光标，标准做法手动合成）：
//! `GetCursorInfo` → `GetIconInfo`（句柄缓存渲染结果，系统光标句柄进程内
//! 稳定）→ 按热点位置叠加。DIB 一律以负 biHeight 读成 top-down 行序；
//! mask 双倍高 = AND 层（前半）+ XOR 层（后半）。彩色光标用 32bpp color
//! 位图 alpha over（无 alpha 的老式光标按 AND mask 不透明填充）；单色光标
//! 按 AND/XOR 两层合成（白/黑/反转）。
//!
//! 混合 DPI/负坐标：区域与光标 `ptScreenPos` 同为虚拟屏幕物理像素坐标，
//! 帧内位置 = 光标位置 − 热点 − 区域原点，无需 DPI 换算。

use std::collections::HashMap;

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DeleteDC, DeleteObject, DIB_RGB_COLORS,
    GetDC, GetDIBits, ReleaseDC, SelectObject, HGDIOBJ, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, HCURSOR, ICONINFO,
};

use super::capture::FrameSource;

/// 渲染好的光标（BGRA 小位图 + 热点）。alpha=1 为「反转像素」哨兵。
struct RenderedCursor {
    bgra: Vec<u8>,
    w: u32,
    h: u32,
    hot_x: i32,
    hot_y: i32,
}

/// 共享采集区域位置（录制中拖动选区热更新；尺寸固定只动 x/y）。
/// bin 侧 `ActiveRecording` 与 `GdiFrameSource` 持同一 Arc：移动命令改这里，
/// 采集线程每帧读取。
pub type SharedRegionPos = std::sync::Arc<std::sync::Mutex<(i32, i32)>>;

/// GDI + 光标帧源。
pub struct GdiFrameSource {
    /// 共享区域位置（None = 固定区域，兼容测试/基准）。
    pos: Option<SharedRegionPos>,
    fixed_x: i32,
    fixed_y: i32,
    w: u32,
    h: u32,
    show_cursor: bool,
    /// HCURSOR → 渲染缓存（自定义光标句柄变化自动 miss 重渲染）。
    cursor_cache: HashMap<isize, Option<RenderedCursor>>,
}

impl GdiFrameSource {
    pub fn new(x: i32, y: i32, w: u32, h: u32, show_cursor: bool) -> Self {
        Self::with_shared_pos(x, y, w, h, show_cursor, None)
    }

    /// 热更新位置版本：`pos` 与移动命令共享（初始值 x/y 在此写入）。
    pub fn with_shared_pos(
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        show_cursor: bool,
        pos: Option<SharedRegionPos>,
    ) -> Self {
        if let Some(p) = &pos {
            if let Ok(mut g) = p.lock() {
                *g = (x, y);
            }
        }
        Self {
            pos,
            fixed_x: x,
            fixed_y: y,
            w,
            h,
            show_cursor,
            cursor_cache: HashMap::new(),
        }
    }

    fn region_origin(&self) -> (i32, i32) {
        self.pos
            .as_ref()
            .and_then(|p| p.lock().ok().map(|g| *g))
            .unwrap_or((0, 0))
    }
}

impl FrameSource for GdiFrameSource {
    fn capture(&mut self) -> Result<Vec<u8>, String> {
        // 有共享位置时每帧读取（拖动中热更新）；否则用构造时的固定值。
        let (x, y) = match &self.pos {
            Some(p) => p.lock().map(|g| *g).unwrap_or((0, 0)),
            None => (self.fixed_x, self.fixed_y),
        };
        let mut frame =
            crate::platform::screenshot::capture_region_continuous(x, y, self.w, self.h)?;
        if self.show_cursor && blend_current_cursor(&mut frame, self.w, self.h, x, y, &mut self.cursor_cache) {
            // 已原地叠加
        }
        Ok(frame)
    }

    fn frame_bytes(&self) -> usize {
        (self.w as usize) * (self.h as usize) * 4
    }
}

/// 取当前光标并叠加进帧；光标隐藏/病态时不动帧。返回是否叠加。
fn blend_current_cursor(
    frame: &mut [u8],
    frame_w: u32,
    frame_h: u32,
    region_x: i32,
    region_y: i32,
    cache: &mut HashMap<isize, Option<RenderedCursor>>,
) -> bool {
    let Some((handle, pos)) = peek_cursor() else {
        return false;
    };
    if !cache.contains_key(&handle) {
        // 句柄缓存上限：系统光标集有限，但频繁换自定义光标的应用可能让
        // 句柄无界增长——超限整表重建（渲染开销毫秒级）。
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(handle, render_cursor(handle));
    }
    let Some(c) = cache.get(&handle).unwrap_or(&None) else {
        return false;
    };
    let origin_x = pos.x - c.hot_x - region_x;
    let origin_y = pos.y - c.hot_y - region_y;
    blend(&mut frame[..], frame_w, frame_h, c, origin_x, origin_y);
    true
}

/// `GetCursorInfo`：显示中返回 (HCURSOR, 虚拟屏幕坐标)。
fn peek_cursor() -> Option<(isize, POINT)> {
    let mut ci = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: ci 已按 cbSize 初始化，指针来自栈对象
    if !unsafe { GetCursorInfo(&mut ci) }.is_ok() {
        return None;
    }
    if (ci.flags.0 & CURSOR_SHOWING.0) == 0 {
        return None; // 光标隐藏（全屏视频/触摸等）
    }
    Some((ci.hCursor.0 as isize, ci.ptScreenPos))
}

/// `GetIconInfo` + `GetDIBits` 渲染光标为 BGRA。失败/病态返回 None。
fn render_cursor(handle: isize) -> Option<RenderedCursor> {
    let hcursor = HCURSOR(handle as *mut _);
    let mut info = ICONINFO::default();
    // SAFETY: handle 来自 GetCursorInfo；info 已初始化
    if !unsafe { GetIconInfo(hcursor.into(), &mut info) }.is_ok() {
        return None;
    }
    // GetIconInfo 返回的位图归调用者，所有路径结束前删除。
    let out = compose_icon(&info);
    // SAFETY: DeleteObject 释放 GetIconInfo 分配的位图
    unsafe {
        let _ = DeleteObject(info.hbmMask.into());
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor.into());
        }
    }
    out
}

struct RawBitmap {
    w: i32,
    h: i32,
    /// top-down 行序；32bpp BGRA。
    data: Vec<u8>,
}

/// GetDIBits 以 top-down（负 biHeight）读取整个位图。
fn read_bitmap_topdown(hbitmap: HBITMAP) -> Option<RawBitmap> {
    // SAFETY: 本函数内 DC 生命周期成对（GetDC/ReleaseDC、Create/Select/Delete）
    unsafe {
        let hdc_screen = GetDC(None);
        if hdc_screen.is_invalid() {
            return None;
        }
        let result = (|| {
            let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
            if hdc_mem.is_invalid() {
                return None;
            }
            let out = (|| {
                let old = SelectObject(hdc_mem, HGDIOBJ(hbitmap.0));
                let mut bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                // 第一遍：hbmpSection=NULL 只查尺寸
                if GetDIBits(hdc_mem, hbitmap, 0, 0, None, &mut bmi, DIB_RGB_COLORS) == 0 {
                    return None;
                }
                let w = bmi.bmiHeader.biWidth;
                let h = bmi.bmiHeader.biHeight;
                if w <= 0 || h <= 0 || w > 512 || h > 1024 {
                    return None; // 病态尺寸（含 mask 双倍高上限）
                }
                let total = (w as usize) * (h as usize) * 4;
                let mut data = vec![0u8; total];
                // 第二遍：负 biHeight = top-down
                bmi.bmiHeader.biHeight = -h;
                bmi.bmiHeader.biSizeImage = total as u32;
                if GetDIBits(
                    hdc_mem,
                    hbitmap,
                    0,
                    h as u32,
                    Some(data.as_mut_ptr() as *mut _),
                    &mut bmi,
                    DIB_RGB_COLORS,
                ) == 0
                {
                    return None;
                }
                SelectObject(hdc_mem, old);
                Some(RawBitmap { w, h, data })
            })();
            let _ = DeleteDC(hdc_mem);
            out
        })();
        ReleaseDC(None, hdc_screen);
        result
    }
}

fn compose_icon(info: &ICONINFO) -> Option<RenderedCursor> {
    let mask = read_bitmap_topdown(info.hbmMask)?;
    let color = if !info.hbmColor.is_invalid() {
        Some(read_bitmap_topdown(info.hbmColor)?)
    } else {
        None
    };
    // 尺寸：彩色光标取 color；单色 mask 双倍高取一半。
    let (w, h) = match &color {
        Some(c) => (c.w as u32, c.h as u32),
        None => ((mask.w as u32), (mask.h / 2) as u32),
    };
    if w == 0 || h == 0 {
        return None;
    }
    let mask_row_bytes = ((w as usize) + 31) / 32 * 4;
    let and_bit = |x: usize, y: usize| -> bool {
        // AND 层在前（top-down），XOR 层在其后
        let layer_y = y.min(h as usize - 1);
        let row = layer_y;
        mask.data
            .get(row * mask_row_bytes + x / 8)
            .map(|b| (b >> (7 - (x % 8))) & 1 == 1)
            .unwrap_or(true)
    };
    let xor_bit = |x: usize, y: usize| -> bool {
        let row = y.min(h as usize - 1) + h as usize; // XOR 层 = 后半
        mask.data
            .get(row * mask_row_bytes + x / 8)
            .map(|b| (b >> (7 - (x % 8))) & 1 == 1)
            .unwrap_or(false)
    };

    let mut bgra = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let di = (y * w as usize + x) * 4;
            match &color {
                Some(c) => {
                    let ci = (y * c.w as usize + x) * 4;
                    let a = c.data[ci + 3];
                    if a > 0 {
                        bgra[di] = c.data[ci];
                        bgra[di + 1] = c.data[ci + 1];
                        bgra[di + 2] = c.data[ci + 2];
                        bgra[di + 3] = a;
                    } else if !and_bit(x, y) {
                        // 无 alpha 的老式彩色光标：AND 不透明处填 color
                        bgra[di] = c.data[ci];
                        bgra[di + 1] = c.data[ci + 1];
                        bgra[di + 2] = c.data[ci + 2];
                        bgra[di + 3] = 255;
                    }
                }
                None => {
                    // 单色两层合成：AND=0,XOR=1 → 白；AND=0,XOR=0 → 黑；
                    // AND=1,XOR=0 → 反转（哨兵 alpha=1）；AND=1,XOR=1 → 透明。
                    let a = and_bit(x, y);
                    let x1 = xor_bit(x, y);
                    if !a && x1 {
                        bgra[di] = 255;
                        bgra[di + 1] = 255;
                        bgra[di + 2] = 255;
                        bgra[di + 3] = 255;
                    } else if !a && !x1 {
                        bgra[di] = 0;
                        bgra[di + 1] = 0;
                        bgra[di + 2] = 0;
                        bgra[di + 3] = 255;
                    } else if a && !x1 {
                        bgra[di + 3] = 1; // 反转哨兵
                    }
                }
            }
        }
    }
    Some(RenderedCursor {
        bgra,
        w,
        h,
        hot_x: info.xHotspot as i32,
        hot_y: info.yHotspot as i32,
    })
}

/// 光标混合：source-over；alpha=1 哨兵 → 反转 dst。越界自动裁剪。
fn blend(frame: &mut [u8], frame_w: u32, frame_h: u32, c: &RenderedCursor, origin_x: i32, origin_y: i32) {
    for cy in 0..c.h as i32 {
        for cx in 0..c.w as i32 {
            let fx = origin_x + cx;
            let fy = origin_y + cy;
            if fx < 0 || fy < 0 || fx >= frame_w as i32 || fy >= frame_h as i32 {
                continue;
            }
            let si = ((cy * c.w as i32 + cx) * 4) as usize;
            let a = c.bgra[si + 3];
            if a == 0 {
                continue;
            }
            let di = ((fy * frame_w as i32 + fx) * 4) as usize;
            if a == 1 {
                // 反转像素（I-beam 等单色光标在深浅底上保持可见）
                frame[di] = 255 - frame[di];
                frame[di + 1] = 255 - frame[di + 1];
                frame[di + 2] = 255 - frame[di + 2];
                continue;
            }
            // source-over（帧 alpha 恒 255）
            let inv = 255 - a;
            frame[di] = ((c.bgra[si] as u16 * a as u16 + frame[di] as u16 * inv as u16) / 255) as u8;
            frame[di + 1] =
                ((c.bgra[si + 1] as u16 * a as u16 + frame[di + 1] as u16 * inv as u16) / 255) as u8;
            frame[di + 2] =
                ((c.bgra[si + 2] as u16 * a as u16 + frame[di + 2] as u16 * inv as u16) / 255) as u8;
        }
    }
}

// ── 纯逻辑测试（DIB/mask 组合逻辑平台无关部分） ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_overwrites_opaque_and_inverts_sentinel() {
        // 4x4 帧：底色灰
        let mut frame = vec![100u8; 4 * 4 * 4];
        for px in frame.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let c = RenderedCursor {
            // 2x1 光标：左像素不透明白（BGRA 白 = 255,255,255），右像素反转
            bgra: vec![255, 255, 255, 255, 0, 0, 0, 1],
            w: 2,
            h: 1,
            hot_x: 0,
            hot_y: 0,
        };
        blend(&mut frame, 4, 4, &c, 1, 1);
        let white = &frame[((1 * 4 + 1) * 4)..][..4];
        assert_eq!((&white[0], &white[1], &white[2], &white[3]), (&255, &255, &255, &255));
        let inv = &frame[((1 * 4 + 2) * 4)..][..4];
        assert_eq!(
            (inv[0], inv[1], inv[2]),
            (155, 155, 155),
            "反转像素 = 255-100"
        );
    }

    #[test]
    fn blend_clips_out_of_bounds_cursor() {
        let mut frame = vec![7u8; 4 * 4 * 4];
        let c = RenderedCursor {
            bgra: [[9u8, 9, 9, 255]].repeat(16).concat(),
            w: 4,
            h: 4,
            hot_x: 0,
            hot_y: 0,
        };
        // 光标整体在帧外
        blend(&mut frame, 4, 4, &c, 10, 10);
        assert!(frame.iter().all(|b| *b == 7), "越界光标不写入");
        // 半出界：只写帧内部分
        blend(&mut frame, 4, 4, &c, 2, 2);
        assert_eq!(frame[(2 * 4 + 2) * 4], 9);
        assert_eq!(frame[0], 7, "(0,0) 未被写");
    }
}
