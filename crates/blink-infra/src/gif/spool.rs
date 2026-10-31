//! GIF 素材块存储（0.28.7，phase §3.6「后台无损压缩/写入素材块」）。
//!
//! **格式**（会话目录 `<root>/sessions/<id>/`）：
//! - `spool.bin`——每帧一个独立 zstd frame（BGRA 原始帧压缩），追加写。
//!   独立 frame → 任意帧可单独解码（phase「压缩块可独立解码」，随机预览
//!   不需要从头解整段）。
//! - `frames.jsonl`——逐帧索引（append + 每帧 flush）：`{i,o,c,r,t}` = 帧号/
//!   spool 偏移/压缩长度/原始长度/时间戳(ms)。**崩溃识别**：解析时丢弃末尾
//!   残行（半行 JSON 反序列化失败），且 `o+c` 超出 spool.bin 实际长度的条目
//!   一并丢弃——崩溃不完整尾块可识别，已完成块保留。
//! - `session.json`——会话元数据（区域/fps/尺寸/状态/统计），停止时原子重写。
//! - `thumbs/<i>.png`——编辑器按需生成的缩略图缓存（非素材真源）。
//!
//! **时长**：帧时长不落盘——由 `t` 时间戳差推导（domain `finalize_durations`），
//! 末帧时长存 `last_dur_ms`。零冗余，重复帧合并语义天然正确。
//!
//! 原始帧不进前端 IPC、不进聊天图片 stash；编辑器经 `read_frame` 按需解码。

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

mod maintenance;
pub use maintenance::{cleanup_expired, cleanup_expired_except};

/// 素材格式版本（`session.json` 的 `spool_version`；读取端拒绝更高版本）。
pub const SPOOL_VERSION: u32 = 1;

/// zstd 压缩级别（1 = 最快；录制期吞吐优先，压缩率次要）。
const ZSTD_LEVEL: i32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    #[error("素材目录不存在或不是有效会话: {0}")]
    InvalidSession(String),
    #[error("素材格式版本不受支持: {0}")]
    UnsupportedVersion(u32),
    #[error("帧索引 {0} 越界（共 {1} 帧）")]
    FrameOutOfRange(u32, u32),
    #[error("帧数据损坏（解压/长度校验失败）: 帧 {0}")]
    CorruptFrame(u32),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("元数据序列化失败: {0}")]
    Serde(#[from] serde_json::Error),
}

/// 录制区域（虚拟屏幕物理像素坐标）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureRegion {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// 会话元数据（`session.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub spool_version: u32,
    pub id: String,
    pub region: CaptureRegion,
    pub fps: u32,
    pub show_cursor: bool,
    /// 创建时间（unix ms）。
    pub created_at: u64,
    /// recording / stopped。
    pub state: String,
    /// 有效帧时间戳（ms，暂停已剔除）；停止后定稿。
    #[serde(default)]
    pub ts_ms: Vec<u64>,
    /// 末帧时长（ms）。
    #[serde(default)]
    pub last_dur_ms: u32,
    /// 压缩素材总字节。
    #[serde(default)]
    pub spool_bytes: u64,
    /// 停止原因（state=stopped 时存在）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

/// 单帧索引条目（`frames.jsonl` 行；短字段名减小索引体积）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrameIndexEntry {
    /// 帧号（0 起，追加递增）。
    i: u32,
    /// spool.bin 内偏移。
    o: u64,
    /// 压缩长度。
    c: u32,
    /// 原始长度（= w*h*4，解压后校验）。
    r: u32,
    /// 时间戳 ms。
    t: u64,
}

/// 追加式素材写入器（录制 worker 压缩线程独占持有）。
pub struct SpoolWriter {
    dir: PathBuf,
    _usage: File,
    bin: BufWriter<File>,
    idx: BufWriter<File>,
    index: Vec<FrameIndexEntry>,
    offset: u64,
    ts: Vec<u64>,
    raw_frame_len: usize,
    meta: SessionMeta,
}

