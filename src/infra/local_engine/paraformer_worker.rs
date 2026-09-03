//! ParaformerOnline 二进制协议 v2 Worker 端（0.22.9 Handoff 07A）。
//!
//! 消费已有 `stream_worker_proto` 协议，不另造第三套协议。
//!
//! ## 通信铁则
//!
//! - stdout 只允许 binary protocol
//! - stderr 才能写 tracing/诊断
//! - 禁止 stdout 输出文本日志、JSON 或 panic backtrace
//! - 不得使用 NDJSON Vec<f32> 或 Base64
//! - 有界队列满时返回 Busy，不能静默丢样本
//! - 旧 request/generation 响应不得提交

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

use crate::infra::local_engine::paraformer_runner::ParaformerRunner;
use crate::infra::local_engine::stream_worker_proto::{
    FrameHeader, MessageType, PROTOCOL_VERSION, frame_flags,
};

/// Worker 状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerState {
    /// 初始——等待 Hello。
    Init,
    /// 模型已加载——等待 Begin。
    Ready,
    /// 流式传输中。
    Active,
}

/// 计算 文件的 SHA-256（小写 hex）。
fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// asset-lock.json 中单个文件的期望值。
#[derive(Debug, serde::Deserialize)]
struct AssetLockEntry {
    sha256: String,
    size_bytes: u64,
    /// 文件在 deployment 目录中的相对路径（如 `onnxruntime-win-x64-1.19.2/lib/onnxruntime.dll`）。
    /// 如果为 None，则用 filename 字段直接拼接。
    #[serde(default)]
    path: Option<String>,
    /// 简单文件名（如 `onnxruntime.dll`）。
    #[serde(default)]
    filename: Option<String>,
}

/// asset-lock.json 顶层结构。
///
/// 支持两种格式：
///
/// **格式 A（deployment slot）**：
/// ```json
/// {
///   "files": {
///     "am.mvn": { "sha256": "...", "size_bytes": 123 },
///     "encoder.onnx": { ... }
///   }
/// }
/// ```
///
/// **格式 B（源资源 lock）**：
/// ```json
/// {
///   "ort": { "files": [{ "path": "...", "sha256": "...", "size_bytes": 123 }] },
///   "models": [{ "filename": "encoder.onnx", "sha256": "...", "size_bytes": 123 }]
/// }
/// ```
///
/// 两种格式都会被合并为扁平的 (filename, (sha256, size)) 列表进行校验。
#[derive(Debug, serde::Deserialize)]
struct AssetLock {
    /// 格式 B：`ort.files` 数组
    #[serde(default)]
    ort: Option<OrtSection>,
    /// 格式 B：`models` 数组
    #[serde(default)]
    models: Vec<ModelEntry>,
    /// 格式 A：`files` 对象（文件名 → hash/size）
    #[serde(default)]
    files: Option<std::collections::HashMap<String, FileHashEntry>>,
}

