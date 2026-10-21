//! clipboard 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use tauri::Manager;

use crate::app::command_error::CommandError;
use crate::domain::clipboard::ClipboardWriteSource;
use crate::domain::clipboard::history::{
    ClipboardEntryKind, ClipboardEntryOutcome, ClipboardEntryRef,
};

/// UI 协议适配：所有删除/钉图都落到同一个 Capability，结果逐项透传。
#[tauri::command]
pub async fn manage_clipboard_entries(
    app: tauri::AppHandle,
    operation: String,
    entries: Vec<ClipboardEntryRef>,
) -> Result<Vec<ClipboardEntryOutcome>, CommandError> {
    let env = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let registry = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    let ctx = crate::domain::capability::InvokeContext {
        env: env.as_ref(),
        origin: crate::domain::capability::InvocationOrigin::LocalSurface,
        runtime: crate::domain::capability::RuntimeCapabilities {
            surface: Some(env.as_ref()),
            main_process: true,
            desktop_session: true,
        },
        deadline: None,
    };
    let result = registry
        .invoke(
            "manage_clipboard_history",
            serde_json::json!({"operation":operation,"entries":entries}),
            &ctx,
        )
        .await?;
    match result {
        crate::domain::capability::CapabilityResult::Items { items } => items
            .into_iter()
            .map(|item| {
                serde_json::from_value(item.data)
                    .map_err(|e| CommandError::new("internal", e.to_string(), false))
            })
            .collect(),
        _ => Err(CommandError::new(
            "internal",
            "剪贴板操作返回了无效结果",
            false,
        )),
    }
}

/// 0.26.16：置顶/取消置顶剪贴板条目（右键菜单入口）。
///
/// 与删除（[`manage_clipboard_entries`]）同为**列表管理操作**：走同一
/// `pin_clipboard_entry` Capability（Registry 门禁与参数校验复用），但不隐藏
/// 主窗——`run_builtin_action` 的通用收尾 hide 对「执行完就走」的动作合理
/// （打开设置/复制/打开 URL），对改变排序的管理操作不适用：置顶后列表强制
/// 重查，用户停留在剪贴板模式继续操作。
#[tauri::command]
pub async fn pin_clipboard_entries(
    app: tauri::AppHandle,
    operation: String,
    entries: Vec<ClipboardEntryRef>,
    hours: Option<u32>,
) -> Result<(), CommandError> {
    let env = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let registry = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    let ctx = crate::domain::capability::InvokeContext {
        env: env.as_ref(),
        origin: crate::domain::capability::InvocationOrigin::LocalSurface,
        runtime: crate::domain::capability::RuntimeCapabilities {
            surface: Some(env.as_ref()),
            main_process: true,
            desktop_session: true,
        },
        deadline: None,
    };
    let result = registry
        .invoke(
            "pin_clipboard_entry",
            serde_json::json!({"operation":operation,"entries":entries,"hours":hours}),
            &ctx,
        )
        .await?;
    tracing::info!(operation, count = entries.len(), "pin_clipboard_entries: 置顶状态更新");
    let _ = result;
    Ok(())
}

/// UI continuation adapter: return the committed identity without hiding the main window.
#[tauri::command]
pub async fn save_clipboard_file_image(
    app: tauri::AppHandle,
    id: String,
) -> Result<String, CommandError> {
    let env = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let registry = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    let ctx = crate::domain::capability::InvokeContext {
        env: env.as_ref(),
        origin: crate::domain::capability::InvocationOrigin::LocalSurface,
        runtime: crate::domain::capability::RuntimeCapabilities {
            surface: Some(env.as_ref()),
            main_process: true,
            desktop_session: true,
        },
        deadline: None,
    };
    let result = registry
        .invoke(
            "save_clipboard_file_as_image",
            serde_json::json!({"id": id}),
            &ctx,
        )
        .await?;
    saved_file_image_id(result)
}

