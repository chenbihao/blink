//! 剪贴板历史监听（0.8.5）：AddClipboardFormatListener 隐藏窗口 → WM_CLIPBOARDUPDATE → 存。
//!
//! **架构（低耦合，仿 selection 范式）**：监听器只依赖 `data::clipboard`（存）+
//! `config::ClipboardConfig`（配置）。不持有 AppHandle、不 emit 事件、不调 domain/commands。
//! 前端读 db（`get_clipboard_history` command）与监听器完全解耦——监听器只管写，
//! 前端只管读，两者不直接对接。
//!
//! **0.9.2.1 补丁**：为了修主窗口保持打开时剪贴板变化 AwarenessSnapshot 不刷新的 bug,
//! 引入 `set_change_hook()` 注册一个泛型回调——listener 侧仍不认 SearchService/domain,
//! 只在剪贴板文本通过 title 黑名单 + 去重后同步调 hook。调用侧（`main.rs`）负责
//! ContextConfig 门控 + 回写 SearchService。listener 架构解耦精神保持。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use sqlx::SqlitePool;

use crate::infra::data::clipboard::ClipboardConfig;

#[cfg(target_os = "windows")]
mod windows;

pub(super) struct State {
    pool: SqlitePool,
    /// cache 库——clipboard_images 表所在（0.16.4 图片历史）。
    cache_pool: SqlitePool,
    blacklist: RwLock<Vec<String>>,
    max_items: u32,
    /// 图片上限（0.16.4）。独立配置，§5.5「独立配置」。
    max_image_items: u32,
    /// 是否采集剪贴板图片（0.16.4）。false 时跳过 CF_DIB 采集。
    capture_images: bool,
}

static STATE: OnceLock<State> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

// ── 0.17.9 自写入标记机制 ─────────────────────────────────────────────
//
// blink 自己写剪贴板有 4 个入口（screenshot_copy / screenshot_copy_region /
// copy_clipboard_image 历史回贴 / write_clipboard Capability）。监听器无脑按
// 前台窗口记来源，自写入时记成别的应用；历史回贴更会因 sha256「删旧留新」覆盖
// 掉原来的真实来源。
//
// **机制**：写入前 `mark_self_write(label, skip_persist)` → 监听器开头
// `take_self_write()` 消费标记。标签存语义 key（`blink:screenshot` 等），
// `clipboard_engine.rs::resolve_source_desc` 查映射表转文案。
//
// **拆「打标外壳 + 不打标内核」**：`write_bgra_to_clipboard_raw` / `write_text_to_clipboard_raw`
// 是不打标的内核（`windows.rs`）；`write_*` 是打标外壳（本文件），内部先 mark 再调 raw。
// `write_png_to_clipboard` 标记后直接调 raw（不经过 `write_bgra_to_clipboard` 外壳），避免重复打标。

/// 自写入标记 TTL——写入到 WM_CLIPBOARDUPDATE 到达之间的兜底超时。
/// 正常流程 <50ms，500ms 留足余量（大 DIB 写入 + 系统调度延迟）。
const SELF_WRITE_TTL: Duration = Duration::from_millis(500);

/// 自写入标记语义 key 常量。
pub const SELF_LABEL_SCREENSHOT: &str = "blink:screenshot";
pub const SELF_LABEL_REPOST: &str = "blink:repost";
pub const SELF_LABEL_APP: &str = "blink:app";
pub const SELF_LABEL_BLINK: &str = "blink:ai";

/// 进程级自写入标记（0.17.9）。
struct SelfWriteMark {
    /// 语义 key（`blink:screenshot` / `blink:repost` / `blink:ai`）。
    label: String,
    /// `true` = 跳过入库但保留 `notify_change`（历史回贴场景）。
    skip_persist: bool,
    /// 打标时间，用于 TTL 过期判断。
    timestamp: Instant,
}

static SELF_WRITE: Mutex<Option<SelfWriteMark>> = Mutex::new(None);

/// 打自写入标记（写入剪贴板前调）。
///
/// 内聚在 `clipboard` 模块：`write_*` 外壳函数内部调，调用方只多传 `&str` label。
fn mark_self_write(label: &str, skip_persist: bool) {
    if let Ok(mut guard) = SELF_WRITE.lock() {
        *guard = Some(SelfWriteMark {
            label: label.to_string(),
            skip_persist,
            timestamp: Instant::now(),
        });
    }
}