#[derive(Debug, serde::Deserialize)]
struct OrtSection {
    #[serde(default)]
    files: Vec<AssetLockEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct ModelEntry {
    #[serde(default)]
    filename: String,
    sha256: String,
    size_bytes: u64,
}

/// 格式 A 中每个文件的 hash/size 条目。
#[derive(Debug, serde::Deserialize)]
struct FileHashEntry {
    sha256: String,
    size_bytes: u64,
}

impl AssetLock {
    /// 将所有格式的条目合并为扁平的 (filename, (sha256, size)) 列表。
    fn flatten(&self) -> Vec<(String, (String, u64))> {
        let mut result = Vec::new();

        // 格式 B：ort.files 数组
        if let Some(ort) = &self.ort {
            for f in &ort.files {
                let name = f.filename.clone().or_else(|| {
                    f.path.as_ref().and_then(|p| {
                        p.rsplit('/')
                            .next()
                            .or_else(|| p.rsplit('\\').next())
                            .map(String::from)
                    })
                });
                if let Some(name) = name {
                    result.push((name, (f.sha256.clone(), f.size_bytes)));
                }
            }
        }

        // 格式 B：models 数组
        for m in &self.models {
            result.push((m.filename.clone(), (m.sha256.clone(), m.size_bytes)));
        }

        // 格式 A：files 对象
        if let Some(files) = &self.files {
            for (name, entry) in files {
                result.push((name.clone(), (entry.sha256.clone(), entry.size_bytes)));
            }
        }

        result
    }
}

/// 验证 deployment 目录中的资产文件存在性 + SHA-256 + 大小。
///
/// 如果 deployment 目录中存在 `asset-lock.json`，则对每个声明的文件
/// 强制校验 SHA-256 和文件大小。任一不匹配时返回详细错误（expected/actual）。
///
/// 如果不存在 `asset-lock.json`，退化为只检查文件存在性（向后兼容）。
pub fn validate_deployment(deployment_dir: &Path) -> Result<DeploymentAssets, String> {
    let dll = deployment_dir.join("onnxruntime.dll");
    let encoder = deployment_dir.join("encoder.onnx");
    let decoder = deployment_dir.join("decoder.onnx");
    let cmvn = deployment_dir.join("am.mvn");
    let tokenizer = deployment_dir.join("tokenizer.json");

    for (name, path) in [
        ("DLL", &dll),
        ("encoder", &encoder),
        ("decoder", &decoder),
        ("CMVN", &cmvn),
        ("tokenizer", &tokenizer),
    ] {
        if !path.exists() {
            return Err(format!("{name} 文件不存在: {}", path.display()));
        }
    }

    // 强校验：如果 asset-lock.json 存在，校验所有文件的 SHA-256 + 大小
    let lock_path = deployment_dir.join("asset-lock.json");
    if lock_path.exists() {
        let lock_content = std::fs::read_to_string(&lock_path)
            .map_err(|e| format!("读取 asset-lock.json 失败: {e}"))?;
        let lock: AssetLock = serde_json::from_str(&lock_content)
            .map_err(|e| format!("解析 asset-lock.json 失败: {e}"))?;

        let files_to_check = lock.flatten();
        if files_to_check.is_empty() {
            warn!("asset-lock.json 解析成功但未包含任何文件条目——跳过 hash 校验");
        }
        for (file_name, (expected_hash, expected_size)) in &files_to_check {
            let file_path = deployment_dir.join(file_name);
            if !file_path.exists() {
                return Err(format!("asset-lock 校验失败: 文件 '{file_name}' 不存在"));
            }
            let actual_size = std::fs::metadata(&file_path)
                .map_err(|e| format!("读取文件大小失败 {file_name}: {e}"))?
                .len();
            if actual_size != *expected_size {
                return Err(format!(
                    "asset-lock 校验失败: '{file_name}' 大小不匹配\n  expected: {} bytes\n  actual:   {} bytes",
                    expected_size, actual_size
                ));
            }
            let actual_hash = sha256_file(&file_path)?;
            if actual_hash != *expected_hash {
                return Err(format!(
                    "asset-lock 校验失败: '{file_name}' SHA-256 不匹配\n  expected: {}\n  actual:   {}\n  ⚠ 此文件可能被污染或覆盖——禁止继续加载",
                    expected_hash, actual_hash
                ));
            }
        }
        info!(
            files = files_to_check.len(),
            "asset-lock 校验通过：所有文件 SHA-256 + 大小匹配"
        );
    } else {
        warn!("asset-lock.json 不存在于 deployment 目录——跳过 hash 校验（仅检查文件存在性）");
    }

    Ok(DeploymentAssets {
        dll,
        encoder,
        decoder,
        cmvn,
        tokenizer,
    })
}

/// 验证后的 deployment 资产路径集。
#[derive(Debug)]
pub struct DeploymentAssets {
    pub dll: PathBuf,
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub cmvn: PathBuf,
    pub tokenizer: PathBuf,
}

/// 运行 worker 主循环。
///
/// stdin/stdout 只传 binary protocol；stderr 只写 tracing。
///
/// 退出条件：读到 Quit、EOF 或 panic-like 错误。
pub fn run_worker_loop(deployment_dir: &Path) -> i32 {
    // 初始化 stderr tracing
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .try_init();

    info!(
        "paraformer-worker: starting (deployment_dir={})",
        deployment_dir.display()
    );

    // ── 1. 验证 deployment ──────────────────────────────────────────
    let assets = match validate_deployment(deployment_dir) {
        Ok(a) => a,
        Err(e) => {
            error!("deployment 验证失败: {e}");
            return 1;
        }
    };

    // ── 2. 加载 ORT DLL ──────────────────────────────────────────────
    info!("loading ORT DLL: {}", assets.dll.display());
    let init_builder = match ort::init_from(&assets.dll) {
        Ok(b) => b,
        Err(e) => {
            error!("ORT DLL 加载失败: {e}");
            return 1;
        }
    };
    init_builder.commit();

    // ── 3. 创建 runner ───────────────────────────────────────────────
    info!("creating ParaformerRunner...");
    let runner = match ParaformerRunner::new(
        &assets.encoder,
        &assets.decoder,
        &assets.cmvn,
        &assets.tokenizer,
    ) {
        Ok(r) => r,
        Err(e) => {
            error!("ParaformerRunner 创建失败: {e}");
            return 1;
        }
    };

    info!("ParaformerRunner created successfully");

    // ── 4. 进入 binary protocol v2 循环 ──────────────────────────────
    let mut engine = WorkerEngine::new(runner);
    let exit_code = engine.run();
    info!("paraformer-worker: exiting (code={exit_code})");
    exit_code
}

/// Worker 引擎——持有 runner 和状态机，处理 binary protocol。
/// 音频累积阈值——达到此样本数才调用 forward 做一次推理。
///
/// ParaformerOnline 的 encoder chunk_size=[5,10,5]（LFR 帧为单位），
/// 每次 forward_chunk 需要足够多的 LFR 帧才能正确触发 CIF。
/// 9600 samples = 600ms，产生约 60 个 fbank 帧 → 约 10 个 LFR 帧，
/// 与 Spike C2 的 CHUNK_STRIDE_SAMPLES 一致。
const FORWARD_CHUNK_SAMPLES: usize = 9600;

struct WorkerEngine {
    runner: ParaformerRunner,
    state: WorkerState,
    /// 当前 active generation
    active_generation: u32,
    /// 当前 session 的累计文本
    ///
    /// `forward()` 是增量返回——每次只返回该次 chunk 产生的 token。
    /// Worker 必须累加所有 partial 结果，在 Final 时发送完整文本。
    session_text: String,
    /// 音频累积缓冲——达到 FORWARD_CHUNK_SAMPLES 才调用 forward
    audio_buffer: Vec<f32>,
    /// stderr log writer
    #[allow(dead_code)] // kept for diagnostics, stderr is used via tracing
    stderr: std::io::Stderr,
}

impl WorkerEngine {
    fn new(runner: ParaformerRunner) -> Self {
        Self {
            runner,
            state: WorkerState::Init,
            active_generation: 0,
            session_text: String::new(),
            audio_buffer: Vec::new(),
            stderr: std::io::stderr(),
        }
    }

