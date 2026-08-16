//! clipboard 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use tauri::Manager;

use crate::domain::clipboard::ClipboardWriteSource;
use crate::domain::event::CapabilityEnv;
use crate::domain::search::SearchResponse;

/// 剪贴板模式直接搜索（bypass SearchService pipeline）。
///
/// Alt+C 进入剪贴板模式后，前端输入直接调此命令，
/// 不经过 SearchService::search / IntentRouter / get_weights / Route 分派，
/// 只走 ClipboardEngine。性能最优。
#[tauri::command]
pub async fn search_clipboard(app: tauri::AppHandle, query: String, seq: u64) -> SearchResponse {
    tracing::debug!(%query, seq, "search_clipboard: 收到请求");
    let service = app.state::<std::sync::Arc<crate::domain::search::SearchService>>();
    // 复用 SearchService 持有的 ClipboardEngine 实例（不重复建池）
    let entries = service.search_clipboard_mode(&query).await;
    tracing::debug!(
        count = entries.len(),
        %query,
        "search_clipboard: 返回结果"
    );
    for (i, item) in entries.iter().enumerate() {
        let detail = item.score_detail.as_deref().unwrap_or("");
        tracing::trace!(
            index = i,
            score = if detail.is_empty() {
                format!("{:.4}", item.score)
            } else {
                format!("{:.4} ({})", item.score, detail)
            },
            source = %item.source,
            name = %item.name,
            lnk_path = %item.lnk_path,
            "剪贴板模式结果项"
        );
    }
    SearchResponse {
        entries,
        suggestion: None,
    }
}

/// 将文本写入系统剪贴板（Windows API）。
/// 右键菜单独立 Popup 窗口中 navigator.clipboard 不可靠，改走后端。
#[tauri::command]
pub async fn copy_to_clipboard(text: String) -> Result<(), String> {
    crate::domain::clipboard::write_text(text, ClipboardWriteSource::User)
        .await
        .map_err(|e| e.to_string())
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

/// 按 id 拉取完整 text（延迟加载）。
///
/// 搜索路径只携带 `id` + `preview`（80 字符截断），用户选中某条历史时
/// 前端调此命令按需拉取完整 `text`。避免搜索路径预载 500 条完整 text
/// 导致 MB 级 JSON 序列化开销。
#[tauri::command]
pub async fn get_clipboard_text(
    app: tauri::AppHandle,
    id: String,
) -> Result<Option<String>, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    let text = crate::infra::data::clipboard::get_text_by_id(pool, &id).await;
    if text.is_none() {
        tracing::warn!(%id, "get_clipboard_text: 未找到记录（可能已被清理）");
    }
    Ok(text)
}

/// 0.20.2：按 id 批量拉取完整 text（批量原子复制用）。
///
/// 接受 id 列表，返回 `[{ id, text }]`——text 为 null 表示未找到。
/// 前端收到后检查是否有 null，有则整体放弃复制。
#[tauri::command]
pub async fn get_clipboard_text_batch(
    app: tauri::AppHandle,
    ids: Vec<String>,
) -> Result<Vec<serde_json::Value>, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    let results = crate::infra::data::clipboard::get_text_batch_by_ids(pool, &id_refs).await;
    let json_results: Vec<serde_json::Value> = results
        .into_iter()
        .map(|(id, text)| {
            serde_json::json!({
                "id": id,
                "text": text,
            })
        })
        .collect();
    Ok(json_results)
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
    let png_data = crate::domain::clipboard::load_history_png(&pools.cache, &image_id)
        .await
        .map_err(|e| e.to_string())?;

    tracing::debug!(id = %image_id, bytes = png_data.len(), "copy_clipboard_image: 开始写入");

    let bytes_len = png_data.len();
    crate::domain::clipboard::write_png(png_data, ClipboardWriteSource::HistoryRepost)
        .await
        .map_err(|e| e.to_string())?;

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
    let png_data = crate::domain::clipboard::load_history_png(&pools.cache, &image_id)
        .await
        .map_err(|e| e.to_string())?;
    let env = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let (screen_x, screen_y) = env.show_pin_image(png_data, None, None)?;

    tracing::debug!(id = %image_id, screen_x, screen_y, "pin_clipboard_image");
    tracing::info!(id = %image_id, "剪贴板图片已钉到桌面");
    Ok(())
}
