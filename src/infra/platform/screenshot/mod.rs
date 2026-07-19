//! 截图模块（0.8.7 §九，0.8.8 预编码优化）。
//!
//! **架构**：
//! - `begin_session()` — 截取整个虚拟屏幕一次，**立即编码 PNG**，存进进程内 `SESSION`
//! - `session_meta()` — 拿元数据（虚拟屏幕坐标 + 像素尺寸），供 overlay 定位窗口
//! - `session_png()` — 返回 SESSION 的**预编码 PNG** 字节（零编码延迟）
//! - `crop(x, y, w, h)` — 按物理像素坐标裁剪，返回子矩形 BGRA
//! - `end_session()` — 清空 SESSION（overlay 关闭时调；防内存驻留）
//!
//! **为什么改成 SESSION**：
//! 旧实现在 `blink-screenshot://` 协议里现截屏，导致 overlay 已 show 后才拿图，
//! 有时序竞态（透明层进入 DWM 合成后再 BitBlt 可能拍到自己）。改后：先截 → 存
//! SESSION → 再建 overlay → overlay 拉协议只是读内存。截屏路径只走一次。
//!
//! **为什么 SESSION 存 BGRA 而不是 RGBA**：BitBlt 原生输出 BGRA，剪贴板 CF_DIB
//! 也要 BGRA。SESSION 存 BGRA 省掉全屏 R↔B swap（3.7M 像素 ~30ms）。只有 PNG
//! 编码这个偏门路径按行 swap，`encode_png` 内部处理。
//!
//! **PNG 编码性能决策**（0.8.7 优化收尾）：
//! - `Compression::Fast + FilterType::NoFilter`：截图 overlay 里的 PNG 看完即弃,
//!   不需要压缩率。跳过 DEFLATE 深度压缩 → 全屏 encode 从 600ms 降到 ~150ms
//! - `u32` 位运算做 R↔B swap（不用 `chunks_exact_mut(4).swap(0, 2)`）：
//!   debug 下每次处理 4 字节而不是逐字节 + auto-vectorize → swap 部分 10-14x 加速
//! - `Cargo.toml` 里 dev profile 单独把 `png / miniz_oxide / flate2 / adler2` 开 opt=3：
//!   这些 crate 是 DEFLATE 热路径，dev debug 循环慢 5-10x。整合 5x 提速
//!
//! **预编码 PNG**（0.8.8 优化）：
//! 原流程：前端拉 `blink-screenshot://` → `session_png()` 现场编码 → 返回。
//! 编码 2560x1440 全屏 PNG 在 dev 下 ~150ms，是用户感知延迟的最大瓶颈。
//! 改后：`begin_session()` 截屏后立即编码 PNG 存进 `png_bytes`，`session_png()` 只读内存。
//! 前端拉取时零编码延迟，总感知从 ~320ms 降到 ~170ms（dev）。
//! 代价：SESSION 多占一份 PNG 内存（~10-30MB），`end_session()` 时一并释放。
//!
//! **纯逻辑抽出**：`crop_rgba` 是纯函数（BGRA 输入 + 矩形输出），带越界 clamp，
//! 覆盖单测；平台相关的 BitBlt 走 `backend_windows.rs`。
//!
//! **0.11.7-f 能力化重构**：
//! - `ScreenshotBackend` trait（`backend.rs`）——可 mock 的截屏平台抽象
//! - `WindowsScreenshotBackend`（`backend_windows.rs`）——生产实现，从旧 `windows.rs` 迁移
//! - `FakeScreenshotBackend`（`backend_fake.rs`）——测试实现
//! - `list_displays()` — 新增多显示器枚举
//! - `SESSION` 保留为运行时状态（overlay 交互需要），但 backend 通过 `install_backend` 注入

pub mod backend;
pub mod backend_fake;
#[cfg(target_os = "windows")]
pub mod backend_windows;

// 兼容层：保留 `windows` 子模块导出的公开函数签名（`capture_virtual_screen()`）
#[cfg(target_os = "windows")]
mod windows;

