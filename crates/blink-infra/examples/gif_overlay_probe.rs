//! 手动原生验收：两块自有纯色窗口验证 GDI 捕获排除与半透明标注仅合成一次。
//! cargo run -p blink-infra --example gif_overlay_probe
#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use blink_infra::gif::{
        FrameSource, GdiFrameSource,
        annotation::{AnnotationFrame, AnnotationState},
    };
    use std::{
        io::Cursor,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use windows::{
        Win32::{
            Foundation::HWND,
            Graphics::{Dwm::DwmFlush, Gdi::UpdateWindow},
            UI::WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, SW_SHOWNOACTIVATE, SetWindowDisplayAffinity,
                ShowWindow, WDA_EXCLUDEFROMCAPTURE, WINDOW_STYLE, WS_EX_NOACTIVATE,
                WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
            },
        },
        core::w,
    };
    unsafe {
        windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    struct OwnedWindow(HWND);
    impl Drop for OwnedWindow {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }
    let display = blink_infra::platform::screenshot::list_displays()
        .into_iter()
        .next()
        .ok_or("无显示器")?;
    let (x, y) = (display.x + 48, display.y + 48);
    let create = |style: u32| -> windows::core::Result<OwnedWindow> {
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                w!("STATIC"),
                w!("Blink capture probe"),
                WS_POPUP | WINDOW_STYLE(style),
                x,
                y,
                128,
                128,
                None,
                None,
                None,
                None,
            )?;
            let owned = OwnedWindow(hwnd);

            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                hwnd,
                Some(windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST),
                0,
                0,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
            )?;
            let _ = UpdateWindow(hwnd);
            Ok(owned)
        }
    };
    // SS_WHITERECT / SS_BLACKRECT：使用系统 STATIC 控件绘制，不读取用户内容。
    let _background = create(6)?;
    let settle = || {
        std::thread::sleep(Duration::from_millis(100));
        unsafe {
            let _ = DwmFlush();
        }
    };
    settle();
    let mut source = GdiFrameSource::new(x + 16, y + 16, 64, 64, false);
    let background = source.capture()?;
    let overlay = create(4)?;
    settle();
    let visible = source.capture()?;
    tracing::info!(pixel=?&visible[..4],"排除前像素");
    assert!(
        visible[..3]
            .iter()
            .zip(&background[..3])
            .any(|(a, b)| a.abs_diff(*b) > 20),
        "覆盖窗口必须先真实入镜，防止空验证"
    );
    unsafe {
        SetWindowDisplayAffinity(overlay.0, WDA_EXCLUDEFROMCAPTURE)?;
    }
    settle();
    let excluded = source.capture()?;
    assert!(
        excluded[..3]
            .iter()
            .zip(&background[..3])
            .all(|(a, b)| a.abs_diff(*b) <= 2),
        "排除后应看到自有白色底窗，而不是黑色控制窗"
    );
    let image = image::RgbaImage::from_pixel(64, 64, image::Rgba([255, 0, 0, 128]));
    let mut png = Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png)?;
    let annotation = Arc::new(Mutex::new(AnnotationState {
        revision: 1,
        frame: Some(Arc::new(AnnotationFrame::decode(png.get_ref(), 64, 64)?)),
    }));
    for fps in [15, 60] {
        let mut source = GdiFrameSource::new(x + 16, y + 16, 64, 64, false)
            .with_fps(fps)
            .with_annotation(annotation.clone());
        let frame = source.capture()?;
        assert!(
            (125..=129).contains(&frame[0]) && (125..=129).contains(&frame[1]) && frame[2] >= 253,
            "半透明红色标注应仅合成一次，fps={fps}, pixel={:?}",
            &frame[..4]
        );
        tracing::info!(fps,pixel=?&frame[..4],"原生捕获排除及标注合成验证通过");
    }
    Ok(())
}
#[cfg(not(target_os = "windows"))]
fn main() {}
