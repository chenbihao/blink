//! Win32 剪贴板监听：隐藏消息窗口 + AddClipboardFormatListener + WM_CLIPBOARDUPDATE。
//!
//! 监听线程跑独立消息循环（GetMessageW），WM_CLIPBOARDUPDATE 时读剪贴板文本，
//! 经黑名单过滤（`data::clipboard::is_blacklisted`）后 spawn 异步存（`save_item`）+ cleanup。

use std::sync::Mutex;

use windows::Win32::Foundation::{HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, GetClipboardData, OpenClipboard,
    RemoveClipboardFormatListener,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, RegisterClassW,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLIPBOARDUPDATE, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use crate::data::clipboard::{self, ClipboardItem};
use crate::data::clipboard_files;
use crate::data::clipboard_images::{self, ClipboardImage};

use super::dropfiles;
use super::{is_active, state, take_self_write};

/// 缩略图最大边长（像素）。采集时生成，不延后到展示——避免列表滚动时重复解码。
const THUMB_MAX_EDGE: u32 = 256;

/// 短窗口去重（0.8.7 修复：有些应用 Ctrl+C 会连发多次 WM_CLIPBOARDUPDATE）。
/// 记录最近一次入库的 (text_hash, ts_ms)；10 秒内同文本再来直接跳过。
static DEDUP_STATE: Mutex<Option<(u64, u128)>> = Mutex::new(None);
const DEDUP_WINDOW_MS: u128 = 10_000;
/// 保证异步入库顺序与 WM_CLIPBOARDUPDATE 顺序一致，避免同内容“删旧留新”反向覆盖。
static TEXT_PERSIST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static IMAGE_PROCESS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// 文件条目落库顺序锁（0.26.9），与文本/图片各自独立。
static FILE_PERSIST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) fn start_watcher_thread() {
    if let Err(e) = std::thread::Builder::new()
        .name("blink-clipboard-watcher".into())
        .spawn(watcher_main)
    {
        tracing::warn!(?e, "剪贴板监听线程启动失败");
    }
}