fn saved_file_image_id(
    result: crate::domain::capability::CapabilityResult,
) -> Result<String, CommandError> {
    if let crate::domain::capability::CapabilityResult::Items { items } = result {
        if items.len() == 1 {
            if let Some(id) = items[0]
                .data
                .get("image_id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
            {
                return Ok(id.to_owned());
            }
        }
    }
    Err(CommandError::new(
        "internal",
        "图片入库未返回有效条目 ID",
        false,
    ))
}

#[cfg(test)]
mod file_image_result_tests {
    use super::*;
    use crate::domain::capability::{CapabilityResult, ItemResult};

    #[test]
    fn continuation_requires_one_exact_committed_image_id() {
        let result = |values: Vec<serde_json::Value>| CapabilityResult::Items {
            items: values
                .into_iter()
                .map(|data| ItemResult {
                    data,
                    desc: None,
                    actions: vec![],
                })
                .collect(),
        };
        assert_eq!(
            saved_file_image_id(result(vec![
                serde_json::json!({"image_id":"committed-image"})
            ]))
            .unwrap(),
            "committed-image"
        );
        for data in [
            vec![],
            vec![serde_json::json!({"image_id":""})],
            vec![serde_json::json!({"id":"file-id"})],
            vec![
                serde_json::json!({"image_id":"a"}),
                serde_json::json!({"image_id":"b"}),
            ],
        ] {
            assert!(saved_file_image_id(result(data)).is_err());
        }
        assert!(
            saved_file_image_id(CapabilityResult::Done {
                summary: "saved".into()
            })
            .is_err()
        );
    }
}

async fn manage_legacy_entries(
    app: tauri::AppHandle,
    operation: &str,
    kind: ClipboardEntryKind,
    ids: Vec<String>,
) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    let outcomes = manage_clipboard_entries(
        app,
        operation.into(),
        ids.into_iter()
            .map(|id| ClipboardEntryRef { kind, id })
            .collect(),
    )
    .await
    .map_err(|e| e.message)?;
    if let Some(error) = outcomes.into_iter().find_map(|o| o.error) {
        return Err(error);
    }
    Ok(())
}

// 同一时刻只允许一次上屏，避免后一次请求覆盖前一次尚未粘贴的剪贴板。
static PASTE_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 剪贴板模式直接搜索（bypass SearchService pipeline）。
///
/// Alt+C 进入剪贴板模式后，前端输入直接调此命令，
/// 不经过 SearchService::search / IntentRouter / get_weights / Route 分派，
/// 只走 ClipboardEngine。性能最优。
///
/// 0.25.23: `kind` 类型筛选（"text"/"image"，缺省或未知 = 全部）——文本与图片
/// 分表存储，筛选下推为跳过另一张表的查询。
///
/// 0.25.29: keyset 流式分页——`cursor`（上一批末条的 `{created_at, id}`，缺省 = 首批）
/// 与 `batch`（本批条数）；响应携带 `next_cursor` / `has_more`，滚到底继续追加。
/// batch 缺省/0 由 engine 按游标区分：首批两页、追加一页 page_size。
#[derive(serde::Serialize)]
pub struct SearchClipboardResponse {
    pub entries: Vec<crate::domain::search::AppEntry>,
    /// 下一批起点；`has_more=false` 时为 None。
    pub next_cursor: Option<crate::infra::data::clipboard::ClipboardCursor>,
    pub has_more: bool,
}

#[tauri::command]
pub async fn search_clipboard(
    app: tauri::AppHandle,
    query: String,
    seq: u64,
    kind: Option<String>,
    cursor: Option<crate::infra::data::clipboard::ClipboardCursor>,
    batch: Option<u32>,
) -> SearchClipboardResponse {
    tracing::debug!(%query, seq, kind = kind.as_deref(), has_cursor = cursor.is_some(), batch, "search_clipboard: 收到请求");
    let service = app.state::<std::sync::Arc<crate::domain::search::SearchService>>();
    // 复用 SearchService 持有的 ClipboardEngine 实例（不重复建池）
    let page = service
        .search_clipboard_page(&query, kind.as_deref(), cursor, batch.unwrap_or(0) as usize)
        .await;
    let entries: Vec<crate::domain::search::AppEntry> = page
        .items
        .into_iter()
        .map(crate::domain::search::SearchItem::into_app_entry)
        .collect();
    tracing::debug!(
        count = entries.len(),
        has_more = page.has_more,
        %query,
        "search_clipboard: 返回结果"
    );
    // 0.25.29：逐项 trace 已删（每批 N 条刷屏）——召回条数与 has_more 在上一行
    SearchClipboardResponse {
        entries,
        next_cursor: page.next_cursor,
        has_more: page.has_more,
    }
}

