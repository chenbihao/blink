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
        crate::infra::platform::clipboard::write_png_to_clipboard(
            &png_data,
            crate::infra::platform::clipboard::SELF_LABEL_SCREENSHOT,
            false,
        )
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
        crate::infra::platform::clipboard::write_bgra_to_clipboard(
            &bgra,
            cw,
            ch,
            crate::infra::platform::clipboard::SELF_LABEL_SCREENSHOT,
            false,
        )?;
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
///
/// **0.14.7 W3**：返回 `CommandError`（结构化错误协议）。
#[tauri::command]
pub async fn ocr_image(
    _app: tauri::AppHandle,
    png_data: Vec<u8>,
) -> Result<serde_json::Value, crate::app::command_error::CommandError> {
    let backend = crate::domain::capability::builtins::ocr_engine::backend();
    let result = backend
        .recognize(&png_data)
        .await
        .map_err(crate::app::command_error::CommandError::from)?;

    let json = serde_json::to_value(&result).map_err(|e| {
        crate::app::command_error::CommandError::new(
            "internal_error",
            &format!("序列化 OCR 结果失败: {e}"),
            false,
        )
    })?;
    tracing::debug!(text_len = result.text.len(), "OCR 识别完成");
    Ok(json)
}

/// 0.17.5：OCR 诊断——返回设备已安装的 OCR 语言列表、当前引擎语言、中文包状态。
///
/// 供截图 overlay 诊断面板调用，帮助用户排查"中文截图识别不出"问题。
#[tauri::command]
pub async fn ocr_diagnose(
    _app: tauri::AppHandle,
) -> Result<serde_json::Value, crate::app::command_error::CommandError> {
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

/// 0.18.2：列出前台窗口的 UIA 控件提示（截图控件级智能吸附用）。
///
/// 从截图会话的 `session_fg_hwnd()` 取前台窗口 HWND，用 UIA 逐层 BFS 收集
/// 控件矩形（200ms deadline + 3 层深度自适应降级）。与 `screenshot_window_list`
/// 完全独立、可并行。
///
/// 返回 `Vec<ControlHint>`，每条含控件矩形（物理像素、虚拟屏幕坐标系）+
/// 控件类型 ID。前端转 CSS 后做 hit-test，**控件优先于窗口**（吸附到更小的控件）。
///
/// `spawn_blocking` 隔离 UIA 同步 COM 调用，不阻塞 tokio runtime。
#[tauri::command]
pub async fn screenshot_control_hints()
-> Result<Vec<crate::infra::platform::window::ControlHint>, String> {
    let hwnd = crate::infra::platform::screenshot::session_fg_hwnd();
    let hwnd = match hwnd {
        Some(h) => h,
        None => {
            tracing::debug!("screenshot_control_hints: session_fg_hwnd 为空，返回空列表");
            return Ok(Vec::new());
        }
    };

    tokio::task::spawn_blocking(move || {
        let hwnd = windows::Win32::Foundation::HWND(hwnd as _);
        crate::infra::platform::uia::collect_control_hints(hwnd)
    })
    .await
    .map_err(|e| format!("spawn_blocking join 失败: {e}"))
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
        return Err(format!("SetWindowDisplayAffinity 失败"));
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
        // BGRA → RGBA（swap R and B per pixel）
        let mut rgba = bgra;
        for chunk in rgba.chunks_exact_mut(4) {
            chunk.swap(0, 2);
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

/// 0.15.7：长截图——转发滚轮事件给目标窗口。
///
/// 手动滚动模式下，overlay 接收 wheel 事件后调用本命令，
/// 通过 `PostMessageW(hwnd, WM_MOUSEWHEEL, ...)` 转发给目标窗口。
///
/// `delta` 为 wheel delta（正值向上滚，负值向下滚），标准 120 为一格。
#[tauri::command]
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