fn watcher_main() {
    unsafe {
        let class_name = w!("BlinkClipboardListener");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(std::ptr::null_mut()),
            lpszClassName: class_name,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!(""),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(?e, "CreateWindowExW 剪贴板监听窗口失败");
                return;
            }
        };
        if let Err(e) = AddClipboardFormatListener(hwnd) {
            tracing::warn!(?e, "AddClipboardFormatListener 失败");
            return;
        }
        tracing::debug!("剪贴板监听窗口已注册");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = RemoveClipboardFormatListener(hwnd);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        let active = is_active();
        tracing::trace!(active, "WM_CLIPBOARDUPDATE 收到");
        if active {
            on_clipboard_change();
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn on_clipboard_change() {
    // 0.17.9：先消费自写入标记（命中则跳过黑名单 + 用标签作 source_app）
    let self_write = take_self_write();

    // 确定来源：自写入标签优先，否则用前台进程名
    // 0.25.27：前台就是 blink 自身（标记与指纹都错位的自写入）统一标
    // blink:app——展示层认识该标签，避免历史出现 "blink.exe" 野标签
    let (mut source_app, title_for_blacklist) = if let Some((ref label, _)) = self_write {
        (Some(label.clone()), None) // 自写入跳过黑名单
    } else {
        let app_info = crate::platform::context::foreground_app();
        let title = app_info.as_ref().map(|a| a.window_title.clone());
        let proc_name = app_info
            .as_ref()
            .map(|a| a.process_name.clone())
            .filter(|n| !n.is_empty());
        let resolved = match proc_name.as_deref() {
            Some(p)
                if p.eq_ignore_ascii_case("blink") || p.eq_ignore_ascii_case("blink.exe") =>
            {
                Some(super::SELF_LABEL_APP.to_string())
            }
            other => other.map(str::to_string),
        };
        (resolved, title)
    };

    // 黑名单：前台窗口标题命中则不记（密码管理器等）—— 自写入跳过
    if let Some(s) = state() {
        let bl = s.blacklist.read().unwrap();
        let title_hit = title_for_blacklist
            .as_deref()
            .is_some_and(|title| clipboard::is_blacklisted(title, &bl));
        let process_hit = self_write.is_none()
            && source_app
                .as_deref()
                .is_some_and(|process| clipboard::is_blacklisted(process, &bl));
        if title_hit || process_hit {
            tracing::debug!(
                title = title_for_blacklist.as_deref().unwrap_or(""),
                process = source_app.as_deref().unwrap_or(""),
                "剪贴板：前台黑名单，跳过"
            );
            return;
        }
    }

    // 自写入 skip_persist 标志
    let skip_persist = self_write.as_ref().is_some_and(|(_, skip)| *skip);

    let Some(s) = state() else { return };
    let capture_images = s.capture_images.load(std::sync::atomic::Ordering::Relaxed);
    let capture_files = s.capture_files.load(std::sync::atomic::Ordering::Relaxed);

    // 0.26.9：单次 OpenClipboard 会话内按「文本 → DIB（若开启）→ HDROP（若开启）」
    // 读取快照——多格式来自同一个剪贴板序列，避免多次打开期间混入另一轮复制；
    // 空白文本/格式读取失败可继续下一类，每轮最多选一类入库，不重复。
    let snap = read_clipboard_snapshot(capture_images, capture_files);

    // ── 文本分支：有效非空文本优先 ──
    if let Some(ref text) = snap.text {
        if text.trim().is_empty() {
            tracing::trace!("剪贴板：文本仅空白,尝试其他格式");
        } else {
            // 0.25.27 兜底：标记 miss（一次写入双事件错位消费 / TTL 过期）时按内容
            // 指纹识别自写入，命中则沿用该写入的标签与 skip 语义（见 mod.rs 模块注释）
            let mut skip_persist = skip_persist;
            if self_write.is_none()
                && let Some((label, skip)) = super::take_self_text_fingerprint(fx_hash(text))
            {
                tracing::debug!(%label, skip, "剪贴板：标记 miss，内容指纹兜底命中自写入");
                source_app = Some(label);
                skip_persist = skip;
            }
            // 短窗口去重（0.8.7）：10 秒内同文本不重复记录。
            // Ctrl+C 在部分应用（浏览器/编辑器）会连发多次 WM_CLIPBOARDUPDATE,不去重会看到
            // 历史里同一条塞了 3~4 遍,后续 cleanup_excess 反而挤掉真正的旧条目。
            let text_hash = fx_hash(text);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            if let Ok(mut guard) = DEDUP_STATE.lock() {
                if let Some((last_hash, last_ms)) = *guard
                    && last_hash == text_hash
                    && now_ms.saturating_sub(last_ms) < DEDUP_WINDOW_MS
                {
                    tracing::debug!(
                        len = text.chars().count(),
                        "剪贴板：10s 内同文本重复,跳过入库"
                    );
                    return;
                }
                *guard = Some((text_hash, now_ms));
            }
            // 0.9.2.1：入库前先触发 hook,让 SearchService.snapshot 局部刷新 Clipboard 项
            //（过滤 + 去重后触发,与真实入库同步;hook 侧再做 ContextConfig 门控与敏感应用
            // 过滤,避免密码管理器 Ctrl+C 悄悄进 snapshot）。
            super::notify_change(text);

            // 0.17.9：自写入 skip_persist（历史回贴场景）跳过入库
            if skip_persist {
                tracing::trace!("剪贴板：自写入 skip_persist, 跳过文本入库");
                return;
            }

            let preview = clipboard::make_preview(text);
            let item = ClipboardItem {
                id: clipboard::generate_id(),
                preview: preview.clone(),
                text: text.clone(),
                created_at: now_ts(),
                source_app,
                hit_count: 0,
            };
            let pool = s.pool.clone();
            let max_items = s.max_items.load(std::sync::atomic::Ordering::Relaxed);
            tauri::async_runtime::spawn(async move {
                let _order_guard = TEXT_PERSIST_LOCK.lock().await;
                match clipboard::save_item(&pool, &item).await {
                    Ok(_) => tracing::trace!(id = %item.id, preview = %preview, "剪贴板：已入库"),
                    Err(e) => {
                        if let Ok(mut guard) = DEDUP_STATE.lock()
                            && *guard == Some((text_hash, now_ms))
                        {
                            *guard = None;
                        }
                        tracing::warn!(?e, "剪贴板 save_item 失败");
                        return;
                    }
                }
                let _ = clipboard::cleanup_excess(&pool, max_items).await;
            });
            return;
        }
    }

    // ── 图片采集（0.16.4）：无有效文本且读到 CF_DIB（截图、画图、浏览器复制图片等）──
    // 0.26.9：图片与 HDROP 并存时（资源管理器复制图片文件）按图片条目采集，
    // source_path 取 HDROP 首文件名；纯 HDROP（无 DIB）落文件分支。
    if capture_images
        && let Some(dib) = snap.dib
    {
        if skip_persist {
            tracing::trace!("剪贴板：自写入 skip_persist, 跳过图片入库");
            return;
        }
        let source_path = snap.hdrop_first_name;
        let cache_pool = s.cache_pool.clone();
        let max_image_items = s.max_image_items.load(std::sync::atomic::Ordering::Relaxed);
        let image_id = clipboard_images::generate_image_id();
        let created_at = now_ts();
        tauri::async_runtime::spawn(async move {
            let _order_guard = IMAGE_PROCESS_LOCK.lock().await;
            let processed = tauri::async_runtime::spawn_blocking(move || {
                let Some((bgra, width, height)) = decode_dib(&dib) else {
                    return Err("CF_DIB 解码失败".to_string());
                };
                let png_data =
                    bgra_to_png(&bgra, width, height).map_err(|e| format!("BGRA→PNG 编码失败: {e}"))?;
                let thumb_data = make_thumbnail_png(&bgra, width, height).unwrap_or_else(|e| {
                    tracing::debug!(?e, "剪贴板：缩略图生成失败,使用完整 PNG 兜底");
                    png_data.clone()
                });
                Ok(ClipboardImage {
                    id: image_id,
                    sha256: compute_sha256(&png_data),
                    png_blob: png_data,
                    thumb_blob: thumb_data,
                    width,
                    height,
                    created_at,
                    source_app,
                    source_path,
                })
            })
            .await;
            let image_item = match processed {
                Ok(Ok(item)) => item,
                Ok(Err(e)) => {
                    tracing::debug!(error = %e, "剪贴板图片处理失败,跳过");
                    return;
                }
                Err(e) => {
                    tracing::warn!(?e, "剪贴板图片处理任务失败");
                    return;
                }
            };
            match clipboard_images::save_image(&cache_pool, &image_item).await {
                Ok(_) => tracing::trace!(
                    id = %image_item.id,
                    w = image_item.width,
                    h = image_item.height,
                    "剪贴板：图片已入库"
                ),
                Err(e) => {
                    tracing::warn!(?e, "剪贴板 save_image 失败");
                    return;
                }
            }
            let _ = clipboard_images::cleanup_excess_images(&cache_pool, max_image_items).await;
        });
        return;
    }

    // ── 文件采集（0.26.9）：无有效文本、无图片（或图片关闭）时读 CF_HDROP ──
    // 文件夹也记录路径，不递归扫描；虚拟文件（仅 FileContents/FileGroupDescriptorW）
    // 无 HDROP，天然不采集。
    if !capture_files {
        return;
    }
    let Some(paths) = snap.file_list else {
        tracing::trace!("剪贴板：无文本无图片无文件列表,跳过");
        return;
    };
    // 指纹兜底：文件写回标记 miss 时按路径组合指纹识别，迟到事件不重复入库
    let mut file_skip_persist = skip_persist;
    if self_write.is_none()
        && let Some((label, skip)) = super::take_self_text_fingerprint(fx_hash(&paths.join("\n")))
    {
        tracing::debug!(%label, skip, "剪贴板：文件标记 miss，组合指纹兜底命中自写入");
        source_app = Some(label);
        file_skip_persist = skip;
    }
    if file_skip_persist {
        tracing::trace!("剪贴板：自写入 skip_persist, 跳过文件入库");
        return;
    }
    let pool = s.pool.clone();
    let max_file_items = s.max_file_items.load(std::sync::atomic::Ordering::Relaxed);
    let path_count = paths.len();
    let item = clipboard_files::ClipboardFileItem {
        id: clipboard_files::generate_file_id(),
        paths,
        created_at: now_ts(),
        source_app,
    };
    tauri::async_runtime::spawn(async move {
        let _order_guard = FILE_PERSIST_LOCK.lock().await;
        match clipboard_files::save_file(&pool, &item).await {
            Ok(_) => tracing::trace!(
                id = %item.id,
                count = path_count,
                "剪贴板：文件条目已入库"
            ),
            Err(e) => {
                tracing::debug!(error = %e, "剪贴板 save_file 失败");
                return;
            }
        }
        clipboard_files::cleanup_excess_files(&pool, max_file_items).await;
    });
}

// ── 图片采集辅助函数（0.16.4）─────────────────────────────────────────────

// ── 快照读取（0.26.9）─────────────────────────────────────────────────────

/// 单次剪贴板快照：同一 OpenClipboard 会话内按「文本 → DIB（若开启）→ HDROP（若开启）」读取。
///
/// 文本有效（非空白）即不再读图片/文件（避免 Word 等场景无谓拷贝大 DIB）；
/// 空白文本/格式缺失继续下一类。`hdrop_first_name` 仅在读到 DIB 时提取
/// （图片条目的 source_path）；纯 HDROP 走 `file_list`。
struct ClipboardSnapshot {
    text: Option<String>,
    dib: Option<Vec<u8>>,
    hdrop_first_name: Option<String>,
    file_list: Option<Vec<String>>,
}

fn read_clipboard_snapshot(capture_images: bool, capture_files: bool) -> ClipboardSnapshot {
    const MAX_ATTEMPTS: u32 = 5;
    const BACKOFF_MS: u64 = 8;
    for attempt in 0..MAX_ATTEMPTS {
        unsafe {
            if OpenClipboard(None).is_ok() {
                let text = read_clipboard_inner();
                let need_more = text.as_deref().is_none_or(|t| t.trim().is_empty());
                let mut snap = ClipboardSnapshot {
                    text,
                    dib: None,
                    hdrop_first_name: None,
                    file_list: None,
                };
                if need_more && capture_images {
                    snap.dib = read_dib_inner();
                    if snap.dib.is_some() {
                        snap.hdrop_first_name = read_hdrop_filename_inner();
                    }
                }
                if need_more && snap.dib.is_none() && capture_files {
                    snap.file_list = read_hdrop_file_list_inner();
                }
                let _ = CloseClipboard();
                if attempt > 0 {
                    tracing::debug!(attempt, "剪贴板快照读取重试成功");
                }
                return snap;
            }
        }
        if attempt + 1 < MAX_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(BACKOFF_MS));
        }
    }
    tracing::debug!("剪贴板快照 OpenClipboard 重试仍失败,放弃");
    ClipboardSnapshot {
        text: None,
        dib: None,
        hdrop_first_name: None,
        file_list: None,
    }
}