/// 消费自写入标记（监听器 `on_clipboard_change` 开头调）。
///
/// 命中且未过期 → 返回 `(label, skip_persist)`；未命中或已过期 → `None`。
/// 取出即清空（一次性语义），防止后续非自写入的剪贴板变化误命中。
pub(super) fn take_self_write() -> Option<(String, bool)> {
    let mut guard = SELF_WRITE.lock().ok()?;
    let mark = guard.take()?; // 取出即清空
    if mark.timestamp.elapsed() > SELF_WRITE_TTL {
        tracing::trace!(label = %mark.label, "自写入标记已过期,丢弃");
        return None;
    }
    Some((mark.label, mark.skip_persist))
}

// ── 打标外壳函数（0.17.9）──────────────────────────────────────────────────
//
// 外部调用方传 `label` + `skip_persist`，内部先 `mark_self_write` 再调 raw 内核。

/// 把 **BGRA** 像素数据写入系统剪贴板（CF_DIB 格式）—— 打标外壳。
///
/// 内部先 `mark_self_write(label, skip_persist)`，再调 `write_bgra_to_clipboard_raw`。
#[cfg(target_os = "windows")]
pub fn write_bgra_to_clipboard(
    pixels: &[u8],
    width: u32,
    height: u32,
    label: &str,
    skip_persist: bool,
) -> Result<(), String> {
    mark_self_write(label, skip_persist);
    windows::write_bgra_to_clipboard_raw(pixels, width, height)
}

/// 剪贴板文本变化的观察者回调（0.9.2.1）。
///
/// **契约**：只在 title 黑名单过滤 + 短窗口去重 + 非空文本三关都过后触发。
/// 参数是刚入库的文本引用——回调应尽快返回,不要阻塞监听线程（内部持有 clipboard
/// 消息循环）。跨 send 边界用 `Fn + Send + Sync`。
pub type ChangeHook = Box<dyn Fn(&str) + Send + Sync + 'static>;
static CHANGE_HOOK: OnceLock<ChangeHook> = OnceLock::new();

/// 剪贴板最后一次文本变化的真实时间戳（hook 触发前记录）。
///
/// 供 `context::collect()` 使用——避免 Clipboard 的 `captured_at` 总是 `Instant::now()`
/// （invoke 瞬间），导致与 Selection 的真实采集时间戳比较无意义。
static LAST_CHANGED_AT: OnceLock<RwLock<Option<Instant>>> = OnceLock::new();

/// 取剪贴板最后一次文本变化的时间戳。
///
/// 返回 `None` 表示自进程启动以来剪贴板从未变化过（或 hook 未触发过）。
pub fn last_changed_at() -> Option<Instant> {
    LAST_CHANGED_AT.get().and_then(|lock| *lock.read().unwrap())
}

/// 启动剪贴板监听（幂等）。监听线程持有 pool + cfg，WM_CLIPBOARDUPDATE 时存。
/// 仿 selection：监听窗口一旦创建不卸，关闭态靠 ACTIVE 短路（跨线程卸载不安全）。
pub fn start_listener(pool: SqlitePool, cache_pool: SqlitePool, cfg: ClipboardConfig) {
    let _ = STATE.set(State {
        pool,
        cache_pool,
        blacklist: RwLock::new(cfg.blacklist_keywords.clone()),
        max_items: cfg.max_items,
        max_image_items: cfg.max_image_items,
        capture_images: cfg.capture_images,
    });
    ACTIVE.store(cfg.enabled, Ordering::Relaxed);
    #[cfg(target_os = "windows")]
    {
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(windows::start_watcher_thread);
    }
    tracing::debug!(enabled = cfg.enabled, "剪贴板监听已就绪");
}

/// 热切换开关（0.8.7 起 `update_clipboard_enabled` 会调它做真热切）。
pub fn set_active(active: bool) {
    ACTIVE.store(active, Ordering::Relaxed);
    tracing::debug!(active, "剪贴板监听 active 切换");
}

/// 热更新黑名单（设置页改调）。
#[allow(dead_code)] // 设置页 API 预留（当前 commands 层直接更新 config）
pub fn set_blacklist(keywords: Vec<String>) {
    if let Some(s) = STATE.get() {
        *s.blacklist.write().unwrap() = keywords;
    }
}

