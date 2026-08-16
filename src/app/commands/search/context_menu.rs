//! 独立右键菜单窗口 commands。

use super::*;

/// 显示右键菜单独立窗口（突破主窗口边界裁剪）。
/// 复用已有窗口：首次创建，后续 hide → 更新数据 → show，避免重复创建 WebView2 的开销。
///
/// `width/height` 是菜单的 **CSS 像素**尺寸；光标物理坐标由后端 `GetCursorPos` 直接读取，
/// 不接受前端传入的 `screenX/Y`（WebView2 里那是 CSS 像素，高 DPI 屏会偏 1/3+）。
/// 定位/缩放/边界翻转全部走 `clamp_context_menu`。
#[tauri::command]
pub async fn show_context_menu(
    app: tauri::AppHandle,
    width: f64,
    height: f64,
    items: String,
) -> Result<(), String> {
    // 主题 resolve（auto → dark/light）
    let theme = {
        let pool = &app.state::<crate::infra::data::DbPools>().config;
        let raw = crate::app::config::get_config(pool).await.theme;
        if raw == "auto" {
            let is_light = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
                .open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
                .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
                .map(|v| v == 1)
                .unwrap_or(false);
            if is_light {
                "light".to_string()
            } else {
                "dark".to_string()
            }
        } else {
            raw
        }
    };

    // 多屏感知定位：Win32 直接拿光标物理坐标 + 目标屏 DPI 缩放 + 智能翻转
    let (fx, fy, fw, fh) = crate::infra::platform::window::clamp_context_menu(width, height);
    crate::infra::platform::window::set_context_menu_payload(items.clone(), theme.clone());

    // 复用已有窗口：resize → reposition → show → eval 渲染新数据 → force_topmost
    // ⚠️ 不能在隐藏态用 emit 传数据：WebView2 在 IsVisible=false 时会丢弃事件
    // （曾导致「窗口尺寸已撑开、内容却没更新」）。改用 eval（走 ExecuteScript 注入
    // 脚本到 webview 队列，show 之后必执行），比事件系统更可靠地更新菜单内容。
    if let Some(win) = app.get_webview_window("context-menu") {
        // ⚠️ 不能用 set_size + set_position：窗口在主屏预热、跨到 DPI 不同的屏时，
        // set_position 会触发 WM_DPICHANGED，tao 据此重设尺寸（不动位置），
        // 与刚排队的 set_size 竞态，导致多屏不同 DPI 下菜单尺寸/位置偏（Tauri #3610）。
        // 改用 SetWindowPos 一次原子设定位置+尺寸，绕开 tao 的 DPI 重设逻辑。
        //
        // 但即使走 SetWindowPos，跨 DPI 屏时 Windows 仍会给 hwnd 发 WM_DPICHANGED，
        // tao 的 wndproc 收到后会按建议 rect 再改一次尺寸——把我们刚设的物理尺寸推翻，
        // 症状是「切屏首次右键宽高错，第二次才对」。破法：show 之后再补一次
        // place_at_physical，让 WM_DPICHANGED 的抢跑跑完后再纠正一次。
        let hwnd_opt = win
            .hwnd()
            .ok()
            .map(|h| windows::Win32::Foundation::HWND(h.0 as _));
        if let Some(hwnd) = hwnd_opt {
            crate::infra::platform::window::place_at_physical(hwnd, fx, fy, fw, fh);
            // 撤销上次 hide_context_menu 设的 DWM Cloak，否则 show 后窗口仍不可见
            crate::infra::platform::window::apply_cloak(hwnd, false);
        } else {
            // hwnd 拿不到时的兜底（理论上不会到这）
            let _ = win.set_size(tauri::PhysicalSize::new(fw, fh));
            let _ = win.set_position(tauri::PhysicalPosition::new(fx, fy));
        }
        let _ = win.show();
        // 补一次：show 触发的 WM_DPICHANGED 若把尺寸改回去了，这里覆盖回来
        if let Some(hwnd) = hwnd_opt {
            crate::infra::platform::window::place_at_physical(hwnd, fx, fy, fw, fh);
        }
        let theme_js = serde_json::to_string(&theme).unwrap_or_else(|_| "\"dark\"".to_string());
        let js = format!(
            "window.__renderContextMenu && window.__renderContextMenu({items}, {theme})",
            items = items,
            theme = theme_js,
        );
        let _ = win.eval(&js);
        // Win32 直接设 TOPMOST，比 Tauri 的 set_always_on_top 更可靠
        if let Some(hwnd) = hwnd_opt {
            crate::infra::platform::window::force_topmost(hwnd);
        }
        tracing::trace!(fx, fy, fw, fh, items_len = items.len(), "右键菜单窗口复用");
        return Ok(());
    }

    // 首次创建：通过 URL 参数传递初始数据
    // ⚠️ builder 的 inner_size / position 是**逻辑像素**（tao 内部按 LogicalSize 处理），
    // 但 fw/fh/fx/fy 是物理像素——直接塞给 builder 会被 Tauri 按主屏 DPI 再放大一遍。
    // 这里传 CSS 尺寸（逻辑像素）让 builder 别炸，位置随便给个占位；build 完立刻
    // place_at_physical 强制矫正到目标物理坐标 + 尺寸，跟截图 overlay 同套路。
    let encoded_items = urlencoding::encode(&items).to_string();
    let url = format!("contextmenu-popup.html?items={encoded_items}&theme={theme}");
    tracing::debug!(fx, fy, fw, fh, "创建右键菜单窗口");
    let (_win, _) = crate::infra::platform::window::get_or_create_context_menu_window(
        &app, url, width, height,
    )?;

    if let Ok(hwnd) = _win.hwnd() {
        let hwnd = windows::Win32::Foundation::HWND(hwnd.0 as _);
        crate::infra::platform::window::place_at_physical(hwnd, fx, fy, fw, fh);
        let _ = _win.show();
        // 首次创建同样补一次：show 若触发 WM_DPICHANGED 会撞乱刚设的尺寸
        crate::infra::platform::window::place_at_physical(hwnd, fx, fy, fw, fh);
        crate::infra::platform::window::force_topmost(hwnd);
    } else {
        // hwnd 拿不到的兜底路径
        let _ = _win.set_size(tauri::PhysicalSize::new(fw, fh));
        let _ = _win.set_position(tauri::PhysicalPosition::new(fx, fy));
        let _ = _win.show();
    }

    // helper 可能复用了刚完成预热的窗口；URL 参数只覆盖“本调用是创建者”的情况。
    // 对已经 ready 的复用窗口再走一次 eval，尚未 ready 时则由 pending pull 兜底。
    let theme_js = serde_json::to_string(&theme).unwrap_or_else(|_| "\"dark\"".to_string());
    let js = format!(
        "window.__renderContextMenu && window.__renderContextMenu({items}, {theme})",
        items = items,
        theme = theme_js,
    );
    let _ = _win.eval(&js);

    tracing::trace!(
        fx,
        fy,
        fw,
        fh,
        items_len = items.len(),
        "右键菜单窗口已创建"
    );
    Ok(())
}