/// 从 `CF_HDROP` 读取首个文件的文件名（不含路径）。
///
/// 需在 `OpenClipboard` 成功后调用。`CF_HDROP` 不存在时返回 `None`。
unsafe fn read_hdrop_filename_inner() -> Option<String> {
    unsafe {
        let handle = GetClipboardData(CF_HDROP.0.into()).ok()?;
        let hdrop = HDROP(handle.0 as _);
        // 先查路径长度（buffer=None → 返回路径字符数，不含 null）
        let path_len = DragQueryFileW(hdrop, 0, None);
        if path_len == 0 {
            return None;
        }
        // 分配缓冲区（+1 for null terminator）
        let mut buf = vec![0u16; (path_len as usize) + 1];
        let copied = DragQueryFileW(hdrop, 0, Some(&mut buf));
        if copied == 0 {
            return None;
        }
        let full_path = String::from_utf16_lossy(&buf[..copied as usize]);
        // 提取文件名（不含目录路径）
        let file_name = std::path::Path::new(&full_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())?;
        if file_name.is_empty() {
            None
        } else {
            Some(file_name)
        }
    }
}

/// 从 `CF_HDROP` 读取全部路径（0.26.9）。需在 `OpenClipboard` 成功后调用。
///
/// **边界语义**：先查数量再分配；数量/单路径长度/合计 UTF-16 units 任一超限
/// 或路径形态非法（相对路径、设备命名空间、无效编码）整条拒绝返回 `None`，
/// 不保存截断后看似完整的列表。日志只记原因/索引/数量，不记路径原文。
unsafe fn read_hdrop_file_list_inner() -> Option<Vec<String>> {
    unsafe {
        let handle = GetClipboardData(CF_HDROP.0.into()).ok()?;
        let hdrop = HDROP(handle.0 as _);
        // index = 0xFFFFFFFF → 返回文件数
        let count = DragQueryFileW(hdrop, 0xFFFFFFFF, None);
        if count == 0 {
            return None;
        }
        if count as usize > dropfiles::MAX_PATHS {
            tracing::debug!(
                count,
                limit = dropfiles::MAX_PATHS,
                "CF_HDROP 路径数超限,整条跳过"
            );
            return None;
        }
        let mut paths = Vec::with_capacity(count as usize);
        let mut total_units = 0usize;
        for index in 0..count {
            let path_len = DragQueryFileW(hdrop, index, None);
            if path_len == 0 {
                tracing::debug!(index, "CF_HDROP 路径长度查询失败,整条跳过");
                return None;
            }
            if path_len as usize > dropfiles::MAX_PATH_UTF16_UNITS {
                tracing::debug!(
                    index,
                    units = path_len,
                    limit = dropfiles::MAX_PATH_UTF16_UNITS,
                    "CF_HDROP 单路径超限,整条跳过"
                );
                return None;
            }
            total_units += path_len as usize;
            if total_units > dropfiles::MAX_SNAPSHOT_UTF16_UNITS {
                tracing::debug!(
                    index,
                    limit = dropfiles::MAX_SNAPSHOT_UTF16_UNITS,
                    "CF_HDROP 合计载荷超限,整条跳过"
                );
                return None;
            }
            let mut buf = vec![0u16; (path_len as usize) + 1];
            let copied = DragQueryFileW(hdrop, index, Some(&mut buf));
            if copied != path_len {
                tracing::debug!(index, "CF_HDROP 路径复制失败,整条跳过");
                return None;
            }
            let path = match String::from_utf16(&buf[..copied as usize]) {
                Ok(p) => p,
                Err(_) => {
                    tracing::debug!(index, "CF_HDROP 路径无效编码,整条跳过");
                    return None;
                }
            };
            if let Err(reject) = dropfiles::validate_single_path(&path) {
                tracing::debug!(index, reason = reject.reason(), "CF_HDROP 路径形态非法,整条跳过");
                return None;
            }
            paths.push(path);
        }
        Some(paths)
    }
}

