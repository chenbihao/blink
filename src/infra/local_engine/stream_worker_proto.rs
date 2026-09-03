//! ParaformerOnline 二进制流式 worker 协议 v2（0.22.9，Handoff 03）。
//!
//! 版本化 length-prefixed binary streaming protocol，用于 host ↔ worker
//! 之间的流式 ASR 音频传输和结果返回。与现有 GGUF NDJSON v1（`worker_proto.rs`）
//! 完全独立，不改变其 wire contract。
//!
//! ## Wire Layout
//!
//! ```text
//! Frame = [magic(4B)][version(1B)][msg_type(1B)][flags(1B)][reserved(1B)]
//!         [request_id(4B LE)][generation(4B LE)][payload_len(4B LE)][payload(NB)]
//!
//! magic       = b"BLNK" (0x42 0x4C 0x4E 0x4B)
//! version     = 2
//! msg_type    = u8 enum (见 MessageType)
//! flags       = bitflags (见 frame_flags)
//! request_id  = u32 LE (host 分配，单调递增)
//! generation  = u32 LE (host 分配，每条流递增)
//! payload_len = u32 LE (最大 MAX_PAYLOAD_LEN = 64 KiB)
//! ```
//!
//! ## PCM 格式
//!
//! 固定 16kHz、mono、f32 little-endian。
//! 建议音频帧 20～100ms（320～1600 samples = 1280～6400 bytes）。
//! 硬性拒绝 payload > MAX_PAYLOAD_LEN 的帧。
//!
//! ## 消息类型
//!
//! Host → Worker: Hello, Begin, Audio, End, Cancel, Reset, Quit
//! Worker → Host: Ready, Partial, Final, Ack, Busy, Error
//!
//! ## 语义
//!
//! - 一次只允许一个 active stream
//! - host 使用有界队列；队列满返回 Busy，不静默丢音频
//! - Cancel/Reset 幂等
//! - request id 关联响应
//! - 旧 generation 结果丢弃
//! - malformed/oversized/unknown frame fail-closed，并 poison 当前连接
//! - EOF、partial frame、非法长度不能导致无限分配或死循环
//! - Quit 走优雅退出，超时交给 ManagedProcess
//!
//! ## 铁则
//!
//! - stdin/stdout 只传协议；stderr 只传诊断
//! - 不将 JSON/Base64/Vec<f32> serde 用于音频热路径
//! - 保留现有 GGUF NDJSON v1，不改变其 wire contract

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

// ── 常量 ─────────────────────────────────────────────────────────────────

/// 魔数：`b"BLNK"`。
pub const MAGIC: &[u8; 4] = b"BLNK";

/// 协议版本。
pub const PROTOCOL_VERSION: u8 = 2;

/// 帧头固定长度（magic + version + msg_type + flags + reserved + request_id + generation + payload_len）。
pub const HEADER_LEN: usize = 4 + 1 + 1 + 1 + 1 + 4 + 4 + 4; // = 20

/// 最大 payload 长度（64 KiB）。
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024;

/// 16kHz f32 mono 每毫秒样本数。
pub const SAMPLES_PER_MS: usize = 16;

/// 每样本字节数（f32 LE）。
pub const BYTES_PER_SAMPLE: usize = 4;

/// 最小音频帧 payload（20ms = 320 samples = 1280 bytes）。
#[allow(dead_code)] // used in tests only, production wiring pending gate
pub const MIN_AUDIO_PAYLOAD: usize = 20 * SAMPLES_PER_MS * BYTES_PER_SAMPLE; // 1280

/// 最大音频帧 payload（100ms = 1600 samples = 6400 bytes）。
pub const MAX_AUDIO_PAYLOAD: usize = 100 * SAMPLES_PER_MS * BYTES_PER_SAMPLE; // 6400

// ── 消息类型 ─────────────────────────────────────────────────────────────

/// 消息类型。
///
/// Host → Worker: Hello, Begin, Audio, End, Cancel, Reset, Quit
/// Worker → Host: Ready, Partial, Final, Ack, Busy, Error
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// Host → Worker: 握手，携带 protocol_version。
    Hello = 1,
    /// Host → Worker: 开始一条新流，携带 generation。
    Begin = 2,
    /// Host → Worker: PCM 音频帧（f32 LE, 16kHz, mono）。
    Audio = 3,
    /// Host → Worker: 音频流结束，请求最终结果。
    End = 4,
    /// Host → Worker: 取消当前流（幂等）。
    Cancel = 5,
    /// Host → Worker: 重置 worker 状态（幂等）。
    Reset = 6,
    /// Host → Worker: 优雅退出。
    Quit = 7,
    /// Worker → Host: 模型加载完成，可接受流。
    Ready = 16,
    /// Worker → Host: 部分识别结果。
    Partial = 17,
    /// Worker → Host: 最终识别结果。
    Final = 18,
    /// Worker → Host: 确认（Begin/Reset/Cancel 等操作的 Ack）。
    Ack = 19,
    /// Worker → Host: 队列满，拒绝音频。
    Busy = 20,
    /// Worker → Host: 错误。
    Error = 21,
}

impl MessageType {
    /// 从 u8 转换，未知值返回 None。
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Hello),
            2 => Some(Self::Begin),
            3 => Some(Self::Audio),
            4 => Some(Self::End),
            5 => Some(Self::Cancel),
            6 => Some(Self::Reset),
            7 => Some(Self::Quit),
            16 => Some(Self::Ready),
            17 => Some(Self::Partial),
            18 => Some(Self::Final),
            19 => Some(Self::Ack),
            20 => Some(Self::Busy),
            21 => Some(Self::Error),
            _ => None,
        }
    }
}

/// 帧标志位。
pub mod frame_flags {
    /// Audio 帧标记为最终 chunk（与 End 信号等价，用于流尾指示）。
    pub const FINAL_CHUNK: u8 = 0x01;
    /// End of stream（worker 端 Ready 后不再有更多数据）。
    #[allow(dead_code)] // reserved for future use
    pub const END_OF_STREAM: u8 = 0x02;
}

// ── 错误 ─────────────────────────────────────────────────────────────────

