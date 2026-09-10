//! FunASR GGUF worker 传输适配（0.22.7）。
//!
//! 职责：
//! - 管理 Blink 受管的临时音频目录（`runtimes/engines/{engine}/audio-tmp/`）；
//! - 把 domain 层 `SttTransport`（WAV 字节 → 文本）适配到 infra 层
//!   `NdjsonWorkerClient`（NDJSON stdin/stdout 协议）；
//! - 请求前对音频路径做 canonicalize + 前缀校验（worker 侧另有同样校验）；
//! - 收到文本后执行 `gguf_postprocess`（emoji/事件描述/CJK 空格清理，
//!   语义继承自被删除的 Python `_postprocess_text`）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::domain::stt::gguf_postprocess::gguf_postprocess;
use crate::domain::stt::{SttTransport, SttTransportError};
use crate::infra::local_engine::worker_proto::{
    NdjsonWorkerClient, TranscribeOptions, WorkerProtoError,
};

/// 单次转录等待 worker 响应的上限。
///
/// SenseVoice/Paraformer 数百 ms；Nano 自回归长音频可达数秒——120s 覆盖
/// 最慢模型的极端情况，超时由错误明确回报（不伪装成功）。
const TRANSCRIBE_TIMEOUT_SECS: u64 = 120;
/// 就绪/健康检查超时。
const HELLO_TIMEOUT_SECS: u64 = 10;
/// 同一音频目录中遗留 wav 的清理上限（防无限膨胀；正常路径请求后即删）。
const MAX_STALE_WAV_FILES: usize = 64;

// ── 音频目录 ─────────────────────────────────────────────────────────────

/// 引擎的受管音频目录：`engine_root/audio-tmp/`。
pub fn engine_audio_tmp_dir(engine_id: &crate::infra::local_engine::runtime::EngineId) -> PathBuf {
    crate::infra::local_engine::runtime::engine_root(engine_id).join("audio-tmp")
}

/// 清空引擎音频目录（start 前 / stop 后调用；目录不存在时为 no-op）。
pub async fn clean_audio_tmp_dir(engine_id: &crate::infra::local_engine::runtime::EngineId) {
    let dir = engine_audio_tmp_dir(engine_id);
    let result = tokio::task::spawn_blocking(move || match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!(error_kind = ?e.kind(), "audio-tmp 清理失败（继续）"),
        Err(e) => tracing::debug!(%e, "audio-tmp 清理任务失败（继续）"),
    }
}

/// 把 WAV 字节写入受管音频目录并返回 canonicalize 后的绝对路径。
///
/// 文件名带纳秒时间戳 + 计数，避免并发覆盖。
fn write_wav_to_audio_dir(dir: &Path, wav_bytes: &[u8]) -> Result<PathBuf, String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("stt-{now}-{seq}.wav"));
    std::fs::create_dir_all(dir).map_err(|e| format!("创建音频目录失败: {e}"))?;
    if let Err(e) = std::fs::write(&path, wav_bytes) {
        // write 失败也可能留下部分文件；此处已在 blocking worker 中，
        // 同步兜底删除不会阻塞 async executor。
        let _ = std::fs::remove_file(&path);
        return Err(format!("写入临时音频失败: {e}"));
    }
    Ok(path)
}

/// 校验路径 canonicalize 后位于受管音频目录内（协议硬约束）。
fn ensure_within_audio_dir(audio_dir: &Path, path: &Path) -> Result<PathBuf, String> {
    let canonical_audio = audio_dir
        .canonicalize()
        .map_err(|e| format!("音频目录不可用: {e}"))?;
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("音频路径不可用: {e}"))?;
    if !canonical.starts_with(&canonical_audio) {
        return Err("音频路径越界（不在受管目录内）".to_string());
    }
    Ok(canonical)
}

/// 清扫遗留 wav：正常路径请求完成即删；此处兜底防止异常残留无限膨胀。
fn sweep_stale_wavs(audio_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(audio_dir) else {
        return;
    };
    let mut wavs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "wav"))
        .collect();
    if wavs.len() <= MAX_STALE_WAV_FILES {
        return;
    }
    wavs.sort(); // 文件名含时间戳，字典序即时间序
    let excess = wavs.len() - MAX_STALE_WAV_FILES;
    for path in wavs.into_iter().take(excess) {
        let _ = std::fs::remove_file(&path);
    }
}

// ── SttTransport 实现 ────────────────────────────────────────────────────

/// RAII 守卫：在 drop 时删除临时音频文件。
///
/// 覆盖 success / error / timeout / cancel / panic 所有路径——
/// async future 被取消时，await 点 panic 或 future 被 drop，
/// guard 的 drop 仍会执行（Rust 语义保证）。
struct AudioFileGuard {
    path: Option<std::path::PathBuf>,
}