/// 将文本写入系统剪贴板（Windows API）。
/// 右键菜单独立 Popup 窗口中 navigator.clipboard 不可靠，改走后端。
///
/// 0.25.27 定案（日志实证回滚）：多选组合同样走 User 入库（删旧留新去重，
/// 同组合只留一条顶置）——组合对用户是可复用内容，必须出现在历史里；
/// 曾改 skip 不入库防"自繁殖"，实测是用户诉求相反。
#[tauri::command]
pub async fn copy_to_clipboard(text: String) -> Result<(), String> {
    crate::domain::clipboard::write_text(text, ClipboardWriteSource::User)
        .await
        .map_err(|e| e.to_string())
}

/// 剪贴板「上屏」：按用户明确的粘贴意图，把文本贴回唤起前的应用。
///
/// 固定原目标窗口身份 → 写系统剪贴板 → 隐藏主窗口 → 等修饰键松开 →
/// 恢复并核对原窗口 → Ctrl+V（终端 Shift+Insert）。失败时保留已复制的文本。
///
/// UIA 不参与粘贴门控或 SetFocus：Typora 的可编辑区可暴露为 Group，Java IDE
/// 可只暴露 SunAwtFrame；两者都能正常接收粘贴。应用自行决定当前控件如何处理
/// 粘贴，保留原光标/选区。该策略仅用于用户显式「上屏」，不改变语音注入链路。
#[tauri::command]
pub async fn paste_to_input(app: tauri::AppHandle, text: String) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }

    let paste_guard = PASTE_GATE
        .try_lock()
        .map_err(|_| "正在粘贴，请稍后重试".to_string())?;
    // 在首次 await 前读取原 HWND，复制/隐藏之前快照身份，不在后台重新选目标。
    let prev_hwnd = crate::infra::platform::window::last_external_foreground_hwnd();
    // 0.26.16：粘贴源主窗 HWND——PasteTarget 放行本体窗口（对话窗/便签等）
    // 后，只需排除 main 自身，防止把粘贴键发回正在隐藏的源窗口。
    let main_hwnd = app
        .get_webview_window("main")
        .and_then(|w| w.hwnd().ok())
        .map(|h| h.0 as isize);
    let (target, text, units) = tokio::task::spawn_blocking(move || {
        let target =
            prev_hwnd.and_then(|hwnd| crate::infra::platform::window::PasteTarget::capture(
                hwnd, main_hwnd,
            ));
        let units = text.encode_utf16().count();
        (target, text, units)
    })
    .await
    .map_err(|e| format!("读取粘贴目标失败: {e}"))?;

    // 1. 兜底复制（同步等待：保证后续注入/降级路径能读到文本）
    crate::domain::clipboard::write_text(text, ClipboardWriteSource::User)
        .await
        .map_err(|e| e.to_string())?;

    // 2. 隐藏主窗口
    crate::infra::platform::window::hide(&app, "paste_to_input");

    // 3. 恢复原窗口并发粘贴键（阻塞 Win32 调用隔离于 async 工作线程）
    //
    // **Alt+数字 激活时用户仍按着 Alt**。Alt 按住期间 Windows 处于系统菜单态，
    // 窗口恢复（SetForegroundWindow）与粘贴键（SendInput）
    // 都会被干扰——语音 G2 从无此问题，正因它是 hold 松开后才注入（Alt 已弹起）。
    // 这里先等修饰键全部物理松开（限时 500ms），超时仍未松开则跳过注入、
    // 保留剪贴板复制语义（文本已在系统剪贴板），不做失败注入。
    tokio::task::spawn_blocking(move || {
        // 即使等待 IPC 的任务被取消，也保持互斥到后台粘贴结束。
        let _paste_guard = paste_guard;
        let Some(target) = target else {
            tracing::info!("paste_to_input: 原目标窗口不可用，仅复制");
            return Ok(());
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while !crate::infra::platform::hotkey::read_physical_modifiers().all_up()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !crate::infra::platform::hotkey::read_physical_modifiers().all_up() {
            tracing::debug!(
                "paste_to_input: 修饰键 500ms 内未松开，跳过注入（保留剪贴板复制语义）"
            );
            return Ok(());
        }
        if !target.restore() {
            tracing::info!(?target, "paste_to_input: 原目标窗口未恢复，仅复制");
            return Ok(());
        }
        let terminal = crate::infra::platform::window::is_terminal_window(target.hwnd());
        // 不按 PID 放宽到同应用的另一个窗口，也不搜索任意输入框强行 SetFocus。
        if !target.is_foreground()
            || !crate::infra::platform::hotkey::read_physical_modifiers().all_up()
        {
            tracing::info!(?target, "paste_to_input: 发键前焦点或修饰键已变化，仅复制");
            return Ok(());
        }
        let (result, method) = if terminal {
            (
                crate::infra::platform::inject::paste_via_shift_insert(),
                "shift_insert",
            )
        } else {
            (
                crate::infra::platform::inject::paste_clipboard_text(),
                "ctrl_v",
            )
        };
        match result {
            Ok(()) => tracing::info!(units, method, ?target, "paste_to_input: 已发送粘贴键"),
            Err(e) => {
                tracing::error!(%e, units, method, ?target, "paste_to_input: 发送粘贴键失败");
                return Err(e.to_string());
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("粘贴任务失败: {e}"))?
}

/// 获取最近的剪贴板历史。
#[tauri::command]
pub async fn get_clipboard_history(
    app: tauri::AppHandle,
    limit: Option<i64>,
) -> Vec<crate::infra::data::clipboard::ClipboardItem> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::query_recent(pool, limit.unwrap_or(20)).await
}

/// 搜索剪贴板历史。
#[tauri::command]
pub async fn search_clipboard_history(
    app: tauri::AppHandle,
    query: String,
    limit: Option<i64>,
) -> Vec<crate::infra::data::clipboard::ClipboardItem> {
    let pools = app.state::<crate::infra::data::DbPools>();
    let cfg = crate::domain::config::ConfigStore::get::<
        crate::infra::data::clipboard::ClipboardConfig,
    >(&pools.config)
    .await;
    crate::infra::data::clipboard::search(
        &pools.history,
        &query,
        limit.unwrap_or(20).clamp(0, 5000),
        cfg.retention_days,
        cfg.candidate_limit,
    )
    .await
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
    manage_legacy_entries(app, "delete", ClipboardEntryKind::Text, vec![id]).await
}

/// 0.25.27：按 id 批量删除剪贴板文本条目（多选右键「删除选中的 N 条」）。
///
/// 单事务：任一条目失败整体回滚，前端整体重试无重复删除副作用。
#[tauri::command]
pub async fn delete_clipboard_items_batch(
    app: tauri::AppHandle,
    ids: Vec<String>,
) -> Result<(), String> {
    manage_legacy_entries(app, "delete", ClipboardEntryKind::Text, ids).await
}

/// 0.17.0：删除指定剪贴板图片条目。
///
/// 图片项的 `lnkPath` 实际持有 image_id（`engine.rs` image 分支投影），
/// 兼容旧入口；新的混选菜单使用 manage_clipboard_entries。
#[tauri::command]
pub async fn delete_clipboard_image(app: tauri::AppHandle, image_id: String) -> Result<(), String> {
    manage_legacy_entries(app, "delete", ClipboardEntryKind::Image, vec![image_id]).await
}

/// 0.17.0：清空所有剪贴板图片历史。
#[tauri::command]
pub async fn clear_clipboard_images(app: tauri::AppHandle) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    crate::infra::data::clipboard_images::clear_all_images(&pools.cache).await;
    let _ = crate::infra::data::compact(&pools.cache).await;
    tracing::info!("剪贴板图片历史已清空");
    Ok(())
}

/// 清空所有剪贴板历史。
#[tauri::command]
pub async fn clear_clipboard_history(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    crate::infra::data::clipboard::clear_all(pool).await;
    // 0.26.9：文件路径历史同属用户历史，随清空入口一并清（只删历史，不碰原文件）
    crate::infra::data::clipboard_files::clear_all_files(pool).await;
    let _ = crate::infra::data::compact(pool).await; // VACUUM + WAL checkpoint 回收空间
    Ok(())
}

/// 获取剪贴板统计信息。
#[tauri::command]
pub async fn get_clipboard_stats(app: tauri::AppHandle) -> serde_json::Value {
    let pools = app.state::<crate::infra::data::DbPools>();
    let text_stats = crate::infra::data::clipboard::get_stats(&pools.history).await;
    let image_stats = crate::infra::data::clipboard_images::get_image_stats(&pools.cache).await;
    let file_stats = crate::infra::data::clipboard_files::get_file_stats(&pools.history).await;
    // 合并文本/图片/文件统计
    let mut stats = text_stats;
    for merged in [image_stats, file_stats] {
        if let serde_json::Value::Object(ref mut map) = stats
            && let serde_json::Value::Object(src) = merged
        {
            for (k, v) in src {
                map.insert(k, v);
            }
        }
    }
    stats
}

/// 0.26.9：按 id 拉取文件条目的完整路径列表（悬浮预览展示用）。
///
/// 默认纯查库；include_metadata 仅在图片展示后按需探测单文件大小。
/// 单路径条目的图片内容走 `blink-clipimg` 的
/// `/file-preview/<id>` 协议，不在此返回文件内容。
#[tauri::command]
pub async fn get_clipboard_file_entry(
    app: tauri::AppHandle,
    id: String,
    include_metadata: Option<bool>,
) -> Result<Option<serde_json::Value>, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    let item = crate::infra::data::clipboard_files::get_paths_by_id(pool, &id).await;
    let size_bytes = if include_metadata.unwrap_or(false) {
        crate::infra::data::clipboard_files::get_file_size_by_id(pool, &id).await.ok()
    } else {
        None
    };
    Ok(item.map(|item| {
        serde_json::json!({
            "id": item.id,
            "paths": item.paths,
            "file_count": item.paths.len(),
            "size_bytes": size_bytes,
        })
    }))
}