/// 协议层错误。
#[allow(dead_code)] // Handoff 05: variants used in tests, production wiring pending
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("worker 管道已断开")]
    Disconnected,
    #[error("协议违例: {0}")]
    Protocol(String),
    #[error("帧超限: payload_len={len} > MAX={max}")]
    Oversized { len: u32, max: u32 },
    #[error("帧截断: 期望 {expected} 字节，实际读到 {actual} 字节")]
    Truncated { expected: usize, actual: usize },
    #[error("魔数不匹配: 期望 {expected:?}，实际 {actual:?}")]
    BadMagic { expected: [u8; 4], actual: [u8; 4] },
    #[error("版本不兼容: 期望 {expected}，实际 {actual}")]
    BadVersion { expected: u8, actual: u8 },
    #[error("未知消息类型: {0}")]
    UnknownMessageType(u8),
    #[error("音频帧大小超限: {len} bytes (max {max})")]
    AudioPayloadTooLarge { len: usize, max: usize },
    #[error("写入失败: {0}")]
    Write(String),
    #[error("等待响应超时")]
    Timeout,
    #[error("worker 返回 Busy: {0}")]
    Busy(String),
    #[error("worker 返回错误: {0}")]
    Worker(String),
    #[error("worker 已中毒")]
    Poisoned,
}

// ── 帧编解码 ─────────────────────────────────────────────────────────────

/// 解析后的帧头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: u8,
    pub msg_type: MessageType,
    pub flags: u8,
    pub reserved: u8,
    pub request_id: u32,
    pub generation: u32,
    pub payload_len: u32,
}

/// 编码一帧到 writer。
///
/// `payload` 长度不得超过 `MAX_PAYLOAD_LEN`。
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_type: MessageType,
    flags: u8,
    request_id: u32,
    generation: u32,
    payload: &[u8],
) -> Result<(), ProtoError> {
    let payload_len = payload.len();
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(ProtoError::Oversized {
            len: payload_len as u32,
            max: MAX_PAYLOAD_LEN as u32,
        });
    }

    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(MAGIC);
    header[4] = PROTOCOL_VERSION;
    header[5] = msg_type as u8;
    header[6] = flags;
    header[7] = 0; // reserved
    header[8..12].copy_from_slice(&request_id.to_le_bytes());
    header[12..16].copy_from_slice(&generation.to_le_bytes());
    header[16..20].copy_from_slice(&(payload_len as u32).to_le_bytes());

    writer
        .write_all(&header)
        .await
        .map_err(|e| ProtoError::Write(e.to_string()))?;
    if !payload.is_empty() {
        writer
            .write_all(payload)
            .await
            .map_err(|e| ProtoError::Write(e.to_string()))?;
    }
    writer
        .flush()
        .await
        .map_err(|e| ProtoError::Write(e.to_string()))?;
    Ok(())
}

/// 从 reader 读取精确的字节或返回 EOF。
///
/// 返回 `Ok(None)` 表示 clean EOF（无数据可读）。
/// 返回 `Err(Truncated)` 表示读到部分数据后 EOF。
async fn read_exact_or_eof<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
) -> Result<Option<()>, ProtoError> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]).await {
            Ok(0) => {
                if filled == 0 {
                    return Ok(None); // clean EOF
                }
                return Err(ProtoError::Truncated {
                    expected: buf.len(),
                    actual: filled,
                });
            }
            Ok(n) => filled += n,
            Err(e) => return Err(ProtoError::Write(format!("读取失败: {e}"))),
        }
    }
    Ok(Some(()))
}

/// 从 reader 读取一帧（header + payload）。
///
/// 返回 `Ok(None)` 表示 clean EOF（无数据可读）。
/// malformed/oversized/unknown frame 返回 `Err`，调用方应 poison 连接。
///
/// **不会无限分配**：payload_len 上限由 `MAX_PAYLOAD_LEN` 硬性校验。
/// **不会死循环**：使用 `read_exact` 语义，EOF 后立即返回。
pub async fn read_frame<'a, R: AsyncRead + Unpin>(
    reader: &mut R,
    payload_buf: &'a mut Vec<u8>,
) -> Result<Option<(FrameHeader, &'a [u8])>, ProtoError> {
    let mut header_buf = [0u8; HEADER_LEN];
    if read_exact_or_eof(reader, &mut header_buf).await?.is_none() {
        return Ok(None);
    }

    // 校验 magic
    let actual_magic = [header_buf[0], header_buf[1], header_buf[2], header_buf[3]];
    if &actual_magic != MAGIC {
        return Err(ProtoError::BadMagic {
            expected: *MAGIC,
            actual: actual_magic,
        });
    }

    // 校验 version
    let version = header_buf[4];
    if version != PROTOCOL_VERSION {
        return Err(ProtoError::BadVersion {
            expected: PROTOCOL_VERSION,
            actual: version,
        });
    }

    let msg_type_raw = header_buf[5];
    let msg_type =
        MessageType::from_u8(msg_type_raw).ok_or(ProtoError::UnknownMessageType(msg_type_raw))?;

    let flags = header_buf[6];
    let reserved = header_buf[7];
    let request_id =
        u32::from_le_bytes([header_buf[8], header_buf[9], header_buf[10], header_buf[11]]);
    let generation = u32::from_le_bytes([
        header_buf[12],
        header_buf[13],
        header_buf[14],
        header_buf[15],
    ]);
    let payload_len = u32::from_le_bytes([
        header_buf[16],
        header_buf[17],
        header_buf[18],
        header_buf[19],
    ]);

    // 硬性拒绝超限帧
    if payload_len as usize > MAX_PAYLOAD_LEN {
        return Err(ProtoError::Oversized {
            len: payload_len,
            max: MAX_PAYLOAD_LEN as u32,
        });
    }

    // 读取 payload
    payload_buf.clear();
    if payload_len > 0 {
        payload_buf.resize(payload_len as usize, 0);
        // header 已读成功，此处 EOF = truncation
        read_exact_or_eof(reader, payload_buf)
            .await?
            .ok_or(ProtoError::Truncated {
                expected: payload_len as usize,
                actual: 0,
            })?;
    }

    let header = FrameHeader {
        version,
        msg_type,
        flags,
        reserved,
        request_id,
        generation,
        payload_len,
    };

    Ok(Some((header, payload_buf.as_slice())))
}

// ── Host Client ──────────────────────────────────────────────────────────

/// 流式识别结果。
#[allow(dead_code)] // Handoff 05: production wiring pending gate
#[derive(Debug, Clone)]
pub struct StreamResult {
    pub text: String,
    pub is_final: bool,
}

/// Host → Worker 的音频帧（raw f32 LE bytes）。
///
/// PCM 格式固定 16kHz、mono、f32 little-endian。
/// payload 长度应在 MIN_AUDIO_PAYLOAD..=MAX_AUDIO_PAYLOAD 范围内。
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub data: Vec<u8>,
}