pub use backend::{DisplayGeometry, ScreenshotBackend};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// 虚拟屏幕元数据（无像素，只描述几何）。
#[derive(Debug, Clone, Copy)]
pub struct ScreenCaptureMeta {
    /// 虚拟屏幕左上角 X（多显示器可能为负，物理像素）。
    pub virtual_x: i32,
    /// 虚拟屏幕左上角 Y。
    pub virtual_y: i32,
    /// 像素宽度（物理像素）。
    pub width: u32,
    /// 像素高度。
    pub height: u32,
}

/// 完整截图会话状态（BGRA 位图 + 预编码 PNG + 元数据）。
///
/// **为什么存 BGRA 不存 RGBA**：BitBlt 原生输出 BGRA，写剪贴板 CF_DIB 也要 BGRA。
/// SESSION 存 BGRA 可以省掉全屏 R↔B swap（3.7M 像素 x 3.5MB shuffle，低配机 ~30ms）。
/// PNG 编码这个偏门路径承担按行 swap 成本（`encode_png` 内部处理）。
///
/// **为什么预编码 PNG**（0.8.8 优化）：
/// 原流程：前端拉 `blink-screenshot://` → `session_png()` 现场编码 → 返回。
/// 编码 2560x1440 全屏 PNG 在 dev 下 ~150ms，是用户感知延迟的最大瓶颈。
/// 改后：`begin_session()` 截屏后立即编码 PNG 存进 `png_bytes`，`session_png()` 只读内存。
/// 前端拉取时零编码延迟，总感知从 ~320ms 降到 ~170ms（dev）。
struct Session {
    /// BGRA、top-down、每行 `width * 4` 字节。
    pixels: Vec<u8>,
    /// 预编码的 PNG 字节（`begin_session` 时编码好，`session_png` 直接返回）。
    png_bytes: Vec<u8>,
    meta: ScreenCaptureMeta,
}

static SESSION: RwLock<Option<Session>> = RwLock::new(None);

/// 标注模式标志（0.11.7）：选区完成后、用户确认输出前为 true。
/// AI 调用 `capture_screen` 时检测此标志，活跃则新截一帧而非复用 SESSION。
static ANNOTATION_MODE: AtomicBool = AtomicBool::new(false);

/// 全局注入的 ScreenshotBackend（0.11.7-f）。
///
/// **默认**：`WindowsScreenshotBackend`（生产），也可以在测试或平台无关代码里替换。
/// 通过 `install_backend()` 注入；`backend()` 返回当前 backend。
///
/// 使用 `OnceLock<RwLock<Arc<dyn>>>` 而非纯 `OnceLock<Arc<dyn>>`：允许运行时替换
/// （测试场景可能需要）。生产路径只写一次（`main.rs::setup`）。
static BACKEND: OnceLock<RwLock<Arc<dyn ScreenshotBackend>>> = OnceLock::new();

/// 安装/替换 backend（0.11.7-f）。
///
/// **调用时机**：`main.rs::setup` 里最早期（在任何 `begin_session` / `list_displays`
/// 调用之前）。可重复调用替换 backend（测试用）。
#[allow(dead_code)] // Step 2 之前默认走 OnceLock 兜底路径
pub fn install_backend(backend: Arc<dyn ScreenshotBackend>) {
    match BACKEND.get() {
        Some(lock) => {
            if let Ok(mut w) = lock.write() {
                *w = backend;
            }
        }
        None => {
            let _ = BACKEND.set(RwLock::new(backend));
        }
    }
}

/// 获取当前 backend（0.11.7-f）。
///
/// **首次调用兜底**：如果没显式 `install_backend`，Windows 平台自动装 `WindowsScreenshotBackend`。
/// 这让平台代码不必显式初始化，但生产路径仍应在 setup 里主动装（便于测试注入）。
pub fn backend() -> Arc<dyn ScreenshotBackend> {
    let lock = BACKEND.get_or_init(|| {
        #[cfg(target_os = "windows")]
        let default: Arc<dyn ScreenshotBackend> =
            Arc::new(backend_windows::WindowsScreenshotBackend::new());
        #[cfg(not(target_os = "windows"))]
        let default: Arc<dyn ScreenshotBackend> =
            panic!("非 Windows 平台需显式 install_backend");
        RwLock::new(default)
    });
    lock.read().expect("backend RwLock 中毒").clone()
}

