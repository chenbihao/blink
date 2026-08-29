//! 截图、OCR 与翻译 commands。

use super::*;

static WHEEL_INJECT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
const WHEEL_PASSTHROUGH_MS: u64 = 260;
const MAX_WHEEL_PASSTHROUGH_MS: u64 = 500;
const SCROLL_PROBE_MAX_W: u32 = 96;
const SCROLL_PROBE_MAX_H: u32 = 64;
const SCROLL_REPLAY_MAX_FILE_BYTES: usize = 64 * 1024 * 1024;

fn scroll_replay_export_path(
    directory_name: &str,
    file_name: &str,
) -> Result<std::path::PathBuf, String> {
    let valid_directory = directory_name.starts_with("blink-scroll-")
        && directory_name.len() <= 96
        && directory_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if !valid_directory {
        return Err("回放目录名无效".into());
    }
    let valid_frame = file_name
        .strip_prefix("frame-")
        .and_then(|value| value.strip_suffix(".png"))
        .is_some_and(|index| index.len() == 4 && index.bytes().all(|byte| byte.is_ascii_digit()));
    if file_name != "manifest.json" && !valid_frame {
        return Err("回放文件名无效".into());
    }
    Ok(crate::infra::utils::paths::logs_dir()
        .join("scroll-replays")
        .join(directory_name)
        .join(file_name))
}

fn downsample_luma_bgra(pixels: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    if w == 0 || h == 0 {
        return Err("稳定性探针区域不能为空".into());
    }
    let expected = (w as usize)
        .checked_mul(h as usize)
        .and_then(|size| size.checked_mul(4))
        .ok_or_else(|| "稳定性探针尺寸溢出".to_string())?;
    if pixels.len() < expected {
        return Err(format!(
            "稳定性探针像素不足: expected={expected}, got={}",
            pixels.len()
        ));
    }

    let probe_w = w.min(SCROLL_PROBE_MAX_W);
    let probe_h = h.min(SCROLL_PROBE_MAX_H);
    let mut probe = Vec::with_capacity((probe_w * probe_h) as usize);
    for py in 0..probe_h {
        let source_y = ((py as u64 * h as u64) / probe_h as u64) as u32;
        for px in 0..probe_w {
            let source_x = ((px as u64 * w as u64) / probe_w as u64) as u32;
            let index = ((source_y as usize * w as usize) + source_x as usize) * 4;
            let b = pixels[index] as u32;
            let g = pixels[index + 1] as u32;
            let r = pixels[index + 2] as u32;
            probe.push(((r * 77 + g * 150 + b * 29) >> 8) as u8);
        }
    }
    Ok(probe)
}

/// 0.19.14 raw IPC 辅助：从 Request body 提取 PNG bytes。
/// 前端用 `invoke(cmd, uint8array, { headers })` 传 raw bytes，body 为 `InvokeBody::Raw`。
fn extract_png_from_request(request: &tauri::ipc::Request<'_>) -> Result<Vec<u8>, String> {
    match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) => Ok(bytes.clone()),
        tauri::ipc::InvokeBody::Json(_) => Err("expected raw bytes payload, got JSON".to_string()),
    }
}

/// 0.19.14 raw IPC 辅助：从 headers 提取 i32。
fn header_i32(headers: &tauri::http::HeaderMap, key: &str) -> Result<i32, String> {
    headers
        .get(key)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("missing header: {key}"))
}

/// 0.19.14 raw IPC 辅助：从 headers 提取 Option<bool>。
fn header_opt_bool(headers: &tauri::http::HeaderMap, key: &str) -> Option<bool> {
    headers
        .get(key)
        .and_then(|v| v.to_str().ok())
        .map(|s| s == "true")
}

