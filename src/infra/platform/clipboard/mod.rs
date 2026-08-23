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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
    max_items: AtomicU32,
    /// 图片上限（0.16.4）。独立配置，§5.5「独立配置」。
    max_image_items: AtomicU32,
    /// 是否采集剪贴板图片（0.16.4）。false 时跳过 CF_DIB 采集。
    capture_images: AtomicBool,
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
const MAX_PENDING_SELF_WRITES: usize = 32;

/// 自写入标记语义 key 常量。
pub const SELF_LABEL_SCREENSHOT: &str = "blink:screenshot";
pub const SELF_LABEL_REPOST: &str = "blink:repost";
pub const SELF_LABEL_APP: &str = "blink:app";
pub const SELF_LABEL_BLINK: &str = "blink:ai";

/// 进程级自写入标记（0.17.9）。
struct SelfWriteMark {
    id: u64,
    /// 语义 key（`blink:screenshot` / `blink:repost` / `blink:ai`）。
    label: String,
    /// `true` = 跳过入库但保留 `notify_change`（历史回贴场景）。
    skip_persist: bool,
    /// 打标时间，用于 TTL 过期判断。
    timestamp: Instant,
}

#[derive(Default)]
struct PendingSelfWrites {
    marks: VecDeque<SelfWriteMark>,
}

impl PendingSelfWrites {
    fn push(&mut self, mark: SelfWriteMark) {
        self.discard_expired();
        if self.marks.len() >= MAX_PENDING_SELF_WRITES
            && let Some(dropped) = self.marks.pop_front()
        {
            tracing::debug!(label = %dropped.label, "自写入标记队列已满,丢弃最旧标记");
        }
        self.marks.push_back(mark);
    }

    fn cancel(&mut self, id: u64) {
        if let Some(index) = self.marks.iter().position(|mark| mark.id == id) {
            self.marks.remove(index);
        }
    }

    fn take(&mut self) -> Option<SelfWriteMark> {
        self.discard_expired();
        self.marks.pop_front()
    }

    fn discard_expired(&mut self) {
        while self
            .marks
            .front()
            .is_some_and(|mark| mark.timestamp.elapsed() > SELF_WRITE_TTL)
        {
            if let Some(mark) = self.marks.pop_front() {
                tracing::trace!(label = %mark.label, "自写入标记已过期,丢弃");
            }
        }
    }
}

static SELF_WRITE: Mutex<PendingSelfWrites> = Mutex::new(PendingSelfWrites {
    marks: VecDeque::new(),
});
static NEXT_SELF_WRITE_ID: AtomicU64 = AtomicU64::new(1);

/// 打自写入标记（写入剪贴板前调）。
///
/// 内聚在 `clipboard` 模块：`write_*` 外壳函数内部调，调用方只多传 `&str` label。
fn mark_self_write(label: &str, skip_persist: bool) -> Option<u64> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    let id = NEXT_SELF_WRITE_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut guard) = SELF_WRITE.lock() {
        guard.push(SelfWriteMark {
            id,
            label: label.to_string(),
            skip_persist,
            timestamp: Instant::now(),
        });
    }
    Some(id)
}

fn cancel_self_write(id: u64) {
    if let Ok(mut guard) = SELF_WRITE.lock() {
        guard.cancel(id);
    }
}

fn with_self_write_mark<T>(
    label: &str,
    skip_persist: bool,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let id = mark_self_write(label, skip_persist);
    let result = operation();
    if result.is_err()
        && let Some(id) = id
    {
        cancel_self_write(id);
    }
    result
}

/// 消费自写入标记（监听器 `on_clipboard_change` 开头调）。
///
/// 命中且未过期 → 返回 `(label, skip_persist)`；未命中或已过期 → `None`。
/// 取出即清空（一次性语义），防止后续非自写入的剪贴板变化误命中。
pub(super) fn take_self_write() -> Option<(String, bool)> {
    let mut guard = SELF_WRITE.lock().ok()?;
    let mark = guard.take()?;
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
    with_self_write_mark(label, skip_persist, || {
        windows::write_bgra_to_clipboard_raw(pixels, width, height)
    })
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
        max_items: AtomicU32::new(cfg.max_items),
        max_image_items: AtomicU32::new(cfg.max_image_items),
        capture_images: AtomicBool::new(cfg.capture_images),
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
    let changed = ACTIVE.swap(active, Ordering::Relaxed) != active;
    if changed && let Ok(mut pending) = SELF_WRITE.lock() {
        pending.marks.clear();
    }
    tracing::debug!(active, "剪贴板监听 active 切换");
}