/// blocking 准备任务的完整产物。
///
/// guard 与路径一起跨越 JoinHandle 边界：若等待方在准备期间被取消，
/// detached blocking task 完成后其无人接收的输出会被 drop，临时文件仍被清理。
struct PreparedAudioFile {
    canonical: PathBuf,
    cleanup: AudioFileGuard,
}

fn prepare_audio_file(audio_dir: &Path, owned_wav: Vec<u8>) -> Result<PreparedAudioFile, String> {
    let raw_path = write_wav_to_audio_dir(audio_dir, &owned_wav)?;
    let cleanup = AudioFileGuard::new(&raw_path);
    let canonical = ensure_within_audio_dir(audio_dir, &raw_path)?;
    Ok(PreparedAudioFile { canonical, cleanup })
}

impl AudioFileGuard {
    fn new(path: &std::path::Path) -> Self {
        Self {
            path: Some(path.to_path_buf()),
        }
    }

    async fn cleanup(mut self, audio_dir: PathBuf) {
        let Some(path) = self.path.take() else {
            return;
        };
        let result = tokio::task::spawn_blocking(move || {
            if let Err(e) = std::fs::remove_file(path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::debug!(error_kind = ?e.kind(), "临时音频文件清理失败");
            }
            sweep_stale_wavs(&audio_dir);
        })
        .await;
        if let Err(e) = result {
            tracing::debug!(%e, "临时音频清理任务失败");
        }
    }
}

impl Drop for AudioFileGuard {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        // future 在任意 await 点被取消时仍会 Drop。把同步 remove 移到
        // blocking pool；若已离开 runtime（测试/进程收尾），退化为专用线程。
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                let _ = std::fs::remove_file(path);
            });
        } else {
            let _ = std::thread::Builder::new()
                .name("blink-audio-cleanup".into())
                .spawn(move || {
                    let _ = std::fs::remove_file(path);
                });
        }
    }
}

/// GGUF worker 的 `SttTransport` 实现（app 层注入 domain）。
pub struct GgufSttTransport {
    client: Arc<NdjsonWorkerClient>,
    audio_dir: PathBuf,
    /// 受限识别选项（从 SttConfig 投影；language 沿配置 → worker 传播）。
    options: TranscribeOptions,
}

impl GgufSttTransport {
    #[allow(dead_code)]
    pub fn new(client: Arc<NdjsonWorkerClient>, audio_dir: PathBuf) -> Self {
        Self {
            client,
            audio_dir,
            options: TranscribeOptions::default(),
        }
    }

    /// 带 `TranscribeOptions` 构造（Handoff 02 §4：参数传播证据链）。
    ///
    /// `language` 从 `SttConfig` → `FunasrEngineConfig` → 此处 →
    /// `TranscribeOptions` → worker NDJSON 协议，形成完整传播链。
    pub fn with_options(
        client: Arc<NdjsonWorkerClient>,
        audio_dir: PathBuf,
        options: TranscribeOptions,
    ) -> Self {
        Self {
            client,
            audio_dir,
            options,
        }
    }
}

fn map_proto_error(e: WorkerProtoError) -> SttTransportError {
    match e {
        WorkerProtoError::Timeout { timeout_ms } => SttTransportError::Timeout {
            detail: format!("worker response exceeded {timeout_ms}ms"),
        },
        WorkerProtoError::Worker(error) => match error.code.as_str() {
            "busy" | "queue_full" | "backlog" => SttTransportError::Busy { detail: error.code },
            "cancelled" | "canceled" => SttTransportError::Cancelled,
            _ => SttTransportError::Unavailable {
                detail: format!("worker error code={}", error.code),
            },
        },
        WorkerProtoError::Disconnected
        | WorkerProtoError::Protocol(_)
        | WorkerProtoError::Write(_) => SttTransportError::Unavailable {
            detail: e.to_string(),
        },
    }
}

#[async_trait::async_trait]
impl SttTransport for GgufSttTransport {
    async fn check_ready(&self) -> Result<(), SttTransportError> {
        self.client
            .hello(std::time::Duration::from_secs(HELLO_TIMEOUT_SECS))
            .await
            .map(|_| ())
            .map_err(map_proto_error)
    }

    async fn transcribe(&self, wav_bytes: &[u8]) -> Result<String, SttTransportError> {
        self.transcribe_with_metrics(wav_bytes)
            .await
            .map(|result| result.text)
    }