/// 0.19.14 raw IPC 辅助：从 headers 提取 Option<String>。
fn header_opt_string(headers: &tauri::http::HeaderMap, key: &str) -> Option<String> {
    headers
        .get(key)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// 0.22.4 raw IPC 辅助：从 headers 提取 Option<u64>。
fn header_opt_u64(headers: &tauri::http::HeaderMap, key: &str) -> Option<u64> {
    headers
        .get(key)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

/// 0.11.7-f：接收前端合成后的 PNG（裁剪区 + 标注），写入剪贴板，结束会话。
///
/// **0.19.14 raw IPC**：前端用 `invoke("screenshot_copy", uint8array)` 传 raw bytes，
/// 避免 JSON 序列化 6MB PNG → 8MB base64 的开销（省 ~2s IPC 时间）。
///
/// **快路径**：如果只需要复制选区（无标注、无全屏合成），前端应走 `screenshot_copy_region`
/// 直接传坐标——避开前端 toBlob PNG 编码 + 后端 PNG 解码的双重开销，全屏路径
/// 快 ~150-250ms。有标注 / 全屏合成时才走本命令。
#[tauri::command]
pub async fn screenshot_copy(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    let png_data = extract_png_from_request(&request)?;
    let bytes_len = png_data.len();
    crate::domain::clipboard::write_png(
        png_data,
        crate::domain::clipboard::ClipboardWriteSource::Screenshot,
    )
    .await
    .map_err(|e| e.to_string())?;
    finish_screenshot_session(&app);
    tracing::info!(bytes = bytes_len, "截图已保存到剪贴板");
    Ok(())
}

/// P7：有标注 copy 直传 RGBA → 写剪贴板，消除 PNG 编解码往返。
///
/// 前端 `getImageData()` 产生 raw RGBA，通过 raw IPC 直传。后端原地 swap 为 BGRA
/// 后写 CF_DIB，跳过 `toBlob('image/png')`（~160ms）+ PNG decode（~289ms）。
///
/// 与 `screenshot_copy`（PNG 路径）的关系：
/// - 无标注快路径走 `screenshot_copy_region`（后端直接 crop BGRA）
/// - 有标注慢路径原本走 `screenshot_copy`（前端 toBlob PNG → 后端 decode）
/// - 有标注慢路径现在走本命令（前端 getImageData RGBA → 后端 swap）
#[tauri::command]
pub async fn screenshot_copy_rgba(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    let rgba_data = extract_png_from_request(&request)?;
    let bytes_len = rgba_data.len();
    let headers = request.headers();
    let w = header_i32(headers, "w")? as u32;
    let h = header_i32(headers, "h")? as u32;
    crate::domain::clipboard::write_rgba(
        rgba_data,
        w,
        h,
        crate::domain::clipboard::ClipboardWriteSource::Screenshot,
    )
    .await
    .map_err(|e| e.to_string())?;
    finish_screenshot_session(&app);
    tracing::info!(bytes = bytes_len, w, h, "截图 RGBA 已直传剪贴板");
    Ok(())
}

/// 0.11.7 快路径：直接从 SESSION 裁剪 BGRA → 写剪贴板，跳过 PNG 编解码往返。
///
/// **适用场景**：无标注（前端 `annot.hasAnnotations() == false`）+ 有选区。
///
/// 相比 `screenshot_copy(png_data)` 的收益：
/// - 前端省 `canvas.toBlob('image/png')`（2560x1440 ~150ms）
/// - 后端省 `image::load_from_memory` PNG 解码（~50-100ms）
/// - IPC payload 从 PNG (~几 MB) 变成 16 字节坐标
///
/// 坐标是物理像素、SESSION 坐标系（虚拟屏幕原点为 (0,0)）——前端 mouseup 时
/// 已按 DPR 转换过。裁剪越界会被 `crop()` 自身 clamp。
#[tauri::command]
pub async fn screenshot_copy_region(
    app: tauri::AppHandle,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> Result<(), String> {
    // ⚠️ 临时打桩日志（0.19.14 性能排查用），收尾时清理
    let t0 = std::time::Instant::now();
    // BGRA 裁剪与剪贴板写入都是同步操作，分别由共享语义隔离到阻塞线程池。
    let (bgra, cw, ch) = tokio::task::spawn_blocking(move || {
        crate::infra::platform::screenshot::crop(x, y, w, h)
            .ok_or_else(|| "SESSION 为空或选区越界".to_string())
    })
    .await
    .map_err(|e| format!("spawn_blocking join 失败: {e}"))??;
    let t_crop = t0.elapsed();
    crate::domain::clipboard::write_bgra(
        bgra,
        cw,
        ch,
        crate::domain::clipboard::ClipboardWriteSource::Screenshot,
    )
    .await
    .map_err(|e| e.to_string())?;
    tracing::info!(
        w = cw,
        h = ch,
        crop_ms = t_crop.as_millis() as u64,
        write_ms = (t0.elapsed() - t_crop).as_millis() as u64,
        total_ms = t0.elapsed().as_millis() as u64,
        "截图选区已直传剪贴板（快路径）"
    );
    finish_screenshot_session(&app);
    Ok(())
}

/// 0.11.7-f：取消截图，结束会话，不保存。
#[tauri::command]
pub fn screenshot_cancel(app: tauri::AppHandle) {
    finish_screenshot_session(&app);
    tracing::info!("截图已取消");
}

/// 0.11.7-f：钉图——接收前端合成后的 PNG，创建钉图窗口。
///
/// `screen_x`/`screen_y` 为选区左上角的**虚拟屏幕物理坐标**，
/// 让钉图窗口定位到截图原位（"就地贴住"）。
///
/// `show_translating`：true 时在 pin 窗口中心显示「翻译中」指示器
/// （用于「翻译并 pin」流程——先 pin 原图，后台翻译完原地替换）。
#[tauri::command]
pub fn screenshot_pin(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    let png_data = extract_png_from_request(&request)?;
    let png_len = png_data.len();
    let headers = request.headers();
    let screen_x = header_i32(headers, "screen-x")?;
    let screen_y = header_i32(headers, "screen-y")?;
    let show_translating = header_opt_bool(headers, "show-translating").unwrap_or(false);
    crate::infra::platform::window::show_pin_window(
        &app,
        crate::infra::platform::window::PinImage::Png(std::sync::Arc::new(png_data)),
        screen_x,
        screen_y,
        show_translating,
    )?;
    finish_screenshot_session(&app);
    tracing::info!(
        screen_x,
        screen_y,
        show_translating,
        png_bytes = png_len,
        "截图已钉到屏幕"
    );
    Ok(())
}

/// 0.19.14：Pin 快路径——后端直接从 SESSION 裁剪 BGRA → 编码 PNG → show_pin_window。
///
/// 与 `screenshot_copy_region` 对称的优化：无标注时前端只传坐标 + 屏幕坐标，
/// 跳过前端 toBlob + IPC PNG 往返。全屏 pin 从 ~1280ms → ~40ms。
///
/// `screen_x`/`screen_y` 为选区左上角的虚拟屏幕物理坐标。
#[tauri::command]
pub async fn screenshot_pin_region(
    app: tauri::AppHandle,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    screen_x: i32,
    screen_y: i32,
) -> Result<(), String> {
    // ⚠️ 临时打桩日志（0.19.14 性能排查用），收尾时清理
    let t0 = std::time::Instant::now();
    // P6: crop BGRA → 直接 store + show_pin，不阻塞 encode_png
    let (bgra, cw, ch) = tokio::task::spawn_blocking(move || {
        crate::infra::platform::screenshot::crop(x, y, w, h)
            .ok_or_else(|| "SESSION 为空或选区越界".to_string())
    })
    .await
    .map_err(|e| format!("spawn_blocking join 失败: {e}"))??;
    let t_crop = t0.elapsed();

    let data_len = bgra.len();
    crate::infra::platform::window::show_pin_window(
        &app,
        crate::infra::platform::window::PinImage::Bgra(std::sync::Arc::new(bgra), cw, ch),
        screen_x,
        screen_y,
        false,
    )?;
    tracing::info!(
        w,
        h,
        screen_x,
        screen_y,
        bgra_bytes = data_len,
        crop_ms = t_crop.as_millis() as u64,
        show_ms = (t0.elapsed() - t_crop).as_millis() as u64,
        total_ms = t0.elapsed().as_millis() as u64,
        "截图已钉到屏幕（快路径 P6 raw BGRA）"
    );
    finish_screenshot_session(&app);
    Ok(())
}

/// 0.18.3：原地刷新钉图窗口的图片（不重定位、不重置缩放）。
///
/// 用于「翻译并 pin」流程：先 pin 原图（`screenshot_pin` + `show_translating=true`），
/// 后台翻译完成后合成含译文的 PNG，调本命令原地替换 pin 窗口的图片。
///
/// `show_translating=false` 时同时隐藏 pin 窗口的「翻译中」指示器。
///
/// pin 窗口不存在或已 hide 时静默返回 Ok（用户已关 pin，丢弃译文）。
#[tauri::command]
pub fn screenshot_pin_refresh(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    let png_data = extract_png_from_request(&request)?;
    let show_translating = header_opt_bool(request.headers(), "show-translating").unwrap_or(false);
    crate::infra::platform::window::refresh_pin_image(&app, png_data, show_translating)?;
    tracing::info!(show_translating, "钉图已原地刷新");
    Ok(())
}

/// 0.11.7-f：保存截图选区为文件（PNG/JPEG）。
///
/// `path=None` 弹出保存对话框；用户取消时返回 Err，前端应识别 "用户取消了保存"
/// 字符串以避免噪音。
#[tauri::command]
pub async fn screenshot_save(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<String, String> {
    let png_data = extract_png_from_request(&request)?;
    let path = header_opt_string(request.headers(), "path");
    let file_path = save_editor_png(&app, &png_data, path, "截图")?;

    finish_screenshot_session(&app);
    tracing::info!(path = %file_path, "截图已保存到文件");
    Ok(file_path)
}

/// 通用图片编辑输出：以用户来源写入剪贴板，不借用截图来源标记或截图 SESSION。
#[tauri::command]
pub async fn image_editor_copy(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    let png_data = extract_png_from_request(&request)?;
    let bytes_len = png_data.len();
    crate::domain::clipboard::write_png(
        png_data,
        crate::domain::clipboard::ClipboardWriteSource::User,
    )
    .await
    .map_err(|e| e.to_string())?;
    finish_image_editor_session(&app);
    tracing::info!(bytes = bytes_len, "编辑图片已复制到剪贴板");
    Ok(())
}

#[tauri::command]
pub fn image_editor_pin(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    use crate::domain::event::CapabilityEnv;
    let png_data = extract_png_from_request(&request)?;
    let show_translating = header_opt_bool(request.headers(), "show-translating").unwrap_or(false);
    let env = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let refresh_png = show_translating.then(|| png_data.clone());
    let (screen_x, screen_y) = env.show_pin_image(png_data, None, None)?;
    if let Some(png_data) = refresh_png {
        crate::infra::platform::window::refresh_pin_image(&app, png_data, true)?;
    }
    finish_image_editor_session(&app);
    tracing::info!(screen_x, screen_y, show_translating, "编辑图片已钉到屏幕");
    Ok(())
}

/// 0.20.x：把编辑结果应用回原 pin 窗口（pin 来源编辑器的勾按钮）。
///
/// 语义（与用户确认）：
/// - 原 pin 窗口仍在 → 按 label 原地替换图片（不重定位、不重置缩放）。
/// - 原 pin 窗口已关 → 把编辑结果重新 pin 到桌面（光标所在显示器居中）。
///
/// raw IPC：body = PNG bytes；header `label` = 原 pin 窗口 label（可选，缺失时视为已关）。
#[tauri::command]
pub fn image_editor_apply_to_pin(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    use crate::domain::event::CapabilityEnv;
    let png_data = extract_png_from_request(&request)?;
    let label = header_opt_string(request.headers(), "label");

    // 原 pin 窗口仍存在且可见 → 原地替换
    let window_alive = if let Some(label) = label.as_deref()
        && app
            .get_webview_window(label)
            .map(|win| win.is_visible().unwrap_or(false))
            .unwrap_or(false)
    {
        crate::infra::platform::window::refresh_pin_image_by_label(
            &app,
            label,
            png_data.clone(),
            false,
        )?;
        tracing::info!(label = %label, "编辑图片已替换回原 pin 窗口");
        true
    } else {
        false
    };

    if !window_alive {
        let env = app
            .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
            .inner()
            .clone();
        let (screen_x, screen_y) = env.show_pin_image(png_data, None, None)?;
        tracing::info!(
            screen_x,
            screen_y,
            "编辑图片已重新 pin 到桌面（原 pin 窗口已关）"
        );
    }

    finish_image_editor_session(&app);
    Ok(())
}

#[tauri::command]
pub fn image_editor_save(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<String, String> {
    let png_data = extract_png_from_request(&request)?;
    let path = header_opt_string(request.headers(), "path");
    let file_path = save_editor_png(&app, &png_data, path, "图片")?;
    finish_image_editor_session(&app);
    tracing::info!(path = %file_path, "编辑图片已保存到文件");
    Ok(file_path)
}

#[tauri::command]
pub fn image_editor_cancel(app: tauri::AppHandle) {
    finish_image_editor_session(&app);
    tracing::info!("用户图片编辑已取消");
}

// ── 0.20.4：多来源图片编辑入口 ──────────────────────────────

/// IPC 错误类型别名（0.20.7：哨兵字符串 → 结构化 CommandError）。
type MediaError = crate::app::command_error::CommandError;

/// 0.20.4：检查单会话约束——已有活跃编辑会话时激活现有窗口并返回结构化错误。
///
/// 三个 `open_image_editor_from_*` 入口共用的前置检查。返回 `Err` 表示应
/// 提前返回（已活跃，code=invalid_state + reason=already_active），`Ok(())`
/// 表示可以继续建立新会话。
fn check_already_active(app: &tauri::AppHandle, log_ctx: &str) -> Result<(), MediaError> {
    if crate::infra::platform::image_editor::is_active() {
        if let Some(win) = app.get_webview_window("chord-screenshot") {
            let _ = win.set_focus();
        }
        tracing::info!(log_ctx, "编辑会话已活跃，激活现有窗口");
        return Err(MediaError::with_detail(
            "invalid_state",
            "图片编辑会话已活跃，已激活现有窗口",
            false,
            serde_json::json!({ "reason": "already_active" }),
        ));
    }
    Ok(())
}

/// 0.20.4：建立编辑会话并显示窗口——三个入口共用的后半段逻辑。
///
/// `begin_session` → `show_image_editor_window`；若 `show` 失败则回滚 `end_session`。
/// `source_kind` 传递给前端 `window.__blinkEditorSource.kind`。
/// `source_label`（0.20.x）仅 pin 来源传原 pin 窗口 label，供前端替换回原窗口。
fn begin_and_show(
    app: &tauri::AppHandle,
    png_data: Vec<u8>,
    source_kind: &str,
    source_label: Option<&str>,
    log_ctx: &str,
) -> Result<(), MediaError> {
    let png_len = png_data.len();
    let meta = crate::infra::platform::image_editor::begin_session(png_data).map_err(|e| {
        tracing::warn!(log_ctx, error = %e, "begin_session 失败");
        MediaError::new("internal_error", format!("建立编辑会话失败: {e}"), false)
    })?;

    if let Err(error) = crate::infra::platform::window::show_image_editor_window(
        app,
        meta,
        source_kind,
        source_label,
    ) {
        crate::infra::platform::image_editor::end_session();
        tracing::error!(log_ctx, error = %error, "显示编辑窗口失败");
        return Err(MediaError::new(
            "internal_error",
            format!("显示编辑窗口失败: {error}"),
            false,
        ));
    }

    tracing::info!(log_ctx, png_bytes = png_len, "图片编辑器已打开");
    Ok(())
}

/// 0.20.4：从当前系统剪贴板图片打开编辑器。
///
/// 读取当前剪贴板 PNG → `begin_session` → `show_image_editor_window`。
/// 剪贴板无图片时返回结构化错误。
#[tauri::command]
pub async fn open_image_editor_from_clipboard(app: tauri::AppHandle) -> Result<(), MediaError> {
    use crate::domain::clipboard::{ClipboardContent, read_current};

    check_already_active(&app, "open_image_editor_from_clipboard")?;

    let content = read_current()
        .await
        .map_err(|e| MediaError::new("internal_error", format!("读取剪贴板失败: {e}"), false))?;

    let png_data = match content {
        ClipboardContent::ImagePng(data) => data,
        ClipboardContent::Text(_) => {
            tracing::info!("open_image_editor_from_clipboard: 剪贴板无图片");
            return Err(MediaError::with_detail(
                "invalid_state",
                "剪贴板中没有图片",
                false,
                serde_json::json!({ "reason": "clipboard_no_image" }),
            ));
        }
    };

    begin_and_show(
        &app,
        png_data,
        "clipboard",
        None,
        "open_image_editor_from_clipboard",
    )
}

/// 0.20.4：从剪贴板历史图片打开编辑器。
///
/// 按 `image_id` 从数据库读取完整 PNG → `begin_session` → `show_image_editor_window`。
/// 不先覆盖系统剪贴板（§5.5 第 7 条）。
#[tauri::command]
pub async fn open_image_editor_from_history(
    app: tauri::AppHandle,
    image_id: String,
) -> Result<(), MediaError> {
    use crate::domain::clipboard::load_history_png;

    check_already_active(&app, "open_image_editor_from_history")?;

    let pools = app.state::<crate::infra::data::DbPools>();
    let png_data = load_history_png(&pools.cache, &image_id)
        .await
        .map_err(|e| {
            tracing::warn!(image_id = %image_id, error = %e, "open_image_editor_from_history: 加载历史图片失败");
            MediaError::with_detail(
                "not_found",
                format!("历史图片不存在或加载失败: {e}"),
                false,
                serde_json::json!({ "reason": "history_image", "image_id": image_id }),
            )
        })?;

    begin_and_show(
        &app,
        png_data,
        "history",
        None,
        "open_image_editor_from_history",
    )
}

/// 0.20.4：从仍显示的 pin 窗口打开编辑器。
///
/// 前端传 pin 窗口 label，后端通过 `get_pin_image_by_label` 查找图片。
/// PinImage 可能是 PNG 或 BGRA：BGRA 在 `spawn_blocking` 中编码为 PNG。
/// 不先覆盖系统剪贴板（§5.5 第 7 条）。
#[tauri::command]
pub async fn open_image_editor_from_pin(
    app: tauri::AppHandle,
    window_label: String,
) -> Result<(), MediaError> {
    use crate::infra::platform::window::{PinImage, get_pin_image_by_label};

    check_already_active(&app, "open_image_editor_from_pin")?;

    // 从 pin registry 获取图片
    let pin_image = get_pin_image_by_label(&window_label).ok_or_else(|| {
        tracing::debug!(window_label = %window_label, "open_image_editor_from_pin: pin 图片不存在或窗口已关闭");
        MediaError::with_detail(
            "not_found",
            "Pin 图片不存在或窗口已关闭",
            false,
            serde_json::json!({ "reason": "pin_image", "window_label": window_label }),
        )
    })?;

    // BGRA 需要 spawn_blocking 编码为 PNG；PNG 直接使用
    let png_data = match pin_image {
        PinImage::Png(arc) => (*arc).clone(),
        PinImage::Bgra(arc, w, h) => {
            let bgra = (*arc).clone();
            let t_encode = std::time::Instant::now();
            let png = tokio::task::spawn_blocking(move || {
                crate::infra::platform::screenshot::encode_png(&bgra, w, h)
            })
            .await
            .map_err(|e| {
                MediaError::new(
                    "internal_error",
                    format!("spawn_blocking join 失败: {e}"),
                    false,
                )
            })?
            .map_err(|e| {
                MediaError::new("internal_error", format!("BGRA 编码 PNG 失败: {e}"), false)
            })?;
            tracing::debug!(
                window_label = %window_label,
                w,
                h,
                elapsed_ms = t_encode.elapsed().as_millis() as u64,
                "open_image_editor_from_pin: BGRA→PNG 编码完成"
            );
            png
        }
    };

    // 0.20.x：把原 pin 窗口 label 贯通到编辑器前端，供「勾按钮替换回原窗口」按 label 定位。
    begin_and_show(
        &app,
        png_data,
        "pin",
        Some(&window_label),
        "open_image_editor_from_pin",
    )
}

fn save_editor_png(
    app: &tauri::AppHandle,
    png_data: &[u8],
    path: Option<String>,
    default_prefix: &str,
) -> Result<String, String> {
    use std::io::Write;
    let file_path = match path {
        Some(path) => path,
        None => {
            let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
            let default_name = format!("{default_prefix}_{timestamp}.png");
            match app
                .dialog()
                .file()
                .add_filter("PNG 图片", &["png"])
                .add_filter("JPEG 图片", &["jpg", "jpeg"])
                .set_file_name(&default_name)
                .blocking_save_file()
            {
                Some(path) => path.to_string(),
                None => return Err("用户取消了保存".to_string()),
            }
        }
    };
    let mut file = std::fs::File::create(&file_path).map_err(|e| format!("创建文件失败: {e}"))?;
    file.write_all(png_data)
        .map_err(|e| format!("写入文件失败: {e}"))?;
    Ok(file_path)
}

/// 把显式开启的长截图诊断回放写入 `%APPDATA%\blink\logs\scroll-replays`。
///
/// 目录名和文件名只接受固定格式，前端不能借此写入日志目录之外的任意路径。
/// manifest 最后写入；因此只有包含 manifest 的目录才是一段完整回放。
#[tauri::command]
pub async fn screenshot_save_replay_file(
    directory_name: String,
    file_name: String,
    data: Vec<u8>,
) -> Result<String, String> {
    if data.is_empty() || data.len() > SCROLL_REPLAY_MAX_FILE_BYTES {
        return Err(format!(
            "回放文件大小无效: bytes={}, max={SCROLL_REPLAY_MAX_FILE_BYTES}",
            data.len()
        ));
    }
    let path = scroll_replay_export_path(&directory_name, &file_name)?;
    let directory = path
        .parent()
        .ok_or_else(|| "无法解析回放目录".to_string())?
        .to_path_buf();
    let result_directory = directory.clone();
    let is_manifest = file_name == "manifest.json";
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        std::fs::create_dir_all(&directory).map_err(|e| format!("创建回放目录失败: {e}"))?;
        std::fs::write(&path, data).map_err(|e| format!("写入回放文件失败: {e}"))
    })
    .await
    .map_err(|e| format!("spawn_blocking join 失败: {e}"))??;
    if is_manifest {
        tracing::info!(directory = %result_directory.display(), "长截图诊断回放已导出");
    }
    Ok(result_directory.to_string_lossy().into_owned())
}

/// 隐藏钉图窗口（多 Pin：通过 label 定位目标窗口，触发 CloseRequested → 回收/销毁）。
#[tauri::command]
pub fn screenshot_pin_hide(app: tauri::AppHandle, label: String) {
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.close();
    }
}

/// 0.11.8：钉图窗口一次性设置位置 + 尺寸（缩放/拖动/onload 跟随共用）。
///
/// 走 Win32 `SetWindowPos` 原子地设位置+尺寸，绕开 Tauri 逻辑像素 DPI 竞态。
/// 参数均为**屏幕物理像素**：
/// - `win_x`/`win_y`：窗口左上角屏幕坐标（= 图片左上 - PIN_PAD）
/// - `win_w`/`win_h`：窗口尺寸（= 图片显示尺寸 + 2×PIN_PAD，含发光区）
///
/// 多 Pin：通过 `label` 定位目标窗口。
#[tauri::command]
pub fn screenshot_pin_transform(
    app: tauri::AppHandle,
    label: String,
    win_x: i32,
    win_y: i32,
    win_w: u32,
    win_h: u32,
) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    if let Some(win) = app.get_webview_window(&label)
        && let Ok(hwnd) = win.hwnd()
    {
        crate::infra::platform::window::place_at_physical(
            HWND(hwnd.0 as _),
            win_x,
            win_y,
            win_w,
            win_h,
        );
    }
    Ok(())
}

/// 0.19.4：钉图窗口仅移动位置（拖拽专用，零 resize 开销）。
///
/// 与 `screenshot_pin_transform` 的区别：用 `SWP_NOSIZE | SWP_NOZORDER` 跳过
/// 尺寸计算和 Z 序调整，只改窗口位置。拖拽时窗口尺寸不变，无需每次都传
/// winW/winH 让 Win32 做无用的 resize 判定。减少每帧 IPC 的后端开销。
///
/// 多 Pin：通过 `label` 定位目标窗口。
#[tauri::command]
pub fn screenshot_pin_move(
    app: tauri::AppHandle,
    label: String,
    win_x: i32,
    win_y: i32,
) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SET_WINDOW_POS_FLAGS, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    };
    if let Some(win) = app.get_webview_window(&label)
        && let Ok(hwnd) = win.hwnd()
    {
        unsafe {
            let _ = SetWindowPos(
                HWND(hwnd.0 as _),
                None,
                win_x,
                win_y,
                0,
                0,
                SET_WINDOW_POS_FLAGS(SWP_NOSIZE.0 | SWP_NOZORDER.0 | SWP_NOACTIVATE.0),
            );
        }
    }
    Ok(())
}