/// 热更新监听器运行时配置。监听窗口和数据库连接保持不变。
pub fn update_runtime_config(cfg: &ClipboardConfig) {
    if let Some(s) = STATE.get() {
        *s.blacklist.write().unwrap() = cfg.blacklist_keywords.clone();
        s.max_items.store(cfg.max_items, Ordering::Relaxed);
        s.max_image_items
            .store(cfg.max_image_items, Ordering::Relaxed);
        s.capture_images
            .store(cfg.capture_images, Ordering::Relaxed);
    }
    set_active(cfg.enabled);
    tracing::debug!(
        enabled = cfg.enabled,
        max_items = cfg.max_items,
        max_image_items = cfg.max_image_items,
        capture_images = cfg.capture_images,
        blacklist_count = cfg.blacklist_keywords.len(),
        "剪贴板监听运行时配置已热更新"
    );
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
/// 解码完成后再打自写标记并调用 raw，避免大图解码耗尽标记 TTL；
/// 不经过 `write_bgra_to_clipboard` 外壳，避免重复打标。
#[cfg(target_os = "windows")]
pub fn write_png_to_clipboard(
    png_data: &[u8],
    label: &str,
    skip_persist: bool,
) -> Result<(), String> {
    use png::ColorType;
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
    buf.truncate(info.buffer_size());

    let bgra = match info.color_type {
        ColorType::Rgba => {
            crate::infra::platform::screenshot::swap_rgba_bgra_in_place(&mut buf);
            tracing::info!(
                w,
                h,
                png_bytes = png_data.len(),
                "write_png_to_clipboard: RGBA→BGRA→CF_DIB"
            );
            buf
        }
        ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|chunk| [chunk[2], chunk[1], chunk[0], 255])
            .collect(),
        ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|chunk| [chunk[0], chunk[0], chunk[0], chunk[1]])
            .collect(),
        ColorType::Grayscale => buf
            .iter()
            .flat_map(|gray| [*gray, *gray, *gray, 255])
            .collect(),
        other => {
            return Err(format!(
                "不支持的 PNG 颜色类型: {other:?}，期望 RGBA/RGB/Grayscale/GrayscaleAlpha"
            ));
        }
    };

    with_self_write_mark(label, skip_persist, || {
        windows::write_bgra_to_clipboard_raw(&bgra, w, h)
    })
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
    with_self_write_mark(label, skip_persist, || {
        windows::write_text_to_clipboard_raw(text)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_self_writes_is_fifo() {
        let mut pending = PendingSelfWrites::default();
        pending.push(mark(1, "first", false, Instant::now()));
        pending.push(mark(2, "second", true, Instant::now()));
        assert_eq!(pending.take().map(|item| item.label), Some("first".into()));
        assert_eq!(pending.take().map(|item| item.label), Some("second".into()));
    }

    #[test]
    fn pending_self_writes_can_cancel_specific_mark() {
        let mut pending = PendingSelfWrites::default();
        pending.push(mark(1, "first", false, Instant::now()));
        pending.push(mark(2, "failed", false, Instant::now()));
        pending.push(mark(3, "third", false, Instant::now()));
        pending.cancel(2);
        assert_eq!(pending.take().map(|item| item.id), Some(1));
        assert_eq!(pending.take().map(|item| item.id), Some(3));
    }

    #[test]
    fn pending_self_writes_discards_expired_prefix() {
        let mut pending = PendingSelfWrites::default();
        pending.push(mark(
            1,
            "expired",
            false,
            Instant::now() - Duration::from_secs(2),
        ));
        pending.push(mark(2, "current", true, Instant::now()));
        let current = pending.take().expect("current mark");
        assert_eq!(current.id, 2);
        assert!(current.skip_persist);
    }

    fn mark(id: u64, label: &str, skip_persist: bool, timestamp: Instant) -> SelfWriteMark {
        SelfWriteMark {
            id,
            label: label.into(),
            skip_persist,
            timestamp,
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
