use super::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GWL_EXSTYLE, WINDOW_EX_STYLE, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

#[test]
#[ignore = "creates an isolated hidden HWND to verify recording style transitions"]
fn recording_overlay_keeps_transparent_identity_when_mouse_interaction_changes() {
    struct Probe(HWND);
    impl Drop for Probe {
        fn drop(&mut self) {
            OVERLAY_RECORD_ACTIVE.store(false, Ordering::SeqCst);
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }
    let window = Probe(
        unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE,
                windows::core::w!("STATIC"),
                windows::core::w!("Blink recording style test"),
                WS_POPUP,
                0,
                0,
                200,
                100,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap(),
    );
    assert!(
        unsafe {
            SetWindowSubclass(
                window.0,
                Some(screenshot_overlay_dpichanged_proc),
                SCREENSHOT_OVERLAY_SUBCLASS_ID,
                0,
            )
        }
        .as_bool()
    );
    let original = unsafe { GetWindowLongPtrW(window.0, GWL_EXSTYLE) } as u32;
    OVERLAY_RECORD_ACTIVE.store(true, Ordering::SeqCst);
    for pass_through in [true, false, true, false] {
        let requested = original
            | if pass_through {
                WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0
            } else {
                0
            };
        unsafe {
            SetWindowLongPtrW(window.0, GWL_EXSTYLE, requested as isize);
        }
        let actual = WINDOW_EX_STYLE(unsafe { GetWindowLongPtrW(window.0, GWL_EXSTYLE) } as u32);
        assert!(actual.contains(WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE));
        assert_eq!(actual.contains(WS_EX_TRANSPARENT), pass_through);
    }
    OVERLAY_RECORD_ACTIVE.store(false, Ordering::SeqCst);
    unsafe {
        SetWindowLongPtrW(window.0, GWL_EXSTYLE, original as isize);
    }
    assert_eq!(
        unsafe { GetWindowLongPtrW(window.0, GWL_EXSTYLE) } as u32,
        original
    );
}
