//! 合成帧生成：确定性伪随机文档 + 视口固定层 + 采集噪声。
//!
//! 输出 RGBA8，与 stitch.js 的 ImageData 布局一致。
//! 所有随机都走显式种子的 xorshift，保证场景可复现。

pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn px(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let i = (y * self.w + x) * 4;
        (self.data[i], self.data[i + 1], self.data[i + 2])
    }
}

pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

#[derive(Clone)]
pub enum DocStyle {
    /// 常规文章：文字行 + 标题 + 图片块，纹理充足
    Article,
    /// 低细节：大片空白 + 稀疏浅色细线（SAD 难区）
    LowDetail,
    /// 周期完全相同的列表（period 像素），按构造存在多义
    RepeatedList(usize),
}

#[derive(Clone, Copy, Default)]
pub struct OverlaySpec {
    pub sticky_header: bool,
    pub bottom_bar: bool,
    pub float_button: bool,
    pub cursor_blink: bool,
}

fn fill_rgb(doc: &mut [u8], w: usize, x0: usize, y0: usize, x1: usize, y1: usize, g: u8) {
    let x1 = x1.min(w);
    let y1 = y1.min(doc.len() / (w * 3));
    let x0 = x0.min(w);
    let y0 = y0.min(y1);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = (y * w + x) * 3;
            doc[i] = g;
            doc[i + 1] = g;
            doc[i + 2] = g;
        }
    }
}

pub fn render_doc(style: &DocStyle, w: usize, h: usize, seed: u64) -> Vec<u8> {
    let mut doc = vec![250u8; w * h * 3];
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    match style {
        DocStyle::Article => {
            // 词块式文本行：一行由多个 30-120px 宽的“词”组成，接近真实 UI
            // 文字密度。密度不足会让空白区任意对齐都拿到噪声级 SAD。
            let mut y = 60usize;
            while y + 60 < h {
                if rng.below(16) == 0 {
                    // 标题块
                    let x1 = (w - 60).min(180 + rng.below(400) as usize);
                    fill_rgb(&mut doc, w, 60, y, x1, y + 9, 30);
                    y += 36;
                }
                let lines = 4 + rng.below(10);
                for _ in 0..lines {
                    let mut x = 60 + rng.below(40) as usize;
                    let end = (w - 60).min(x + 400 + rng.below(640) as usize);
                    let g = 45 + rng.below(50) as u8;
                    while x + 30 < end {
                        let word_w = 30 + rng.below(90) as usize;
                        fill_rgb(&mut doc, w, x, y, (x + word_w).min(end), y + 5, g);
                        x += word_w + 8 + rng.below(7) as usize;
                    }
                    y += 16;
                }
                if rng.below(6) == 0 {
                    // 4x4 块状噪声“图片”
                    let iw = 240 + rng.below(480) as usize;
                    let ih = 100 + rng.below(120) as usize;
                    let room = (w - 160).saturating_sub(iw);
                    let ix = 80 + rng.below(room as u64) as usize;
                    if y + ih < h {
                        for cy in (0..ih - 3).step_by(4) {
                            for cx in (0..iw - 3).step_by(4) {
                                let g = 90 + rng.below(120) as u8;
                                fill_rgb(&mut doc, w, ix + cx, y + cy, ix + cx + 4, y + cy + 4, g);
                            }
                        }
                        fill_rgb(&mut doc, w, ix, y, ix + iw, y + 1, 60);
                        fill_rgb(&mut doc, w, ix, y + ih - 1, ix + iw, y + ih, 60);
                        y += ih + 12;
                    }
                }
                y += 30;
            }
        }
        DocStyle::LowDetail => {
            for v in doc.iter_mut() {
                *v = 252;
            }
            let mut y = 80usize;
            while y + 60 < h {
                if rng.below(4) == 0 {
                    let x1 = 60 + 140 + rng.below(200) as usize;
                    fill_rgb(&mut doc, w, 60, y, x1, y + 2, 170);
                    y += 30;
                }
                let lines = 2 + rng.below(4);
                for _ in 0..lines {
                    if rng.below(4) == 0 {
                        let x1 = 60 + 180 + rng.below(420) as usize;
                        fill_rgb(&mut doc, w, 60, y, x1, y + 2, 205);
                    }
                    y += 26;
                }
                y += 120 + rng.below(160) as usize;
            }
        }
        DocStyle::RepeatedList(period) => {
            let p = *period;
            let mut row = 0usize;
            while (row + 1) * p <= h {
                let base = row * p;
                let bg = if row % 2 == 0 { 248 } else { 253 };
                fill_rgb(&mut doc, w, 0, base, w, base + p, bg);
                let text_y = base + p / 3;
                fill_rgb(&mut doc, w, 80, text_y, w - 80, text_y + 5, 55);
                fill_rgb(&mut doc, w, 80, text_y + 14, 80 + 320, text_y + 18, 150);
                fill_rgb(&mut doc, w, 40, base + p - 1, w - 40, base + p, 225);
                row += 1;
            }
        }
    }
    doc
}

