//! 有界日志管道（0.22.1）。
//!
//! stdout/stderr 同时异步排空，不使用 unbounded_channel。
//! 日志历史使用容量明确的 ring buffer。
//! 消费者缺席或过慢时不阻塞 child 管道排空，不无限增长内存。
//!
//! ## 设计
//!
//! 方案 B（无中转 channel）：
//! - pump 直接写 ring/broadcast（通过 `append`）。
//! - mutex 临界区足够短（push_back + send）。
//! - 内存边界来自 line accumulator（`max_line_bytes`）、ring capacity、broadcast capacity。
//!
//! 不在 async 上下文中执行同步阻塞等待。
//! 不使用 unbounded_channel。
//! 不使用 `read_until`（无界增长）。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex, broadcast};

/// 日志来源（stdout / stderr）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogSource {
    Stdout,
    Stderr,
}

/// 单条日志记录。
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogEntry {
    /// 来源管道。
    pub source: LogSource,
    /// 文本内容（已做 UTF-8 lossy + 长度截断）。
    pub text: String,
    /// 序号（单调递增，消费者用于去重/检测 gap）。
    pub seq: u64,
    /// 事件时间戳（Unix 毫秒），在 append 时记录。
    #[serde(skip)]
    pub timestamp_ms: u64,
}

/// 日志管道配置。
#[derive(Debug, Clone)]
pub struct LogPipeConfig {
    /// 历史环形缓冲区容量（条数）。
    pub history_capacity: usize,
    /// broadcast 通道容量（实时订阅者）。
    pub broadcast_capacity: usize,
    /// 单行最大字节数（防止无换行超长输出导致内存无界）。
    /// pipe reader 的 line accumulator 达到此上限后截断并丢弃剩余字节。
    pub max_line_bytes: usize,
}

impl Default for LogPipeConfig {
    fn default() -> Self {
        Self {
            history_capacity: 500,
            broadcast_capacity: 64,
            max_line_bytes: 8192, // 8KB per line
        }
    }
}

/// 实时日志订阅者。
pub type LogSubscriber = broadcast::Receiver<LogEntry>;

/// 有界日志管道：ring buffer + broadcast。
///
/// 内部使用 `tokio::sync::Mutex` 保护 ring buffer，不跨 await 持有 `std::sync::MutexGuard`。
pub struct LogPipe {
    /// Ring buffer（最近 N 条日志）。
    history: Mutex<VecDeque<LogEntry>>,
    /// 序号计数器。
    seq: Mutex<u64>,
    /// broadcast 发送端（实时订阅者）。
    tx: broadcast::Sender<LogEntry>,
    /// 无订阅者时的计数（信号性，不阻断）。
    no_subscriber_count: Arc<std::sync::atomic::AtomicU64>,
    /// 截断行计数。
    truncated_line_count: Arc<std::sync::atomic::AtomicU64>,
    config: LogPipeConfig,
}

impl LogPipe {
    pub fn new(config: LogPipeConfig) -> Self {
        let (tx, _) = broadcast::channel(config.broadcast_capacity);
        Self {
            history: Mutex::new(VecDeque::with_capacity(config.history_capacity)),
            seq: Mutex::new(0),
            tx,
            no_subscriber_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            truncated_line_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            config,
        }
    }