    async fn transcribe_with_metrics(
        &self,
        wav_bytes: &[u8],
    ) -> Result<crate::domain::stt::SttTransportResult, SttTransportError> {
        // spawn_blocking 要求 'static：在边界只做一次显式 owned copy，绝不
        // 跨线程借用调用方 WAV slice。
        let owned_wav = wav_bytes.to_vec();
        let audio_dir = self.audio_dir.clone();
        let prepared =
            tokio::task::spawn_blocking(move || prepare_audio_file(&audio_dir, owned_wav))
                .await
                .map_err(|e| SttTransportError::Internal {
                    detail: format!("audio file preparation task failed: {e}"),
                })?
                .map_err(|detail| SttTransportError::Internal { detail })?;

        let PreparedAudioFile { canonical, cleanup } = prepared;

        let result = self
            .client
            .transcribe(
                &canonical,
                &self.options,
                std::time::Duration::from_secs(TRANSCRIBE_TIMEOUT_SECS),
            )
            .await;

        // 正常路径等待 blocking cleanup；取消路径由 guard Drop 调度清理。
        cleanup.cleanup(self.audio_dir.clone()).await;

        let output = result.map_err(map_proto_error)?;
        if let Some(ms) = output.elapsed_ms {
            tracing::debug!(elapsed_ms = ms, "GGUF worker 转录完成");
        }
        Ok(crate::domain::stt::SttTransportResult {
            text: gguf_postprocess(&output.text),
            inference_ms: output.elapsed_ms,
        })
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_until_removed(path: &Path) {
        for _ in 0..100 {
            let candidate = path.to_path_buf();
            let exists = tokio::task::spawn_blocking(move || candidate.exists())
                .await
                .unwrap();
            if !exists {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("临时文件未在期限内清理");
    }

    async fn wait_until_dir_empty(path: &Path) {
        for _ in 0..100 {
            let candidate = path.to_path_buf();
            let is_empty = tokio::task::spawn_blocking(move || {
                std::fs::read_dir(candidate)
                    .map(|mut entries| entries.next().is_none())
                    .unwrap_or(true)
            })
            .await
            .unwrap();
            if is_empty {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("临时目录未在期限内清空");
    }

    #[test]
    fn wav_write_and_boundary_check() {
        let tmp = tempfile::tempdir().unwrap();
        let wav = crate::domain::stt::wav::pcm_to_wav(&[0.0f32; 1600], 16000, 1);
        let path = write_wav_to_audio_dir(tmp.path(), &wav).unwrap();
        assert!(path.exists());
        assert!(ensure_within_audio_dir(tmp.path(), &path).is_ok());

        // 越界路径必须被拒绝
        let outside = std::env::temp_dir().join("blink-outside-test.wav");
        std::fs::write(&outside, b"x").unwrap();
        assert!(ensure_within_audio_dir(tmp.path(), &outside).is_err());
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn stale_wav_sweep_keeps_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..(MAX_STALE_WAV_FILES + 10) {
            let name = format!("stt-00000000000{i:04}.wav");
            std::fs::write(tmp.path().join(name), b"x").unwrap();
        }
        sweep_stale_wavs(tmp.path());
        let count = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "wav"))
            .count();
        assert_eq!(count, MAX_STALE_WAV_FILES, "清扫后应保持有界");
    }

    // ── AudioFileGuard 测试：覆盖 success / error / cancel 路径清理 ──

    #[tokio::test]
    async fn audio_file_guard_cleans_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test-guard.wav");
        std::fs::write(&path, b"x").unwrap();
        assert!(path.exists());
        {
            let _guard = AudioFileGuard::new(&path);
        }
        wait_until_removed(&path).await;
    }

    #[tokio::test]
    async fn audio_file_guard_cleans_on_error_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test-err.wav");
        std::fs::write(&path, b"x").unwrap();
        assert!(path.exists());

        // 模拟错误路径：guard 在 ? 返回错误前创建，drop 仍执行
        let result: Result<(), String> = {
            let _guard = AudioFileGuard::new(&path);
            Err("simulated error".to_string())
        };
        assert!(result.is_err());
        wait_until_removed(&path).await;
    }

    #[tokio::test]
    async fn audio_file_guard_cleans_on_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test-panic.wav");
        std::fs::write(&path, b"x").unwrap();
        assert!(path.exists());

        let result = std::panic::catch_unwind(|| {
            let _guard = AudioFileGuard::new(&path);
            panic!("simulated panic");
        });
        assert!(result.is_err());
        wait_until_removed(&path).await;
    }

    #[tokio::test]
    async fn cancelled_prepare_wait_still_cleans_completed_file() {
        let tmp = tempfile::tempdir().unwrap();
        let audio_dir = tmp.path().to_path_buf();
        let wav = crate::domain::stt::wav::pcm_to_wav(&[0.0f32; 1600], 16000, 1);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        let waiter = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                release_rx.recv().unwrap();
                let prepared = prepare_audio_file(&audio_dir, wav);
                let _ = done_tx.send(());
                prepared
            })
            .await
        });

        started_rx.await.unwrap();
        waiter.abort();
        release_tx.send(()).unwrap();
        done_rx.await.unwrap();
        wait_until_dir_empty(tmp.path()).await;
    }

    #[test]
    fn audio_file_guard_noop_if_file_already_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.wav");
        // 文件不存在时 guard drop 不应 panic
        let _guard = AudioFileGuard::new(&path);
        drop(_guard);
    }
}