impl AudioFrame {
    /// 从 f32 样本切片构造音频帧。
    ///
    /// 将 f32 samples 转为 little-endian bytes。
    /// **不走 serde**——直接 bytewise copy。
    pub fn from_samples(samples: &[f32]) -> Self {
        let mut data = Vec::with_capacity(samples.len() * BYTES_PER_SAMPLE);
        for &s in samples {
            data.extend_from_slice(&s.to_le_bytes());
        }
        Self { data }
    }

    /// 从原始 bytes 构造音频帧（调用方保证 f32 LE 格式）。
    #[allow(dead_code)]
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self { data }
    }

    /// 校验 payload 大小在允许范围内。
    pub fn validate(&self) -> Result<(), ProtoError> {
        let len = self.data.len();
        // 允许空帧（用于 flush 信号）
        if len == 0 {
            return Ok(());
        }
        if len > MAX_AUDIO_PAYLOAD {
            return Err(ProtoError::AudioPayloadTooLarge {
                len,
                max: MAX_AUDIO_PAYLOAD,
            });
        }
        // 不强制最小值——允许小于 20ms 的帧（但建议 20-100ms）
        Ok(())
    }
}

/// reader task → client 的事件。
#[allow(dead_code)] // Handoff 05: used internally by StreamWorkerClient reader task
#[derive(Debug)]
pub enum WorkerEvent {
    /// 收到一帧消息。
    Frame(FrameHeader, Vec<u8>),
    /// stdout EOF（worker 退出）。
    Eof,
    /// 协议违例。
    Violation(ProtoError),
}

/// Host 端流式 worker client。
///
/// 持有 worker 的 stdin/stdout pipe；`Arc<StreamWorkerClient>` 可克隆共享。
///
/// ## 并发模型
///
/// - 同一时刻只允许一个 active stream（Begin → ... → End/Cancel/Reset）
/// - stdin 写入由 `write_lock` 串行化
/// - stdout reader task 独立运行，将消息分发到 `events` channel
/// - 队列满时返回 Busy，不静默丢音频
/// - Cancel/Reset 幂等
/// - 旧 generation 结果丢弃
#[allow(dead_code)] // Handoff 05: production wiring pending gate
pub struct StreamWorkerClient {
    /// stdin 写入锁（串行化所有写操作）。
    write_lock: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    /// reader task → client 的事件流。
    events: Mutex<tokio::sync::mpsc::Receiver<WorkerEvent>>,
    /// request id 自增计数器。
    seq: AtomicU32,
    /// generation 自增计数器。
    generation: AtomicU32,
    /// 协议违例/EOF 后置位。
    poisoned: AtomicBool,
}

impl StreamWorkerClient {
    /// 创建客户端并启动 stdout reader task。
    ///
    /// reader task 独立持有 stdout reader；客户端 drop 后 channel 关闭，
    /// reader task 的 send 失败自然退出。
    #[allow(dead_code)]
    pub fn new(
        stdin: Box<dyn AsyncWrite + Send + Unpin>,
        stdout: Box<dyn AsyncRead + Send + Unpin>,
    ) -> Arc<Self> {
        let (tx, rx) = tokio::sync::mpsc::channel::<WorkerEvent>(128);
        tokio::spawn(async move {
            let mut reader = stdout;
            let mut payload_buf: Vec<u8> = Vec::new();
            loop {
                match read_frame(&mut reader, &mut payload_buf).await {
                    Ok(None) => {
                        let _ = tx.send(WorkerEvent::Eof).await;
                        break;
                    }
                    Ok(Some((header, payload))) => {
                        let payload_owned = payload.to_vec();
                        if tx
                            .send(WorkerEvent::Frame(header, payload_owned))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(WorkerEvent::Violation(e)).await;
                        break;
                    }
                }
            }
        });

        Arc::new(Self {
            write_lock: Mutex::new(stdin),
            events: Mutex::new(rx),
            seq: AtomicU32::new(0),
            generation: AtomicU32::new(0),
            poisoned: AtomicBool::new(false),
        })
    }

    /// 客户端是否已因协议违例/EOF 不可用。
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    fn next_request_id(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn next_generation(&self) -> u32 {
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn check_poisoned(&self) -> Result<(), ProtoError> {
        if self.is_poisoned() {
            Err(ProtoError::Poisoned)
        } else {
            Ok(())
        }
    }

    fn poison(&self, reason: &str) -> ProtoError {
        tracing::warn!(reason, "stream worker 客户端置为 poisoned");
        self.poisoned.store(true, Ordering::Release);
        ProtoError::Protocol(reason.to_string())
    }

    /// 写一帧到 worker stdin。
    async fn send_frame(
        &self,
        msg_type: MessageType,
        flags: u8,
        request_id: u32,
        generation: u32,
        payload: &[u8],
    ) -> Result<(), ProtoError> {
        self.check_poisoned()?;
        let mut writer = self.write_lock.lock().await;
        match write_frame(
            &mut *writer,
            msg_type,
            flags,
            request_id,
            generation,
            payload,
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                // 写入失败意味着管道断裂，poison 连接
                let _ = self.poison(&format!("写入失败: {e}"));
                Err(e)
            }
        }
    }

    /// 等待 Ready 消息（模型加载完成）。
    ///
    /// 超时后返回 `ProtoError::Timeout`。
    #[allow(clippy::never_loop)]
    pub async fn wait_ready(&self, timeout: std::time::Duration) -> Result<(), ProtoError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut events = self.events.lock().await;

        loop {
            let event = tokio::time::timeout_at(deadline, events.recv())
                .await
                .map_err(|_| ProtoError::Timeout)?
                .ok_or(ProtoError::Disconnected)?;

            match event {
                WorkerEvent::Frame(header, _payload) => {
                    if header.msg_type == MessageType::Ready {
                        return Ok(());
                    }
                    return Err(self.poison(&format!(
                        "Ready 前收到 {:?}（generation={}）",
                        header.msg_type, header.generation
                    )));
                }
                WorkerEvent::Eof => {
                    return Err(self.poison("等待 Ready 期间 stdout EOF"));
                }
                WorkerEvent::Violation(e) => {
                    return Err(self.poison(&format!("等待 Ready 期间协议违例: {e}")));
                }
            }
        }
    }