/// 枚举所有显示器（0.11.7-f）——转发到当前 backend。
#[allow(dead_code)] // Step 2 Capability 消费
pub fn list_displays() -> Vec<DisplayGeometry> {
    backend().list_displays()
}

/// 截取指定显示器（0.11.7-f）——转发到当前 backend。
#[allow(dead_code)] // Step 2 Capability 消费
pub fn capture_display(display_id: u32) -> Result<(Vec<u8>, DisplayGeometry), String> {
    backend().capture_display(display_id)
}

/// 启动截图会话：截取整个虚拟屏幕，**立即编码 PNG**，存进 SESSION，返回元数据。
///
/// **调用时机**：主窗已隐藏、overlay 尚未显示。这样 BitBlt 拍到的是"没有 blink"的桌面。
///
/// **预编码 PNG**（0.8.8 优化）：截屏后立即编码 PNG 存进 `png_bytes`，前端拉取时
/// `session_png()` 只读内存返回，省掉 ~150ms 编码延迟。
///
/// **0.11.7-f**：改走注入的 backend 而非直接调 Win32。
///
/// **标注模式重置**：新会话开始前复位 `ANNOTATION_MODE`，防止前一次崩溃残留导致
/// AI `capture_screen` 永远跳过 SESSION 缓存。
pub fn begin_session() -> Result<ScreenCaptureMeta, String> {
    // 新会话开始时清除前一次可能残留的标注模式（backend crash / 前端异常退出等情况）
    set_annotation_mode(false);

    let (pixels, meta) = backend().capture_virtual_screen()?;

    // 截屏后立即编码 PNG——此时主窗已 hide，BitBlt 不会拍到自己
    let png_bytes = encode_png(&pixels, meta.width, meta.height)
        .map_err(|e| format!("预编码 PNG 失败: {e}"))?;

    let meta_copy = meta;
    *SESSION
        .write()
        .map_err(|e| format!("SESSION 写锁失败: {e}"))? = Some(Session {
        pixels,
        png_bytes,
        meta,
    });
    tracing::debug!(?meta_copy, "截图 SESSION 已建立（含预编码 PNG）");
    Ok(meta_copy)
}

/// 返回 SESSION 的预编码 PNG 字节。SESSION 为空返回 None。
///
/// 供 `blink-screenshot://capture` 协议使用；overlay 前端 `<img src>` 拉这个。
///
/// **0.8.8 优化**：PNG 已在 `begin_session()` 时编码好存进 `png_bytes`，
/// 这里只读内存返回，零编码延迟。
pub fn session_png() -> Option<Vec<u8>> {
    let guard = SESSION.read().ok()?;
    let s = guard.as_ref()?;
    Some(s.png_bytes.clone())
}

/// 按物理像素坐标裁剪 SESSION 的位图，返回 **BGRA** 子矩形 + 尺寸。
///
/// 坐标越界会被 clamp 到有效范围；裁完 SESSION 不清空（清空由 `end_session` 显式做）。
/// SESSION 为空或裁剪后尺寸为 0 时返回 None。
///
/// 返回 BGRA 是为了直接喂给 `write_rgba_to_clipboard`（其实现按 BGRA 写 CF_DIB，
/// 后 rename 见 note）——避免"RGBA 裁完 → 剪贴板再 swap 回 BGRA"的多余搬运。
pub fn crop(x: i32, y: i32, w: u32, h: u32) -> Option<(Vec<u8>, u32, u32)> {
    let guard = SESSION.read().ok()?;
    let s = guard.as_ref()?;
    let (bgra, cw, ch) = crop_rgba(&s.pixels, s.meta.width, s.meta.height, x, y, w, h)?;
    Some((bgra, cw, ch))
}