/// 0.26.9：探测文件条目内容读取状态（前端 img 加载失败后取原因分类展示）。
///
/// 查库 + 有界内容读取/元数据校验（不展开像素）；探测走有界文件 I/O 通道，
/// 忙时返回 `busy`。原因码与 `FilePreviewError::reason` 对齐。
#[tauri::command]
pub async fn get_clipboard_file_preview_status(
    app: tauri::AppHandle,
    id: String,
) -> Result<String, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().history;
    Ok(crate::infra::data::clipboard_files::probe_file_entry_status(pool, &id).await.to_string())
}

/// 0.26.10：按条目引用派生右键「智能操作」（只读查询，打开类 Kind 白名单）。
///
/// 输入类型化条目引用，后端查库取完整原文/单文件原始路径派生；不存在条目/
/// 无适用操作返回空组。数据库故障返回结构化错误（不拿当前剪贴板兜底）。
/// 执行仍复用 `run_builtin_action`（点击时 Registry 门禁重检）。
#[tauri::command]
pub async fn get_clipboard_item_actions(
    app: tauri::AppHandle,
    entry: ClipboardEntryRef,
) -> Result<Vec<crate::domain::clipboard::item_actions::ClipboardItemAction>, CommandError> {
    let pools = app.state::<crate::infra::data::DbPools>();
    let registry = app
        .state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>()
        .inner()
        .clone();
    // 与建议侧/result 侧共享同源禁用列表（设置页保存热生效）
    let disabled: Vec<String> = app
        .try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
        .and_then(|service| service.disabled_builtin_actions_snapshot())
        .unwrap_or_default();
    let actions = crate::domain::clipboard::item_actions::clipboard_item_actions(
        pools.inner(),
        &registry,
        &entry,
        &disabled,
    )
    .await
    .map_err(|detail| CommandError::new("database_error", detail, true))?;
    tracing::debug!(
        kind = ?entry.kind,
        count = actions.len(),
        "get_clipboard_item_actions: 派生完成"
    );
    Ok(actions)
}

