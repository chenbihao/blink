//! 截图、OCR 与翻译 commands。

use super::*;

/// 0.11.7-f：接收前端合成后的 PNG（裁剪区 + 标注），写入剪贴板，结束会话。
///
/// **替代** 0.8.7 `capture_region`（后端从 SESSION 裁剪）——现在前端一份合成路径
/// 走通所有输出（复制/保存/钉图），双击全屏也走这里。
///
/// **异步执行**（0.11.7 review 修）：PNG 解码 + BGRA swap + Win32 剪贴板写入都是
/// 同步 CPU/syscall 密集操作，全屏 2560x1440 约 50-100ms。放在异步命令直接跑会
/// 阻塞 tokio 工作线程，影响其他并发任务。用 `spawn_blocking` 挪到阻塞线程池。
///
/// **快路径**：如果只需要复制选区（无标注、无全屏合成），前端应走 `screenshot_copy_region`
/// 直接传坐标——避开前端 toBlob PNG 编码 + 后端 PNG 解码的双重开销，全屏路径
/// 快 ~150-250ms。有标注 / 全屏合成时才走本命令。
#[tauri::command]
pub async fn screenshot_copy(app: tauri::AppHandle, png_data: Vec<u8>) -> Result<(), String> {
    let bytes_len = png_data.len();
    tokio::task::spawn_blocking(move || {
        crate::infra::platform::clipboard::write_png_to_clipboard(&png_data)
    })
    .await
    .map_err(|e| format!("spawn_blocking join 失败: {e}"))??;
    finish_screenshot_session(&app);
    tracing::info!(bytes = bytes_len, "截图已保存到剪贴板");
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
    // BGRA 裁剪 + 剪贴板写入都同步，挪到阻塞线程池
    tokio::task::spawn_blocking(move || -> Result<(u32, u32), String> {
        let (bgra, cw, ch) = crate::infra::platform::screenshot::crop(x, y, w, h)
            .ok_or_else(|| "SESSION 为空或选区越界".to_string())?;
        crate::infra::platform::clipboard::write_bgra_to_clipboard(&bgra, cw, ch)?;
        Ok((cw, ch))
    })
    .await
    .map_err(|e| format!("spawn_blocking join 失败: {e}"))?
    .map(|(cw, ch)| tracing::info!(w = cw, h = ch, "截图选区已直传剪贴板（快路径）"))?;
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
#[tauri::command]
pub fn screenshot_pin(
    app: tauri::AppHandle,
    png_data: Vec<u8>,
    screen_x: i32,
    screen_y: i32,
) -> Result<(), String> {
    crate::infra::platform::window::show_pin_window(&app, png_data, screen_x, screen_y)?;
    finish_screenshot_session(&app);
    tracing::info!(screen_x, screen_y, "截图已钉到屏幕");
    Ok(())
}

/// 0.11.7-f：保存截图选区为文件（PNG/JPEG）。
///
/// `path=None` 弹出保存对话框；用户取消时返回 Err，前端应识别 "用户取消了保存"
/// 字符串以避免噪音。
#[tauri::command]
pub async fn screenshot_save(
    app: tauri::AppHandle,
    png_data: Vec<u8>,
    path: Option<String>,
) -> Result<String, String> {
    use std::io::Write;

    let file_path = match path {
        Some(p) => p,
        None => {
            let timestamp = chrono::Local::now().format("截图_%Y%m%d_%H%M%S");
            let default_name = format!("{}.png", timestamp);
            let dialog = app.dialog().file();
            let picked = dialog
                .add_filter("PNG 图片", &["png"])
                .add_filter("JPEG 图片", &["jpg", "jpeg"])
                .set_file_name(&default_name)
                .blocking_save_file();
            match picked {
                Some(path) => path.to_string(),
                None => return Err("用户取消了保存".to_string()),
            }
        }
    };

    let mut file = std::fs::File::create(&file_path).map_err(|e| format!("创建文件失败: {e}"))?;
    file.write_all(&png_data)
        .map_err(|e| format!("写入文件失败: {e}"))?;

    finish_screenshot_session(&app);
    tracing::info!(path = %file_path, "截图已保存到文件");
    Ok(file_path)
}

/// 0.11.7-d：隐藏钉图窗口（hide 而非 close，保留窗口实例供下次钉图复用）。
#[tauri::command]
pub fn screenshot_pin_hide(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("chord-pin") {
        let _ = win.hide();
    }
}

/// 0.11.8：钉图窗口一次性设置位置 + 尺寸（缩放/拖动/onload 跟随共用）。
///
/// 走 Win32 `SetWindowPos` 原子地设位置+尺寸，绕开 Tauri 逻辑像素 DPI 竞态。
/// 参数均为**屏幕物理像素**：
/// - `win_x`/`win_y`：窗口左上角屏幕坐标（= 图片左上 - PIN_PAD）
/// - `win_w`/`win_h`：窗口尺寸（= 图片显示尺寸 + 2×PIN_PAD，含发光区）
///
/// 前端在缩放/拖动时算好这 4 个值一次性传入，避免多次 set_position/set_size 竞态。
#[tauri::command]
pub fn screenshot_pin_transform(
    app: tauri::AppHandle,
    win_x: i32,
    win_y: i32,
    win_w: u32,
    win_h: u32,
) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    if let Some(win) = app.get_webview_window("chord-pin") {
        if let Ok(hwnd) = win.hwnd() {
            crate::infra::platform::window::place_at_physical(
                HWND(hwnd.0 as _),
                win_x,
                win_y,
                win_w,
                win_h,
            );
        }
    }
    Ok(())
}