pub fn crop(doc: &[u8], w: usize, top: usize, vh: usize) -> Frame {
    let mut data = vec![255u8; w * vh * 4];
    for y in 0..vh {
        let src = (top + y) * w * 3;
        let dst = y * w * 4;
        for x in 0..w {
            data[dst + x * 4] = doc[src + x * 3];
            data[dst + x * 4 + 1] = doc[src + x * 3 + 1];
            data[dst + x * 4 + 2] = doc[src + x * 3 + 2];
        }
    }
    Frame { w, h: vh, data }
}

fn frame_rect(f: &mut Frame, x0: usize, y0: usize, x1: usize, y1: usize, g: u8) {
    let x1 = x1.min(f.w);
    let y1 = y1.min(f.h);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = (y * f.w + x) * 4;
            f.data[i] = g;
            f.data[i + 1] = g;
            f.data[i + 2] = g;
        }
    }
}

fn frame_set(f: &mut Frame, x: i32, y: i32, g: u8) {
    if x < 0 || y < 0 || x as usize >= f.w || y as usize >= f.h {
        return;
    }
    let i = (y as usize * f.w + x as usize) * 4;
    f.data[i] = g;
    f.data[i + 1] = g;
    f.data[i + 2] = g;
}

/// 视口固定层：随视口而非文档滚动，逐帧绘制在相同屏幕位置。
/// cursor_blink 用“实心/空心”交替模拟光标闪烁这类帧间动态。
pub fn apply_overlays(f: &mut Frame, o: &OverlaySpec, frame_index: usize) {
    let (w, h) = (f.w, f.h);
    if o.sticky_header {
        frame_rect(f, 0, 0, w, 64, 28);
        for ty in (12..52).step_by(10) {
            let x1 = ((w * 7 / 10).max(60)).min(w - 8);
            frame_rect(f, 16, ty, x1, ty + 3, 215);
        }
        frame_rect(f, 0, 62, w, 64, 84);
    }
    if o.bottom_bar {
        frame_rect(f, 0, h - 56, w, h, 24);
        frame_rect(f, 40, h - 42, 160, h - 14, 66);
        frame_rect(f, 200, h - 42, 320, h - 14, 66);
        frame_rect(f, 360, h - 42, 480, h - 14, 66);
    }
    if o.float_button {
        let (cx, cy, r) = ((w as i32 - 64), (h as i32 - 88), 26i32);
        for y in (cy - r)..=(cy + r) {
            for x in (cx - r)..=(cx + r) {
                let d2 = (x - cx) * (x - cx) + (y - cy) * (y - cy);
                if d2 <= r * r {
                    let g = if d2 >= (r - 3) * (r - 3) { 130 } else { 58 };
                    frame_set(f, x, y, g);
                }
            }
        }
    }
    if o.cursor_blink {
        if frame_index % 2 == 0 {
            frame_rect(f, 300, 180, 310, 198, 20);
        } else {
            frame_rect(f, 300, 180, 310, 181, 20);
            frame_rect(f, 300, 197, 310, 198, 20);
            frame_rect(f, 300, 180, 301, 198, 20);
            frame_rect(f, 309, 180, 310, 198, 20);
        }
    }
}

pub fn add_noise(f: &mut Frame, amp: u8, rng: &mut Rng) {
    let span = 2 * amp as u64 + 1;
    for i in (0..f.data.len()).step_by(4) {
        for c in 0..3 {
            let d = rng.below(span) as i32 - amp as i32;
            let v = (f.data[i + c] as i32 + d).clamp(0, 255);
            f.data[i + c] = v as u8;
        }
    }
}

/// 生成一串相邻视口帧。top_i = i * shift，返回 (帧序列, 真值 shift)。
pub fn make_frames(
    doc: &[u8],
    w: usize,
    doc_h: usize,
    vh: usize,
    shift: usize,
    overlays: &OverlaySpec,
    noise_amp: u8,
    max_frames: usize,
) -> Vec<Frame> {
    let n = ((doc_h - vh) / shift).min(max_frames);
    let mut rng = Rng(0xC0FFEE);
    let mut frames = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let mut f = crop(doc, w, i * shift, vh);
        apply_overlays(&mut f, overlays, i);
        add_noise(&mut f, noise_amp, &mut rng);
        frames.push(f);
    }
    frames
}