#[cfg(target_os = "windows")]
pub(super) fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

#[cfg(target_os = "windows")]
pub(super) fn state() -> Option<&'static State> {
    STATE.get()
}

/// 注册剪贴板文本变化的观察者回调（0.9.2.1，一次性，OnceLock 兜底避免重复注册）。
///
/// **调用时机**：`main.rs::setup` 里在 SearchService 已构造 + AppHandle 可用后调用。
/// 回调闭包需自行 clone `Arc<SearchService>` 并持有 `AppHandle`。
///
/// **重复调用**：静默忽略（OnceLock 语义）；测试场景先 `hook` 后 `start_listener` 也 OK。
pub fn set_change_hook(hook: ChangeHook) {
    if CHANGE_HOOK.set(hook).is_err() {
        tracing::debug!("剪贴板 change hook 已注册过,忽略后续注册");
    }
}

/// 内部触发（windows.rs 调用）——非空文本 + 已入库策略后触发一次。
///
/// 在调用 hook 前先记录 `LAST_CHANGED_AT`，确保 `context::collect()` 读到的
/// Clipboard `captured_at` 是"剪贴板真正变化的瞬间"，而非 invoke 的 `Instant::now()`。
#[cfg(target_os = "windows")]
pub(super) fn notify_change(text: &str) {
    // 先记录时间戳（hook 可能触发 update_clipboard_text → upsert_text，
    // 但 collect() 用的是本时间戳而非 upsert 的 Instant::now()）
    let now = Instant::now();
    *LAST_CHANGED_AT
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap() = Some(now);
    if let Some(hook) = CHANGE_HOOK.get() {
        hook(text);
    }
}

/// 把 **PNG 字节**解码为 BGRA 后写入剪贴板（0.11.7）—— 打标外壳。
///
/// 前端合成 PNG（裁剪区 + 标注）后通过 command 传给后端，后端解码为 BGRA
/// 再走 `write_bgra_to_clipboard_raw` 写入 CF_DIB。相比直接传 BGRA 多一次解码 + swap
/// 开销，但避免了前端传 BGRA 的 IPC 大 payload（PNG 压缩后小 5-10x）。
///
/// **0.17.9**：先 `mark_self_write`，再解码 + 调 raw（不经过 `write_bgra_to_clipboard` 外壳，避免重复打标）。
#[cfg(target_os = "windows")]
pub fn write_png_to_clipboard(
    png_data: &[u8],
    label: &str,
    skip_persist: bool,
) -> Result<(), String> {
    use png::ColorType;
    mark_self_write(label, skip_persist);
    tracing::debug!(bytes = png_data.len(), "write_png_to_clipboard: 开始解码");

    let decoder = png::Decoder::new(std::io::Cursor::new(png_data));
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("PNG 读取失败: {e}"))?;
    let (w, h) = (reader.info().width, reader.info().height);
    let color_type = reader.info().color_type;
    tracing::debug!(w, h, ?color_type, "write_png_to_clipboard: PNG header");

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("PNG 解码失败: {e}"))?;
    // 截断到实际解码字节数（`output_buffer_size` 可能包含 padding）
    buf.truncate(info.buffer_size());

    match info.color_type {
        ColorType::Rgba => {
            // RGBA → BGRA：swap R↔B
            let mut bgra = buf;
            for px in bgra.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
            windows::write_bgra_to_clipboard_raw(&bgra, w, h)
        }
        ColorType::Rgb => {
            // RGB → 扩展为 BGRA（A=255）
            let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
            for chunk in buf.chunks_exact(3) {
                bgra.extend_from_slice(&[chunk[2], chunk[1], chunk[0], 255]);
            }
            windows::write_bgra_to_clipboard_raw(&bgra, w, h)
        }
        ColorType::GrayscaleAlpha => {
            // GrayscaleAlpha → 扩展为 BGRA（灰度值三通道相同）
            let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
            for chunk in buf.chunks_exact(2) {
                let gray = chunk[0];
                bgra.extend_from_slice(&[gray, gray, gray, chunk[1]]);
            }
            windows::write_bgra_to_clipboard_raw(&bgra, w, h)
        }
        ColorType::Grayscale => {
            // Grayscale → 扩展为 BGRA（灰度值三通道相同，A=255）
            let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
            for &gray in &buf {
                bgra.extend_from_slice(&[gray, gray, gray, 255]);
            }
            windows::write_bgra_to_clipboard_raw(&bgra, w, h)
        }
        other => Err(format!(
            "不支持的 PNG 颜色类型: {other:?}，期望 RGBA/RGB/Grayscale/GrayscaleAlpha"
        )),
    }
}