impl SpoolWriter {
    /// 创建会话目录并打开写入器。目录已存在时拒绝（防重复会话 id）。
    pub fn create(root: &Path, meta: SessionMeta) -> Result<Self, SpoolError> {
        let dir = session_dir(root, &meta.id);
        if dir.exists() {
            return Err(SpoolError::InvalidSession(format!(
                "会话目录已存在: {}",
                dir.display()
            )));
        }
        fs::create_dir_all(&dir)?;
        let usage = maintenance::usage_lock(&dir, false)?;
        write_session_json(&dir, &meta)?;

        let bin = BufWriter::with_capacity(
            1 << 20,
            OpenOptions::new().create(true).append(true).open(dir.join("spool.bin"))?,
        );
        let idx = BufWriter::with_capacity(
            1 << 14,
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("frames.jsonl"))?,
        );
        let raw_frame_len = (meta.region.w as usize) * (meta.region.h as usize) * 4;
        Ok(Self {
            dir,
            _usage: usage,
            bin,
            idx,
            index: Vec::new(),
            offset: 0,
            ts: Vec::new(),
            raw_frame_len,
            meta,
        })
    }

    pub fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    /// 压缩并追加一帧（ts 为有效时间戳 ms）。返回压缩后字节。
    pub fn append_frame(&mut self, ts_ms: u64, bgra: &[u8]) -> Result<u64, SpoolError> {
        if bgra.len() != self.raw_frame_len {
            return Err(SpoolError::CorruptFrame(self.index.len() as u32));
        }
        let compressed = zstd::stream::encode_all(bgra, ZSTD_LEVEL)?;
        self.bin.write_all(&compressed)?;
        self.idx.write_all(
            serde_json::to_string(&FrameIndexEntry {
                i: self.index.len() as u32,
                o: self.offset,
                c: compressed.len() as u32,
                r: bgra.len() as u32,
                t: ts_ms,
            })
            .unwrap()
            .as_bytes(),
        )?;
        self.idx.write_all(b"\n")?;
        // 每帧 flush（ BufWriter→OS）：崩溃时至多丢当前帧，已完成帧可恢复。
        self.bin.flush()?;
        self.idx.flush()?;
        self.index.push(FrameIndexEntry {
            i: self.index.len() as u32,
            o: self.offset,
            c: compressed.len() as u32,
            r: bgra.len() as u32,
            t: ts_ms,
        });
        self.offset += compressed.len() as u64;
        self.ts.push(ts_ms);
        self.meta.spool_bytes = self.offset;
        Ok(compressed.len() as u64)
    }

    /// 当前已写帧数。
    pub fn frame_count(&self) -> u32 {
        self.index.len() as u32
    }

    /// 已写压缩字节。
    pub fn spool_bytes(&self) -> u64 {
        self.offset
    }

    /// 停止定稿：flush、写时长表（时间戳 + 末帧时长）、state=stopped。
    pub fn finalize_at(mut self, stop_reason: &str, elapsed_ms: u64) -> Result<SessionMeta, SpoolError> {
        if let Some(last_ts) = self.ts.last() {
            self.meta.last_dur_ms = elapsed_ms.saturating_sub(*last_ts).clamp(1, u32::MAX as u64) as u32;
        }
        self.finalize(stop_reason)
    }

    /// 无完整停止时钟的调用方保留元数据中的末帧兜底时长。
    pub fn finalize(mut self, stop_reason: &str) -> Result<SessionMeta, SpoolError> {
        self.bin.flush()?;
        self.idx.flush()?;
        self.meta.state = "stopped".into();
        self.meta.stop_reason = Some(stop_reason.to_string());
        self.meta.ts_ms = std::mem::take(&mut self.ts);
        write_session_json(&self.dir, &self.meta)?;
        Ok(self.meta)
    }

    /// 丢弃（取消）：消费写入器并删除目录。
    pub fn discard(self) {
        let dir = self.dir.clone();
        drop((self.bin, self.idx, self._usage));
        if let Err(e) = fs::remove_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "GIF 素材目录删除失败（待 TTL 清理兜底）");
        }
    }
}

fn session_dir(root: &Path, id: &str) -> PathBuf {
    root.join("sessions").join(id)
}

fn write_session_json(dir: &Path, meta: &SessionMeta) -> Result<(), SpoolError> {
    let final_path = dir.join("session.json");
    crate::utils::fs::atomic_write_bytes(&final_path, &serde_json::to_vec_pretty(meta)?)?;
    Ok(())
}

/// 会话 ID 是单个目录名，读取与导出共用同一边界。
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 素材读取器（编辑器/导出按需解码）。
pub struct SpoolReader {
    dir: PathBuf,
    _usage: File,
    pub meta: SessionMeta,
    index: Vec<FrameIndexEntry>,
}