/// 多 Pin N+1：前端 preheat init 完成后调用，将 spare 注册为可用。
#[tauri::command]
pub async fn pin_spare_ready(window: tauri::Window) {
    let label = window.label().to_string();
    crate::infra::platform::window::mark_pin_spare_ready(&label);
}

/// 获取 Pin 窗口的当前物理矩形和目标屏 DPR。
///
/// 用于 DPI reconcile：`onScaleChanged` 或拖动跨 DPI 边界后，
/// 前端调用此命令回读窗口实际物理位置，再用 `pin-geometry.js::reconcileDpi` 重算状态。
///
/// 返回 `{ x, y, w, h, dpr }`，窗口不存在时返回 null。
#[tauri::command]
pub fn screenshot_pin_get_rect(app: tauri::AppHandle, label: String) -> Option<serde_json::Value> {
    crate::infra::platform::window::get_pin_window_rect(&app, &label)
        .map(|(x, y, w, h, dpr)| serde_json::json!({ "x": x, "y": y, "w": w, "h": h, "dpr": dpr }))
}

/// 将 pin 窗口图片复制到剪贴板。
#[tauri::command]
pub async fn pin_save_clipboard(request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let png_data = extract_png_from_request(&request)?;
    crate::domain::clipboard::write_png(
        png_data,
        crate::domain::clipboard::ClipboardWriteSource::User,
    )
    .await
    .map_err(|e| e.to_string())?;
    tracing::info!("pin 图已复制到剪贴板");
    Ok(())
}