    /// 追加一条日志。不阻塞 pipe reader。
    pub async fn append(&self, source: LogSource, text: String, truncated: bool) {
        if text.is_empty() {
            return;
        }

        if truncated {
            self.truncated_line_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        let seq = {
            let mut g = self.seq.lock().await;
            *g += 1;
            *g
        };

        let entry = LogEntry {
            source,
            text,
            seq,
            timestamp_ms: now_ms(),
        };

        // 写入 ring buffer（始终保留最近记录）
        {
            let mut hist = self.history.lock().await;
            if hist.len() >= self.config.history_capacity {
                hist.pop_front();
            }
            hist.push_back(entry.clone());
        }

        // 广播到实时订阅者
        // broadcast::send 在无 receiver 时返回 SendError，不是"满"。
        // 慢 receiver 在 recv 时收到 Lagged，由消费者处理。
        if self.tx.receiver_count() > 0 {
            // send 失败（Lagged）只影响慢消费者，不影响 ring buffer
            let _ = self.tx.send(entry);
        } else {
            // 无订阅者：记 no_subscriber_count（信号性，不阻断）
            self.no_subscriber_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// 获取历史日志快照（最近 N 条）。
    pub async fn history(&self) -> Vec<LogEntry> {
        let hist = self.history.lock().await;
        hist.iter().cloned().collect()
    }

    /// 订阅实时日志流。
    pub fn subscribe(&self) -> LogSubscriber {
        self.tx.subscribe()
    }

    /// 获取无订阅者时的消息计数（信号性指标）。
    pub fn no_subscriber_count(&self) -> u64 {
        self.no_subscriber_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 获取截断行计数。
    pub fn truncated_line_count(&self) -> u64 {
        self.truncated_line_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 获取 broadcast 接收者数量。
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// 当前 Unix 毫秒时间戳。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 有界行累加器：读取管道数据，按换行符切分，每行有严格字节上限。
///
/// 达到 `max_line_bytes` 后：
/// - 生成一条带 `[truncated]` 标记的日志；
/// - 继续排空并丢弃该逻辑行剩余字节，直到遇到换行；
/// - 不停止读取 child 管道；
/// - 不让内存继续增长。
///
/// 正确处理：
/// - `\n` — Unix 换行
/// - `\r\n` — Windows 换行
/// - 单独 `\r` — 进度输出（如 FunASR tqdm），按逻辑行切分
/// - EOF 前无换行的尾部内容
/// - 非法 UTF-8（lossy 转换，不 panic）
pub struct LineAccumulator {
    buf: Vec<u8>,
    max_line_bytes: usize,
    /// 是否已超过 max_line_bytes（当前行正在被丢弃）。
    overflowing: bool,
}

impl LineAccumulator {
    pub fn new(max_line_bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(max_line_bytes.min(8192)),
            max_line_bytes,
            overflowing: false,
        }
    }

    /// 处理一批管道数据，返回完成的行。
    ///
    /// 返回 `(lines, truncated_count)`：
    /// - `lines`：完成的行列表，每行 `(text, was_truncated)`
    /// - `truncated_count`：因超长被截断的行数
    pub fn push_data(&mut self, data: &[u8]) -> Vec<(String, bool)> {
        let mut lines = Vec::new();

        for &byte in data {
            match byte {
                b'\n' => {
                    // 换行：完成当前行
                    // 去除尾部 \r（Windows \r\n）
                    if self.buf.last() == Some(&b'\r') {
                        self.buf.pop();
                    }
                    let (text, truncated) = self.finish_line();
                    if !text.is_empty() || truncated {
                        lines.push((text, truncated));
                    }
                }
                b'\r' => {
                    // 单独 \r（不在 \r\n 中）：按逻辑行切分
                    // tqdm 进度条用 \r 覆盖同一行
                    let (text, truncated) = self.finish_line();
                    if !text.is_empty() || truncated {
                        lines.push((text, truncated));
                    }
                }
                _ => {
                    if self.overflowing {
                        // 当前行已超长，丢弃剩余字节直到换行
                        continue;
                    }
                    if self.buf.len() >= self.max_line_bytes {
                        // 达到上限：标记 overflow，继续丢弃直到换行
                        self.overflowing = true;
                    } else {
                        self.buf.push(byte);
                    }
                    // 检查是否刚好达到上限
                    if self.buf.len() == self.max_line_bytes && !self.overflowing {
                        // 下一字节将触发 overflow
                        // 但当前可能还有数据，先不 flush
                    }
                }
            }
        }

        lines
    }

    /// EOF 时调用，返回未完成的尾部内容。
    pub fn finish(&mut self) -> Option<(String, bool)> {
        if self.buf.is_empty() && !self.overflowing {
            return None;
        }
        // 去除尾部 \r
        if self.buf.last() == Some(&b'\r') {
            self.buf.pop();
        }
        let (text, truncated) = self.finish_line();
        if text.is_empty() && !truncated {
            None
        } else {
            Some((text, truncated))
        }
    }

    /// 完成当前行，重置累加器。
    fn finish_line(&mut self) -> (String, bool) {
        let was_overflowing = self.overflowing;

        // 截取到 max_line_bytes（如果超过的话，但 overflowing 时 buf 已被限制）
        let raw = if self.buf.len() > self.max_line_bytes {
            &self.buf[..self.max_line_bytes]
        } else {
            &self.buf[..]
        };

        // UTF-8 lossy 转换
        let mut text = String::from_utf8_lossy(raw).to_string();

        if was_overflowing {
            text.push_str("...[truncated]");
        }

        text = text.trim().to_string();

        // 重置
        self.buf.clear();
        self.overflowing = false;

        (text, was_overflowing)
    }

    /// 当前是否有未完成的行。
    pub fn has_pending(&self) -> bool {
        !self.buf.is_empty() || self.overflowing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_accumulator_basic_newline() {
        let mut acc = LineAccumulator::new(8192);
        let lines = acc.push_data(b"hello\nworld\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, "hello");
        assert!(!lines[0].1);
        assert_eq!(lines[1].0, "world");
        assert!(!lines[1].1);
    }

    #[test]
    fn line_accumulator_crlf() {
        let mut acc = LineAccumulator::new(8192);
        let lines = acc.push_data(b"line1\r\nline2\r\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, "line1");
        assert_eq!(lines[1].0, "line2");
    }

    #[test]
    fn line_accumulator_cr_only() {
        let mut acc = LineAccumulator::new(8192);
        // tqdm 风格：\r 覆盖
        let lines = acc.push_data(b"progress1\rprogress2\r");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, "progress1");
        assert_eq!(lines[1].0, "progress2");
    }

    #[test]
    fn line_accumulator_eof_without_newline() {
        let mut acc = LineAccumulator::new(8192);
        acc.push_data(b"no newline at end");
        let tail = acc.finish();
        assert!(tail.is_some());
        assert_eq!(tail.unwrap().0, "no newline at end");
    }

    #[test]
    fn line_accumulator_truncates_long_line() {
        let mut acc = LineAccumulator::new(10);
        // 超过 10 字节的行
        let long = b"AAAAAAAAAAAAAAA\n"; // 15 A's + newline
        let lines = acc.push_data(long);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].1, "应标记为 truncated");
        assert!(lines[0].0.ends_with("...[truncated]"));
        assert!(lines[0].0.len() <= 10 + "...[truncated]".len());
    }

    #[test]
    fn line_accumulator_truncates_very_long_no_newline() {
        let mut acc = LineAccumulator::new(10);
        // 远超 max_line_bytes 的无换行数据
        let huge = vec![b'B'; 100_000];
        let lines = acc.push_data(&huge);
        // 还没遇到换行，不应产生行
        assert!(lines.is_empty());
        // EOF 后应有截断行
        let tail = acc.finish();
        assert!(tail.is_some());
        assert!(tail.unwrap().1);
    }

    #[test]
    fn line_accumulator_truncate_then_normal() {
        let mut acc = LineAccumulator::new(5);
        // 超长行 + 正常行（正常行长度 < max_line_bytes）
        let lines = acc.push_data(b"AAAAAAAAAA\nhi\n");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].1); // 第一行截断
        assert!(!lines[1].1); // 第二行正常
        assert_eq!(lines[1].0, "hi");
    }

    #[test]
    fn line_accumulator_invalid_utf8_no_panic() {
        let mut acc = LineAccumulator::new(8192);
        let mut data = vec![b'A'; 10];
        data.extend_from_slice(&[0xFF, 0xFE, 0xFD]);
        data.push(b'\n');
        let lines = acc.push_data(&data);
        assert_eq!(lines.len(), 1);
        // lossy 替换非法字节
        assert!(lines[0].0.contains('A'));
    }

    #[test]
    fn line_accumulator_empty_lines() {
        let mut acc = LineAccumulator::new(8192);
        let lines = acc.push_data(b"\n\n\n");
        // 空行被过滤
        assert!(lines.is_empty());
    }

    #[test]
    fn line_accumulator_partial_then_complete() {
        let mut acc = LineAccumulator::new(8192);
        // 分批到达
        acc.push_data(b"hello ");
        let lines = acc.push_data(b"world\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, "hello world");
    }

    #[tokio::test]
    async fn log_pipe_append_and_history() {
        let pipe = LogPipe::new(LogPipeConfig::default());

        pipe.append(LogSource::Stdout, "hello world".into(), false)
            .await;
        pipe.append(LogSource::Stderr, "error line".into(), false)
            .await;

        let history = pipe.history().await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].text, "hello world");
        assert_eq!(history[0].source, LogSource::Stdout);
        assert_eq!(history[1].text, "error line");
        assert_eq!(history[1].source, LogSource::Stderr);
        // 时间戳应为非零
        assert!(history[0].timestamp_ms > 0);
    }

    #[tokio::test]
    async fn log_pipe_ring_buffer_eviction() {
        let config = LogPipeConfig {
            history_capacity: 3,
            broadcast_capacity: 4,
            max_line_bytes: 8192,
        };
        let pipe = LogPipe::new(config);

        for i in 0..5u8 {
            let line = format!("line {i}");
            pipe.append(LogSource::Stdout, line, false).await;
        }

        let history = pipe.history().await;
        assert_eq!(history.len(), 3, "ring buffer 应只保留最近 3 条");
        assert_eq!(history[0].text, "line 2");
        assert_eq!(history[2].text, "line 4");
    }

    #[tokio::test]
    async fn log_pipe_seq_monotonic() {
        let pipe = LogPipe::new(LogPipeConfig::default());

        pipe.append(LogSource::Stdout, "a".into(), false).await;
        pipe.append(LogSource::Stdout, "b".into(), false).await;
        pipe.append(LogSource::Stdout, "c".into(), false).await;

        let history = pipe.history().await;
        assert_eq!(history.len(), 3);
        assert!(history[0].seq < history[1].seq);
        assert!(history[1].seq < history[2].seq);
    }

    #[tokio::test]
    async fn log_pipe_empty_lines_skipped() {
        let pipe = LogPipe::new(LogPipeConfig::default());

        pipe.append(LogSource::Stdout, "".into(), false).await;

        let history = pipe.history().await;
        assert!(history.is_empty(), "空行应被跳过");
    }

    #[tokio::test]
    async fn log_pipe_broadcast_subscription() {
        let pipe = LogPipe::new(LogPipeConfig::default());
        let mut sub = pipe.subscribe();

        pipe.append(LogSource::Stdout, "broadcast test".into(), false)
            .await;

        let entry = sub.recv().await.unwrap();
        assert_eq!(entry.text, "broadcast test");
        assert_eq!(entry.source, LogSource::Stdout);
        assert!(entry.timestamp_ms > 0);
    }

    #[tokio::test]
    async fn log_pipe_truncated_count() {
        let pipe = LogPipe::new(LogPipeConfig::default());

        pipe.append(LogSource::Stdout, "normal".into(), false).await;
        pipe.append(LogSource::Stdout, "long...".into(), true).await;
        pipe.append(LogSource::Stdout, "also long".into(), true)
            .await;

        assert_eq!(pipe.truncated_line_count(), 2);
    }

    #[tokio::test]
    async fn log_pipe_no_subscriber_count() {
        let pipe = LogPipe::new(LogPipeConfig::default());

        // 无订阅者时 append
        pipe.append(LogSource::Stdout, "no sub".into(), false).await;
        pipe.append(LogSource::Stdout, "no sub 2".into(), false)
            .await;

        assert_eq!(pipe.no_subscriber_count(), 2);
    }

    #[tokio::test]
    async fn log_pipe_lagged_recovery() {
        // 小 broadcast capacity 制造 Lagged
        let config = LogPipeConfig {
            history_capacity: 100,
            broadcast_capacity: 2,
            max_line_bytes: 8192,
        };
        let pipe = Arc::new(LogPipe::new(config));
        let mut sub = pipe.subscribe();

        // 发送超过 capacity 的消息，制造 Lagged
        for i in 0..10u8 {
            pipe.append(LogSource::Stdout, format!("msg {i}"), false)
                .await;
        }

        // recv 应该先收到 Lagged
        let mut got_lagged = false;
        let mut got_after_lag = false;
        loop {
            match sub.recv().await {
                Ok(_entry) => {
                    got_after_lag = true;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    got_lagged = true;
                    tracing::info!(lag = n, "收到 Lagged");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }

        assert!(got_lagged, "应收到 Lagged");
        assert!(got_after_lag, "Lagged 后应能继续收到消息");
    }

    #[tokio::test]
    async fn log_pipe_history_preserves_timestamp() {
        let pipe = LogPipe::new(LogPipeConfig::default());

        let ts_before = now_ms();
        pipe.append(LogSource::Stdout, "timed".into(), false).await;
        let ts_after = now_ms();

        let history = pipe.history().await;
        assert_eq!(history.len(), 1);
        assert!(history[0].timestamp_ms >= ts_before);
        assert!(history[0].timestamp_ms <= ts_after);
    }
}