impl SpoolReader {
    /// 打开会话：读元数据 + 索引（丢崩溃残行/越界尾块），校验格式版本。
    pub fn open(root: &Path, id: &str) -> Result<Self, SpoolError> {
        if !is_valid_session_id(id) {
            return Err(SpoolError::InvalidSession("会话标识无效".into()));
        }
        let dir = session_dir(root, id);
        let usage = maintenance::usage_lock(&dir, false)?;
        let meta_path = dir.join("session.json");
        if !meta_path.is_file() {
            return Err(SpoolError::InvalidSession(id.to_string()));
        }
        let meta: SessionMeta = serde_json::from_slice(&fs::read(&meta_path)?)
            .map_err(|e| SpoolError::InvalidSession(format!("session.json 解析失败: {e}")))?;
        if meta.spool_version > SPOOL_VERSION {
            return Err(SpoolError::UnsupportedVersion(meta.spool_version));
        }
        let spool_len = fs::metadata(dir.join("spool.bin")).map(|m| m.len()).unwrap_or(0);
        let mut index = Vec::new();
        if spool_len > 0 {
            let reader = BufReader::new(File::open(dir.join("frames.jsonl"))?);
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    // 读 IO 错误中断：保留已解析部分（崩溃残留语义）
                    Err(_) => break,
                };
                if line.is_empty() {
                    continue;
                }
                // 崩溃半行：反序列化失败即停止解析（后续行不可能有效）
                let Ok(entry) = serde_json::from_str::<FrameIndexEntry>(&line) else {
                    break;
                };
                // 尾块不完整（索引已写但 spool 未刷完）：丢弃该帧
                if entry.o + entry.c as u64 > spool_len {
                    break;
                }
                index.push(entry);
            }
        }
        Ok(Self { dir, _usage: usage, meta, index })
    }

    pub fn frame_count(&self) -> u32 {
        self.index.len() as u32
    }

    /// 帧时间戳（优先元数据定稿值；未定稿回退索引）。
    pub fn timestamps(&self) -> Vec<u64> {
        if !self.meta.ts_ms.is_empty() {
            return self.meta.ts_ms.clone();
        }
        self.index.iter().map(|e| e.t).collect()
    }

    pub fn durations(&self) -> Vec<u32> {
        let mut durations = super::overload::finalize_durations(&self.timestamps(), self.meta.fps);
        if let Some(last) = durations.last_mut() {
            *last = self.meta.last_dur_ms.max(1);
        }
        durations
    }

    /// 按需解码单帧（随机访问；校验长度）。
    pub fn read_frame(&self, idx: u32) -> Result<Vec<u8>, SpoolError> {
        self.read_frame_bounded(idx, 16 << 20)
    }

    /// 解压前校验索引预算，解压时限制输出，损坏素材不能突破帧缓冲上限。
    pub fn read_frame_bounded(&self, idx: u32, max_bytes: usize) -> Result<Vec<u8>, SpoolError> {
        let Some(entry) = self.index.get(idx as usize) else {
            return Err(SpoolError::FrameOutOfRange(idx, self.index.len() as u32));
        };
        if entry.r as usize > max_bytes || entry.c as usize > max_bytes + (1 << 20) {
            return Err(SpoolError::CorruptFrame(idx));
        }
        let mut f = File::open(self.dir.join("spool.bin"))?;
        f.seek(SeekFrom::Start(entry.o))?;
        let mut compressed = vec![0u8; entry.c as usize];
        f.read_exact(&mut compressed)?;
        let mut decoder = zstd::stream::read::Decoder::new(&compressed[..])
            .map_err(|_| SpoolError::CorruptFrame(idx))?;
        decoder.window_log_max(24)
            .map_err(|_| SpoolError::CorruptFrame(idx))?;
        let mut raw = Vec::with_capacity(entry.r as usize);
        decoder.take(max_bytes as u64 + 1).read_to_end(&mut raw)
            .map_err(|_| SpoolError::CorruptFrame(idx))?;
        if raw.len() != entry.r as usize || raw.len() > max_bytes {
            return Err(SpoolError::CorruptFrame(idx));
        }
        Ok(raw)
    }

    /// 素材目录（缩略图缓存等）。
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// 列出素材根下全部会话 id（按目录名排序）。
pub fn list_session_ids(root: &Path) -> Vec<String> {
    let Some(entries) = fs::read_dir(root.join("sessions")).ok() else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    ids.sort();
    ids
}