/// 清空 SESSION（释放位图内存）。overlay 关闭时调。
pub fn end_session() {
    if let Ok(mut g) = SESSION.write() {
        if g.is_some() {
            tracing::debug!("截图 SESSION 已清空");
        }
        *g = None;
    }
}

/// 设置/清除标注模式（0.11.7）。
///
/// 标注模式 = 选区完成到用户确认输出之间。AI 调用 `capture_screen` 时检测此标志，
/// 活跃则新截一帧而非复用 SESSION（避免读到标注进行中的截图）。
pub fn set_annotation_mode(active: bool) {
    ANNOTATION_MODE.store(active, Ordering::SeqCst);
    tracing::debug!(active, "标注模式状态");
}

/// 查询标注模式是否活跃（0.11.7）。
pub fn is_annotation_active() -> bool {
    ANNOTATION_MODE.load(Ordering::SeqCst)
}

// ── 纯逻辑（跨平台，可单测） ─────────────────────────────────────────────────

/// 从 RGBA 位图中裁剪子矩形。
///
/// - `pixels`：RGBA、top-down、每行 `src_w * 4` 字节
/// - `x/y`：裁剪起点（可能为负，会被 clamp 到 0）
/// - `w/h`：裁剪尺寸（会 clamp 到不越界）
/// - 返回 `(裁剪后 RGBA, 实际宽, 实际高)`；如果裁剪后尺寸为 0 返回 None
///
/// 纯函数，覆盖单测。
pub fn crop_rgba(
    pixels: &[u8],
    src_w: u32,
    src_h: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    if pixels.len() != (src_w as usize) * (src_h as usize) * 4 {
        return None;
    }
    // clamp 起点到 [0, src_w) / [0, src_h)
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    if x0 >= src_w || y0 >= src_h {
        return None;
    }
    // clamp 尺寸不越界
    let cw = w.min(src_w - x0);
    let ch = h.min(src_h - y0);
    if cw == 0 || ch == 0 {
        return None;
    }

    let src_stride = (src_w as usize) * 4;
    let dst_stride = (cw as usize) * 4;
    let mut out = Vec::with_capacity(dst_stride * (ch as usize));
    for row in 0..ch as usize {
        let src_row_start = (y0 as usize + row) * src_stride + (x0 as usize) * 4;
        out.extend_from_slice(&pixels[src_row_start..src_row_start + dst_stride]);
    }
    Some((out, cw, ch))
}