/// 将 pin 窗口图片另存为文件，同时复制到剪贴板方便流转。
///
/// `path=None` 弹出保存对话框；用户取消时返回 Err。
#[tauri::command]
pub async fn pin_save_as(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<String, String> {
    let png_data = extract_png_from_request(&request)?;
    let path = header_opt_string(request.headers(), "path");
    // 先写剪贴板（方便流转）
    let png_clone = png_data.clone();
    crate::domain::clipboard::write_png(
        png_clone,
        crate::domain::clipboard::ClipboardWriteSource::User,
    )
    .await
    .map_err(|e| e.to_string())?;
    // 再保存文件
    let file_path = save_editor_png(&app, &png_data, path, "钉图")?;
    tracing::info!(path = %file_path, "pin 图已另存为文件并复制到剪贴板");
    Ok(file_path)
}

/// 0.11.7-c：OCR 识别图片中的文字，返回 `{text, lines}`。
///
/// 0.11.7-f：改走 `ocr_engine::backend()` 注入的后端（测试可替换）。
///
/// **0.14.7 W3**：返回 `CommandError`（结构化错误协议）。
///
/// **0.19.1**：用户侧 command 改经 `CapabilityRegistry` 调 `OcrImage` Capability，
/// 与 AI 走同一个入口（照搬 `translate_text` 模式）。底层 `ocr_engine::backend()`
/// 不动，OcrImage Capability 仍调它。消除双入口行为漂移。
const OCR_CAPABILITY_ID: &str = "ocr_image";

fn project_ocr_command_result(
    result: crate::domain::capability::CapabilityResult,
) -> Result<serde_json::Value, crate::app::command_error::CommandError> {
    use crate::app::command_error::CommandError;

    match result {
        crate::domain::capability::CapabilityResult::Text { content, .. } => {
            serde_json::from_str(&content).map_err(|error| {
                CommandError::new(
                    "internal_error",
                    format!("解析 OCR 结果失败: {error}"),
                    false,
                )
            })
        }
        other => {
            tracing::warn!(?other, "ocr_image: OcrImage Capability 返回意外的结果类型");
            Err(CommandError::new(
                "internal_error",
                "OCR 返回意外的结果类型",
                false,
            ))
        }
    }
}

/// Task 10: RAII cleanup guard for ImageStash refs.
///
/// `ocr_image` 创建 stash ref 后，无论成功/失败/取消，都需显式删除 ref
/// 避免占用 stash 项数上限（16 项）。Drop 时调用 cleanup closure。
struct ImageStashCleanupGuard<F: FnOnce()> {
    cleanup: Option<F>,
}

impl<F: FnOnce()> ImageStashCleanupGuard<F> {
    fn new(cleanup: F) -> Self {
        Self {
            cleanup: Some(cleanup),
        }
    }
}

impl<F: FnOnce()> Drop for ImageStashCleanupGuard<F> {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

#[tauri::command]
pub async fn ocr_image(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<serde_json::Value, crate::app::command_error::CommandError> {
    use crate::app::command_error::CommandError;

    let png_data = extract_png_from_request(&request)
        .map_err(|e| CommandError::new("invalid_args", e, false))?;
    let bytes_len = png_data.len();

    let registry = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    if registry.get(OCR_CAPABILITY_ID).is_none() {
        tracing::warn!("ocr_image: OcrImage Capability 未注册");
        return Err(CommandError::new("not_found", "OCR 能力未注册", false));
    }

    // Task 10: 消除 PNG JSON 数字数组和多份图片复制
    // 旧方式：serde_json::json!({ "png": png_data }) → Vec<u8> 被序列化为 JSON 数字数组
    // 新方式：存入 ImageStash（以 Bytes 零拷贝），传 image_ref 给 Capability
    //
    // 铁则：
    // - png_data 只在此处出现一次，不 .clone()
    // - 无 stash 环境（CLI/MCP）也不回退 JSON 数组——直接返回错误
    // - stash.put 失败（超过 32MiB）也不回退 JSON 数组——直接返回错误
    // - 请求结束后显式删除 stash ref

    // 0.22.4：从 headers 提取截图 session 信息（可选）
    let screenshot_session = header_opt_u64(request.headers(), "X-Screenshot-Session");
    let screenshot_revision = header_opt_u64(request.headers(), "X-Screenshot-Revision");
    // 0.22.4：从 headers 提取 request_id（前端生成，用于 cancel tracker）
    let request_id_header = header_opt_string(request.headers(), "X-Request-Id");

    // 构造 InvokeContext（确定性调用，无超时）
    let env_arc = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();

    // Task 10: 通过 ImageStash 传 image_ref，避免 PNG 字节被 JSON 数字数组序列化
    // Bytes::from(Vec) 消费 Vec 不复制；stash.put 接收 Bytes，不额外复制
    let png_bytes = bytes::Bytes::from(png_data);
    let image_ref = {
        use crate::domain::event::CapabilityEnv;
        let stash = env_arc.image_stash().ok_or_else(|| {
            CommandError::new("internal_error", "ImageStash 不可用（运行时未启用）", false)
        })?;
        stash.put(png_bytes, "image/png".into()).ok_or_else(|| {
            CommandError::new("invalid_args", "图片过大（超过 32MiB 上限）或为空", false)
        })?
    };

    // Task 10: RAII cleanup guard——请求结束后显式删除 stash ref
    // 无论成功/失败/取消，都删除临时 stash ref 避免占用上限
    let stash_for_cleanup = env_arc.clone();
    let image_ref_for_cleanup = image_ref.clone();
    let _cleanup_guard = ImageStashCleanupGuard::new(move || {
        use crate::domain::event::CapabilityEnv;
        if let Some(stash) = stash_for_cleanup.image_stash() {
            stash.remove(&image_ref_for_cleanup);
        }
    });

    let mut arguments = serde_json::json!({});
    arguments["image_ref"] = serde_json::Value::from(image_ref);
    if let (Some(epoch), Some(rev)) = (screenshot_session, screenshot_revision) {
        arguments["screenshot_session"] = serde_json::Value::from(epoch);
        arguments["screenshot_revision"] = serde_json::Value::from(rev);
    }
    if let Some(ref rid) = request_id_header {
        arguments["request_id"] = serde_json::Value::from(rid.clone());
    }

    let ctx = crate::domain::capability::InvokeContext {
        env: env_arc.as_ref(),
        origin: crate::domain::capability::InvocationOrigin::LocalCommand,
        runtime: crate::domain::capability::RuntimeCapabilities {
            surface: None,
            main_process: true,
            desktop_session: true,
        },
        deadline: None,
    };

    tracing::debug!(bytes = bytes_len, "ocr_image: 调 OcrImage Capability");

    let result = registry
        .invoke(OCR_CAPABILITY_ID, arguments, &ctx)
        .await
        .map_err(CommandError::from)?;

    // OcrImage Capability 返回 Text{ content = OcrResult 的 JSON 序列化字符串 }。
    let json = project_ocr_command_result(result)?;
    let text_len = json
        .get("text")
        .and_then(|value| value.as_str())
        .map(str::len)
        .unwrap_or(0);
    tracing::debug!(text_len, "OCR 识别完成");
    Ok(json)
}

/// 0.22.4：取消在途的 OCR 请求。
///
/// 前端在选区变化或用户取消时调用，通过 `request_id` 取消后端正在处理的 OCR 请求。
/// 返回 `true` 表示成功取消，`false` 表示请求已完成或不存在。
#[tauri::command]
pub async fn cancel_ocr_request(
    request_id: String,
) -> Result<bool, crate::app::command_error::CommandError> {
    let tracker = crate::domain::ocr::context::ocr_request_tracker();
    let cancelled = tracker.cancel(&request_id);
    tracing::info!(request_id = %request_id, cancelled, "cancel_ocr_request");
    Ok(cancelled)
}

/// 0.20.7：分析截图选区或图片编辑会话的配色方案。
///
/// **两种来源**（P0-1 修订：直调配色核心，不走 Capability JSON 像素搬运）：
/// - `source = "screenshot"`：从截图 SESSION 按物理坐标裁剪 BGRA → swap 为 RGBA →
///   直接调 `palette::analyze_palette`。坐标是物理像素、SESSION 坐标系（虚拟屏幕原点为
///   (0,0)）——与 `screenshot_copy_region` 相同的坐标系。
/// - `source = "editor"`：从图片编辑会话 SESSION 取原始 PNG → 解码为 RGBA →
///   按可选选区裁剪 → 直接调 `palette::analyze_palette`。
///
/// **零回传**：前端只传坐标（screenshot）或选区坐标（editor），不传 Canvas RGBA/PNG/Base64。
///
/// **零 JSON 像素搬运**：不再构造 `serde_json::json!({"rgba_flat": ...})`，
/// 避免 4K 选区每字节变 `serde_json::Value::Number`（~24-32B/byte）的内存炸弹。
///
/// **长截图禁用**：长截图来源在前后端均拒绝配色提取。
///
/// `algo` 可选聚类算法（`kmeans` / `mean_shift` / `mmcq`），缺省或未知值回退 kmeans。
///
/// 返回 `PaletteResult` 的 JSON 序列化（直连 Rust 核心，不经 Capability Text 反序列化）。
#[tauri::command]
pub async fn analyze_palette(
    source: String,
    x: Option<i32>,
    y: Option<i32>,
    w: Option<u32>,
    h: Option<u32>,
    crop: Option<CropRect>,
    algo: Option<String>,
) -> Result<serde_json::Value, crate::app::command_error::CommandError> {
    use crate::app::command_error::CommandError;

    let algo = match algo.as_deref() {
        Some("mean_shift") => crate::domain::palette::ClusteringAlgo::MeanShift,
        Some("mmcq") => crate::domain::palette::ClusteringAlgo::Mmcq,
        _ => crate::domain::palette::ClusteringAlgo::Kmeans,
    };
    tracing::debug!(?algo, "analyze_palette: 聚类算法");

    // P1-4：长截图来源拒绝配色提取
    if source == "long-screenshot" {
        return Err(CommandError::with_detail(
            "invalid_state",
            "长截图不支持配色提取",
            false,
            serde_json::json!({ "reason": "long_screenshot_disabled" }),
        ));
    }

    let (rgba_flat, width, height) = match source.as_str() {
        "screenshot" => {
            let x = x.ok_or_else(|| {
                CommandError::new("invalid_args", "screenshot 来源需要 x 参数", false)
            })?;
            let y = y.ok_or_else(|| {
                CommandError::new("invalid_args", "screenshot 来源需要 y 参数", false)
            })?;
            let w = w.ok_or_else(|| {
                CommandError::new("invalid_args", "screenshot 来源需要 w 参数", false)
            })?;
            let h = h.ok_or_else(|| {
                CommandError::new("invalid_args", "screenshot 来源需要 h 参数", false)
            })?;

            // 从截图 SESSION 裁剪 BGRA → swap 为 RGBA（spawn_blocking 隔离 CPU）
            tokio::task::spawn_blocking(move || {
                let (bgra, cw, ch) = crate::infra::platform::screenshot::crop(x, y, w, h)
                    .ok_or_else(|| "SESSION 为空或选区越界".to_string())?;
                // BGRA → RGBA（u32 位运算批量 swap R↔B）
                let mut rgba = bgra;
                for chunk in rgba.chunks_exact_mut(4) {
                    let px = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    let rb = px & 0x00FF00FF;
                    let ga = px & 0xFF00FF00;
                    let swapped = ga | (rb << 16) | (rb >> 16);
                    chunk.copy_from_slice(&swapped.to_ne_bytes());
                }
                Ok::<(Vec<u8>, usize, usize), String>((rgba, cw as usize, ch as usize))
            })
            .await
            .map_err(|e| {
                CommandError::new(
                    "internal_error",
                    format!("spawn_blocking join 失败: {e}"),
                    false,
                )
            })?
            .map_err(|e| CommandError::new("invalid_state", e, false))?
        }
        "editor" => {
            // 从图片编辑会话 SESSION 取原始 PNG
            let png_bytes =
                crate::infra::platform::image_editor::session_png().ok_or_else(|| {
                    tracing::warn!("analyze_palette: 图片编辑会话不活跃");
                    CommandError::with_detail(
                        "invalid_state",
                        "图片编辑会话不活跃",
                        false,
                        serde_json::json!({ "reason": "editor_session_inactive" }),
                    )
                })?;
            let png_bytes = (*png_bytes).clone();

            // 解码 PNG → RGBA（spawn_blocking 隔离 CPU）
            let (rgba, img_w, img_h) = tokio::task::spawn_blocking(move || {
                crate::infra::platform::screenshot::decode_png_to_rgba(&png_bytes)
            })
            .await
            .map_err(|e| {
                CommandError::new(
                    "internal_error",
                    format!("spawn_blocking join 失败: {e}"),
                    false,
                )
            })?
            .map_err(|e| CommandError::new("invalid_data", format!("PNG 解码失败: {e}"), false))?;

            // P1-4：按可选选区裁剪
            if let Some(crop) = crop {
                let (cropped, cw, ch) = crop_rgba(&rgba, img_w as usize, img_h as usize, &crop);
                (cropped, cw, ch)
            } else {
                (rgba, img_w as usize, img_h as usize)
            }
        }
        other => {
            return Err(CommandError::new(
                "invalid_args",
                format!("不支持的 source: {other}（仅支持 screenshot / editor）"),
                false,
            ));
        }
    };

    // P0-1：直调配色核心，不经 JSON 像素数组搬运
    let result = tokio::task::spawn_blocking(move || {
        crate::domain::palette::analyze_palette_with(&rgba_flat, width, height, algo)
    })
    .await
    .map_err(|e| CommandError::new("internal_error", format!("配色分析 task 崩溃: {e}"), false))?;

    // 直连 Rust 核心，序列化 PaletteResult 返回前端
    let json = serde_json::to_value(&result).map_err(|e| {
        CommandError::new(
            "internal_error",
            format!("序列化 PaletteResult 失败: {e}"),
            false,
        )
    })?;

    tracing::debug!(
        source = %source,
        roles = result.roles.len(),
        empty = result.empty,
        "analyze_palette: 分析完成"
    );
    Ok(json)
}

/// P1-4：editor 选区裁剪参数。
#[derive(serde::Deserialize)]
pub struct CropRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// 0.20.7 P1-1：基于显式基准色生成配色方案（OKLCH 数学方案接线）。
///
/// 前端"生成当前色配色方案"展开时调用此命令，由后端 OKLCH 单一真源生成：
/// - 同色层级（monochrome）：基准色 OKLCH 明度梯度
/// - 邻近协调（analogous）：基准色 ±30° 色相
/// - 互补强调（complement）：基准色 + 互补色 + 原图灰阶
///
/// **删除前端 HSL 双算法**：前端不再自己用 HSL 生成方案，统一走后端 OKLCH。
///
/// 参数：
/// - `anchor_hex`：基准色 HEX 字符串（如 "#FF5500"）
/// - `source_colors`：原图角色色 RGB 数组（可为空）
///
/// 返回 `Vec<HarmonyScheme>` 的 JSON 序列化。
#[tauri::command]
pub async fn generate_palette_schemes(
    anchor_hex: String,
    source_colors: Option<Vec<[u8; 3]>>,
) -> Result<serde_json::Value, crate::app::command_error::CommandError> {
    use crate::app::command_error::CommandError;

    // 解析 HEX → RGB
    let hex = anchor_hex.trim_start_matches('#');
    if hex.len() != 6 {
        return Err(CommandError::new(
            "invalid_args",
            format!("anchor_hex 格式无效: {anchor_hex}（需要 #RRGGBB）"),
            false,
        ));
    }
    let r = u8::from_str_radix(&hex[0..2], 16).map_err(|e| {
        CommandError::new("invalid_args", format!("anchor_hex R 解析失败: {e}"), false)
    })?;
    let g = u8::from_str_radix(&hex[2..4], 16).map_err(|e| {
        CommandError::new("invalid_args", format!("anchor_hex G 解析失败: {e}"), false)
    })?;
    let b = u8::from_str_radix(&hex[4..6], 16).map_err(|e| {
        CommandError::new("invalid_args", format!("anchor_hex B 解析失败: {e}"), false)
    })?;

    let source: Vec<[u8; 3]> = source_colors.unwrap_or_default();

    // 调后端 OKLCH 方案生成（纯 CPU，spawn_blocking 隔离）
    let schemes = tokio::task::spawn_blocking(move || {
        crate::domain::palette::generate_design_palettes([r, g, b], &source)
    })
    .await
    .map_err(|e| {
        CommandError::new(
            "internal_error",
            format!("配色方案生成 task 崩溃: {e}"),
            false,
        )
    })?;

    let json = serde_json::to_value(&schemes).map_err(|e| {
        CommandError::new(
            "internal_error",
            format!("序列化 HarmonyScheme 失败: {e}"),
            false,
        )
    })?;

    tracing::debug!(
        anchor = %anchor_hex,
        schemes = schemes.len(),
        "generate_palette_schemes: 生成完成"
    );
    Ok(json)
}

/// 对 RGBA flat 数据执行裁剪（P1-4：复用 analyze_image_palette 的 apply_crop 逻辑）。
fn crop_rgba(rgba: &[u8], width: usize, height: usize, crop: &CropRect) -> (Vec<u8>, usize, usize) {
    let x = (crop.x.max(0) as usize).min(width);
    let y = (crop.y.max(0) as usize).min(height);
    let cw = (crop.w as usize).min(width.saturating_sub(x)).max(1);
    let ch = (crop.h as usize).min(height.saturating_sub(y)).max(1);

    let stride = width * 4;
    let crop_stride = cw * 4;
    let mut result = Vec::with_capacity(crop_stride * ch);

    for row in y..(y + ch) {
        let start = row * stride + x * 4;
        let end = start + crop_stride;
        result.extend_from_slice(&rgba[start..end]);
    }

    (result, cw, ch)
}

/// 0.17.5：OCR 诊断——返回设备已安装的 OCR 语言列表、当前引擎语言、中文包状态。
///
/// 供截图 overlay 诊断面板调用，帮助用户排查"中文截图识别不出"问题。
///
/// **0.19.1**：本命令保留为诊断专用直调 `ocr_engine::backend()`——`available_languages()`
/// 和 `engine_language()` 是 `OcrBackend` trait 的诊断方法，不在 OcrImage Capability
/// （只做 `recognize`）的职责范围内。为诊断面板单独建 Capability 属过度工程。
///
/// **0.22.4**：优先使用 `OcrBackendRouter::diagnose()` 获取路由诊断快照
/// （包含 PaddleOCR 状态、in-flight、fallback 等）。如果未安装 router，
/// 回退到直接调用 `backend()`。
#[tauri::command]
pub async fn ocr_diagnose(
    _app: tauri::AppHandle,
) -> Result<serde_json::Value, crate::app::command_error::CommandError> {
    // 0.22.4：优先使用 OcrBackendRouter 诊断
    if let Some(router) = crate::domain::ocr::router::router() {
        let diag = router.diagnose().await;
        return Ok(serde_json::to_value(&diag).unwrap_or_else(|_| {
            serde_json::json!({
                "configured_backend": format!("{:?}", diag.configured_backend),
                "error": "diagnostics serialization failed"
            })
        }));
    }

    // 回退：直接调用 WinRT backend 诊断
    let backend = crate::domain::capability::builtins::ocr_engine::backend();
    let available = backend.available_languages().await;
    let engine_lang = backend.engine_language().await;
    let has_chinese = available.iter().any(|tag| tag.starts_with("zh"));

    Ok(serde_json::json!({
        "available_languages": available,
        "engine_language": engine_lang,
        "has_chinese": has_chinese,
    }))
}

/// 0.22.4：读取 OCR 配置。
#[tauri::command]
pub async fn get_ocr_config(
    _app: tauri::AppHandle,
) -> Result<crate::domain::config::ocr_config::OcrConfig, String> {
    Ok(crate::domain::config::ocr_config::get_ocr_config())
}

/// 0.22.4：保存 OCR 配置。
#[tauri::command]
pub async fn set_ocr_config(
    app: tauri::AppHandle,
    config: crate::domain::config::ocr_config::OcrConfig,
) -> Result<(), String> {
    // 校验
    config
        .validate()
        .map_err(|e| format!("OCR 配置校验失败: {e}"))?;

    // 保存到 SQLite ConfigStore
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::domain::config::store::ConfigStore::set(pool, &config)
        .await
        .map_err(|e| format!("保存 OCR 配置失败: {e}"))?;

    // 更新内存缓存
    crate::domain::config::ocr_config::update_cache(&config);

    // 广播配置变更事件
    let _ = app.emit(
        crate::domain::event_names::EventNames::CONFIG_CHANGED,
        serde_json::json!({ "scope": "ocr" }),
    );

    tracing::info!(
        backend = %config.backend,
        paddle_model = %config.paddle_model,
        "OCR 配置已保存"
    );
    Ok(())
}

/// 0.11.9-d：翻译文本命令——OCR 面板/工具栏"翻译"按钮的后端入口。
///
/// **绕过 AI 路径**：翻译是确定性动作(用户主动点按钮),不该经过 AI 意图判断
/// + 网络往返。直接走 `CapabilityRegistry` 找 translate 插件的 `translate` tool
/// (id = `builtin_translate_translate`),调 `invoke()` 拿 `CapabilityResult::Items`，
/// 读 `items[0].data.translated` 即译文。
///
/// **0.13.7 迁移**：从 ActionRegistry 迁到 CapabilityRegistry（插件体系收敛）。
///
/// **参数**：
/// - `text`: 待翻译文本（必填）
/// - `target_lang`: 目标语言代码(zh/en/ja/ko);`None` 时插件读 setting 默认值
///
/// **失败模式**：
/// - 插件未启用 / manifest 未加载 → 返 `not_found` code
/// - 插件返回空/错误 → 传递原错误信息
/// - 插件返回非 Items 结果 → `internal_error` code (理论不会,防御)
///
/// **0.14.7 W3**：返回 `CommandError`（结构化错误协议）。
#[tauri::command]
pub async fn translate_text(
    app: tauri::AppHandle,
    text: String,
    target_lang: Option<String>,
) -> Result<String, crate::app::command_error::CommandError> {
    use crate::app::command_error::CommandError;

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(CommandError::new("invalid_args", "翻译文本不能为空", false));
    }

    let registry = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    // translate 插件的 tool 注册 id = plugin_tool_id("builtin.translate", "translate")
    const TRANSLATE_CAPABILITY_ID: &str = "builtin_translate_translate";
    if registry.get(TRANSLATE_CAPABILITY_ID).is_none() {
        tracing::warn!("translate_text: 翻译插件未注册");
        return Err(CommandError::new(
            "not_found",
            "翻译插件未安装或未启用",
            false,
        ));
    }

    // 构造插件 tool arguments —— text 必填,target_lang 有值才传(None 让插件读 setting)
    let mut args = serde_json::Map::new();
    args.insert(
        "text".into(),
        serde_json::Value::String(trimmed.to_string()),
    );
    if let Some(lang) = target_lang
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        args.insert(
            "target_lang".into(),
            serde_json::Value::String(lang.to_string()),
        );
    }
    let arguments = serde_json::Value::Object(args);

    tracing::debug!(
        text_len = trimmed.chars().count(),
        ?target_lang,
        "translate_text: 调翻译插件"
    );

    // 构造 InvokeContext（确定性调用，无超时——翻译插件内部已有 manifest timeout_ms）
    let env_arc = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let ctx = crate::domain::capability::InvokeContext {
        env: env_arc.as_ref(),
        origin: crate::domain::capability::InvocationOrigin::LocalCommand,
        runtime: crate::domain::capability::RuntimeCapabilities {
            surface: None,
            main_process: true,
            desktop_session: true,
        },
        deadline: None,
    };
    let result = registry
        .invoke(TRANSLATE_CAPABILITY_ID, arguments, &ctx)
        .await
        .map_err(CommandError::from)?;

    match result {
        crate::domain::capability::CapabilityResult::Items { items } => {
            // 0.14: 优先读 data.translated（干净译文）
            let translated = items
                .first()
                .and_then(|it| {
                    it.data
                        .get("translated")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CommandError::new("internal_error", "翻译插件返回空结果", false))?;
            tracing::info!(
                src_len = trimmed.chars().count(),
                dst_len = translated.chars().count(),
                "translate_text 完成"
            );
            Ok(translated)
        }
        crate::domain::capability::CapabilityResult::Text { content, .. } => {
            // 兼容:如果插件未来改走 Text 结果,也取到译文
            Ok(content)
        }
        other => {
            tracing::warn!(?other, "translate_text: 翻译插件返回意外的结果");
            Err(CommandError::new(
                "internal_error",
                "翻译插件返回意外的结果类型",
                false,
            ))
        }
    }
}

/// 0.11.10-g:批量翻译多行文本。
///
/// 首选一次调用插件 `translate_batch` tool，由插件加 tag 后单次请求翻译引擎并保序拆回。
/// 插件版本不匹配、tag 被引擎破坏或结构化结果异常时，降级为并发单行 `translate_text`，
/// 保证截图翻译功能不因批量优化失败而不可用。
///
/// **0.14.7 W3**：返回 `CommandError`（结构化错误协议）。
#[tauri::command]
pub async fn translate_lines(
    app: tauri::AppHandle,
    lines: Vec<String>,
    target_lang: Option<String>,
) -> Result<Vec<String>, crate::app::command_error::CommandError> {
    if lines.is_empty() {
        return Ok(Vec::new());
    }
    let n = lines.len();
    tracing::debug!(count = n, ?target_lang, "translate_lines: 批量翻译开始");
    let started = std::time::Instant::now();

    // 空行不送插件，保留原索引；插件契约只接收非空文本。
    let non_empty: Vec<(usize, String)> = lines
        .iter()
        .enumerate()
        .filter(|(_, text)| !text.trim().is_empty())
        .map(|(idx, text)| (idx, text.clone()))
        .collect();
    if non_empty.is_empty() {
        return Ok(lines);
    }

    const TRANSLATE_BATCH_CAPABILITY_ID: &str = "builtin_translate_translate_batch";
    let registry = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    if registry.get(TRANSLATE_BATCH_CAPABILITY_ID).is_some() {
        let texts: Vec<String> = non_empty.iter().map(|(_, text)| text.clone()).collect();
        let mut args = serde_json::Map::new();
        args.insert("texts".into(), serde_json::json!(texts));
        if let Some(lang) = target_lang
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            args.insert(
                "target_lang".into(),
                serde_json::Value::String(lang.to_string()),
            );
        }
        let env_arc = app
            .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
            .inner()
            .clone();
        let ctx = crate::domain::capability::InvokeContext {
            env: env_arc.as_ref(),
            origin: crate::domain::capability::InvocationOrigin::LocalCommand,
            runtime: crate::domain::capability::RuntimeCapabilities {
                surface: None,
                main_process: true,
                desktop_session: true,
            },
            deadline: None,
        };
        match registry
            .invoke(
                TRANSLATE_BATCH_CAPABILITY_ID,
                serde_json::Value::Object(args),
                &ctx,
            )
            .await
        {
            Ok(result) => {
                if let Some(batch_results) = parse_translate_batch_payload(&result, non_empty.len())
                {
                    let mut results = lines.clone();
                    for ((idx, _), translated) in non_empty.iter().zip(batch_results) {
                        results[*idx] = translated;
                    }
                    tracing::info!(
                        count = n,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "translate_lines 完成（单次批量 tool）"
                    );
                    return Ok(results);
                }
                tracing::warn!("translate_lines: 批量 tool 返回结构异常，降级为单行并发");
            }
            Err(e) => {
                tracing::warn!(error = %e, "translate_lines: 批量 tool 失败，降级为单行并发");
            }
        }
    } else {
        tracing::warn!("translate_lines: translate_batch 未注册，降级为单行并发");
    }

    let mut handles = Vec::with_capacity(non_empty.len());
    for (idx, text) in non_empty {
        let app_clone = app.clone();
        let tl = target_lang.clone();
        let src_for_fallback = text.clone();
        handles.push(tokio::spawn(async move {
            let result = translate_text(app_clone, text, tl).await;
            match result {
                Ok(dst) => (idx, dst),
                Err(e) => {
                    tracing::warn!(line = idx, error = %e, "translate_lines: 单行翻译失败，降级到原文");
                    (idx, src_for_fallback)
                }
            }
        }));
    }

    let mut results = lines;
    for handle in handles {
        match handle.await {
            Ok((idx, dst)) => results[idx] = dst,
            Err(e) => tracing::warn!(error = %e, "translate_lines: 任务 join 失败"),
        }
    }
    tracing::info!(
        count = n,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "translate_lines 完成（单行并发降级）"
    );
    Ok(results)
}

/// 0.11.7：设置/清除标注模式（前端通知后端）。
#[tauri::command]
pub fn screenshot_set_annotation_mode(active: bool) {
    crate::infra::platform::screenshot::set_annotation_mode(active);
}

/// 隐藏截图覆盖窗（ESC / 失焦 / 选区过小时调）。
#[tauri::command]
pub fn hide_screenshot_overlay(app: tauri::AppHandle) {
    crate::infra::platform::window::hide_screenshot_overlay(&app);
}

/// 0.15.8：列出可吸附窗口（截图 overlay 智能窗口吸附用）。
///
/// 返回 `Vec<PickableWindow>`，每条含 DWM 扩展边框（物理像素、虚拟屏幕坐标系）+
/// 窗口标题 + 进程名。前端在选区拖拽阶段做 hit-test：鼠标悬停 → 虚线框；单击 → 吸附。
///
/// `spawn_blocking` 隔离 Win32 EnumWindows（~5-15ms），不阻塞 tokio runtime。
#[tauri::command]
pub async fn screenshot_window_list()
-> Result<Vec<crate::infra::platform::window::PickableWindow>, String> {
    tokio::task::spawn_blocking(crate::infra::platform::window::enumerate_pickable_windows)
        .await
        .map_err(|e| format!("spawn_blocking join 失败: {e}"))
}

/// 0.18.x：截图控件 hints 流式事件 payload。
///
/// `hwnd` + `generation` 由调用方传入，原样回传，前端双重校验防过期。
/// `kind` 区分 batch（一层完成）和 done（全部结束/超时/出错）。
#[derive(Debug, Clone, serde::Serialize)]
struct ControlHintsEvent {
    /// 请求对应的窗口 HWND，前端校验 payload.hwnd === activeHwnd
    hwnd: isize,
    /// 调用方传入的 generation，原样回传，前端防过期
    generation: u32,
    /// "batch" = 一层完成；"done" = 全部结束（正常/超时/出错）
    kind: &'static str,
    /// batch 时为当前层 depth（0-based），done 时为实际到达的最大 depth+1
    depth: usize,
    /// batch 时为本层新增的 hints（物理坐标），done 时为空
    #[serde(skip_serializing_if = "Vec::is_empty")]
    hints: Vec<crate::infra::platform::window::ControlHint>,
    /// done 时携带：总收集数
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
    /// done 时携带：是否因 deadline 截断
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,
}

/// 纯函数：检查 HWND 是否在可吸附窗口列表中。
///
/// 校验逻辑可独立测试，不依赖 Win32 调用。
fn is_hwnd_pickable(
    hwnd: isize,
    windows: &[crate::infra::platform::window::PickableWindow],
) -> bool {
    windows.iter().any(|w| w.hwnd == hwnd)
}

/// 校验目标 HWND 是否有效：非零 + 仍然存在 + 属于可吸附窗口列表。
///
/// 重新枚举顶层窗口（`enumerate_pickable_windows`），确认该 HWND：
/// - 当前仍然存在（`IsWindow`）
/// - 属于可吸附窗口列表（非工具窗口、非 NOACTIVATE、非 Cloaked、有标题、有有效 DWM 矩形）
///
/// 校验失败时返回 false，调用方应发送空的 done 事件，不要启动 UIA 遍历。
///
/// 不记录窗口标题或 UIA 文本，只记录 hwnd。
fn validate_target_hwnd(hwnd: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;

    if hwnd == 0 {
        return false;
    }

    let hwnd_ptr = HWND(hwnd as *mut _);
    if !unsafe { IsWindow(Some(hwnd_ptr)) }.as_bool() {
        return false;
    }

    let windows = crate::infra::platform::window::enumerate_pickable_windows();
    is_hwnd_pickable(hwnd, &windows)
}

/// 0.18.x：流式收集控件 hints，每层 emit 一批，结束发 done。
///
/// 立即返回 `Ok(())`，实际收集在后台 `spawn_blocking` 中进行。
/// 前端通过监听 `blink://screenshot-control-hints` 事件增量接收 hints。
///
/// `hwnd` 由前端传入（当前悬停的顶层窗口 HWND），后端校验后用于 UIA 遍历。
/// `generation` 由前端传入，每次 emit 原样回带，前端校验防过期。
///
/// 事件 payload 同时携带 `hwnd` 和 `generation`，前端必须同时校验两者。
#[tauri::command]
pub async fn screenshot_control_hints(
    app: tauri::AppHandle,
    hwnd: isize,
    generation: u32,
) -> Result<(), String> {
    // 校验 HWND：非零 + 存在 + 属于可吸附窗口列表
    if !validate_target_hwnd(hwnd) {
        tracing::debug!(
            hwnd,
            generation,
            "screenshot_control_hints: HWND 校验失败，发送空 done"
        );
        let _ = app.emit_to(
            "chord-screenshot",
            EventNames::SCREENSHOT_CONTROL_HINTS,
            &ControlHintsEvent {
                hwnd,
                generation,
                kind: "done",
                depth: 0,
                hints: vec![],
                total: Some(0),
                truncated: Some(false),
            },
        );
        return Ok(());
    }

    // 从 DB 读截图配置，取控件吸附参数
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let cfg =
        crate::app::config::ConfigStore::get::<crate::app::config::ScreenshotConfig>(pool).await;
    let deadline = std::time::Duration::from_millis(cfg.control_snap_deadline_ms as u64);
    let max_depth = cfg.control_snap_depth as usize;
    let min_size = cfg.control_snap_min_size as i32;

    tracing::debug!(
        hwnd,
        generation,
        deadline_ms = cfg.control_snap_deadline_ms,
        max_depth,
        min_size,
        "screenshot_control_hints 开始流式收集"
    );

    let app_clone = app.clone();
    tokio::task::spawn_blocking(move || {
        let win_handle = windows::Win32::Foundation::HWND(hwnd as _);
        let (hints, truncated) = crate::infra::platform::uia::collect_control_hints_streaming(
            win_handle,
            deadline,
            max_depth,
            min_size,
            |batch, depth| {
                let _ = app_clone.emit_to(
                    "chord-screenshot",
                    EventNames::SCREENSHOT_CONTROL_HINTS,
                    &ControlHintsEvent {
                        hwnd,
                        generation,
                        kind: "batch",
                        depth,
                        hints: batch.to_vec(),
                        total: None,
                        truncated: None,
                    },
                );
            },
        );
        // done
        let _ = app_clone.emit_to(
            "chord-screenshot",
            EventNames::SCREENSHOT_CONTROL_HINTS,
            &ControlHintsEvent {
                hwnd,
                generation,
                kind: "done",
                depth: max_depth,
                hints: vec![],
                total: Some(hints.len()),
                truncated: Some(truncated),
            },
        );
    });
    Ok(())
}

/// 0.15.7：长截图——设置/清除 overlay 捕获排除（WDA_EXCLUDEFROMCAPTURE）。
///
/// 设为 true 时，overlay 窗口在 BitBlt 屏幕采集中不可见（但用户仍能看到）。
/// 进入长截图采集阶段时设 true，退出时设 false。
#[tauri::command]
pub fn screenshot_set_capture_exclusion(
    app: tauri::AppHandle,
    exclude: bool,
) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
    };

    let win = app
        .get_webview_window("chord-screenshot")
        .ok_or_else(|| "截图 overlay 窗口未找到".to_string())?;
    let hwnd = win.hwnd().map_err(|e| format!("获取 HWND 失败: {e}"))?;
    let target = HWND(hwnd.0 as _);

    let affinity = if exclude {
        WDA_EXCLUDEFROMCAPTURE
    } else {
        WDA_NONE
    };
    let ok = unsafe { SetWindowDisplayAffinity(target, affinity) };
    if ok.is_err() {
        return Err("SetWindowDisplayAffinity 失败".to_string());
    }
    tracing::debug!(exclude, "overlay 捕获排除已设置");
    Ok(())
}