/// 0.26.10：右键智能操作的采纳遥测（独立于双槽 impression/fatigue）。
///
/// 只记录 Kind/动作 id/结果，不记录完整 URL、路径、原文、条目 id 或文件名。
#[tauri::command]
pub fn report_clipboard_item_action(
    kind: String,
    capability_id: String,
    outcome: String,
) -> Result<(), String> {
    tracing::info!(
        origin = "ClipboardItem",
        kind = %kind,
        capability_id = %capability_id,
        outcome = %outcome,
        "clipboard item action adopted"
    );
    Ok(())
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

    // HistoryRepost 跳过监听器重新采集以保留原始来源；成功后显式更新历史顺序。
    let promoted = crate::infra::data::clipboard_images::promote_image(&pools.cache, &image_id)
        .await
        .map_err(|e| format!("图片已复制，但历史置顶失败: {e}"))?;

    tracing::info!(id = %image_id, bytes = bytes_len, promoted, "剪贴板图片已写回系统剪贴板");
    Ok(())
}

/// 0.16.5：将剪贴板图片钉到桌面（pin 窗口）。
///
/// 前端图片 item 右键"钉图"调此命令。从 cache 库加载完整 PNG，
/// 调 `show_pin_window` 创建/复用钉图窗口。位置居中于主显示器。
#[tauri::command]
pub async fn pin_clipboard_image(app: tauri::AppHandle, image_id: String) -> Result<(), String> {
    manage_legacy_entries(app, "pin", ClipboardEntryKind::Image, vec![image_id]).await
}