/// 把 **BGRA** 像素编码为 PNG 字节（PNG 要 RGBA，函数内按行做 R↔B swap）。
///
/// `pixels` 格式：BGRA、top-down、每行 `width * 4` 字节。
/// 逐行分配 tmp buffer 而不是整块 clone，减少峰值内存（3840x2160 全屏 ~33MB → 逐行 ~15KB）。
///
/// **速度优先**（截图 overlay 里的 PNG 看完即弃，压缩率无意义）：
/// - `Compression::Fast + FilterType::NoFilter` 跳过 DEFLATE 深度压缩和 filter 预处理
/// - `u32` 位运算做 R↔B swap（比 `chunks_exact_mut(4).swap(0, 2)` 快 5-14x，尤其 debug）
/// - dev profile 里 png/miniz_oxide/flate2 单独开 opt=3（见 Cargo.toml）
///
/// 效果：2560x1440 全屏 encode 从 600ms 降到 ~150ms（dev），release 更快。
pub fn encode_png(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let stride = (width as usize) * 4;
    if pixels.len() != stride * (height as usize) {
        return Err(format!(
            "像素长度与尺寸不符: {} vs {}",
            pixels.len(),
            stride * height as usize
        ));
    }
    // 预分配 buffer：无压缩 PNG 大小 ≈ 像素字节数 + header 开销
    let mut buf = Vec::with_capacity(stride * (height as usize) + 4096);
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::FilterType::NoFilter);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        let mut stream = writer.stream_writer().map_err(|e| e.to_string())?;
        let px_per_row = width as usize;
        // u32 视图批量 swap R↔B：
        //   BGRA [B,G,R,A] 在 little-endian 下 = u32 0xAARRGGBB
        //   RGBA [R,G,B,A]            = u32 0xAABBGGRR
        //   差别仅 R/B 位置对换 → mask 0x00FF00FF 提出 R/B 两字节交换
        let mut row_buf = vec![0u32; px_per_row];
        // Safety: row_buf 是 Vec<u32>，转 &mut [u8] 长度乘 4，对齐无问题
        let row_buf_bytes =
            unsafe { std::slice::from_raw_parts_mut(row_buf.as_mut_ptr() as *mut u8, stride) };
        for row_idx in 0..height as usize {
            let src = &pixels[row_idx * stride..(row_idx + 1) * stride];
            row_buf_bytes.copy_from_slice(src);
            for px in row_buf.iter_mut() {
                let v = *px;
                let rb = v & 0x00FF00FF;
                let ga = v & 0xFF00FF00;
                *px = ga | (rb << 16) | (rb >> 16);
            }
            std::io::Write::write_all(&mut stream, row_buf_bytes).map_err(|e| e.to_string())?;
        }
        std::io::Write::flush(&mut stream).map_err(|e| e.to_string())?;
    }
    Ok(buf)
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 4x3 的 RGBA 位图，每像素 R=col*10, G=row*10, B=0, A=255。
    fn sample_4x3() -> Vec<u8> {
        let mut v = Vec::with_capacity(4 * 3 * 4);
        for row in 0..3u8 {
            for col in 0..4u8 {
                v.extend_from_slice(&[col * 10, row * 10, 0, 255]);
            }
        }
        v
    }

    #[test]
    fn crop_full_matches_source() {
        let src = sample_4x3();
        let (out, w, h) = crop_rgba(&src, 4, 3, 0, 0, 4, 3).unwrap();
        assert_eq!((w, h), (4, 3));
        assert_eq!(out, src);
    }

    #[test]
    fn crop_center_2x1() {
        let src = sample_4x3();
        // 取第 1 行、col 1..3
        let (out, w, h) = crop_rgba(&src, 4, 3, 1, 1, 2, 1).unwrap();
        assert_eq!((w, h), (2, 1));
        // 期望：(10,10,0,255) (20,10,0,255)
        assert_eq!(out, vec![10, 10, 0, 255, 20, 10, 0, 255]);
    }

    #[test]
    fn crop_clamps_negative_origin() {
        let src = sample_4x3();
        // x=-1,y=-1 会被 clamp 到 (0,0)；宽高保持
        let (out, w, h) = crop_rgba(&src, 4, 3, -1, -1, 2, 2).unwrap();
        assert_eq!((w, h), (2, 2));
        // 应等于左上 2x2
        let (full, _, _) = crop_rgba(&src, 4, 3, 0, 0, 2, 2).unwrap();
        assert_eq!(out, full);
    }

    #[test]
    fn crop_clamps_oversize() {
        let src = sample_4x3();
        // 要 10x10，源只有 4x3，clamp 到 4x3
        let (out, w, h) = crop_rgba(&src, 4, 3, 0, 0, 10, 10).unwrap();
        assert_eq!((w, h), (4, 3));
        assert_eq!(out, src);
    }

    #[test]
    fn crop_out_of_bounds_returns_none() {
        let src = sample_4x3();
        assert!(crop_rgba(&src, 4, 3, 4, 0, 1, 1).is_none());
        assert!(crop_rgba(&src, 4, 3, 0, 3, 1, 1).is_none());
        assert!(crop_rgba(&src, 4, 3, 0, 0, 0, 1).is_none());
    }

    #[test]
    fn crop_invalid_buffer_length_returns_none() {
        let bad = vec![0u8; 10]; // 4x3 需要 48 字节
        assert!(crop_rgba(&bad, 4, 3, 0, 0, 1, 1).is_none());
    }

    #[test]
    fn encode_png_produces_valid_magic() {
        let src = sample_4x3();
        let png = encode_png(&src, 4, 3).unwrap();
        // PNG 魔数 89 50 4E 47 0D 0A 1A 0A
        assert_eq!(&png[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    }
}
