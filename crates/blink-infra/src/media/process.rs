//! 隐藏子进程 + Job Object；持续排空输出，取消/超时后先停止输入再回收进程。
use super::{MediaError, Progress, ProgressCallback};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

pub(super) type FrameQueue = mpsc::Receiver<Arc<Vec<u8>>>;
pub(super) struct RunOptions<'a> {
    pub cancel: &'a CancellationToken,
    pub deadline: Instant,
    pub input: Option<FrameQueue>,
    pub progress: Option<(ProgressCallback, &'static str, u32)>,
    pub output_budget: Option<(&'a Path, u64)>,
    pub peak_bytes: &'a AtomicU64,
}

async fn stdout<R: AsyncRead + Unpin>(
    mut reader: R,
    progress: Option<(ProgressCallback, &'static str, u32)>,
) -> Result<Vec<u8>, MediaError> {
    let mut bytes = [0u8; 4096];
    let mut out = Vec::new();
    let mut line = Vec::new();
    loop {
        let len = reader.read(&mut bytes).await?;
        if len == 0 {
            break;
        }
        if let Some((callback, stage, total)) = &progress {
            for byte in &bytes[..len] {
                if *byte == b'\n' {
                    if let Ok(text) = std::str::from_utf8(&line) {
                        if let Some(frame) = text
                            .trim()
                            .strip_prefix("frame=")
                            .and_then(|v| v.trim().parse::<u32>().ok())
                        {
                            callback(Progress {
                                stage,
                                frames_done: frame,
                                frames_total: *total,
                            });
                        }
                    }
                    line.clear();
                } else {
                    if line.len() >= 1024 {
                        return Err(MediaError::Process("进度行超限"));
                    }
                    line.push(*byte);
                }
            }
        } else {
            if out.len() + len > 64 * 1024 {
                return Err(MediaError::Process("元数据输出超限"));
            }
            out.extend_from_slice(&bytes[..len]);
        }
    }
    Ok(out)
}

async fn drain<R: AsyncRead + Unpin>(mut reader: R) -> Result<u64, MediaError> {
    // 不保存 stderr 原文（可能含用户路径），持续读取以防管道阻塞。
    let mut buf = [0u8; 4096];
    let mut count = 0;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(count);
        }
        count += n as u64;
    }
}

pub(super) async fn write_input<W: tokio::io::AsyncWrite + Unpin>(
    mut stdin: W,
    mut input: FrameQueue,
) -> Result<(), MediaError> {
    while let Some(frame) = input.recv().await {
        stdin.write_all(&frame).await?;
    }
    stdin.shutdown().await?;
    Ok(())
}

pub(super) async fn run(
    exe: &Path,
    args: &[std::ffi::OsString],
    options: RunOptions<'_>,
) -> Result<Vec<u8>, MediaError> {
    if options.cancel.is_cancelled() {
        return Err(MediaError::Cancelled);
    }
    if Instant::now() >= options.deadline {
        return Err(MediaError::Timeout);
    }
    let mut command = crate::platform::background_media_command(tokio::process::Command::new(exe));
    command
        .args(args)
        .stdin(if options.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| MediaError::Process("无法启动托管程序"))?;
    let pid = child.id().ok_or(MediaError::Process("子进程身份不可用"))?;
    #[cfg(windows)]
    let _job = match crate::platform::process::assign_job_object(pid) {
        Ok(job) => Some(job),
        Err(_) => {
            if child.try_wait()?.is_none() {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                return Err(MediaError::Process("无法监管子进程树"));
            }
            None // 极短命令已结束，仍需读取其输出。
        }
    };
    let out = child
        .stdout
        .take()
        .ok_or(MediaError::Process("stdout 不可用"))?;
    let err = child
        .stderr
        .take()
        .ok_or(MediaError::Process("stderr 不可用"))?;
    let stdin = child.stdin.take();
    let input = async move {
        if let (Some(input), Some(stdin)) = (options.input, stdin) {
            write_input(stdin, input).await?;
        }
        Ok::<_, MediaError>(())
    };
    let monitor = async {
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        loop {
            ticker.tick().await;
            if let Some(peak) = crate::platform::process::peak_working_set_bytes(pid) {
                options.peak_bytes.fetch_max(peak, Ordering::Relaxed);
            }
            if let Some((path, limit)) = options.output_budget {
                if tokio::fs::metadata(path)
                    .await
                    .map(|m| m.len() > limit)
                    .unwrap_or(false)
                {
                    return MediaError::Budget("输出文件最多 256 MB");
                }
            }
        }
    };
    let result = tokio::select! {
        biased;
        _ = options.cancel.cancelled() => Err(MediaError::Cancelled),
        _ = tokio::time::sleep_until(options.deadline) => Err(MediaError::Timeout),
        error = monitor => Err(error),
        result = async {
            let wait = async { child.wait().await.map_err(MediaError::Io) };
            let (exit, out, err_bytes, _) = tokio::try_join!(wait, stdout(out, options.progress), drain(err), input)?;
            tracing::debug!(pid, exit = ?exit.code(), stderr_bytes = err_bytes, "媒体子进程结束");
            if !exit.success() { return Err(MediaError::Process("编码或解码失败，请检查素材并尝试缩小尺寸")); }
            Ok(out)
        } => result,
    };
    if result.is_err() {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
    result
}

pub(super) fn args(values: &[&str]) -> Vec<std::ffi::OsString> {
    values.iter().map(std::ffi::OsString::from).collect()
}

/// 同目录唯一临时成品；失败或 future 被丢弃时自动清理。
pub(super) struct TempOutput(pub PathBuf);
impl TempOutput {
    pub fn create(target: &Path) -> Result<Self, MediaError> {
        let parent = target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or(MediaError::InvalidPlan("请选择绝对输出路径"))?;
        if !target.is_absolute() {
            return Err(MediaError::InvalidPlan("请选择绝对输出路径"));
        }
        let path = parent.join(format!(
            ".blink-media-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        Ok(Self(path))
    }
}
impl Drop for TempOutput {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn progress_is_structured_and_output_buffers_are_bounded() {
        let values = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = values.clone();
        let callback: ProgressCallback = Arc::new(move |p| {
            sink.lock()
                .unwrap()
                .push((p.stage, p.frames_done, p.frames_total))
        });
        let output = stdout(
            &b"frame=2\nunknown=field\nframe=18\nprogress=end\n"[..],
            Some((callback, "encoding", 18)),
        )
        .await
        .unwrap();
        assert!(output.is_empty());
        assert_eq!(
            *values.lock().unwrap(),
            vec![("encoding", 2, 18), ("encoding", 18, 18)]
        );
        let long = vec![b'x'; 1025];
        assert!(
            stdout(&long[..], Some((Arc::new(|_| {}), "encoding", 1)))
                .await
                .is_err()
        );
        let large = vec![b'x'; 65537];
        assert!(stdout(&large[..], None).await.is_err());
        assert_eq!(stdout(&large[..65536], None).await.unwrap().len(), 65536);
        assert_eq!(drain(&large[..]).await.unwrap(), 65537);
    }
}