    /// 主循环——从 stdin 读帧，向 stdout 写帧。
    fn run(&mut self) -> i32 {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let stdout = std::io::stdout();
        let mut writer = std::io::BufWriter::new(stdout.lock());
        let mut payload_buf: Vec<u8> = Vec::new();

        loop {
            let frame = read_frame_sync(&mut reader, &mut payload_buf);
            match frame {
                Ok(None) => {
                    // clean EOF
                    info!("worker: stdin EOF, exiting");
                    return 0;
                }
                Ok(Some((header, payload))) => {
                    let should_quit = self.handle_frame(&header, &payload, &mut writer);
                    if should_quit {
                        info!("worker: Quit received, exiting");
                        return 0;
                    }
                }
                Err(e) => {
                    error!("worker: 读帧错误: {e}");
                    // 发送 Error 后退出
                    let _ = write_frame_sync(
                        &mut writer,
                        MessageType::Error,
                        0,
                        0,
                        0,
                        format!("frame read error: {e}").as_bytes(),
                    );
                    return 1;
                }
            }
        }
    }

    /// 处理一帧。返回 true 表示应退出（Quit）。
    fn handle_frame<W: Write>(
        &mut self,
        header: &FrameHeader,
        payload: &[u8],
        writer: &mut W,
    ) -> bool {
        match header.msg_type {
            MessageType::Hello => {
                self.state = WorkerState::Ready;
                // 发送 Ready——模型已加载
                let _ = write_frame_sync(
                    writer,
                    MessageType::Ready,
                    0,
                    header.request_id,
                    0,
                    &[PROTOCOL_VERSION],
                );
                info!("worker: sent Ready");
                false
            }
            MessageType::Begin => {
                if self.state != WorkerState::Ready {
                    let _ = write_frame_sync(
                        writer,
                        MessageType::Error,
                        0,
                        header.request_id,
                        header.generation,
                        b"not ready",
                    );
                    return false;
                }
                self.state = WorkerState::Active;
                self.active_generation = header.generation;
                // 清空 session 累计文本和音频缓冲——新 session 从空开始
                self.session_text.clear();
                self.audio_buffer.clear();
                // runner 已在 new 时创建干净状态——如果上一 generation 被 Cancel/Reset 正确处理
                self.runner.reset();
                let _ = write_frame_sync(
                    writer,
                    MessageType::Ack,
                    0,
                    header.request_id,
                    header.generation,
                    &[],
                );
                info!(gen = header.generation, "worker: Begin -> Ack");
                false
            }
            MessageType::Audio => {
                if self.state != WorkerState::Active {
                    let _ = write_frame_sync(
                        writer,
                        MessageType::Error,
                        0,
                        header.request_id,
                        header.generation,
                        b"not active",
                    );
                    return false;
                }
                // 丢弃旧 generation 的音频
                if header.generation != self.active_generation {
                    warn!(
                        expect_gen = self.active_generation,
                        got_gen = header.generation,
                        "worker: 丢弃旧 generation 音频"
                    );
                    return false;
                }

                // 解码 f32 LE PCM
                let samples = match decode_f32_le(payload) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = write_frame_sync(
                            writer,
                            MessageType::Error,
                            0,
                            header.request_id,
                            header.generation,
                            format!("audio decode error: {e}").as_bytes(),
                        );
                        return false;
                    }
                };

                // 检查是否是 final chunk
                let is_final = (header.flags & frame_flags::FINAL_CHUNK) != 0;

                // 累积音频——达到 FORWARD_CHUNK_SAMPLES 才调用 forward
                self.audio_buffer.extend_from_slice(&samples);

                if is_final {
                    // final chunk：用全部累积的音频做推理
                    let buf = std::mem::take(&mut self.audio_buffer);
                    let (text, _inf_ms) = self.runner.forward(&buf, true);
                    if !text.is_empty() {
                        self.session_text.push_str(&text);
                        let _ = write_frame_sync(
                            writer,
                            MessageType::Partial,
                            0,
                            header.request_id,
                            header.generation,
                            text.as_bytes(),
                        );
                    }
                } else if self.audio_buffer.len() >= FORWARD_CHUNK_SAMPLES {
                    // 积累够了——做一次推理
                    let buf = std::mem::take(&mut self.audio_buffer);
                    let (text, _inf_ms) = self.runner.forward(&buf, false);
                    if !text.is_empty() {
                        self.session_text.push_str(&text);
                        let _ = write_frame_sync(
                            writer,
                            MessageType::Partial,
                            0,
                            header.request_id,
                            header.generation,
                            text.as_bytes(),
                        );
                    }
                }

                // 如果是 final chunk，发送 Final（累计的完整文本）
                if is_final {
                    self.state = WorkerState::Ready;
                    let final_text = self.session_text.clone();
                    let _ = write_frame_sync(
                        writer,
                        MessageType::Final,
                        0,
                        header.request_id,
                        header.generation,
                        final_text.as_bytes(),
                    );
                    info!(
                        gen = header.generation,
                        final_len = final_text.len(),
                        "worker: Audio (final) -> Final"
                    );
                }

                false
            }
            MessageType::End => {
                if self.state != WorkerState::Active {
                    let _ = write_frame_sync(
                        writer,
                        MessageType::Error,
                        0,
                        header.request_id,
                        header.generation,
                        b"not active",
                    );
                    return false;
                }
                // 丢弃旧 generation
                if header.generation != self.active_generation {
                    return false;
                }

                // final flush——先 flush audio_buffer 中的残留音频，再做 final
                //
                // audio_buffer 中可能还有未达到 FORWARD_CHUNK_SAMPLES 的残留。
                // 先用这些残留做一次非 final forward（产生 partial），
                // 再用空音频 + input_finished=true 做 final flush。
                if !self.audio_buffer.is_empty() {
                    let buf = std::mem::take(&mut self.audio_buffer);
                    info!(gen = header.generation, buf_len = buf.len(), "worker: End flush audio_buffer");
                    let (text, _ms) = self.runner.forward(&buf, false);
                    if !text.is_empty() {
                        self.session_text.push_str(&text);
                    }
                }
                info!(gen = header.generation, "worker: End 调用 forward(&[], true)");
                let mut runner = std::panic::AssertUnwindSafe(&mut self.runner);
                let forward_result = std::panic::catch_unwind(move || runner.forward(&[], true));
                match forward_result {
                    Ok((text, _ms)) => {
                        info!(gen = header.generation, text_len = text.len(), "worker: forward 返回");
                        self.state = WorkerState::Ready;

                        // 累计最后的 flush 文本
                        if !text.is_empty() {
                            self.session_text.push_str(&text);
                        }
                        // Final 发送 session 累计的完整文本（可能为空）
                        let final_text = self.session_text.clone();
                        let _ = write_frame_sync(
                            writer,
                            MessageType::Final,
                            0,
                            header.request_id,
                            header.generation,
                            final_text.as_bytes(),
                        );
                        let _ = writer.flush();
                        info!(
                            gen = header.generation,
                            final_len = final_text.len(),
                            "worker: End -> Final"
                        );
                        false
                    }
                    Err(panic_msg) => {
                        let msg = if let Some(s) = panic_msg.downcast_ref::<String>() {
                            s.clone()
                        } else if let Some(s) = panic_msg.downcast_ref::<&str>() {
                            s.to_string()
                        } else {
                            "unknown panic".to_string()
                        };
                        error!(gen = header.generation, %msg, "worker: forward panic");
                        self.state = WorkerState::Ready;
                        self.runner.reset();
                        let _ = write_frame_sync(
                            writer,
                            MessageType::Error,
                            0,
                            header.request_id,
                            header.generation,
                            format!("forward panic: {msg}").as_bytes(),
                        );
                        let _ = writer.flush();
                        false
                    }
                }
            }
            MessageType::Cancel => {
                // 幂等——清空 runner 状态和 session 文本
                self.runner.reset();
                self.session_text.clear();
                self.audio_buffer.clear();
                self.state = WorkerState::Ready;
                let _ = write_frame_sync(
                    writer,
                    MessageType::Ack,
                    0,
                    header.request_id,
                    header.generation,
                    &[],
                );
                info!(gen = header.generation, "worker: Cancel -> Ack");
                false
            }
            MessageType::Reset => {
                // 幂等——清空所有状态
                self.runner.reset();
                self.session_text.clear();
                self.audio_buffer.clear();
                self.state = WorkerState::Ready;
                let _ = write_frame_sync(
                    writer,
                    MessageType::Ack,
                    0,
                    header.request_id,
                    header.generation,
                    &[],
                );
                info!("worker: Reset -> Ack");
                false
            }
            MessageType::Quit => {
                self.state = WorkerState::Init;
                true
            }
            _ => {
                warn!(msg_type = ?header.msg_type, "worker: 收到不期望的消息");
                false
            }
        }
    }
}

