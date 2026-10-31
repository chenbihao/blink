//! 安装自检：实际编码、结构检查与全段解码。所有子进程有取消和总期限。
use super::*;
use std::{ffi::OsStr, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;

const OUTPUT_LIMIT: u64 = 64 * 1024;

async fn run<I, S>(exe: &Path, args: I, cancel: &CancellationToken) -> Result<Vec<u8>, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = crate::platform::no_window_tokio(tokio::process::Command::new(exe));
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| Error::SelfTestFailed("无法启动 FFmpeg 自检程序".into()))?;
    #[cfg(windows)]
    let _job = match child.id().and_then(|pid| crate::platform::process::assign_job_object(pid).ok()) {
        Some(job) => Some(job),
        None if child.try_wait()?.is_some() => None,
        None => {
            let _ = child.kill().await;
            return Err(Error::SelfTestFailed("无法监管 FFmpeg 自检进程树".into()));
        }
    };
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::SelfTestFailed("自检 stdout 不可用".into()))?
        .take(OUTPUT_LIMIT + 1);
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::SelfTestFailed("自检 stderr 不可用".into()))?
        .take(OUTPUT_LIMIT + 1);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(Error::Cancelled),
        _ = tokio::time::sleep(Duration::from_secs(20)) => Err(Error::SelfTestFailed("FFmpeg 自检超时".into())),
        result = async {
            let (exit, _, _) = tokio::try_join!(child.wait(), stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))?;
            if !exit.success() || out.len() as u64 > OUTPUT_LIMIT || err.len() as u64 > OUTPUT_LIMIT {
                return Err(Error::SelfTestFailed("FFmpeg 自检失败（程序退出或输出超限）".into()));
            }
            Ok(())
        } => result,
    };
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result?;
    Ok(out)
}

pub(super) async fn verify(
    ffmpeg: &Path,
    ffprobe: &Path,
    scratch: &Path,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    let lock = lock()?;
    let out = run(ffmpeg, ["-version"], cancel).await?;
    if !String::from_utf8_lossy(&out).starts_with(&lock.runtime.version_check) {
        return Err(Error::SelfTestFailed("FFmpeg 构建版本不符".into()));
    }
    let out = run(ffprobe, ["-version"], cancel).await?;
    if !String::from_utf8_lossy(&out)
        .starts_with(&lock.runtime.version_check.replacen("ffmpeg", "ffprobe", 1))
    {
        return Err(Error::SelfTestFailed("ffprobe 构建版本不符".into()));
    }
    let sample = scratch.join("self-test.mp4");
    let mut args: Vec<&OsStr> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "color=c=red:s=64x64:r=10:d=0.3",
        "-frames:v",
        "3",
        "-an",
        "-c:v",
        "libx264",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "mp4",
    ]
    .iter()
    .map(OsStr::new)
    .collect();
    args.push(sample.as_os_str());
    run(ffmpeg, args, cancel).await?;
    let args = [
        OsStr::new("-v"),
        OsStr::new("error"),
        OsStr::new("-show_streams"),
        OsStr::new("-of"),
        OsStr::new("json"),
        sample.as_os_str(),
    ];
    verify_metadata(&run(ffprobe, args, cancel).await?)?;
    let mut command: Vec<&OsStr> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-xerror",
        "-threads",
        "1",
        "-i",
    ]
    .iter()
    .map(OsStr::new)
    .collect();
    command.push(sample.as_os_str());
    command.extend(["-f", "null", "-"].iter().map(OsStr::new));
    run(ffmpeg, command, cancel).await?;
    tokio::fs::remove_file(sample).await?;
    Ok(())
}

pub(super) fn verify_metadata(bytes: &[u8]) -> Result<(), Error> {
    let metadata: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| Error::SelfTestFailed("ffprobe 输出无效".into()))?;
    let video = metadata["streams"]
        .as_array()
        .filter(|s| s.len() == 1)
        .and_then(|s| s.first());
    let valid = video.is_some_and(|v| {
        v["codec_name"] == "h264"
            && v["width"] == 64
            && v["height"] == 64
            && v["pix_fmt"] == "yuv420p"
            && v["nb_frames"] == "3"
            && v["duration"]
                .as_str()
                .and_then(|n| n.parse::<f64>().ok())
                .is_some_and(|d| (d - 0.3).abs() < 0.02)
    });
    if valid {
        Ok(())
    } else {
        Err(Error::SelfTestFailed(
            "MP4 自检产物的编码、尺寸或时长不符".into(),
        ))
    }
}