unsafe fn read_dib_inner() -> Option<Vec<u8>> {
    unsafe {
        let handle = GetClipboardData(CF_DIB.0.into()).ok()?;
        let hg = HGLOBAL(handle.0);
        let size = GlobalSize(hg);
        if size == 0 {
            return None;
        }
        let ptr = GlobalLock(hg);
        if ptr.is_null() {
            let _ = GlobalUnlock(hg);
            return None;
        }
        let slice = std::slice::from_raw_parts(ptr as *const u8, size as usize);
        let data = slice.to_vec();
        let _ = GlobalUnlock(hg);
        Some(data)
    }
}

/// 解码 DIB（BITMAPINFOHEADER + 像素）为 BGRA 像素 + 宽高。
///
/// CF_DIB 格式：BITMAPINFOHEADER 后紧跟像素数据。
/// - biHeight 正值 = bottom-up（最常见），负值 = top-down（罕见）
/// - biBitCount 32 = BGRA（每像素 4 字节）；24 = BGR（每像素 3 字节）
///
/// 返回 top-down BGRA（与截图 `write_bgra_to_clipboard` 输入格式一致）。
fn decode_dib(dib: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;

    let header_size = std::mem::size_of::<BITMAPINFOHEADER>();
    if dib.len() < header_size {
        tracing::debug!("DIB 数据过短: {} < {header_size}", dib.len());
        return None;
    }

    // 安全读取 BITMAPINFOHEADER
    let header: BITMAPINFOHEADER = unsafe { std::ptr::read_unaligned(dib.as_ptr() as *const _) };
    let width = header.biWidth as u32;
    let raw_height = header.biHeight;
    let height = raw_height.unsigned_abs() as u32;
    let bit_count = header.biBitCount;
    let compression = header.biCompression; // 0 = BI_RGB

    if width == 0 || height == 0 {
        return None;
    }

    // P1-#14 fix: 尺寸上界检查——防止恶意/损坏 DIB 导致分配过大或乘法溢出 panic
    if width > 65535 || height > 65535 {
        tracing::warn!(width, height, "DIB 尺寸过大，拒绝解码");
        return None;
    }

    let bpp = (bit_count / 8) as usize; // bytes per pixel
    if bpp != 4 && bpp != 3 {
        tracing::debug!("不支持的 DIB 位深: {bit_count}");
        return None;
    }
    if compression != 0 {
        tracing::debug!("不支持的 DIB 压缩格式: {compression}");
        return None;
    }

    let row_bytes = width as usize * bpp;
    // BMP 行对齐到 4 字节
    let row_stride = (row_bytes + 3) & !3;
    let expected_pixel_size = row_stride * height as usize;
    if dib.len() < header_size + expected_pixel_size {
        tracing::debug!(
            "DIB 像素数据不足: {} < {}",
            dib.len(),
            header_size + expected_pixel_size
        );
        return None;
    }

    let pixel_data = &dib[header_size..];

    // 统一输出为 top-down BGRA（P1-#14 fix: checked_mul 防溢出）
    let bgra_size = (width as usize)
        .checked_mul(height as usize)
        .and_then(|p| p.checked_mul(4))?;
    let mut bgra = vec![0u8; bgra_size];
    let out_row_bytes = width as usize * 4;
    let is_bottom_up = raw_height > 0;

    for y in 0..height as usize {
        // bottom-up: 第一行是图片底部；翻转行序
        let src_y = if is_bottom_up {
            height as usize - 1 - y
        } else {
            y
        };
        let src_offset = src_y * row_stride;
        let dst_offset = y * out_row_bytes;

        if bpp == 4 {
            // BGRA → BGRA（直接拷贝）
            bgra[dst_offset..dst_offset + row_bytes]
                .copy_from_slice(&pixel_data[src_offset..src_offset + row_bytes]);
        } else {
            // BGR → BGRA（补 A=255）
            for x in 0..width as usize {
                let si = src_offset + x * 3;
                let di = dst_offset + x * 4;
                bgra[di] = pixel_data[si]; // B
                bgra[di + 1] = pixel_data[si + 1]; // G
                bgra[di + 2] = pixel_data[si + 2]; // R
                bgra[di + 3] = 255; // A
            }
        }
    }

    Some((bgra, width, height))
}