/// 0.15.7：长截图——截取屏幕区域为 RGBA bytes。
///
/// 调用 `screenshot::capture_region` 截取指定虚拟屏幕坐标区域的 BGRA 像素，
/// 转为 RGBA（前端 ImageData 需 RGBA 格式）后返回。
///
/// **必须在 `screenshot_set_capture_exclusion(true)` 之后调用**，
/// 否则 BitBlt 会拍到 overlay 自身。
#[tauri::command]
pub async fn screenshot_capture_band(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> Result<tauri::ipc::Response, String> {
    let bytes = tokio::task::spawn_blocking(move || {
        let bgra = crate::infra::platform::screenshot::capture_region(x, y, w, h)?;
        // H5 优化：u32 位运算批量 swap R↔B，替代 chunks_exact_mut(4).swap(0, 2)
        // BGRA [B,G,R,A] 在 little-endian 下 = u32 0xAARRGGBB
        // RGBA [R,G,B,A]            = u32 0xAABBGGRR
        // 差别仅 R/B 位置对换 → mask 0x00FF00FF 提出 R/B 两字节交换
        let mut rgba = bgra;
        for chunk in rgba.chunks_exact_mut(4) {
            let px = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let rb = px & 0x00FF00FF;
            let ga = px & 0xFF00FF00;
            let swapped = ga | (rb << 16) | (rb >> 16);
            chunk.copy_from_slice(&swapped.to_ne_bytes());
        }
        Ok::<Vec<u8>, String>(rgba)
    })
    .await
    .map_err(|e| format!("spawn_blocking join 失败: {e}"))??;

    Ok(tauri::ipc::Response::new(bytes))
}

/// 0.15.7-R1：截取长截图采集带的低分辨率灰度探针。
///
/// BitBlt 仍在 Rust 侧完成，但跨 IPC 只返回至多 96×64 字节；前端用相邻探针
/// 判断滚动动画是否已经稳定，稳定后才请求完整 RGBA 帧。
#[tauri::command]
pub async fn screenshot_capture_probe(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> Result<tauri::ipc::Response, String> {
    let bytes = tokio::task::spawn_blocking(move || {
        let bgra = crate::infra::platform::screenshot::capture_region(x, y, w, h)?;
        downsample_luma_bgra(&bgra, w, h)
    })
    .await
    .map_err(|e| format!("spawn_blocking join 失败: {e}"))??;

    Ok(tauri::ipc::Response::new(bytes))
}

/// 返回当前光标的物理屏幕坐标（虚拟屏幕坐标系，可能为负）。
///
/// 通过 Win32 `GetCursorPos` 获取，供取色器逐物理像素直接采样截图 bitmap。
/// 前端用 `bitmapX = screenX - meta.vx`, `bitmapY = screenY - meta.vy` 直接定位像素，
/// 不需要通过 CSS 坐标插值。
#[tauri::command]
pub fn screenshot_cursor_position() -> Result<serde_json::Value, String> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut pt = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut pt).map_err(|e| format!("GetCursorPos 失败: {e}"))?;
        }
        Ok(serde_json::json!({ "x": pt.x, "y": pt.y }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("screenshot_cursor_position 仅支持 Windows".to_string())
    }
}