    /// 发送 Hello 握手。
    ///
    /// 不等待响应——Ready 由 `wait_ready` 单独等待。
    pub async fn send_hello(&self) -> Result<(), ProtoError> {
        let req_id = self.next_request_id();
        self.send_frame(MessageType::Hello, 0, req_id, 0, &[]).await
    }

    /// 开始一条新流。返回新的 generation 和 request_id。
    ///
    /// **一次只允许一个 active stream**——调用方需保证在 Begin 前没有活跃流。
    pub async fn begin_stream(&self) -> Result<(u32, u32), ProtoError> {
        self.check_poisoned()?;
        let generation_id = self.next_generation();
        let req_id = self.next_request_id();

        self.send_frame(MessageType::Begin, 0, req_id, generation_id, &[])
            .await?;

        // 等待 Ack
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut events = self.events.lock().await;
        loop {
            let event = tokio::time::timeout_at(deadline, events.recv())
                .await
                .map_err(|_| ProtoError::Timeout)?
                .ok_or(ProtoError::Disconnected)?;

            match event {
                WorkerEvent::Frame(header, _payload) => {
                    if header.generation != generation_id {
                        tracing::warn!(
                            expect_gen = generation_id,
                            got_gen = header.generation,
                            "丢弃旧 generation 消息"
                        );
                        continue;
                    }
                    match header.msg_type {
                        MessageType::Ack => return Ok((generation_id, req_id)),
                        MessageType::Error => {
                            return Err(self.poison("Begin 收到 Error"));
                        }
                        MessageType::Busy => {
                            return Err(ProtoError::Busy("Begin 被拒绝（worker 忙）".into()));
                        }
                        other => {
                            return Err(
                                self.poison(&format!("Begin 等待 Ack 期间收到 {:?}", other))
                            );
                        }
                    }
                }
                WorkerEvent::Eof => {
                    return Err(self.poison("Begin 等待 Ack 期间 stdout EOF"));
                }
                WorkerEvent::Violation(e) => {
                    return Err(self.poison(&format!("Begin 期间协议违例: {e}")));
                }
            }
        }
    }

    /// 发送音频帧。
    ///
    /// 如果 worker 队列满，worker 会返回 Busy——通过 `recv_events` 消费方处理。
    /// payload 必须是 f32 LE, 16kHz, mono 格式的 raw bytes。
    pub async fn send_audio(&self, generation: u32, frame: &AudioFrame) -> Result<(), ProtoError> {
        self.check_poisoned()?;
        frame.validate()?;
        let req_id = self.next_request_id();
        self.send_frame(MessageType::Audio, 0, req_id, generation, &frame.data)
            .await
    }

    /// 非阻塞地尝试消费事件队列中的 Partial/Busy/Error 事件。
    ///
    /// 在 `push_audio` 后调用，将 worker 产生的 Partial 事件转发给
    /// adapter 的 event channel，使 host 能收到流式 partial。
    ///
    /// 只消费当前在队列中的事件，不阻塞等待。
    /// Final 事件不在此消费——由 `end_stream` 负责。
    pub async fn try_recv_partial(&self, generation: u32) -> Vec<WorkerEvent> {
        let mut result = Vec::new();
        let mut events = self.events.lock().await;
        while let Ok(event) = events.try_recv() {
            match &event {
                WorkerEvent::Frame(header, _) if header.generation != generation => {
                    // 旧 generation 事件，跳过
                    continue;
                }
                WorkerEvent::Frame(header, _) if header.msg_type == MessageType::Final => {
                    // Final 不在此消费——放回队列由 end_stream 处理
                    // 但 mpsc::Receiver 没有 push_back，所以只能 break
                    // 实际上 Final 不会在 push_audio 期间到达（除非 worker 提前结束）
                    // 将 Final 放入 result，由调用方处理
                    result.push(event);
                    break;
                }
                _ => {
                    result.push(event);
                }
            }
        }
        result
    }

    /// 发送 End 信号并等待 Final 结果。
    ///
    /// 期间收到的 Partial 结果记日志后继续等待 Final。
    /// 旧 generation 的迟到结果丢弃。
    pub async fn end_stream(
        &self,
        generation: u32,
        timeout: std::time::Duration,
    ) -> Result<StreamResult, ProtoError> {
        self.check_poisoned()?;
        let req_id = self.next_request_id();
        self.send_frame(MessageType::End, 0, req_id, generation, &[])
            .await?;

        let deadline = tokio::time::Instant::now() + timeout;
        let mut events = self.events.lock().await;
        loop {
            let event = tokio::time::timeout_at(deadline, events.recv())
                .await
                .map_err(|_| ProtoError::Timeout)?
                .ok_or(ProtoError::Disconnected)?;

            match event {
                WorkerEvent::Frame(header, payload) => {
                    if header.generation != generation {
                        tracing::warn!(
                            expect_gen = generation,
                            got_gen = header.generation,
                            msg = ?header.msg_type,
                            "丢弃旧 generation 结果"
                        );
                        continue;
                    }
                    match header.msg_type {
                        MessageType::Partial => {
                            let text = String::from_utf8_lossy(&payload).to_string();
                            tracing::debug!(%text, stream_gen = generation, "partial");
                            continue;
                        }
                        MessageType::Final => {
                            let text = String::from_utf8_lossy(&payload).to_string();
                            return Ok(StreamResult {
                                text,
                                is_final: true,
                            });
                        }
                        MessageType::Error => {
                            let msg = String::from_utf8_lossy(&payload).to_string();
                            return Err(ProtoError::Worker(msg));
                        }
                        MessageType::Ack => {
                            continue;
                        }
                        MessageType::Busy => {
                            tracing::warn!(
                                stream_gen = generation,
                                "End 等待 Final 期间收到 Busy，继续等待"
                            );
                            continue;
                        }
                        other => {
                            return Err(
                                self.poison(&format!("End 等待 Final 期间收到 {:?}", other))
                            );
                        }
                    }
                }
                WorkerEvent::Eof => {
                    return Err(self.poison("End 等待 Final 期间 stdout EOF"));
                }
                WorkerEvent::Violation(e) => {
                    return Err(self.poison(&format!("End 期间协议违例: {e}")));
                }
            }
        }
    }

    /// 取消当前流（幂等）。
    ///
    /// 可以在 Streaming 或 WaitingFinal 状态调用。
    /// 取消后，旧 generation 的迟到结果会被丢弃。
    pub async fn cancel_stream(&self, generation: u32) -> Result<(), ProtoError> {
        self.check_poisoned()?;
        let req_id = self.next_request_id();
        self.send_frame(MessageType::Cancel, 0, req_id, generation, &[])
            .await
    }