/// BGRA 像素 → PNG 字节。输入为 top-down BGRA。
fn bgra_to_png(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    use png::ColorType;

    let mut buf = Vec::new();
    // BGRA → RGBA：swap R↔B
    let mut rgba = bgra.to_vec();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(ColorType::Rgba);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG 写 header 失败: {e}"))?;
        writer
            .write_image_data(&rgba)
            .map_err(|e| format!("PNG 写像素失败: {e}"))?;
    }
    Ok(buf)
}

/// 生成缩略图 PNG（max 边 256px）。输入为 top-down BGRA。
///
/// 使用 nearest-neighbor 采样——缩略图只用于列表预览，不需要高质量缩放。
fn make_thumbnail_png(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    use png::ColorType;

    // 计算缩略图尺寸
    let max_edge = width.max(height);
    if max_edge <= THUMB_MAX_EDGE {
        // 图片已经够小，直接用原图
        return bgra_to_png(bgra, width, height);
    }
    let scale = THUMB_MAX_EDGE as f64 / max_edge as f64;
    let tw = (width as f64 * scale).round() as u32;
    let th = (height as f64 * scale).round() as u32;
    if tw == 0 || th == 0 {
        return Err("缩略图尺寸为 0".into());
    }

    let src_row_bytes = width as usize * 4;
    let dst_row_bytes = tw as usize * 4;
    let mut thumb = vec![0u8; dst_row_bytes * th as usize];

    // nearest-neighbor 采样
    for dy in 0..th as usize {
        let sy = ((dy as f64 / scale).round() as usize).min(height as usize - 1);
        for dx in 0..tw as usize {
            let sx = ((dx as f64 / scale).round() as usize).min(width as usize - 1);
            let src = sy * src_row_bytes + sx * 4;
            let dst = dy * dst_row_bytes + dx * 4;
            thumb[dst..dst + 4].copy_from_slice(&bgra[src..src + 4]);
        }
    }

    // BGRA → RGBA → PNG
    for px in thumb.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, tw, th);
        encoder.set_color(ColorType::Rgba);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("缩略图 PNG header 失败: {e}"))?;
        writer
            .write_image_data(&thumb)
            .map_err(|e| format!("缩略图 PNG 像素失败: {e}"))?;
    }
    Ok(buf)
}