/// 0.15.7：长截图——转发滚轮事件给目标窗口。
///
/// 手动滚动模式下，overlay 接收 wheel 事件后调用本命令，
/// 通过 `PostMessageW(hwnd, WM_MOUSEWHEEL, ...)` 转发给目标窗口。
///
/// `delta` 为 wheel delta（正值向上滚，负值向下滚），标准 120 为一格。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn screenshot_forward_wheel(
    app: tauri::AppHandle,
    hwnd: Option<isize>,
    delta: i32,
    screen_x: i32,
    screen_y: i32,
    passthrough_ms: Option<u64>,
    position_cursor: Option<bool>,
    force_message: Option<bool>,
) -> Result<(), String> {
    let hwnd_val = hwnd
        .filter(|value| *value != 0)
        .ok_or("未提供有效窗口 HWND")?;
    let passthrough_ms = passthrough_ms
        .unwrap_or(WHEEL_PASSTHROUGH_MS)
        .min(MAX_WHEEL_PASSTHROUGH_MS);
    if position_cursor.unwrap_or(false) {
        unsafe { windows::Win32::UI::WindowsAndMessaging::SetCursorPos(screen_x, screen_y) }
            .map_err(|e| format!("定位自动滚动光标失败: {e}"))?;
    }
    if force_message.unwrap_or(false) {
        tracing::debug!(hwnd = hwnd_val, delta, "自动滚动切换到窗口消息兜底");
        return post_wheel_to_target(hwnd_val, delta, screen_x, screen_y);
    }
    if let Err(inject_error) = inject_wheel_through_overlay(&app, delta, passthrough_ms) {
        tracing::warn!(error = %inject_error, "真实滚轮注入失败，回退到窗口消息转发");
        return post_wheel_to_target(hwnd_val, delta, screen_x, screen_y);
    }
    Ok(())
}

/// overlay 会截获首个滚轮事件。开启一个短时连续穿透窗口并用
/// SendInput 补发首个事件，后续真实滚轮可直达底层应用。这避免每一格
/// 都切换 overlay hit-test 并阻塞线程，同时兼容忽略 WM_MOUSEWHEEL 的自绘窗口。
fn inject_wheel_through_overlay(
    app: &tauri::AppHandle,
    delta: i32,
    passthrough_ms: u64,
) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
    };

    let _guard = WHEEL_INJECT_LOCK
        .lock()
        .map_err(|_| "滚轮注入状态锁中毒".to_string())?;
    let overlay = app
        .get_webview_window("chord-screenshot")
        .ok_or_else(|| "截图 overlay 窗口未找到".to_string())?;
    overlay
        .set_ignore_cursor_events(true)
        .map_err(|e| format!("开启 overlay 鼠标穿透失败: {e}"))?;

    let wheel_delta = delta.clamp(i16::MIN as i32, i16::MAX as i32);
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: wheel_delta as u32,
                dwFlags: MOUSEEVENTF_WHEEL,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };

    if sent != 1 {
        let _ = overlay.set_ignore_cursor_events(false);
        return Err("SendInput 未能注入滚轮事件".to_string());
    }
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(passthrough_ms));
        if let Err(error) = overlay.set_ignore_cursor_events(false) {
            tracing::warn!(%error, "恢复截图 overlay 鼠标交互失败");
        }
    });
    // tracing::debug!(delta, passthrough_ms, "已开启连续滚轮穿透");
    Ok(())
}