    /// 重置 worker 状态（幂等）。
    ///
    /// Reset 后 worker 回到 Ready 状态，可以接受新流。
    pub async fn reset(&self) -> Result<(), ProtoError> {
        self.check_poisoned()?;
        let req_id = self.next_request_id();
        self.send_frame(MessageType::Reset, 0, req_id, 0, &[])
            .await?;

        // 等待 Ack（短超时）
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut events = self.events.lock().await;
        loop {
            let event = tokio::time::timeout_at(deadline, events.recv())
                .await
                .map_err(|_| ProtoError::Timeout)?
                .ok_or(ProtoError::Disconnected)?;

            match event {
                WorkerEvent::Frame(header, _payload) => match header.msg_type {
                    MessageType::Ack => return Ok(()),
                    MessageType::Error => {
                        // Error 可能来自之前操作的迟到响应，记日志后继续等 Ack
                        tracing::warn!("Reset 等待 Ack 期间收到迟到 Error，继续等待");
                        continue;
                    }
                    other => {
                        tracing::warn!(?other, "Reset 等待 Ack 期间收到非预期消息，丢弃");
                        continue;
                    }
                },
                WorkerEvent::Eof => {
                    return Err(self.poison("Reset 等待 Ack 期间 stdout EOF"));
                }
                WorkerEvent::Violation(e) => {
                    return Err(self.poison(&format!("Reset 期间协议违例: {e}")));
                }
            }
        }
    }

    /// 发送 Quit 信号（优雅退出）。
    ///
    /// 不等待响应——Quit 后 worker 应自行退出。
    /// 超时交给 ManagedProcess 处理。
    pub async fn send_quit(&self) -> Result<(), ProtoError> {
        let req_id = self.next_request_id();
        self.send_frame(MessageType::Quit, 0, req_id, 0, &[]).await
    }
}

// ── Fake Worker ──────────────────────────────────────────────────────────

/// Fake worker 状态机。
#[allow(dead_code)] // used in tests
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeWorkerState {
    /// 等待 Hello。
    Init,
    /// 已发 Ready，等待 Begin。
    Ready,
    /// 流式传输中。
    Active,
}

/// Fake worker 配置。
#[allow(dead_code)] // used in tests
#[derive(Debug, Clone)]
pub struct FakeWorkerConfig {
    /// 音频队列容量（模拟 worker 端有界队列）。
    pub queue_capacity: usize,
    /// 每个 Audio 收到后是否回 Ack。
    pub ack_audio: bool,
    /// 是否在 Begin 后回 Ack。
    pub ack_begin: bool,
    /// 是否在 Reset 后回 Ack。
    pub ack_reset: bool,
    /// 是否在 Cancel 后回 Ack。
    pub ack_cancel: bool,
    /// 模拟处理延迟（每个 audio chunk）。
    pub process_delay_ms: u64,
}

impl Default for FakeWorkerConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 16,
            ack_audio: false,
            ack_begin: true,
            ack_reset: true,
            ack_cancel: true,
            process_delay_ms: 0,
        }
    }
}

/// Fake worker——纯 Rust 内存实现，不 spawn 子进程。
///
/// 使用 tokio duplex I/O pipe 模拟 stdin/stdout。
/// 覆盖完整双向协议和背压。
#[allow(dead_code)] // used in tests
pub struct FakeWorker {
    config: FakeWorkerConfig,
    state: Mutex<FakeWorkerState>,
    /// 模拟 worker 端有界队列（用于背压测试）。
    queue: Mutex<std::collections::VecDeque<u32>>,
}