/// 计算 SHA-256 哈希（十六进制字符串）。
fn compute_sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex_encode(&result)
}

/// 十六进制编码（不依赖 hex crate）。
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 快哈希（FxHash 简化版）:去重仅用于短窗口冲突判断,不追求密码学强度。
pub(super) fn fx_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h = h.wrapping_mul(0x100000001b3).wrapping_add(*b as u64);
    }
    h
}
fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 读当前剪贴板文本（0.9.7：公开给 read_clipboard Capability）。
///
/// 含短重试（与监听器内部同一逻辑），读文本成功返回 Some，非文本/空返回 None。
pub fn read_current_text() -> Option<String> {
    read_clipboard_text()
}

/// 读当前剪贴板图片（0.19.1：供 read_clipboard Capability 图片分支用）。
///
/// 尝试读 CF_DIB → 解码 DIB → BGRA → PNG。成功返回 `Some(png_bytes)`，
/// 非图片剪贴板（文本/文件列表/空）返回 `None`。
///
/// 含短重试（与 `read_current_text` 同一逻辑），读不到返回 None 不报错。
pub fn read_current_image() -> Option<Vec<u8>> {
    const MAX_ATTEMPTS: u32 = 5;
    const BACKOFF_MS: u64 = 8;
    for attempt in 0..MAX_ATTEMPTS {
        unsafe {
            if OpenClipboard(None).is_ok() {
                let dib = read_dib_inner();
                let _ = CloseClipboard();
                if let Some(dib_data) = dib {
                    if attempt > 0 {
                        tracing::debug!(attempt, "剪贴板图片读取重试成功");
                    }
                    let (bgra, width, height) = decode_dib(&dib_data)?;
                    let png = bgra_to_png(&bgra, width, height).ok()?;
                    tracing::debug!(
                        width,
                        height,
                        bytes = png.len(),
                        "read_current_image: 读到图片"
                    );
                    return Some(png);
                }
                // Open 成功但读 DIB 失败（非 CF_DIB） —— 不是竞争问题，不重试
                return None;
            }
        }
        if attempt + 1 < MAX_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(BACKOFF_MS));
        }
    }
    tracing::debug!("剪贴板图片 OpenClipboard 重试仍失败,放弃");
    None
}

fn read_clipboard_text() -> Option<String> {
    // 短重试 + 微退避:写入方(浏览器/编辑器)刚 SetClipboardData + CloseClipboard,
    // 我们收到 WM_CLIPBOARDUPDATE 立即 OpenClipboard 有几率抢不过 —— 系统内部把
    // 剪贴板持有权切给写入方的窗口需要几毫秒。不重试会直接漏掉这次变化,导致
    // AwarenessSnapshot 不刷新(bug-2026-07-09)。
    //
    // 最多重试 5 次,每次退避 8ms,总上限 ~40ms —— clipboard listener 独立线程
    // 消息循环,阻塞可接受;不至于让消息循环卡到影响后续 WM 到达。
    const MAX_ATTEMPTS: u32 = 5;
    const BACKOFF_MS: u64 = 8;
    for attempt in 0..MAX_ATTEMPTS {
        unsafe {
            if OpenClipboard(None).is_ok() {
                let res = read_clipboard_inner();
                let _ = CloseClipboard();
                if res.is_some() {
                    if attempt > 0 {
                        tracing::debug!(attempt, "剪贴板读取重试成功");
                    }
                    return res;
                }
                // Open 成功但读文本失败(非 CF_UNICODETEXT 等) —— 不是竞争问题,不重试
                return None;
            }
        }
        // Open 失败(竞争) —— 退避后重试
        if attempt + 1 < MAX_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(BACKOFF_MS));
        }
    }
    tracing::debug!(
        attempts = MAX_ATTEMPTS,
        "剪贴板 OpenClipboard 重试仍失败,放弃"
    );
    None
}