/// 生成会话 id：`rec-<unix秒>-<pid>-<序号>`（同秒碰撞由目录存在性拒绝；
/// 无密码学随机需求——id 只需目录名唯一）。
pub fn generate_session_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("rec-{secs:x}-{}-{seq:02x}", std::process::id())
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn meta(w: u32, h: u32) -> SessionMeta {
        SessionMeta {
            spool_version: SPOOL_VERSION,
            id: generate_session_id(),
            region: CaptureRegion { x: 0, y: 0, w, h },
            fps: 10,
            show_cursor: true,
            created_at: 0,
            state: "recording".into(),
            ts_ms: Vec::new(),
            last_dur_ms: 100,
            spool_bytes: 0,
            stop_reason: None,
        }
    }

    fn frame(idx: usize, w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for (i, b) in v.iter_mut().enumerate() {
            *b = (idx as u8).wrapping_add(i as u8);
        }
        v
    }

    #[test]
    fn session_id_cannot_escape_its_directory() {
        assert!(is_valid_session_id("rec-20261031_1"));
        for id in ["", "..", "../escape", "x/y", "x\\y", "x:y", " x", "x."] {
            assert!(!is_valid_session_id(id), "{id}");
        }
        assert!(!is_valid_session_id(&"a".repeat(101)));
    }

    #[test]
    fn frame_budget_checks_both_index_and_actual_expansion() {
        let root = tempfile::tempdir().unwrap();
        let meta = meta(8, 8);
        let id = meta.id.clone();
        let mut writer = SpoolWriter::create(root.path(), meta).unwrap();
        writer.append_frame(0, &frame(0, 8, 8)).unwrap();
        writer.finalize("user").unwrap();
        let mut reader = SpoolReader::open(root.path(), &id).unwrap();
        assert!(matches!(reader.read_frame_bounded(0, 128), Err(SpoolError::CorruptFrame(0))));
        reader.index[0].r = 1; // 索引谎报小值，仍不能绕过解压流的真实输出上限。
        assert!(matches!(reader.read_frame_bounded(0, 128), Err(SpoolError::CorruptFrame(0))));
    }

    #[test]
    fn append_read_roundtrip_and_stats() {
        let root = tempfile::tempdir().unwrap();
        let m = meta(64, 48);
        let id = m.id.clone();
        let mut w = SpoolWriter::create(root.path(), m).unwrap();
        for i in 0..5 {
            w.append_frame(i as u64 * 100, &frame(i, 64, 48)).unwrap();
        }
        assert_eq!(w.frame_count(), 5);
        assert!(w.spool_bytes() > 0);
        let finalized = w.finalize("user").unwrap();
        assert_eq!(finalized.state, "stopped");
        assert_eq!(finalized.ts_ms, vec![0, 100, 200, 300, 400]);

        let r = SpoolReader::open(root.path(), &id).unwrap();
        assert_eq!(r.frame_count(), 5);
        assert_eq!(r.timestamps(), vec![0, 100, 200, 300, 400]);
        for i in 0..5 {
            assert_eq!(r.read_frame(i).unwrap(), frame(i as usize, 64, 48), "帧 {i}");
        }
        assert!(matches!(
            r.read_frame(5),
            Err(SpoolError::FrameOutOfRange(5, 5))
        ));
    }

    #[test]
    fn reopen_recording_session_reads_index_from_jsonl() {
        // 崩溃场景：未 finalize（session.json 仍是 recording），索引从 jsonl 恢复
        let root = tempfile::tempdir().unwrap();
        let m = meta(32, 32);
        let id = m.id.clone();
        let mut w = SpoolWriter::create(root.path(), m).unwrap();
        w.append_frame(0, &frame(0, 32, 32)).unwrap();
        w.append_frame(50, &frame(1, 32, 32)).unwrap();
        drop(w); // 不 finalize，模拟崩溃

        let r = SpoolReader::open(root.path(), &id).unwrap();
        assert_eq!(r.frame_count(), 2);
        // ts 回退到索引值（meta.ts_ms 为空）
        assert_eq!(r.timestamps(), vec![0, 50]);
        assert_eq!(r.read_frame(1).unwrap(), frame(1, 32, 32));
    }

    #[test]
    fn truncated_tail_line_and_partial_block_are_dropped() {
        let root = tempfile::tempdir().unwrap();
        let m = meta(32, 32);
        let id = m.id.clone();
        let dir = session_dir(root.path(), &id);
        {
            let mut w = SpoolWriter::create(root.path(), m).unwrap();
            w.append_frame(0, &frame(0, 32, 32)).unwrap();
            w.append_frame(50, &frame(1, 32, 32)).unwrap();
            w.bin.flush().unwrap();
            w.idx.flush().unwrap();
        }
        // 模拟崩溃残迹 1：索引半行
        {
            let mut f = OpenOptions::new().append(true).open(dir.join("frames.jsonl")).unwrap();
            writeln!(f, r#"{{"i":2,"o":123,"c":45"#).unwrap();
        }
        // 模拟崩溃残迹 2：索引行完整但 spool.bin 尾块不足（o 超出实际长度）
        {
            let spool_len = fs::metadata(dir.join("spool.bin")).unwrap().len();
            let mut f = OpenOptions::new().append(true).open(dir.join("frames.jsonl")).unwrap();
            writeln!(
                f,
                r#"{{"i":2,"o":{},"c":100,"r":4096,"t":99}}"#,
                spool_len
            )
            .unwrap();
        }
        let r = SpoolReader::open(root.path(), &id).unwrap();
        assert_eq!(r.frame_count(), 2, "残行与不完整尾块均被识别丢弃");
        assert_eq!(r.read_frame(1).unwrap(), frame(1, 32, 32));
    }

    #[test]
    fn corrupt_frame_detected_by_length_check() {
        let root = tempfile::tempdir().unwrap();
        let m = meta(32, 32);
        let id = m.id.clone();
        let dir = session_dir(root.path(), &id);
        {
            let mut w = SpoolWriter::create(root.path(), m).unwrap();
            w.append_frame(0, &frame(0, 32, 32)).unwrap();
        }
        // 篡改 spool.bin：同长度垃圾
        let len = fs::metadata(dir.join("spool.bin")).unwrap().len() as usize;
        fs::write(dir.join("spool.bin"), vec![0xABu8; len]).unwrap();
        let r = SpoolReader::open(root.path(), &id).unwrap();
        assert!(matches!(r.read_frame(0), Err(SpoolError::CorruptFrame(0))));
    }

    #[test]
    fn version_gate_rejects_future_spool() {
        let root = tempfile::tempdir().unwrap();
        let mut m = meta(32, 32);
        m.spool_version = SPOOL_VERSION + 1;
        let id = m.id.clone();
        std::fs::create_dir_all(session_dir(root.path(), &id)).unwrap();
        write_session_json(&session_dir(root.path(), &id), &m).unwrap();
        assert!(matches!(
            SpoolReader::open(root.path(), &id),
            Err(SpoolError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn duplicate_session_dir_rejected_and_discard_allows_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let m = meta(32, 32);
        let mut w = SpoolWriter::create(root.path(), m.clone()).unwrap();
        assert!(
            matches!(SpoolWriter::create(root.path(), m.clone()), Err(SpoolError::InvalidSession(_))),
            "同 id 已存在目录时拒绝重建"
        );
        w.discard(); // discard 删目录后可重建
        assert!(SpoolWriter::create(root.path(), m).is_ok());
    }

    #[test]
    fn cleanup_expired_removes_only_stale_sessions() {
        let root = tempfile::tempdir().unwrap();
        // 新会话：不清理
        {
            let mut m = meta(32, 32);
            m.created_at = now_ms();
            let mut w = SpoolWriter::create(root.path(), m).unwrap();
            w.append_frame(0, &frame(0, 32, 32)).unwrap();
            let _ = w.finalize("user").unwrap();
        }
        // 旧会话：直接手搓目录（mtime 设为过去不可移植——改用 session.json
        // created_at + 目录 mtime 组合不可行，Windows set_modified 可用）
        let stale_id = format!("rec-deadbeef-01");
        let dir = session_dir(root.path(), &stale_id);
        fs::create_dir_all(&dir).unwrap();
        let mut old = meta(32, 32);
        old.id = stale_id.clone();
        old.created_at = 1; // epoch
        write_session_json(&dir, &old).unwrap();
        // 把 session.json mtime 拨回过去（>TTL）
        let past = SystemTime::now() - std::time::Duration::from_secs(3600 * 24 * 8);
        let f = File::options().write(true).open(dir.join("session.json")).unwrap();
        f.set_modified(past).unwrap();

        let removed = cleanup_expired(root.path(), std::time::Duration::from_secs(3600 * 24 * 3));
        assert_eq!(removed, 1);
        assert!(!dir.exists(), "过期会话已删除");
        assert_eq!(list_session_ids(root.path()).len(), 1, "新会话保留");
    }

    #[test]
    fn session_ids_are_uniqueish_and_dir_shaped() {
        let a = generate_session_id();
        let b = generate_session_id();
        assert_ne!(a, b);
        assert!(a.starts_with("rec-"));
        assert!(!a.contains(std::path::MAIN_SEPARATOR_STR), "可作目录名");
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
}

/// `SpoolWriter::append_frame` 压缩路径的暴露包装（基准 example 用）。
pub fn compress_frame(bgra: &[u8]) -> std::io::Result<Vec<u8>> {
    zstd::stream::encode_all(bgra, ZSTD_LEVEL)
}