fn post_wheel_to_target(
    hwnd_val: isize,
    delta: i32,
    screen_x: i32,
    screen_y: i32,
) -> Result<(), String> {
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::{
        CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, ChildWindowFromPointEx, IsWindow,
        PostMessageW, WM_MOUSEWHEEL,
    };

    let root = HWND(hwnd_val as _);
    if !unsafe { IsWindow(Some(root)) }.as_bool() {
        return Err(format!("滚动目标窗口已失效: hwnd={hwnd_val}"));
    }

    // WM_MOUSEWHEEL 正常会发给光标所在控件。overlay 截获输入后需显式沿选区中心
    // 找到最深子 HWND；只投顶层窗口对 Chromium/Electron/自绘控件通常无效。
    let mut target = root;
    for _ in 0..8 {
        let mut client_point = POINT {
            x: screen_x,
            y: screen_y,
        };
        if !unsafe { ScreenToClient(target, &mut client_point) }.as_bool() {
            break;
        }
        let child = unsafe {
            ChildWindowFromPointEx(
                target,
                client_point,
                CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
            )
        };
        if child.is_invalid() || child == target {
            break;
        }
        target = child;
    }

    let wheel_delta = delta.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    let wparam = WPARAM(((wheel_delta as u16 as usize) & 0xFFFF) << 16);
    let packed_point = ((screen_x as u16 as u32) | ((screen_y as u16 as u32) << 16)) as isize;
    unsafe { PostMessageW(Some(target), WM_MOUSEWHEEL, wparam, LPARAM(packed_point)) }
        .map_err(|e| format!("投递 WM_MOUSEWHEEL 失败: {e}"))?;
    tracing::debug!(
        hwnd = hwnd_val,
        child_hwnd = target.0 as isize,
        delta,
        screen_x,
        screen_y,
        "已转发滚轮事件"
    );
    Ok(())
}

fn finish_screenshot_session(app: &tauri::AppHandle) {
    crate::infra::platform::screenshot::set_annotation_mode(false);
    crate::infra::platform::window::hide_screenshot_overlay(app);
}

fn finish_image_editor_session(app: &tauri::AppHandle) {
    crate::infra::platform::window::hide_image_editor_window(app);
}

/// 列出系统已安装的字体名称列表，供截图文字工具选择字体用。
///
/// 使用 GDI `EnumFontFamiliesExW` 枚举所有可用的 TrueType/OpenType 字体，
/// 返回去重后的字体 family 名称列表（按字母序排序）。
#[tauri::command]
pub async fn list_system_fonts() -> Result<Vec<String>, String> {
    tokio::task::spawn_blocking(enum_system_fonts)
        .await
        .map_err(|e| format!("spawn_blocking join 失败: {e}"))?
}

/// 使用 GDI 枚举系统已安装的字体 family 名称（去重 + 排序）。
fn enum_system_fonts() -> Result<Vec<String>, String> {
    #[cfg(target_os = "windows")]
    {
        use std::collections::BTreeSet;
        use windows::Win32::Foundation::LPARAM;
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, DeleteDC, EnumFontFamiliesExW, LOGFONTW, TEXTMETRICW,
        };

        static FONT_NAMES: std::sync::Mutex<Option<BTreeSet<String>>> = std::sync::Mutex::new(None);

        unsafe extern "system" fn font_enum_proc(
            lf: *const LOGFONTW,
            _tm: *const TEXTMETRICW,
            _lparam: u32,
            _data: LPARAM,
        ) -> i32 {
            if lf.is_null() {
                return 1;
            }
            let lf = unsafe { &*lf };
            // 读取 face name（UTF-16，LF_FACESIZE=32）
            let mut end = 0;
            for i in 0..32 {
                if lf.lfFaceName[i] == 0 {
                    end = i;
                    break;
                }
            }
            if end == 0 {
                end = 32;
            }
            let name = String::from_utf16_lossy(&lf.lfFaceName[..end]);
            // 跳过以 @ 开头的竖排字体
            if name.starts_with('@') {
                return 1;
            }
            if let Ok(mut guard) = FONT_NAMES.lock() {
                if guard.is_none() {
                    *guard = Some(BTreeSet::new());
                }
                if let Some(set) = guard.as_mut() {
                    set.insert(name);
                }
            }
            1
        }

        {
            let mut guard = FONT_NAMES.lock().map_err(|e| format!("锁失败: {e}"))?;
            *guard = Some(BTreeSet::new());
        }

        let hdc = unsafe { CreateCompatibleDC(None) };
        if hdc.is_invalid() {
            return Err("CreateCompatibleDC 失败".to_string());
        }

        let lf: LOGFONTW = unsafe { std::mem::zeroed() };
        let _ = unsafe { EnumFontFamiliesExW(hdc, &lf, Some(font_enum_proc), LPARAM(0), 0) };

        let _ = unsafe { DeleteDC(hdc) };

        let mut guard = FONT_NAMES.lock().map_err(|e| format!("锁失败: {e}"))?;
        let names = guard.take().unwrap_or_default();
        Ok(names.into_iter().collect())
    }

    #[cfg(not(target_os = "windows"))]
    {
        Ok(vec![
            "sans-serif".to_string(),
            "serif".to_string(),
            "monospace".to_string(),
        ])
    }
}

#[cfg(test)]
mod scroll_probe_tests {
    use super::*;

    #[test]
    fn probe_converts_bgra_to_luma() {
        let pixels = [0_u8, 0, 255, 255, 255, 255, 255, 255];
        let probe = downsample_luma_bgra(&pixels, 2, 1).unwrap();
        assert_eq!(probe, vec![76, 255]);
    }

    #[test]
    fn probe_is_bounded_for_large_capture_band() {
        let pixels = vec![128_u8; 200 * 100 * 4];
        let probe = downsample_luma_bgra(&pixels, 200, 100).unwrap();
        assert_eq!(
            probe.len(),
            (SCROLL_PROBE_MAX_W * SCROLL_PROBE_MAX_H) as usize
        );
    }

    #[test]
    fn replay_export_path_accepts_only_owned_file_shapes() {
        let path =
            scroll_replay_export_path("blink-scroll-2026-08-02T10-20-30-000Z", "frame-0042.png")
                .unwrap();
        assert!(
            path.ends_with("scroll-replays/blink-scroll-2026-08-02T10-20-30-000Z/frame-0042.png")
        );
        assert!(scroll_replay_export_path("../escape", "manifest.json").is_err());
        assert!(scroll_replay_export_path("blink-scroll-valid", "../blink.log").is_err());
        assert!(scroll_replay_export_path("blink-scroll-valid", "frame-42.png").is_err());
    }

    #[test]
    fn ocr_command_projection_preserves_json_contract() {
        let projected =
            project_ocr_command_result(crate::domain::capability::CapabilityResult::Text {
                content: r#"{"text":"识别结果","lines":[],"words":[]}"#.into(),
                desc: None,
            })
            .unwrap();
        assert_eq!(projected["text"], "识别结果");
        assert!(projected["lines"].is_array());

        let invalid =
            project_ocr_command_result(crate::domain::capability::CapabilityResult::Done {
                summary: "unexpected".into(),
            })
            .unwrap_err();
        assert_eq!(invalid.code, "internal_error");
    }
}

/// 0.18.x：HWND 校验纯函数测试。
#[cfg(test)]
mod hwnd_validation_tests {
    use super::*;
    use crate::infra::platform::window::PickableWindow;

    fn make_window(hwnd: isize, x: i32, y: i32, w: i32, h: i32) -> PickableWindow {
        PickableWindow {
            hwnd,
            x,
            y,
            w,
            h,
            title: format!("Win-{hwnd}"),
            process_name: "test".to_string(),
        }
    }

    #[test]
    fn pickable_hwnd_passes_validation() {
        let windows = vec![
            make_window(100, 0, 0, 800, 600),
            make_window(200, 800, 0, 800, 600),
        ];
        assert!(is_hwnd_pickable(100, &windows));
        assert!(is_hwnd_pickable(200, &windows));
    }

    #[test]
    fn unpickable_hwnd_rejected() {
        let windows = vec![
            make_window(100, 0, 0, 800, 600),
            make_window(200, 800, 0, 800, 600),
        ];
        assert!(!is_hwnd_pickable(999, &windows));
        assert!(!is_hwnd_pickable(0, &windows));
        assert!(!is_hwnd_pickable(-1, &windows));
    }

    #[test]
    fn empty_window_list_rejects_all() {
        let windows: Vec<PickableWindow> = vec![];
        assert!(!is_hwnd_pickable(100, &windows));
    }

    #[test]
    fn control_hints_event_serializes_with_hwnd() {
        let event = ControlHintsEvent {
            hwnd: 12345,
            generation: 7,
            kind: "batch",
            depth: 1,
            hints: vec![],
            total: None,
            truncated: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"hwnd\":12345"));
        assert!(json.contains("\"generation\":7"));
        assert!(json.contains("\"kind\":\"batch\""));
    }

    #[test]
    fn control_hints_event_done_serializes_with_hwnd() {
        let event = ControlHintsEvent {
            hwnd: 12345,
            generation: 3,
            kind: "done",
            depth: 5,
            hints: vec![],
            total: Some(42),
            truncated: Some(false),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"hwnd\":12345"));
        assert!(json.contains("\"generation\":3"));
        assert!(json.contains("\"kind\":\"done\""));
        assert!(json.contains("\"total\":42"));
    }
}

/// 从 translate_batch 的首项 payload 读取保序结果。
fn parse_translate_batch_payload(
    result: &crate::domain::capability::CapabilityResult,
    expected: usize,
) -> Option<Vec<String>> {
    let crate::domain::capability::CapabilityResult::Items { items } = result else {
        return None;
    };
    let results = items.first()?.data.get("results")?.as_array()?;
    if results.len() != expected {
        return None;
    }
    results
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect()
}

// ── 0.22.4 PaddleOCR 管理命令 ──────────────────────────────────────────────
//
// Task 5: 提供最小、受限的 PaddleOCR 管理协议。
//
// **安全约束**：
// - engine_id 由编译期常量 `PADDLEOCR_ENGINE_ID` 锁定，不接受前端传入。
// - 前端不能提交 runtime kind、URL、executable、argv、script 或 env。
// - 所有执行参数由 adapter 从 locked artifact 解析。

use std::sync::Arc;

use crate::app::local_engine::EngineManager;
use crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID;
use crate::domain::local_engine::AdapterConfig;
use crate::infra::local_engine::runtime::EngineId;
use tauri::Manager;

/// 从 Tauri managed state 获取 `EngineManager` 引用。
fn get_engine_service(app: &tauri::AppHandle) -> Result<Arc<EngineManager>, String> {
    app.try_state::<Arc<EngineManager>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| "EngineManager 尚未注册".to_string())
}

/// Task 16: 从 Tauri managed state 获取 `OcrCoordinator` 引用。
///
/// 管理命令（install/repair/start/stop）执行后必须通知 coordinator，
/// 使其清理缓存的 start_state/start_result、取消旧 idle timer、递增 epoch。
fn get_ocr_coordinator(
    app: &tauri::AppHandle,
) -> Option<Arc<crate::app::local_engine::ocr_coordinator::OcrCoordinator>> {
    app.try_state::<Arc<crate::app::local_engine::ocr_coordinator::OcrCoordinator>>()
        .map(|s| s.inner().clone())
}

/// Task 16: 通知 OcrCoordinator 外部管理命令改变了引擎状态。
///
/// 在 install/repair/start/stop 成功后调用，使 coordinator 清理缓存状态。
async fn notify_coordinator_state_change(app: &tauri::AppHandle) {
    if let Some(coordinator) = get_ocr_coordinator(app) {
        coordinator.notify_external_state_change().await;
    }
}

/// 构建编译期锁定的 PaddleOCR EngineId。
fn paddleocr_engine_id() -> EngineId {
    EngineId::new(PADDLEOCR_ENGINE_ID).expect("paddleocr engine id is valid")
}

/// 从当前 OcrConfig 构建 PaddleOCR AdapterConfig。
///
/// 配置只包含业务参数（模型选择、计算偏好），不包含执行参数。
///
/// **Task 14**: `compute_preference` 从 `OcrConfig` 读取，不再硬编码。
/// 用户在设置页选择的 `auto` / `cpu` 会真正传递给 install/start/profile resolve。
fn build_paddleocr_adapter_config() -> AdapterConfig {
    let ocr_config = crate::domain::config::ocr_config::get_ocr_config();
    let engine_config =
        crate::app::local_engine::paddleocr::PaddleOcrEngineConfig::from_ocr_config();
    AdapterConfig {
        preferred_port: None,
        compute_preference: Some(ocr_config.compute_preference),
        engine_config: engine_config.to_json(),
    }
}

