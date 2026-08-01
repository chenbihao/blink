//! `FakeScreenshotBackend`：测试用假后端（0.11.7-f）。
//!
//! 生产代码不用，`#[cfg(test)]` 也可以，但放在非-test 因为集成测试也可能需要。
//! 通过 `builder` 模式配置：
//!
//! ```ignore
//! let backend = FakeScreenshotBackend::builder()
//!     .display(0, 0, 0, 2560, 1440, true)   // 主屏
//!     .display(1, 2560, 0, 1920, 1080, false) // 副屏
//!     .fill_color(0x00, 0x80, 0xFF, 0xFF)     // BGRA 蓝色填充
//!     .build();
//! ```

#![allow(dead_code)] // 测试专用；builder/single_primary/fill_color 由测试调用

use super::ScreenCaptureMeta;
use super::backend::{DisplayGeometry, ScreenshotBackend};

/// 测试用假后端。
#[derive(Debug, Clone)]
pub struct FakeScreenshotBackend {
    displays: Vec<DisplayGeometry>,
    /// BGRA 填充色，所有截屏都返回这个颜色的填充位图
    fill_bgra: [u8; 4],
}

impl FakeScreenshotBackend {
    pub fn builder() -> FakeScreenshotBackendBuilder {
        FakeScreenshotBackendBuilder::default()
    }

    /// 便捷构造：单主屏 2560x1440，蓝色填充。
    pub fn single_primary(w: u32, h: u32) -> Self {
        Self {
            displays: vec![DisplayGeometry {
                id: 0,
                name: "Fake Primary".into(),
                x: 0,
                y: 0,
                w,
                h,
                dpi: 96,
                primary: true,
            }],
            fill_bgra: [0xFF, 0x80, 0x00, 0xFF], // BGRA = 蓝
        }
    }
}

impl ScreenshotBackend for FakeScreenshotBackend {
    fn list_displays(&self) -> Vec<DisplayGeometry> {
        self.displays.clone()
    }

    fn capture_virtual_screen(&self) -> Result<(Vec<u8>, ScreenCaptureMeta), String> {
        if self.displays.is_empty() {
            return Err("无显示器".into());
        }
        // 虚拟屏幕 = 所有显示器的包围矩形
        let min_x = self.displays.iter().map(|d| d.x).min().unwrap();
        let min_y = self.displays.iter().map(|d| d.y).min().unwrap();
        let max_x = self
            .displays
            .iter()
            .map(|d| d.x + d.w as i32)
            .max()
            .unwrap();
        let max_y = self
            .displays
            .iter()
            .map(|d| d.y + d.h as i32)
            .max()
            .unwrap();
        let w = (max_x - min_x) as u32;
        let h = (max_y - min_y) as u32;

        let pixels = fill_bgra(w, h, self.fill_bgra);
        Ok((
            pixels,
            ScreenCaptureMeta {
                virtual_x: min_x,
                virtual_y: min_y,
                width: w,
                height: h,
            },
        ))
    }

    fn capture_display(&self, display_id: u32) -> Result<(Vec<u8>, DisplayGeometry), String> {
        let display = self
            .displays
            .iter()
            .find(|d| d.id == display_id)
            .ok_or_else(|| format!("display_id={display_id} 不存在"))?
            .clone();
        let pixels = fill_bgra(display.w, display.h, self.fill_bgra);
        Ok((pixels, display))
    }

    fn capture_region(&self, _x: i32, _y: i32, w: u32, h: u32) -> Result<Vec<u8>, String> {
        Ok(fill_bgra(w, h, self.fill_bgra))
    }
}

fn fill_bgra(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        v.extend_from_slice(&color);
    }
    v
}

// ── Builder ────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct FakeScreenshotBackendBuilder {
    displays: Vec<DisplayGeometry>,
    fill_bgra: Option<[u8; 4]>,
}

impl FakeScreenshotBackendBuilder {
    pub fn display(mut self, id: u32, x: i32, y: i32, w: u32, h: u32, primary: bool) -> Self {
        self.displays.push(DisplayGeometry {
            id,
            name: format!("Fake #{id}"),
            x,
            y,
            w,
            h,
            dpi: 96,
            primary,
        });
        self
    }

    pub fn fill_color(mut self, b: u8, g: u8, r: u8, a: u8) -> Self {
        self.fill_bgra = Some([b, g, r, a]);
        self
    }

    pub fn build(self) -> FakeScreenshotBackend {
        FakeScreenshotBackend {
            displays: self.displays,
            fill_bgra: self.fill_bgra.unwrap_or([0, 0, 0, 0xFF]),
        }
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_primary_returns_expected_shape() {
        let b = FakeScreenshotBackend::single_primary(1920, 1080);
        let list = b.list_displays();
        assert_eq!(list.len(), 1);
        assert!(list[0].primary);
        assert_eq!(list[0].w, 1920);
        assert_eq!(list[0].h, 1080);
    }

    #[test]
    fn builder_configures_dual_displays() {
        let b = FakeScreenshotBackend::builder()
            .display(0, 0, 0, 2560, 1440, true)
            .display(1, 2560, 0, 1920, 1080, false)
            .fill_color(0xFF, 0x80, 0x00, 0xFF)
            .build();
        let list = b.list_displays();
        assert_eq!(list.len(), 2);
        assert!(list[0].primary);
        assert!(!list[1].primary);
    }

    #[test]
    fn capture_virtual_screen_spans_all_displays() {
        let b = FakeScreenshotBackend::builder()
            .display(0, 0, 0, 2560, 1440, true)
            .display(1, 2560, 0, 1920, 1080, false)
            .build();
        let (pixels, meta) = b.capture_virtual_screen().unwrap();
        // 虚拟屏幕包围矩形：宽 = 2560 + 1920 = 4480, 高 = max(1440, 1080) = 1440
        assert_eq!(meta.width, 4480);
        assert_eq!(meta.height, 1440);
        assert_eq!(pixels.len(), (4480 * 1440 * 4) as usize);
    }

    #[test]
    fn capture_display_returns_configured_size() {
        let b = FakeScreenshotBackend::builder()
            .display(0, 0, 0, 800, 600, true)
            .display(1, 800, 0, 400, 300, false)
            .fill_color(0x11, 0x22, 0x33, 0xFF)
            .build();
        let (pixels, geom) = b.capture_display(1).unwrap();
        assert_eq!(geom.w, 400);
        assert_eq!(geom.h, 300);
        assert_eq!(pixels.len(), (400 * 300 * 4) as usize);
        // 检查填充色
        assert_eq!(&pixels[..4], &[0x11, 0x22, 0x33, 0xFF]);
    }

    #[test]
    fn capture_display_unknown_id_errors() {
        let b = FakeScreenshotBackend::single_primary(100, 100);
        assert!(b.capture_display(999).is_err());
    }

    #[test]
    fn empty_displays_capture_virtual_screen_errors() {
        let b = FakeScreenshotBackend::builder().build();
        assert!(b.capture_virtual_screen().is_err());
    }
}
