//! 系统托盘菜单构建 + 文案 i18n。
//!
//! 托盘菜单文字构建在 Rust 侧（`main.rs` setup），不走前端 i18n 字典——前端字典是 JS
//! 对象 Rust 无法 import，且托盘文字仅 3 条，本地 match 比维护一份共享字典更直接。
//!
//! **运行时热切换**：`set_config` 的 `"language"` 分支调用 [`rebuild_menu`] 重建菜单。
//! `on_menu_event` 挂在 `TrayIcon` 上而非 `Menu` 上，`set_menu` 替换菜单不影响 id 路由。
//!
//! 托盘默认 id 为 `"main"`（`TrayIconBuilder::new()` 未显式 `.id()`），`rebuild_menu`
//! 通过 `app.tray_by_id("main")` 取回。

use tauri::Manager;
use tauri::menu::{Menu, MenuItem};

/// 托盘菜单项 key（与菜单 item id 一一对应）。
#[derive(Clone, Copy)]
pub enum TrayText {
    ShowMain,
    StickyManager,
    ChatWindow,
    Settings,
    RecoverInputHook,
    About,
    Quit,
}

/// 托盘菜单项文案（按语言解析）。
///
/// `lang` 走 BCP47 前缀（`"zh"` / `"en"`），与 `AppConfig.language` 一致。
/// 未识别语言降级为英文。
pub fn text(lang: &str, key: TrayText) -> &'static str {
    match (lang.starts_with("zh"), key) {
        (true, TrayText::ShowMain) => "显示主窗口",
        (true, TrayText::StickyManager) => "便签管理",
        (true, TrayText::ChatWindow) => "AI 对话窗口",
        (true, TrayText::Settings) => "设置",
        (true, TrayText::RecoverInputHook) => "恢复输入钩子",
        (true, TrayText::About) => "关于 Blink",
        (true, TrayText::Quit) => "退出 Blink",
        (false, TrayText::ShowMain) => "Show Main Window",
        (false, TrayText::StickyManager) => "Sticky Manager",
        (false, TrayText::ChatWindow) => "AI Chat Window",
        (false, TrayText::Settings) => "Settings",
        (false, TrayText::RecoverInputHook) => "Recover Input Hook",
        (false, TrayText::About) => "About Blink",
        (false, TrayText::Quit) => "Quit Blink",
    }
}

/// 构建托盘菜单（不挂事件——事件由 `TrayIconBuilder::on_menu_event` 统一挂）。
///
/// 菜单 item id（`"settings"` / `"about"` / `"quit"`）是稳定的，重建菜单后 id 不变，
/// `on_menu_event` 路由依然有效。
pub fn build_menu(app: &impl Manager<tauri::Wry>, lang: &str) -> tauri::Result<Menu<tauri::Wry>> {
    // 0.18.4：菜单重组——便签管理 + AI 对话窗口归为 chord 能力组，设置上提
    // 0.19.17：新增「恢复输入钩子」逃生舱——Alt+Space 失效时用户可从此恢复
    // 结构：show_main → sep → sticky_manager → chat_window → sep → settings → recover_hook → about → sep → quit
    let show_main = MenuItem::with_id(
        app,
        "show_main",
        text(lang, TrayText::ShowMain),
        true,
        None::<&str>,
    )?;
    let sticky_manager = MenuItem::with_id(
        app,
        "sticky_manager",
        text(lang, TrayText::StickyManager),
        true,
        None::<&str>,
    )?;
    let chat_window = MenuItem::with_id(
        app,
        "chat_window",
        text(lang, TrayText::ChatWindow),
        true,
        None::<&str>,
    )?;
    let settings = MenuItem::with_id(
        app,
        "settings",
        text(lang, TrayText::Settings),
        true,
        None::<&str>,
    )?;
    let recover_hook = MenuItem::with_id(
        app,
        "recover_hook",
        text(lang, TrayText::RecoverInputHook),
        true,
        None::<&str>,
    )?;
    let about = MenuItem::with_id(
        app,
        "about",
        text(lang, TrayText::About),
        true,
        None::<&str>,
    )?;
    let sep = tauri::menu::PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", text(lang, TrayText::Quit), true, None::<&str>)?;
    Menu::with_items(
        app,
        &[
            &show_main,
            &sep,
            &sticky_manager,
            &chat_window,
            &sep,
            &settings,
            &recover_hook,
            &about,
            &sep,
            &quit,
        ],
    )
}

/// 重建托盘菜单（运行时语言切换时调用）。
///
/// 托盘默认 id `"main"`（见模块级注释）。重建失败仅 warn，不阻断语言切换主流程。
///
/// 接收 `AppHandle` 而非泛型 `impl Manager`——`tray_by_id` 是 `App`/`AppHandle` 的
/// inherent method，不在 `Manager` trait 上。`build_menu` 仍走泛型，setup（`&mut App`）
/// 与此处都能调用。
pub fn rebuild_menu(app: &tauri::AppHandle, lang: &str) {
    let Some(tray) = app.tray_by_id("main") else {
        tracing::warn!("rebuild_menu: tray_by_id(\"main\") 未找到，跳过托盘重建");
        return;
    };
    match build_menu(app, lang) {
        Ok(menu) => {
            if let Err(e) = tray.set_menu(Some(menu)) {
                tracing::warn!(%e, "rebuild_menu: set_menu 失败");
            }
        }
        Err(e) => tracing::warn!(%e, "rebuild_menu: build_menu 失败"),
    }
}

// ── TrayAnimator: 托盘呼吸动画（0.17.2 §3.6 B） ──────────────────────

use std::sync::{Mutex, OnceLock};
use tauri::image::Image;