/// 前端 ready 主动拉取首次菜单载荷，避免预热刚建完但模块脚本尚未注册时 eval 丢失。
#[tauri::command]
pub fn take_context_menu_payload() -> Option<serde_json::Value> {
    crate::infra::platform::window::take_context_menu_payload().map(|(items, theme)| {
        serde_json::json!({
            "items": items,
            "theme": theme,
        })
    })
}

/// 隐藏右键菜单窗口（hide 而非 close，保留窗口供下次复用）。
#[tauri::command]
pub async fn hide_context_menu(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("context-menu") {
        // DWM Cloak 先行：瞬间消失无 fade 动画，避免截图时拍到右键窗口残影
        if let Ok(hwnd) = win.hwnd() {
            crate::infra::platform::window::apply_cloak(
                windows::Win32::Foundation::HWND(hwnd.0 as _),
                true,
            );
        }
        let _ = win.hide();
        tracing::trace!("hide_context_menu: 已隐藏右键菜单窗口");
    }
    Ok(())
}

/// Popup 窗口菜单项被点击 → 通知主窗口执行动作。
/// action_id 是菜单项的唯一标识（JSON 数组索引）。
///
/// **顺序很重要**：先隐藏 Popup + 主窗口获焦，再 emit 事件。
/// 否则前端收到事件时 Popup 仍是前台窗口，`document.hasFocus() === false`，
/// `navigator.clipboard.readText()` 会被 Chromium 以「document 未获焦」为由拒绝，
/// `execCommand("paste")` 同样失效——症状就是「点粘贴，输入框仍空」（右键在
/// 主窗口边框时尤其容易复现，此时主窗口本就不是前台）。
#[tauri::command]
pub async fn context_menu_action(app: tauri::AppHandle, action_id: u32) -> Result<(), String> {
    // 1. 先隐藏 Popup 窗口，让主窗口有机会重回前台
    hide_context_menu(app.clone()).await?;
    // 2. 显式把主窗口置为前台并聚焦，保证 clipboard/execCommand 可用
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.show();
        let _ = main.set_focus();
    }
    // 3. 最后再通知前端执行动作
    app.emit(EventNames::CONTEXT_MENU_ACTION, action_id)
        .map_err(|e| e.to_string())?;
    Ok(())
}