unsafe fn read_clipboard_inner() -> Option<String> {
    // Rust 2024：unsafe fn 内不再默认 unsafe 上下文，显式包块
    unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT.0.into()).ok()?;
        let hg = HGLOBAL(handle.0);
        let ptr = GlobalLock(hg);
        if ptr.is_null() {
            let _ = GlobalUnlock(hg);
            return None;
        }
        let result = PCWSTR(ptr as *const u16).to_string().ok();
        let _ = GlobalUnlock(hg);
        result
    }
}

/// 把 **BGRA** 像素数据写入系统剪贴板（CF_DIB 格式）—— **raw 内核**（不打自写入标记）。
///
/// `pixels` 格式：BGRA、top-down、每行 `width * 4` 字节。CF_DIB 要求 BGRA + bottom-up，
/// 所以只做 top-down → bottom-up 翻转（`copy_from_slice` 整行拷贝，不再逐像素 shuffle）。
///
/// **失败清理**：`SetClipboardData` 成功前所有权始终在调用方；任何中途错误都必须
/// `GlobalFree` 释放 hmem（截图一次 ~14MB DIB，泄漏会累积）。`OpenClipboard` 成功
/// 后无论后续走哪条分支都必须 `CloseClipboard`，否则会锁住系统剪贴板一段时间。
///
/// **0.17.9**：重命名为 `_raw`——外部应走 `mod.rs` 的 `write_bgra_to_clipboard`（打标外壳），
/// 仅 `write_png_to_clipboard` 内部转调本函数（避免重复打标）。
pub(super) fn write_bgra_to_clipboard_raw(
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<(), String> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_DIB;

    let row_bytes = width as usize * 4;
    let expected = row_bytes * height as usize;
    if pixels.len() != expected {
        return Err(format!(
            "像素数据长度不匹配: {} vs {expected}",
            pixels.len()
        ));
    }

    // 构造 DIB：BITMAPINFOHEADER + BGRA 像素（CF_DIB 要求 BGRA + bottom-up）
    let header_size = std::mem::size_of::<BITMAPINFOHEADER>();
    let dib_size = header_size + expected;

    unsafe {
        let hmem =
            GlobalAlloc(GMEM_MOVEABLE, dib_size).map_err(|e| format!("GlobalAlloc 失败: {e}"))?;

        // 从这里往下的任意 early return 都必须 GlobalFree(hmem)——只有
        // SetClipboardData 成功后所有权才转给系统。
        let fill_result = (|| -> Result<(), String> {
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                return Err("GlobalLock 失败".into());
            }

            // 写 BITMAPINFOHEADER
            let header = BITMAPINFOHEADER {
                biSize: header_size as u32,
                biWidth: width as i32,
                biHeight: height as i32, // 正值 = bottom-up
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                biSizeImage: expected as u32,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            std::ptr::copy_nonoverlapping(
                &header as *const _ as *const u8,
                ptr as *mut u8,
                header_size,
            );

            // 写像素：仅做 top-down → bottom-up 翻转（整行 memcpy，无逐像素 shuffle）
            let pixel_dst = (ptr as *mut u8).add(header_size);
            let stride = row_bytes;
            for y in 0..height as usize {
                let src_row = &pixels[y * stride..(y + 1) * stride];
                // bottom-up: 目标行 = (height - 1 - y)
                let dst_row = std::slice::from_raw_parts_mut(
                    pixel_dst.add((height as usize - 1 - y) * stride),
                    stride,
                );
                dst_row.copy_from_slice(src_row);
            }

            let _ = GlobalUnlock(hmem);
            Ok(())
        })();

        if let Err(e) = fill_result {
            let _ = GlobalFree(Some(hmem));
            return Err(e);
        }

        // OpenClipboard 后必须成对 CloseClipboard——包一层保证任意分支都清
        if let Err(e) = OpenClipboard(None) {
            let _ = GlobalFree(Some(hmem));
            return Err(format!("OpenClipboard 失败: {e}"));
        }

        let set_result: Result<(), String> = (|| {
            let _ = EmptyClipboard();
            SetClipboardData(CF_DIB.0.into(), Some(HANDLE(hmem.0 as _)))
                .map_err(|e| format!("SetClipboardData 失败: {e}"))?;
            Ok(())
        })();
        let _ = CloseClipboard();

        if let Err(e) = set_result {
            // SetClipboardData 失败时所有权未移交，仍需 free
            let _ = GlobalFree(Some(hmem));
            return Err(e);
        }
    }

    tracing::debug!(width, height, "截图已写入剪贴板");
    Ok(())
}