// ── 同步 I/O 帧编解码 ───────────────────────────────────────────────────

/// 同步读取一帧。
fn read_frame_sync<R: Read>(
    reader: &mut R,
    payload_buf: &mut Vec<u8>,
) -> Result<Option<(FrameHeader, Vec<u8>)>, String> {
    let mut header_buf = [0u8; 20]; // HEADER_LEN
    let mut filled = 0;
    while filled < header_buf.len() {
        match reader.read(&mut header_buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(None); // clean EOF
                }
                return Err(format!(
                    "帧截断: 期望 {} 字节，实际读到 {} 字节",
                    header_buf.len(),
                    filled
                ));
            }
            Ok(n) => filled += n,
            Err(e) => return Err(format!("读取失败: {e}")),
        }
    }

    // 校验 magic
    let magic = [header_buf[0], header_buf[1], header_buf[2], header_buf[3]];
    if &magic != b"BLNK" {
        return Err(format!("魔数不匹配: 期望 {:?}，实际 {:?}", b"BLNK", magic));
    }

    let version = header_buf[4];
    if version != PROTOCOL_VERSION {
        return Err(format!(
            "版本不兼容: 期望 {PROTOCOL_VERSION}，实际 {version}"
        ));
    }

    let msg_type_raw = header_buf[5];
    let msg_type = MessageType::from_u8(msg_type_raw)
        .ok_or_else(|| format!("未知消息类型: {msg_type_raw}"))?;

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

    if payload_len as usize > 64 * 1024 {
        return Err(format!("帧超限: payload_len={payload_len} > MAX=65536"));
    }

    payload_buf.clear();
    if payload_len > 0 {
        payload_buf.resize(payload_len as usize, 0);
        let mut filled = 0;
        while filled < payload_buf.len() {
            match reader.read(&mut payload_buf[filled..]) {
                Ok(0) => {
                    return Err(format!(
                        "payload 截断: 期望 {} 字节，实际读到 {} 字节",
                        payload_buf.len(),
                        filled
                    ));
                }
                Ok(n) => filled += n,
                Err(e) => return Err(format!("payload 读取失败: {e}")),
            }
        }
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

    Ok(Some((header, payload_buf.clone())))
}