/// 呼吸动画任务句柄。None = 未在呼吸中。
static BREATHING_TASK: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> = Mutex::new(None);

/// 呼吸动画帧（3 帧，懒加载）。首次 start_breathing 时从内置 icon.png 生成。
static BREATHING_FRAMES: OnceLock<Vec<Image<'static>>> = OnceLock::new();

/// 获取呼吸动画帧（首次调用时从内置 icon.png 生成 3 帧绿点叠加）。
fn get_breathing_frames() -> &'static [Image<'static>] {
    BREATHING_FRAMES
        .get_or_init(|| {
            // 从内置 icon.png 解码 RGBA 数据
            let png_bytes = include_bytes!("../../icons/icon.png");
            let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes.as_slice()));
            let mut reader = decoder.read_info().expect("内置 icon.png 应可解码");
            let info = reader.info();
            let w = info.width;
            let h = info.height;
            let mut buf = vec![0u8; reader.output_buffer_size()];
            let frame = reader
                .next_frame(&mut buf)
                .expect("内置 icon.png 应可读取像素");
            let rgba = &buf[..frame.buffer_size()];

            // 绿点位于右下角，半径 = 最小边的 12%
            let radius = ((w.min(h) as f32) * 0.12).round() as usize;
            let cx = (w as usize).saturating_sub(radius + radius / 2);
            let cy = (h as usize).saturating_sub(radius + radius / 2);

            // 3 帧不同透明度，形成呼吸效果
            let opacities: [f32; 3] = [0.3, 0.65, 1.0];
            opacities
                .iter()
                .map(|&alpha| {
                    let mut frame = rgba.to_vec();
                    blend_green_dot(&mut frame, w as usize, h as usize, cx, cy, radius, alpha);
                    // new_owned 创建 Image<'static>（Cow::Owned，无需外部数据引用）
                    Image::new_owned(frame, w, h)
                })
                .collect()
        })
        .as_slice()
}

/// 在 RGBA 缓冲上叠加半透明绿点（alpha 混合 + 边缘平滑）。
fn blend_green_dot(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    cx: usize,
    cy: usize,
    radius: usize,
    alpha: f32,
) {
    if radius == 0 || width == 0 || height == 0 {
        return;
    }
    let r2 = (radius * radius) as f32;
    let x_start = cx.saturating_sub(radius);
    let x_end = (cx + radius + 1).min(width);
    let y_start = cy.saturating_sub(radius);
    let y_end = (cy + radius + 1).min(height);

    for y in y_start..y_end {
        for x in x_start..x_end {
            let dx = x as f32 - cx as f32;
            let dy = y as f32 - cy as f32;
            let dist2 = dx * dx + dy * dy;
            if dist2 > r2 {
                continue;
            }
            // 边缘平滑：距圆心越远 alpha 越低
            let edge_factor = 1.0 - (dist2 / r2).min(1.0);
            let pixel_alpha = alpha * edge_factor;

            let idx = (y * width + x) * 4;
            if idx + 3 >= rgba.len() {
                continue;
            }
            // Alpha 混合：绿点 (0, 255, 0) 覆盖在原像素上
            let dst_r = rgba[idx] as f32;
            let dst_g = rgba[idx + 1] as f32;
            let dst_b = rgba[idx + 2] as f32;
            rgba[idx] = (0.0 * pixel_alpha + dst_r * (1.0 - pixel_alpha)).round() as u8;
            rgba[idx + 1] = (255.0 * pixel_alpha + dst_g * (1.0 - pixel_alpha)).round() as u8;
            rgba[idx + 2] = (0.0 * pixel_alpha + dst_b * (1.0 - pixel_alpha)).round() as u8;
            // alpha 通道保持不变（图标通常全不透明）
        }
    }
}

/// 启动托盘呼吸动画（语音输入中）。
///
/// 幂等：已在呼吸中则跳过。失败打 warn 不阻断。
///
/// **状态驱动方式**：直接在 `service.rs` hotkey 分支 + `voice.rs` chat 路径调用，
/// 不走 `VOICE_RECORDING_START/END` 事件（避免 hidden-webview event-drop 问题，
/// `main.rs:371-373` 已文档化此问题）。
pub fn start_breathing(app: &tauri::AppHandle) {
    let mut guard = BREATHING_TASK.lock().unwrap();
    if guard.is_some() {
        return; // 已在呼吸中
    }

    let frames = get_breathing_frames();
    let app_clone = app.clone();

    let task = tauri::async_runtime::spawn(async move {
        let mut idx = 0usize;
        loop {
            if let Some(tray) = app_clone.tray_by_id("main") {
                let _ = tray.set_icon(Some(frames[idx].clone()));
                let _ = tray.set_tooltip(Some("Blink - 语音输入中…"));
            }
            idx = (idx + 1) % frames.len();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    });

    *guard = Some(task);
    tracing::debug!("TrayAnimator: 呼吸动画已启动");
}

/// 停止托盘呼吸动画，恢复默认图标 + tooltip。
///
/// 幂等：未在呼吸中则跳过动画取消（仍恢复默认状态）。
pub fn stop_breathing(app: &tauri::AppHandle) {
    let mut guard = BREATHING_TASK.lock().unwrap();
    if let Some(handle) = guard.take() {
        handle.abort();
        tracing::debug!("TrayAnimator: 呼吸动画已停止");
    }

    // 恢复默认图标 + tooltip
    if let Some(tray) = app.tray_by_id("main") {
        if let Some(icon) = app.default_window_icon() {
            let _ = tray.set_icon(Some(icon.clone()));
        }
        let _ = tray.set_tooltip(Some("Blink"));
    }
}