/// 0.11.7-c：OCR 识别图片中的文字，返回 `{text, lines}`。
///
/// 0.11.7-f：改走 `ocr_engine::backend()` 注入的后端（测试可替换）。
#[tauri::command]
pub async fn ocr_image(
    _app: tauri::AppHandle,
    png_data: Vec<u8>,
) -> Result<serde_json::Value, String> {
    let backend = crate::domain::capability::builtins::ocr_engine::backend();
    let result = backend
        .recognize(&png_data)
        .await
        .map_err(|e| format!("OCR 识别失败: {e}"))?;

    let json = serde_json::to_value(&result).map_err(|e| format!("序列化 OCR 结果失败: {e}"))?;
    tracing::debug!(text_len = result.text.len(), "OCR 识别完成");
    Ok(json)
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
/// - 插件未启用 / manifest 未加载 → 返 `"翻译插件未安装或未启用"`
/// - 插件返回空/错误 → 传递原错误信息
/// - 插件返回非 Items 结果 → `"翻译插件返回意外的结果类型"`(理论不会,防御)
#[tauri::command]
pub async fn translate_text(
    app: tauri::AppHandle,
    text: String,
    target_lang: Option<String>,
) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("翻译文本不能为空".into());
    }

    let registry = app.state::<std::sync::Arc<crate::domain::capability::CapabilityRegistry>>();
    // translate 插件的 tool 注册 id = plugin_tool_id("builtin.translate", "translate")
    const TRANSLATE_CAPABILITY_ID: &str = "builtin_translate_translate";
    if registry.get(TRANSLATE_CAPABILITY_ID).is_none() {
        tracing::warn!("translate_text: 翻译插件未注册");
        return Err("翻译插件未安装或未启用".into());
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
        deadline: None,
    };
    let result = registry
        .invoke(TRANSLATE_CAPABILITY_ID, arguments, &ctx)
        .await
        .map_err(|e| format!("翻译执行失败: {e}"))?;

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
                .ok_or_else(|| "翻译插件返回空结果".to_string())?;
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
            Err("翻译插件返回意外的结果类型".into())
        }
    }
}

/// 0.11.10-g:批量翻译多行文本。
///
/// 首选一次调用插件 `translate_batch` tool，由插件加 tag 后单次请求翻译引擎并保序拆回。
/// 插件版本不匹配、tag 被引擎破坏或结构化结果异常时，降级为并发单行 `translate_text`，
/// 保证截图翻译功能不因批量优化失败而不可用。
#[tauri::command]
pub async fn translate_lines(
    app: tauri::AppHandle,
    lines: Vec<String>,
    target_lang: Option<String>,
) -> Result<Vec<String>, String> {
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

fn finish_screenshot_session(app: &tauri::AppHandle) {
    crate::infra::platform::screenshot::set_annotation_mode(false);
    crate::infra::platform::window::hide_screenshot_overlay(app);
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