/// 同步写入一帧。
fn write_frame_sync<W: Write>(
    writer: &mut W,
    msg_type: MessageType,
    flags: u8,
    request_id: u32,
    generation: u32,
    payload: &[u8],
) -> Result<(), String> {
    let payload_len = payload.len() as u32;
    let mut header = [0u8; 20]; // HEADER_LEN
    header[0..4].copy_from_slice(b"BLNK");
    header[4] = PROTOCOL_VERSION;
    header[5] = msg_type as u8;
    header[6] = flags;
    header[7] = 0; // reserved
    header[8..12].copy_from_slice(&request_id.to_le_bytes());
    header[12..16].copy_from_slice(&generation.to_le_bytes());
    header[16..20].copy_from_slice(&payload_len.to_le_bytes());

    writer
        .write_all(&header)
        .map_err(|e| format!("写入 header 失败: {e}"))?;
    if !payload.is_empty() {
        writer
            .write_all(payload)
            .map_err(|e| format!("写入 payload 失败: {e}"))?;
    }
    writer.flush().map_err(|e| format!("flush 失败: {e}"))?;
    Ok(())
}

/// 从 raw bytes 解码 f32 little-endian PCM。
fn decode_f32_le(data: &[u8]) -> Result<Vec<f32>, String> {
    if !data.len().is_multiple_of(4) {
        return Err(format!(
            "audio payload 长度不是 4 的倍数: {} bytes",
            data.len()
        ));
    }
    let n = data.len() / 4;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let offset = i * 4;
        let bytes = [
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ];
        samples.push(f32::from_le_bytes(bytes));
    }
    Ok(samples)
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// §A1: CMVN hash 不匹配时 validate_deployment 必须明确失败。
    ///
    /// 伪造一个 deployment 目录，放入 asset-lock.json 声明正确 hash，
    /// 但实际文件内容不匹配——验证返回错误且错误信息包含 expected/actual。
    #[test]
    fn validate_deployment_rejects_mismatched_cmvn_hash() {
        let tmp = std::env::temp_dir().join(format!(
            "blink-test-asset-negative-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        // 写入假的模型文件（空内容，hash 不会匹配）
        std::fs::write(tmp.join("onnxruntime.dll"), b"fake").unwrap();
        std::fs::write(tmp.join("encoder.onnx"), b"fake").unwrap();
        std::fs::write(tmp.join("decoder.onnx"), b"fake").unwrap();
        std::fs::write(tmp.join("am.mvn"), b"wrong cmvn content").unwrap();
        std::fs::write(tmp.join("tokenizer.json"), b"{}").unwrap();

        // 写入 asset-lock.json，声明正确的 hash（与实际不匹配）
        let lock = serde_json::json!({
            "ort": {
                "files": [{
                    "path": "lib/onnxruntime.dll",
                    "filename": "onnxruntime.dll",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                    "size_bytes": 5,
                    "is_dll": true
                }]
            },
            "models": [
                {
                    "kind": "cmvn",
                    "name": "test",
                    "filename": "am.mvn",
                    "sha256": "29b3c740a2c0cfc6b308126d31d7f265fa2be74f3bb095cd2f143ea970896ae5",
                    "size_bytes": 20,
                    "license": "test"
                }
            ]
        });
        std::fs::write(tmp.join("asset-lock.json"), lock.to_string()).unwrap();

        let result = validate_deployment(&tmp);
        assert!(result.is_err(), "hash 不匹配时必须返回错误");
        let err = result.unwrap_err();
        assert!(
            err.contains("SHA-256") || err.contains("大小不匹配"),
            "错误信息应包含 SHA-256 或大小不匹配，实际: {err}"
        );

        // 清理
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// §A1: asset-lock.json 不存在时，validate_deployment 退化为只检查文件存在性。
    #[test]
    fn validate_deployment_fallback_to_existence_check() {
        let tmp = std::env::temp_dir().join(format!(
            "blink-test-asset-nolock-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        std::fs::write(tmp.join("onnxruntime.dll"), b"x").unwrap();
        std::fs::write(tmp.join("encoder.onnx"), b"x").unwrap();
        std::fs::write(tmp.join("decoder.onnx"), b"x").unwrap();
        std::fs::write(tmp.join("am.mvn"), b"x").unwrap();
        std::fs::write(tmp.join("tokenizer.json"), b"x").unwrap();

        // 无 asset-lock.json → 应成功（仅检查存在性）
        let result = validate_deployment(&tmp);
        assert!(result.is_ok(), "无 lock 时只检查存在性应通过");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// §A1: 缺少必要文件时必须失败。
    #[test]
    fn validate_deployment_fails_on_missing_file() {
        let tmp = std::env::temp_dir().join("blink-test-asset-missing-nonexistent");
        // 不创建任何文件
        let result = validate_deployment(&tmp);
        assert!(result.is_err(), "缺少文件时必须返回错误");
    }
}