/// 安装 PaddleOCR 引擎环境（uv venv + paddlepaddle + paddleocr + fastapi）。
///
/// 走现有 `EngineManager::install` → `InstallTransaction`，
/// 不新造安装器。安装过程通过 `blink://local-engine-status` 事件投影进度。
#[tauri::command]
pub async fn install_paddleocr(
    app: tauri::AppHandle,
) -> Result<(), crate::app::command_error::CommandError> {
    use crate::app::command_error::CommandError;
    let svc =
        get_engine_service(&app).map_err(|e| CommandError::new("internal_error", e, false))?;
    let engine_id = paddleocr_engine_id();
    let adapter_config = build_paddleocr_adapter_config();

    svc.install(&engine_id, adapter_config).await.map_err(|e| {
        CommandError::new("install_failed", format!("PaddleOCR 安装失败: {e}"), true)
    })?;

    // Task 16: 通知 OcrCoordinator 外部状态变更——清理缓存
    notify_coordinator_state_change(&app).await;

    tracing::info!("PaddleOCR 引擎安装完成");
    Ok(())
}

/// 修复 PaddleOCR 环境（先停止再重新安装）。
///
/// 适用于缓存损坏、self-test 失败等场景。
///
/// **安全 repair 流程**：
/// 1. 通过 coordinator lifecycle 进入 Stopping，停止当前 PaddleOCR 实例
/// 2. 校验目标绝对路径位于 `%APPDATA%\blink\models\paddleocr`，防御 `..`、symlink/junction
/// 3. 只清理 paddleocr 模型缓存，不碰其他引擎或公共缓存
/// 4. 删除失败时返回结构化错误（不静默 warn）
/// 5. 重新安装并验证恢复
/// 6. 失败时返回结构化错误
#[tauri::command]
pub async fn repair_paddleocr(
    app: tauri::AppHandle,
) -> Result<(), crate::app::command_error::CommandError> {
    use crate::app::command_error::CommandError;
    let svc =
        get_engine_service(&app).map_err(|e| CommandError::new("internal_error", e, false))?;
    let engine_id = paddleocr_engine_id();

    // 1. 通过 coordinator lifecycle 进入 repair 模式，阻止新 lease 并条件停止当前实例
    //    begin_repair 会：设 repair_mode → 取消 idle timer → 等 in-flight → conditional_stop → Idle(next gen)
    //    Task 6: 返回 RepairGuard RAII——确保无论 repair 路径如何结束，end_repair 都会被调用
    let _repair_guard = if let Some(coordinator) = get_ocr_coordinator(&app) {
        Some(coordinator.begin_repair().await)
    } else {
        None
    };
    // 也直接调 svc.stop 确保进程退出（begin_repair 已包含，但双重保障）
    let _ = svc.stop(&engine_id).await;

    // 2. 安全清理 PaddleOCR 模型缓存——路径防御
    let model_cache_dir = crate::infra::local_engine::runtime::engine_model_cache_dir(&engine_id);
    let blink_root = crate::infra::utils::paths::app_data_dir();
    let canonical_cache = blink_root.join("models").join("paddleocr");

    // 2a. 防御 `..` 路径遍历——组件中不得出现 ".."
    for component in model_cache_dir.components() {
        if let std::path::Component::ParentDir = component {
            return Err(CommandError::new(
                "repair_safety_violation",
                format!(
                    "模型缓存路径包含 `..` 组件，疑似路径遍历攻击：{}",
                    model_cache_dir.display()
                ),
                false,
            ));
        }
    }

    // 2b. 校验目标绝对路径位于 `%APPDATA%\blink\models\paddleocr`
    //     canonicalize 会解析 symlink/junction 到真实路径
    let canonical_cache = canonical_cache.canonicalize().unwrap_or(canonical_cache);
    let actual_cache = model_cache_dir
        .canonicalize()
        .unwrap_or(model_cache_dir.clone());
    if actual_cache != canonical_cache {
        return Err(CommandError::new(
            "repair_safety_violation",
            format!(
                "模型缓存路径校验失败: 期望 {} 但实际 {}（疑似 symlink/junction 重定向）",
                canonical_cache.display(),
                actual_cache.display()
            ),
            false,
        ));
    }

    // 2c. 防御 symlink/junction——canonicalize 后的路径不得指向 Blink 专属目录之外
    //     检查 canonical 路径是否以 `%APPDATA%\blink\models\paddleocr` 开头
    let blink_models_root = blink_root.join("models");
    let blink_models_canonical = blink_models_root
        .canonicalize()
        .unwrap_or(blink_models_root);
    if !actual_cache.starts_with(&blink_models_canonical) {
        return Err(CommandError::new(
            "repair_safety_violation",
            format!(
                "模型缓存路径越界：{} 不在 Blink models 目录 {} 内",
                actual_cache.display(),
                blink_models_canonical.display()
            ),
            false,
        ));
    }

    // 2d. 额外检查：目标路径的文件系统类型不应是 symlink（即使 canonicalize 已解析）
    //     如果 cache dir 本身是 symlink，metadata().file_type().is_symlink() 会为 true
    if let Ok(meta) = std::fs::symlink_metadata(&model_cache_dir) {
        if meta.file_type().is_symlink() {
            return Err(CommandError::new(
                "repair_safety_violation",
                format!(
                    "模型缓存路径是 symlink，拒绝清理：{}",
                    model_cache_dir.display()
                ),
                false,
            ));
        }
    }

    // 3. 只清理 paddleocr 模型缓存——删除失败时返回结构化错误
    //    Task 6: 清理失败必须 fail-closed——不继续安装
    if actual_cache.exists() {
        tracing::info!(path = %actual_cache.display(), "清理 PaddleOCR 模型缓存");
        if let Err(e) = std::fs::remove_dir_all(&actual_cache) {
            return Err(CommandError::new(
                "repair_cleanup_failed",
                format!(
                    "清理 PaddleOCR 模型缓存失败（可能有文件被占用）：{e}（路径：{}）",
                    actual_cache.display()
                ),
                true,
            ));
        }
    }

    // 4. 清理旧的 generation（如果有）
    //    Task 6: cleanup 失败也是 fail-closed
    //    0.22.5: 旧 svc.cleanup(engine_id) 已移除，generation 切换由 install 内部处理。
    //    旧 generation 可通过 get_local_engine_storage + cleanup_local_engine 手动清理。
    tracing::info!(engine = %engine_id, "跳过旧 generation 清理（由 install 事务内部处理）");

    // 5. 重新安装
    let adapter_config = build_paddleocr_adapter_config();
    svc.install(&engine_id, adapter_config).await.map_err(|e| {
        CommandError::new("repair_failed", format!("PaddleOCR 修复失败: {e}"), true)
    })?;

    // 5b. 恢复验证——确认安装后环境状态为 Ready
    let status = svc.get_status(&engine_id).await.map_err(|e| {
        CommandError::new(
            "repair_verification_failed",
            format!("修复后状态查询失败: {e}"),
            false,
        )
    })?;
    if status.status.environment != crate::domain::local_engine::status::EnvironmentHealth::Ready {
        return Err(CommandError::new(
            "repair_verification_failed",
            format!("修复后环境状态非 Ready: {:?}", status.status.environment),
            true,
        ));
    }

    // 5c. start 并等待模型 Ready（Handoff B.V.6）
    let adapter_config = build_paddleocr_adapter_config();
    svc.start(&engine_id, adapter_config).await.map_err(|e| {
        CommandError::new("repair_start_failed", format!("修复后启动失败: {e}"), true)
    })?;

    // 等待 model Ready（最多 120s）
    let model_ready =
        wait_for_model_ready(&svc, &engine_id, std::time::Duration::from_secs(120)).await;
    if !model_ready {
        // RepairGuard RAII 会自动调用 end_repair()
        return Err(CommandError::new(
            "repair_model_timeout",
            "修复后模型加载超时（120s）",
            true,
        ));
    }

    // 5d. 验证 health model contract/fingerprint（Handoff B.V.7）
    let status = svc.get_status(&engine_id).await.map_err(|e| {
        CommandError::new(
            "repair_verification_failed",
            format!("修复后状态查询失败: {e}"),
            false,
        )
    })?;
    if status.status.model != crate::domain::local_engine::status::ModelHealth::Ready {
        // RepairGuard RAII 会自动调用 end_repair()
        return Err(CommandError::new(
            "repair_model_not_ready",
            format!("修复后模型状态非 Ready: {:?}", status.status.model),
            true,
        ));
    }

    // 6. 退出 repair 模式，恢复正常生命周期
    //    Task 6: RepairGuard RAII 在 drop 时自动调用 end_repair()
    //    成功路径需要 disarm 取消自动退出，然后手动调用 end_repair + notify
    if let Some(coordinator) = get_ocr_coordinator(&app) {
        coordinator.notify_external_state_change().await;
    }

    tracing::info!("PaddleOCR 引擎修复完成（含 start + model ready 验证）");
    Ok(())
}

/// 等待模型 Ready，轮询直到 Ready 或超时。
async fn wait_for_model_ready(
    svc: &crate::app::local_engine::EngineManager,
    engine_id: &crate::infra::local_engine::runtime::EngineId,
    timeout: std::time::Duration,
) -> bool {
    let start = std::time::Instant::now();
    let poll_interval = std::time::Duration::from_secs(1);
    loop {
        if start.elapsed() >= timeout {
            return false;
        }
        match svc.get_status(engine_id).await {
            Ok(status) => {
                if status.status.model == crate::domain::local_engine::status::ModelHealth::Ready {
                    return true;
                }
                if status.status.model == crate::domain::local_engine::status::ModelHealth::Failed {
                    return false;
                }
            }
            Err(e) => {
                tracing::warn!(%e, "wait_for_model_ready: 状态查询失败");
            }
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// 启动 PaddleOCR 引擎。
///
/// 如果环境未安装，先自动安装。
#[tauri::command]
pub async fn start_paddleocr(
    app: tauri::AppHandle,
) -> Result<(), crate::app::command_error::CommandError> {
    use crate::app::command_error::CommandError;
    let svc =
        get_engine_service(&app).map_err(|e| CommandError::new("internal_error", e, false))?;
    let engine_id = paddleocr_engine_id();
    let adapter_config = build_paddleocr_adapter_config();

    // 确保环境已安装
    svc.ensure_installed(&engine_id, adapter_config.clone())
        .await
        .map_err(|e| {
            CommandError::new(
                "install_failed",
                format!("PaddleOCR 环境安装失败: {e}"),
                true,
            )
        })?;

    // 启动引擎
    svc.start(&engine_id, adapter_config)
        .await
        .map_err(|e| CommandError::new("start_failed", format!("PaddleOCR 启动失败: {e}"), true))?;

    // Task 16: 通知 OcrCoordinator 外部状态变更——清理缓存
    notify_coordinator_state_change(&app).await;

    tracing::info!("PaddleOCR 引擎已启动");
    Ok(())
}

/// 停止 PaddleOCR 引擎。
#[tauri::command]
pub async fn stop_paddleocr(
    app: tauri::AppHandle,
) -> Result<(), crate::app::command_error::CommandError> {
    use crate::app::command_error::CommandError;
    let svc =
        get_engine_service(&app).map_err(|e| CommandError::new("internal_error", e, false))?;
    let engine_id = paddleocr_engine_id();

    svc.stop(&engine_id).await.map_err(|e| {
        CommandError::new("stop_failed", format!("停止 PaddleOCR 失败: {e}"), false)
    })?;

    // Task 16: 通知 OcrCoordinator 外部状态变更——清理缓存
    notify_coordinator_state_change(&app).await;

    tracing::info!("PaddleOCR 引擎已停止");
    Ok(())
}

/// 查询 PaddleOCR 引擎状态。
///
/// 返回结构化状态快照，包含 environment/process/service/model 各维度。
#[tauri::command]
pub async fn get_paddleocr_status(
    app: tauri::AppHandle,
) -> Result<serde_json::Value, crate::app::command_error::CommandError> {
    use crate::app::command_error::CommandError;
    let svc =
        get_engine_service(&app).map_err(|e| CommandError::new("internal_error", e, false))?;
    let engine_id = paddleocr_engine_id();

    let snapshot = svc.get_status(&engine_id).await.map_err(|e| {
        CommandError::new(
            "status_query_failed",
            format!("查询 PaddleOCR 状态失败: {e}"),
            false,
        )
    })?;

    // 投影为前端友好的 JSON
    Ok(serde_json::to_value(&snapshot).unwrap_or_else(|_| {
        serde_json::json!({
            "engine_id": PADDLEOCR_ENGINE_ID,
            "error": "status serialization failed"
        })
    }))
}