/// 读当前剪贴板文本（0.9.7 read_clipboard Capability）。
///
/// 返回 `Some(text)` = 文本剪贴板；`None` = 空/非文本（图片/文件列表）。
/// 含短重试（与监听器同一逻辑），读不到返回 None 不报错。
#[cfg(target_os = "windows")]
pub fn read_current_text() -> Option<String> {
    windows::read_current_text()
}

/// 读当前剪贴板图片（0.19.1 read_clipboard Capability 图片分支）。
///
/// 返回 `Some(png_bytes)` = 图片剪贴板（PNG 字节）；`None` = 空/非图片。
/// 含短重试（与 `read_current_text` 同一逻辑）。
#[cfg(target_os = "windows")]
pub fn read_current_image() -> Option<Vec<u8>> {
    windows::read_current_image()
}

/// 把文本写入系统剪贴板（CF_UNICODETEXT 格式）—— 打标外壳。
///
/// 内部先 `mark_self_write(label, skip_persist)`，再调 `write_text_to_clipboard_raw`。
#[cfg(target_os = "windows")]
pub fn write_text_to_clipboard(text: &str, label: &str, skip_persist: bool) -> Result<(), String> {
    mark_self_write(label, skip_persist);
    windows::write_text_to_clipboard_raw(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_self_write_returns_none_when_no_mark() {
        // 确保没有残留标记
        if let Ok(mut guard) = SELF_WRITE.lock() {
            *guard = None;
        }
        let result = take_self_write();
        assert!(result.is_none(), "无标记时应返回 None");
    }

    #[test]
    fn take_self_write_returns_label_and_skip_after_mark() {
        if let Ok(mut guard) = SELF_WRITE.lock() {
            *guard = None;
        }
        mark_self_write("blink:screenshot", false);
        let result = take_self_write();
        assert!(result.is_some(), "打标后应能取到");
        let (label, skip) = result.unwrap();
        assert_eq!(label, "blink:screenshot");
        assert!(!skip, "skip_persist=false 应保留");
    }

    #[test]
    fn take_self_write_clears_mark() {
        if let Ok(mut guard) = SELF_WRITE.lock() {
            *guard = None;
        }
        mark_self_write("blink:test", true);
        let first = take_self_write();
        assert!(first.is_some(), "首次 take 应命中");
        let second = take_self_write();
        assert!(second.is_none(), "二次 take 应返回 None（已清空）");
    }

    #[test]
    fn take_self_write_preserves_skip_persist_true() {
        if let Ok(mut guard) = SELF_WRITE.lock() {
            *guard = None;
        }
        mark_self_write("blink:repost", true);
        let result = take_self_write();
        assert!(result.is_some());
        let (_, skip) = result.unwrap();
        assert!(skip, "skip_persist=true 应保留");
    }

    #[test]
    fn take_self_write_expired_mark_returns_none() {
        // 直接构造过期标记（绕过 mark_self_write 的时间戳）
        if let Ok(mut guard) = SELF_WRITE.lock() {
            *guard = Some(SelfWriteMark {
                label: "blink:expired".to_string(),
                skip_persist: false,
                timestamp: Instant::now() - Duration::from_secs(2),
            });
        }
        let result = take_self_write();
        assert!(result.is_none(), "过期标记应返回 None");
        // 过期标记也应被清空
        if let Ok(guard) = SELF_WRITE.lock() {
            assert!(guard.is_none(), "过期标记 take 后应清空");
        }
    }

    #[test]
    fn label_constants_are_correct() {
        assert_eq!(SELF_LABEL_SCREENSHOT, "blink:screenshot");
        assert_eq!(SELF_LABEL_REPOST, "blink:repost");
        assert_eq!(SELF_LABEL_APP, "blink:app");
        assert_eq!(SELF_LABEL_BLINK, "blink:ai");
    }
}
