//! clipboard 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use tauri::Manager;

/// 将文本写入系统剪贴板（Windows API）。
/// 右键菜单独立 Popup 窗口中 navigator.clipboard 不可靠，改走后端。
#[tauri::command]
pub async fn copy_to_clipboard(text: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
        };

        // RAII guard: 确保 CloseClipboard 在所有路径上被调用
        struct ClipboardGuard;
        impl Drop for ClipboardGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseClipboard();
                }
            }
        }

        unsafe {
            if OpenClipboard(Some(HWND(std::ptr::null_mut()))).is_err() {
                return Err("打开剪贴板失败".into());
            }
            let _guard = ClipboardGuard;

            let _ = EmptyClipboard();

            // 分配全局内存（+1 for null terminator）
            let wchars: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let byte_size = wchars.len() * 2;
            let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_size)
                .map_err(|e| format!("GlobalAlloc 失败: {e}"))?;
            let ptr = GlobalLock(hmem) as *mut u16;
            if ptr.is_null() {
                return Err("GlobalLock 失败".into());
            }
            std::ptr::copy_nonoverlapping(wchars.as_ptr(), ptr, wchars.len());
            let _ = GlobalUnlock(hmem);

            // CF_UNICODETEXT = 13; SetClipboardData 要求 HANDLE 而非 HGLOBAL
            if SetClipboardData(13, Some(std::mem::transmute(hmem))).is_err() {
                return Err("SetClipboardData 失败".into());
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("spawn_blocking 失败: {e}"))?
}

/// 获取最近的剪贴板历史。
#[tauri::command]
pub async fn get_clipboard_history(
    app: tauri::AppHandle,
    limit: Option<i64>,
) -> Vec<crate::infra::data::clipboard::ClipboardItem> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::query_recent(&pool, limit.unwrap_or(20)).await
}

/// 搜索剪贴板历史。
#[tauri::command]
pub async fn search_clipboard_history(
    app: tauri::AppHandle,
    query: String,
    limit: Option<i64>,
) -> Vec<crate::infra::data::clipboard::ClipboardItem> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::search(&pool, &query, limit.unwrap_or(20)).await
}

/// 删除指定剪贴板条目。
#[tauri::command]
pub async fn delete_clipboard_item(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::delete_item(&pool, &id).await;
    Ok(())
}

/// 0.17.0：删除指定剪贴板图片条目。
///
/// 图片项的 `lnkPath` 实际持有 image_id（`engine.rs` image 分支投影），
/// 前端 `contextmenu.js` 按 `isImage` 分发到此命令。
#[tauri::command]
pub async fn delete_clipboard_image(app: tauri::AppHandle, image_id: String) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::data::clipboard_images::delete_image(pool, &image_id).await;
    tracing::info!(id = %image_id, "剪贴板图片已删除");
    Ok(())
}

/// 0.17.0：清空所有剪贴板图片历史。
#[tauri::command]
pub async fn clear_clipboard_images(app: tauri::AppHandle) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::clipboard_images::clear_all_images(&pools.cache).await;
    crate::infra::data::vacuum(&pools.cache).await;
    tracing::info!("剪贴板图片历史已清空");
    Ok(())
}

/// 清空所有剪贴板历史。
#[tauri::command]
pub async fn clear_clipboard_history(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::clear_all(&pool).await;
    crate::infra::data::vacuum(&pool).await; // 0.16.0: 收缩数据库文件
    Ok(())
}

/// 获取剪贴板统计信息。
#[tauri::command]
pub async fn get_clipboard_stats(app: tauri::AppHandle) -> serde_json::Value {
    let pools = app.state::<crate::infra::data::DbPools>();
    let text_stats = crate::infra::data::clipboard::get_stats(&pools.history).await;
    let image_stats = crate::infra::data::clipboard_images::get_image_stats(&pools.cache).await;
    // 合并文本和图片统计
    let mut stats = text_stats;
    if let serde_json::Value::Object(ref mut map) = stats {
        if let serde_json::Value::Object(img_map) = image_stats {
            for (k, v) in img_map {
                map.insert(k, v);
            }
        }
    }
    stats
}

/// 0.16.4：将剪贴板图片写回系统剪贴板。
///
/// 前端图片 item 的 action.runId = "copy_clipboard_image" 时调此命令。
/// 从 cache 库 clipboard_images 表加载完整 PNG，通过 `write_png_to_clipboard` 写回。
#[tauri::command]
pub async fn copy_clipboard_image(app: tauri::AppHandle, image_id: String) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    let png_data = crate::infra::data::clipboard_images::get_png_by_id(&pools.cache, &image_id)
        .await
        .ok_or_else(|| format!("图片不存在: {image_id}"))?;

    tracing::debug!(id = %image_id, bytes = png_data.len(), "copy_clipboard_image: 开始写入");

    let bytes_len = png_data.len();
    tokio::task::spawn_blocking(move || {
        crate::infra::platform::clipboard::write_png_to_clipboard(
            &png_data,
            crate::infra::platform::clipboard::SELF_LABEL_REPOST,
            true,
        )
    })
    .await
    .map_err(|e| format!("spawn_blocking join 失败: {e}"))??;

    tracing::info!(id = %image_id, bytes = bytes_len, "剪贴板图片已写回系统剪贴板");
    Ok(())
}

/// 0.16.5：将剪贴板图片钉到桌面（pin 窗口）。
///
/// 前端图片 item 右键"钉图"调此命令。从 cache 库加载完整 PNG，
/// 调 `show_pin_window` 创建/复用钉图窗口。位置居中于主显示器。
#[tauri::command]
pub async fn pin_clipboard_image(app: tauri::AppHandle, image_id: String) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    let png_data = crate::infra::data::clipboard_images::get_png_by_id(&pools.cache, &image_id)
        .await
        .ok_or_else(|| format!("图片不存在: {image_id}"))?;

    // 解析图片尺寸用于定位（居中于主显示器工作区）
    let (w, h) = crate::infra::platform::screenshot::parse_png_size(&png_data)
        .map(|(pw, ph)| (pw as i32, ph as i32))
        .unwrap_or((400, 300));

    // 获取光标所在显示器工作区，居中放置（0.19.6 从本文件提升到 window 模块）
    let (screen_x, screen_y) =
        crate::infra::platform::window::get_primary_monitor_center(w, h);

    tracing::debug!(id = %image_id, w, h, screen_x, screen_y, "pin_clipboard_image");

    crate::infra::platform::window::show_pin_window(&app, png_data, screen_x, screen_y, false)?;
    tracing::info!(id = %image_id, "剪贴板图片已钉到桌面");
    Ok(())
}