/// 把文本写入系统剪贴板（CF_UNICODETEXT 格式）—— **raw 内核**（不打自写入标记）。
///
/// 与 `write_bgra_to_clipboard_raw` 同模式：GlobalAlloc → GlobalLock → 写 UTF-16 →
/// EmptyClipboard → SetClipboardData。失败清理 GlobalFree，成对 CloseClipboard。
///
/// **0.17.9**：重命名为 `_raw`——外部应走 `mod.rs` 的 `write_text_to_clipboard`（打标外壳）。
pub(super) fn write_text_to_clipboard_raw(text: &str) -> Result<(), String> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    // 编码 UTF-16 + null 终止符
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0); // null terminator
    let byte_len = wide.len() * 2;

    unsafe {
        let hmem =
            GlobalAlloc(GMEM_MOVEABLE, byte_len).map_err(|e| format!("GlobalAlloc 失败: {e}"))?;

        let fill_result = (|| -> Result<(), String> {
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                return Err("GlobalLock 失败".into());
            }
            // 写 UTF-16 字节（外层已 unsafe，内层不需再包）
            let byte_slice = std::slice::from_raw_parts(wide.as_ptr() as *const u8, byte_len);
            std::ptr::copy_nonoverlapping(byte_slice.as_ptr(), ptr as *mut u8, byte_len);
            let _ = GlobalUnlock(hmem);
            Ok(())
        })();

        if let Err(e) = fill_result {
            let _ = GlobalFree(Some(hmem));
            return Err(e);
        }

        if let Err(e) = OpenClipboard(None) {
            let _ = GlobalFree(Some(hmem));
            return Err(format!("OpenClipboard 失败: {e}"));
        }

        let set_result: Result<(), String> = (|| {
            let _ = EmptyClipboard();
            SetClipboardData(CF_UNICODETEXT.0.into(), Some(HANDLE(hmem.0 as _)))
                .map_err(|e| format!("SetClipboardData 失败: {e}"))?;
            Ok(())
        })();
        let _ = CloseClipboard();

        if let Err(e) = set_result {
            let _ = GlobalFree(Some(hmem));
            return Err(e);
        }
    }

    tracing::debug!(len = text.chars().count(), "文本已写入剪贴板");
    Ok(())
}

/// 把文件路径列表写入系统剪贴板（CF_HDROP 格式，0.26.9）—— **raw 内核**（不打自写入标记）。
///
/// 载荷 = `DROPFILES` 头（fWide=1）+ 每路径 UTF-16 + NUL + 末尾额外 NUL（双 NUL 终止），
/// 编码见 `dropfiles::encode_dropfiles`（纯函数，含编解码往返测试）。
///
/// 与 `write_text_to_clipboard_raw` 同模式：GlobalAlloc → GlobalLock → 写载荷 →
/// EmptyClipboard → SetClipboardData(CF_HDROP)。**复制语义**：不继承剪切标记
/// （DROPFILES 的 fNC/pt 保持默认），失败清理 GlobalFree，成对 CloseClipboard。
pub(super) fn write_file_list_raw(paths: &[String]) -> Result<(), String> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_HDROP;

    let units = dropfiles::encode_dropfiles(paths)?;
    let byte_len = units.len() * 2;
    if byte_len == 0 {
        return Err("文件列表为空".into());
    }

    unsafe {
        let hmem =
            GlobalAlloc(GMEM_MOVEABLE, byte_len).map_err(|e| format!("GlobalAlloc 失败: {e}"))?;

        // SetClipboardData 成功前所有权始终在调用方；任何 early return 必须 GlobalFree
        let fill_result = (|| -> Result<(), String> {
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                return Err("GlobalLock 失败".into());
            }
            let byte_slice =
                std::slice::from_raw_parts(units.as_ptr() as *const u8, byte_len);
            std::ptr::copy_nonoverlapping(byte_slice.as_ptr(), ptr as *mut u8, byte_len);
            let _ = GlobalUnlock(hmem);
            Ok(())
        })();

        if let Err(e) = fill_result {
            let _ = GlobalFree(Some(hmem));
            return Err(e);
        }

        if let Err(e) = OpenClipboard(None) {
            let _ = GlobalFree(Some(hmem));
            return Err(format!("OpenClipboard 失败: {e}"));
        }

        let set_result: Result<(), String> = (|| {
            let _ = EmptyClipboard();
            SetClipboardData(CF_HDROP.0.into(), Some(HANDLE(hmem.0 as _)))
                .map_err(|e| format!("SetClipboardData 失败: {e}"))?;
            Ok(())
        })();
        let _ = CloseClipboard();

        if let Err(e) = set_result {
            let _ = GlobalFree(Some(hmem));
            return Err(e);
        }
    }

    tracing::debug!(count = paths.len(), "文件列表已写入剪贴板 (CF_HDROP)");
    Ok(())
}