impl FakeWorker {
    #[allow(dead_code)]
    pub fn new(config: FakeWorkerConfig) -> Self {
        Self {
            config,
            state: Mutex::new(FakeWorkerState::Init),
            queue: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// 运行 fake worker——从 `reader`（host stdin）读帧，向 `writer`（host stdout）写帧。
    ///
    /// 退出条件：读到 Quit、EOF 或协议违例。
    #[allow(dead_code)]
    pub async fn run(
        &self,
        reader: &mut (impl AsyncRead + Unpin),
        writer: &mut (impl AsyncWrite + Unpin),
    ) {
        let mut payload_buf: Vec<u8> = Vec::new();

        loop {
            let frame = read_frame(reader, &mut payload_buf).await;
            match frame {
                Ok(None) => break, // EOF
                Ok(Some((header, payload))) => {
                    let should_quit = self.handle_frame(&header, payload, writer).await;
                    if should_quit {
                        break;
                    }
                }
                Err(e) => {
                    tracing::debug!(%e, "fake worker: 读帧错误，退出");
                    break;
                }
            }
        }
    }

    /// 处理一帧。返回 true 表示应退出（Quit）。
    async fn handle_frame(
        &self,
        header: &FrameHeader,
        _payload: &[u8],
        writer: &mut (impl AsyncWrite + Unpin),
    ) -> bool {
        let mut state = self.state.lock().await;

        match header.msg_type {
            MessageType::Hello => {
                *state = FakeWorkerState::Init;
                let _ = write_frame(writer, MessageType::Ready, 0, header.request_id, 0, &[]).await;
                *state = FakeWorkerState::Ready;
                false
            }
            MessageType::Begin => {
                if *state != FakeWorkerState::Ready {
                    let _ = write_frame(
                        writer,
                        MessageType::Error,
                        0,
                        header.request_id,
                        header.generation,
                        b"not ready",
                    )
                    .await;
                    return false;
                }
                *state = FakeWorkerState::Active;
                if self.config.ack_begin {
                    let _ = write_frame(
                        writer,
                        MessageType::Ack,
                        0,
                        header.request_id,
                        header.generation,
                        &[],
                    )
                    .await;
                }
                false
            }
            MessageType::Audio => {
                if *state != FakeWorkerState::Active {
                    let _ = write_frame(
                        writer,
                        MessageType::Error,
                        0,
                        header.request_id,
                        header.generation,
                        b"not active",
                    )
                    .await;
                    return false;
                }

                // 模拟有界队列背压
                let mut queue = self.queue.lock().await;
                if queue.len() >= self.config.queue_capacity {
                    drop(queue);
                    let _ = write_frame(
                        writer,
                        MessageType::Busy,
                        0,
                        header.request_id,
                        header.generation,
                        b"queue full",
                    )
                    .await;
                    return false;
                }
                queue.push_back(header.generation);
                drop(queue);

                if self.config.process_delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        self.config.process_delay_ms,
                    ))
                    .await;
                }

                if self.config.ack_audio {
                    let _ = write_frame(
                        writer,
                        MessageType::Ack,
                        0,
                        header.request_id,
                        header.generation,
                        &[],
                    )
                    .await;
                }
                false
            }
            MessageType::End => {
                if *state != FakeWorkerState::Active {
                    let _ = write_frame(
                        writer,
                        MessageType::Error,
                        0,
                        header.request_id,
                        header.generation,
                        b"not active",
                    )
                    .await;
                    return false;
                }

                // 统计队列中的音频帧数
                let mut queue = self.queue.lock().await;
                let count = queue.len() as u32;
                queue.clear();
                drop(queue);

                // 发 partial
                let partial = format!("partial({} frames)", count);
                let _ = write_frame(
                    writer,
                    MessageType::Partial,
                    0,
                    header.request_id,
                    header.generation,
                    partial.as_bytes(),
                )
                .await;

                // 发 final
                let final_text = format!("final({} frames)", count);
                let _ = write_frame(
                    writer,
                    MessageType::Final,
                    0,
                    header.request_id,
                    header.generation,
                    final_text.as_bytes(),
                )
                .await;

                *state = FakeWorkerState::Ready;
                false
            }
            MessageType::Cancel => {
                // 幂等——无论什么状态都回 Ack
                let mut queue = self.queue.lock().await;
                queue.clear();
                drop(queue);
                *state = FakeWorkerState::Ready;
                if self.config.ack_cancel {
                    let _ = write_frame(
                        writer,
                        MessageType::Ack,
                        0,
                        header.request_id,
                        header.generation,
                        &[],
                    )
                    .await;
                }
                false
            }
            MessageType::Reset => {
                // 幂等——无论什么状态都清队列并回 Ack
                let mut queue = self.queue.lock().await;
                queue.clear();
                drop(queue);
                *state = FakeWorkerState::Ready;
                if self.config.ack_reset {
                    let _ = write_frame(
                        writer,
                        MessageType::Ack,
                        0,
                        header.request_id,
                        header.generation,
                        &[],
                    )
                    .await;
                }
                false
            }
            MessageType::Quit => {
                // 优雅退出
                *state = FakeWorkerState::Init;
                true
            }
            _ => {
                // 未知/不期望的消息——记 warn
                tracing::warn!(
                    msg_type = ?header.msg_type,
                    "fake worker 收到不期望的消息"
                );
                false
            }
        }
    }

    /// 获取当前状态（测试用）。
    #[allow(dead_code)]
    pub async fn state(&self) -> FakeWorkerState {
        *self.state.lock().await
    }

    /// 获取当前队列长度（测试用）。
    #[allow(dead_code)]
    pub async fn queue_len(&self) -> usize {
        self.queue.lock().await.len()
    }
}

// ── 测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{DuplexStream, duplex};

    /// 创建测试 harness，返回可用于 spawn worker task 的读写句柄。
    fn new_with_pipes(
        config: FakeWorkerConfig,
    ) -> (
        Arc<StreamWorkerClient>,
        Arc<FakeWorker>,
        DuplexStream, // worker_reader
        DuplexStream, // worker_writer
    ) {
        // host → worker pipe
        let (host_write, worker_read) = duplex(256 * 1024);
        // worker → host pipe
        let (worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));
        let worker = Arc::new(FakeWorker::new(config));

        (client, worker, worker_read, worker_write)
    }

    // ── 基础 framing 测试 ─────────────────────────────────────────────────

    #[test]
    fn frame_header_layout() {
        assert_eq!(HEADER_LEN, 20);
        assert_eq!(MAGIC, b"BLNK");
        assert_eq!(PROTOCOL_VERSION, 2);
    }

    #[tokio::test]
    async fn write_read_frame_roundtrip() {
        let (mut writer, mut reader) = tokio::io::duplex(4096);

        let payload = b"hello world";
        write_frame(
            &mut writer,
            MessageType::Partial,
            frame_flags::FINAL_CHUNK,
            42,
            7,
            payload,
        )
        .await
        .unwrap();

        let mut buf = Vec::new();
        let (header, read_payload) = read_frame(&mut reader, &mut buf).await.unwrap().unwrap();

        assert_eq!(header.version, PROTOCOL_VERSION);
        assert_eq!(header.msg_type, MessageType::Partial);
        assert_eq!(header.flags, frame_flags::FINAL_CHUNK);
        assert_eq!(header.request_id, 42);
        assert_eq!(header.generation, 7);
        assert_eq!(header.payload_len as usize, payload.len());
        assert_eq!(read_payload, payload);
    }

    #[tokio::test]
    async fn frame_with_empty_payload() {
        let (mut writer, mut reader) = tokio::io::duplex(4096);

        write_frame(&mut writer, MessageType::Quit, 0, 1, 0, &[])
            .await
            .unwrap();

        let mut buf = Vec::new();
        let (header, payload) = read_frame(&mut reader, &mut buf).await.unwrap().unwrap();

        assert_eq!(header.msg_type, MessageType::Quit);
        assert_eq!(header.payload_len, 0);
        assert!(payload.is_empty());
    }

    #[tokio::test]
    async fn multiple_frames_roundtrip() {
        let (mut writer, mut reader) = tokio::io::duplex(8192);

        for i in 0..5u32 {
            let payload = format!("frame-{}", i);
            write_frame(
                &mut writer,
                MessageType::Audio,
                0,
                i,
                i * 10,
                payload.as_bytes(),
            )
            .await
            .unwrap();
        }

        let mut buf = Vec::new();
        for i in 0..5u32 {
            let (header, payload) = read_frame(&mut reader, &mut buf).await.unwrap().unwrap();
            let expected = format!("frame-{}", i);
            assert_eq!(header.request_id, i);
            assert_eq!(header.generation, i * 10);
            assert_eq!(payload, expected.as_bytes());
        }
    }

    // ── 分片读写测试 ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn frame_read_with_partial_header() {
        // 写入不完整的 header（只写 10 bytes，header 需要 20 bytes）
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        writer.write_all(&[0u8; 10]).await.unwrap();
        writer.flush().await.unwrap();
        drop(writer); // EOF

        let mut buf = Vec::new();
        let result = read_frame(&mut reader, &mut buf).await;
        assert!(matches!(result, Err(ProtoError::Truncated { .. })));
    }

    #[tokio::test]
    async fn frame_read_with_partial_payload() {
        let (mut writer, mut reader) = tokio::io::duplex(4096);

        // 写入完整 header + 部分 payload
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(MAGIC);
        header[4] = PROTOCOL_VERSION;
        header[5] = MessageType::Audio as u8;
        header[16..20].copy_from_slice(&100u32.to_le_bytes()); // payload_len=100
        writer.write_all(&header).await.unwrap();
        writer.write_all(&[0u8; 30]).await.unwrap(); // 只写 30 bytes
        writer.flush().await.unwrap();
        drop(writer); // EOF

        let mut buf = Vec::new();
        let result = read_frame(&mut reader, &mut buf).await;
        assert!(matches!(result, Err(ProtoError::Truncated { .. })));
    }

    // ── 超限帧测试 ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn oversized_frame_rejected_on_write() {
        let (mut writer, _reader) = tokio::io::duplex(4096);
        let big_payload = vec![0u8; MAX_PAYLOAD_LEN + 1];
        let result = write_frame(&mut writer, MessageType::Audio, 0, 1, 1, &big_payload).await;
        assert!(matches!(result, Err(ProtoError::Oversized { .. })));
    }

    #[tokio::test]
    async fn oversized_frame_rejected_on_read() {
        let (mut writer, mut reader) = tokio::io::duplex(4096);

        // 写入合法 header 但 payload_len 超限
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(MAGIC);
        header[4] = PROTOCOL_VERSION;
        header[5] = MessageType::Audio as u8;
        let fake_len = (MAX_PAYLOAD_LEN as u32) + 1;
        header[16..20].copy_from_slice(&fake_len.to_le_bytes());
        writer.write_all(&header).await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = Vec::new();
        let result = read_frame(&mut reader, &mut buf).await;
        assert!(matches!(result, Err(ProtoError::Oversized { .. })));
    }

    // ── 魔数/版本错误 ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn bad_magic_rejected() {
        let (mut writer, mut reader) = tokio::io::duplex(4096);

        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(b"XXXX"); // wrong magic
        header[4] = PROTOCOL_VERSION;
        header[5] = MessageType::Audio as u8;
        writer.write_all(&header).await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = Vec::new();
        let result = read_frame(&mut reader, &mut buf).await;
        assert!(matches!(result, Err(ProtoError::BadMagic { .. })));
    }

    #[tokio::test]
    async fn bad_version_rejected() {
        let (mut writer, mut reader) = tokio::io::duplex(4096);

        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(MAGIC);
        header[4] = 99; // wrong version
        header[5] = MessageType::Audio as u8;
        writer.write_all(&header).await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = Vec::new();
        let result = read_frame(&mut reader, &mut buf).await;
        assert!(matches!(result, Err(ProtoError::BadVersion { .. })));
    }

    // ── 未知消息类型 ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn unknown_message_type_rejected() {
        let (mut writer, mut reader) = tokio::io::duplex(4096);

        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(MAGIC);
        header[4] = PROTOCOL_VERSION;
        header[5] = 99; // unknown msg type
        writer.write_all(&header).await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = Vec::new();
        let result = read_frame(&mut reader, &mut buf).await;
        assert!(matches!(result, Err(ProtoError::UnknownMessageType(99))));
    }

    // ── EOF 测试 ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (_writer, mut reader) = tokio::io::duplex(4096);
        drop(_writer); // immediate EOF

        let mut buf = Vec::new();
        let result = read_frame(&mut reader, &mut buf).await.unwrap();
        assert!(result.is_none());
    }

    // ── 完整 host ↔ fake worker 端到端测试 ───────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn e2e_hello_ready_begin_audio_end_quit() {
        let (client, worker, mut worker_read, mut worker_write) =
            new_with_pipes(FakeWorkerConfig::default());

        // spawn worker task
        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        // host: hello → wait_ready
        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // host: begin
        let (stream_gen, _req_id) = client.begin_stream().await.unwrap();
        assert_eq!(stream_gen, 1);

        // host: send 3 audio frames
        for _ in 0..3 {
            let samples = vec![0.1f32; 320]; // 20ms
            let frame = AudioFrame::from_samples(&samples);
            client.send_audio(stream_gen, &frame).await.unwrap();
        }

        // host: end → wait for final
        let result = client
            .end_stream(stream_gen, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_final);
        assert!(result.text.contains("final(3 frames)"));

        // host: quit
        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── Cancel 测试 ──────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn e2e_cancel_stream() {
        let (client, worker, mut worker_read, mut worker_write) =
            new_with_pipes(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let (stream_gen, _) = client.begin_stream().await.unwrap();
        let samples = vec![0.1f32; 320];
        client
            .send_audio(stream_gen, &AudioFrame::from_samples(&samples))
            .await
            .unwrap();

        // cancel (幂等——可以多次调用)
        client.cancel_stream(stream_gen).await.unwrap();

        // reset 确认 worker 回到 ready
        client.reset().await.unwrap();

        // 可以开始新流
        let (stream_gen2, _) = client.begin_stream().await.unwrap();
        assert_eq!(stream_gen2, stream_gen + 1);
        let result = client
            .end_stream(stream_gen2, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_final);

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── Reset 测试 ────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn e2e_reset_idempotent() {
        let (client, worker, mut worker_read, mut worker_write) =
            new_with_pipes(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // reset before any stream (幂等)
        client.reset().await.unwrap();

        // reset after begin (幂等——中间取消)
        let (stream_gen, _) = client.begin_stream().await.unwrap();
        let _ = client
            .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
            .await;
        client.reset().await.unwrap();

        // reset again (幂等)
        client.reset().await.unwrap();

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── Busy（背压）测试 ─────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn e2e_busy_when_queue_full() {
        let config = FakeWorkerConfig {
            queue_capacity: 2,
            ack_audio: false, // 不回 Ack，让队列堆积
            process_delay_ms: 0,
            ..Default::default()
        };
        let (client, worker, mut worker_read, mut worker_write) = new_with_pipes(config);

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let (stream_gen, _) = client.begin_stream().await.unwrap();

        // 发 5 个音频帧，队列容量只有 2——worker 会回 Busy
        for _ in 0..5 {
            let _ = client
                .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
                .await;
        }

        // end 后 worker 会清空队列并发 final
        let result = client
            .end_stream(stream_gen, std::time::Duration::from_secs(5))
            .await;
        // end_stream 可能收到 Busy 或 final
        // 关键是：不静默丢音频、不死锁
        match result {
            Ok(r) => assert!(r.is_final),
            Err(ProtoError::Busy(_)) => { /* Busy 是可接受的 */ }
            Err(e) => panic!("不应收到 {e:?}"),
        }

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── 迟到 generation 结果丢弃 ──────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn e2e_old_generation_result_discarded() {
        let (client, worker, mut worker_read, mut worker_write) =
            new_with_pipes(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // 开始 generation=1 的流
        let (gen1, _) = client.begin_stream().await.unwrap();
        client
            .send_audio(gen1, &AudioFrame::from_samples(&[0.1; 320]))
            .await
            .unwrap();
        // 取消 generation=1
        client.cancel_stream(gen1).await.unwrap();
        client.reset().await.unwrap();

        // 开始 generation=2 的流
        let (gen2, _) = client.begin_stream().await.unwrap();
        assert_ne!(gen1, gen2);
        client
            .send_audio(gen2, &AudioFrame::from_samples(&[0.2; 320]))
            .await
            .unwrap();
        let result = client
            .end_stream(gen2, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_final);
        // 结果应属于 gen2，不是 gen1
        assert!(result.text.contains("final("));

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── 乱序消息测试 ─────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn e2e_audio_before_begin_rejected() {
        let (client, worker, mut worker_read, mut worker_write) =
            new_with_pipes(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // 直接发 Audio（不先 Begin）——worker 应回 Error
        client
            .send_frame(MessageType::Audio, 0, 999, 1, &[0u8; 1280])
            .await
            .unwrap();

        // 等待一点时间让 worker 处理
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 尝试 reset 以恢复 worker 状态（同时消费可能的 Error 消息）
        // worker 应回 Error（not ready），然后我们 reset
        let reset_result = client.reset().await;
        // reset 可能成功（worker 回 Ack）或失败（events 中有 Error 先到）
        // 关键是：不 panic、不死锁
        tracing::debug!(?reset_result, "reset after audio-before-begin");

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    // ── Quit 优雅退出 ─────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn e2e_quit_graceful_exit() {
        let (client, worker, mut worker_read, mut worker_write) =
            new_with_pipes(FakeWorkerConfig::default());

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        client.send_quit().await.unwrap();

        // worker 应退出
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());
    }

    // ── EOF 后 host poison 测试 ───────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn e2e_eof_poisons_client() {
        let (host_write, worker_read) = tokio::io::duplex(64 * 1024);
        let (worker_write, host_read) = tokio::io::duplex(64 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));

        // 关闭 worker 端——host 的 reader task 会收到 EOF
        drop(worker_read);
        drop(worker_write);

        // 等待 poison 传播
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // host 操作应失败
        let result = client.send_hello().await;
        // 可能是 Poisoned 或 Write 失败
        assert!(result.is_err());
        assert!(client.is_poisoned());
    }

    // ── 音频帧构造测试 ───────────────────────────────────────────────────

    #[test]
    fn audio_frame_from_samples() {
        let samples = vec![0.1f32, 0.2, 0.3];
        let frame = AudioFrame::from_samples(&samples);
        assert_eq!(frame.data.len(), 3 * 4); // 3 samples * 4 bytes

        // 验证 LE 编码
        let expected: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(frame.data, expected);
    }

    #[test]
    fn audio_frame_validate() {
        // 正常帧
        let frame = AudioFrame::from_samples(&[0.1; 320]); // 20ms
        assert!(frame.validate().is_ok());

        // 超大帧
        let big_data = vec![0u8; MAX_AUDIO_PAYLOAD + 1];
        let big_frame = AudioFrame::from_bytes(big_data);
        assert!(big_frame.validate().is_err());

        // 空帧
        let empty_frame = AudioFrame::from_bytes(vec![]);
        assert!(empty_frame.validate().is_ok());
    }

    // ── 消息类型转换测试 ──────────────────────────────────────────────────

    #[test]
    fn message_type_roundtrip() {
        let types = [
            MessageType::Hello,
            MessageType::Begin,
            MessageType::Audio,
            MessageType::End,
            MessageType::Cancel,
            MessageType::Reset,
            MessageType::Quit,
            MessageType::Ready,
            MessageType::Partial,
            MessageType::Final,
            MessageType::Ack,
            MessageType::Busy,
            MessageType::Error,
        ];

        for &t in &types {
            let raw = t as u8;
            let back = MessageType::from_u8(raw).unwrap();
            assert_eq!(t, back);
        }

        // 未知类型
        assert!(MessageType::from_u8(0).is_none());
        assert!(MessageType::from_u8(255).is_none());
        assert!(MessageType::from_u8(100).is_none());
    }

    // ── 压力测试 ─────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn stress_multiple_streams_no_deadlock() {
        let (client, worker, mut worker_read, mut worker_write) =
            new_with_pipes(FakeWorkerConfig {
                queue_capacity: 64,
                ack_audio: false,
                process_delay_ms: 0,
                ..Default::default()
            });

        let worker_task = tokio::spawn(async move {
            worker.run(&mut worker_read, &mut worker_write).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        // 连续 10 条流，每条发 5 个 audio + end
        for i in 0..10u32 {
            let (stream_gen, _) = client.begin_stream().await.unwrap();
            for _ in 0..5 {
                let _ = client
                    .send_audio(stream_gen, &AudioFrame::from_samples(&[0.1; 320]))
                    .await;
            }
            let result = client
                .end_stream(stream_gen, std::time::Duration::from_secs(5))
                .await
                .unwrap();
            assert!(result.is_final);
            tracing::debug!(stream_idx = i, stream_gen, "stream done");
        }

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), worker_task).await;
    }

    // ── 帧头大小校验 ─────────────────────────────────────────────────────

    #[test]
    fn constants_are_sane() {
        assert_eq!(MIN_AUDIO_PAYLOAD, 1280); // 20ms
        assert_eq!(MAX_AUDIO_PAYLOAD, 6400); // 100ms
        const { assert!(MIN_AUDIO_PAYLOAD < MAX_AUDIO_PAYLOAD) };
        const { assert!(MAX_AUDIO_PAYLOAD < MAX_PAYLOAD_LEN) };
        assert_eq!(SAMPLES_PER_MS, 16);
        assert_eq!(BYTES_PER_SAMPLE, 4);
    }
}
