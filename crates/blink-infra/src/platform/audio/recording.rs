//! 录屏双源 WASAPI worker。cpal 对象只活在此线程；回调不做磁盘 IO。
use crate::gif::audio::*;
use crate::gif::audio_capture::CaptureSignal;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[path = "recording_monitor.rs"]
mod monitor;
pub use monitor::Monitor;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    id: String,
    name: String,
    source: usize,
    is_default: bool,
}
pub fn devices() -> Result<Vec<DeviceInfo>, String> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    for source in 0..2 {
        let default = default_device(&host, source).and_then(|d| d.id().ok());
        let devices = if source == 0 {
            host.input_devices()
        } else {
            host.output_devices()
        }
        .map_err(|_| "无法枚举声音设备")?;
        for d in devices {
            let id = d.id().map_err(|_| "无法读取设备身份")?;
            out.push(DeviceInfo {
                is_default: Some(&id) == default.as_ref(),
                id: id.to_string(),
                name: d.to_string(),
                source,
            });
        }
    }
    Ok(out)
}
fn default_device(host: &cpal::Host, source: usize) -> Option<cpal::Device> {
    if source == 0 {
        host.default_input_device()
    } else {
        host.default_output_device()
    }
}
fn device(
    host: &cpal::Host,
    source: usize,
    selected: Option<&str>,
) -> Result<cpal::Device, String> {
    let d = match selected {
        Some(id) => {
            let id = id.parse().map_err(|_| "设备身份无效，请重新选择")?;
            let d = host
                .device_by_id(&id)
                .ok_or("指定声音设备已不可用，请选择替代设备")?;
            // 不接受把输入/输出身份交叉提交；同一来源才可替换。
            let valid = if source == 0 {
                host.input_devices()
            } else {
                host.output_devices()
            }
            .map_err(|_| "设备枚举失败")?
            .any(|v| v.id().ok().as_ref() == Some(&id));
            if !valid {
                return Err("指定设备不属于此声音来源".into());
            }
            d
        }
        None => default_device(host, source).ok_or("系统没有默认声音设备")?,
    };
    Ok(d)
}
struct Packet {
    source: usize,
    start: u64,
    samples: Vec<i16>,
}

/// 控制句柄可跨线程，Stream 不跨线程。暂停/退出同步 join，释放设备与在途缓冲。
pub struct Session {
    pub shared: SharedAudio,
    dir: Option<PathBuf>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Session {
    pub fn new(shared: SharedAudio, dir: Option<PathBuf>) -> Self {
        Self {
            shared,
            dir,
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }
    pub fn resume(&mut self, offset_ms: u64) -> Result<Instant, String> {
        self.close();
        {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            g.status.error = None;
            g.status.tracks.clear();
            g.offset_ms = offset_ms;
            g.status.rms = [0.0; 2];
            g.status.peak = [0.0; 2];
            g.meter_updated = [None; 2];
            g.preview = self.dir.is_none();
        }
        self.stop = Arc::new(AtomicBool::new(false));
        let shared = self.shared.clone();
        let stop = self.stop.clone();
        let dir = self.dir.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        self.worker = Some(
            std::thread::Builder::new()
                .name("recording-audio".into())
                .spawn(move || {
                    let result = run(shared.clone(), dir, stop, tx.clone());
                    if let Err(error) = result {
                        shared
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .fail(&error);
                        let _ = tx.try_send(Err(error));
                    }
                })
                .map_err(|_| "无法创建声音采集线程")?,
        );
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                let now = Instant::now();
                let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(error) = g.status.error.clone() {
                    drop(g);
                    self.close();
                    return Err(error);
                }
                g.start = Some(now);
                g.status.active = true;
                Ok(now)
            }
            result => {
                self.close();
                Err(match result {
                    Ok(Err(e)) => e,
                    _ => "声音设备就绪超时".into(),
                })
            }
        }
    }
    pub fn pause(&mut self, end_ms: u64) {
        {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            g.status.active = false;
            g.offset_ms = end_ms;
            g.start = None;
            g.status.rms = [0.0; 2];
            g.status.peak = [0.0; 2];
        }
        self.close();
    }
    fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
    pub fn fault(&self) -> Option<String> {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .error
            .clone()
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

fn run(
    shared: SharedAudio,
    dir: Option<PathBuf>,
    stop: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let options = shared
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .options
        .clone();
    let mut manifest = if let Some(dir) = &dir {
        manifest(dir)
            .map_err(|_| "声音元数据损坏")?
            .unwrap_or_else(|| AudioManifest {
                version: 1,
                enabled: [options.enabled(0), options.enabled(1)],
                ..Default::default()
            })
    } else {
        AudioManifest::default()
    };
    if manifest.segments.len() >= 512 {
        return Err("声音设备分段过多，请停止并保留素材".into());
    }
    let _mic = if options.enabled(0) {
        Some(super::MicrophoneLease::acquire().map_err(|e| e.to_string())?)
    } else {
        None
    };
    if options.enabled(0) {
        check_privacy()?;
    }
    // 先保存有声录制意图，部分打开失败后仍可重试，也不会把缺失声音误认成旧无声素材。
    manifest.complete = false;
    if let Some(dir) = &dir {
        write_manifest(dir, &manifest).map_err(|_| "声音检查点写入失败")?;
    }
    let host = cpal::default_host();
    let mut streams = Vec::new();
    let mut tracks = Vec::new();
    let mut files: [Option<File>; 2] = [None, None];
    let mut signals = Vec::new();
    let (tx, rx) = mpsc::sync_channel::<Packet>(16);
    for source in 0..2 {
        if !options.enabled(source) {
            continue;
        }
        let selected = if source == 0 {
            options.microphone.as_deref()
        } else {
            options.system.as_deref()
        };
        let d = device(&host, source, selected)?;
        let config = if source == 0 {
            d.default_input_config()
        } else {
            d.default_output_config()
        }
        .map_err(|_| "声音设备格式不可用")?;
        let rate = config.sample_rate();
        let channels = config.channels();
        let format = config.sample_format();
        if !(8_000..=192_000).contains(&rate)
            || channels == 0
            || channels > 8
            || !super::pcm::supported(format)
        {
            return Err("声音设备采样格式不支持".into());
        }
        tracks.push(TrackInfo {
            source,
            device_id: d.id().map_err(|_| "设备身份读取失败")?.to_string(),
            name: d.to_string(),
            native_rate: rate,
            native_channels: channels,
        });
        if let Some(dir) = &dir {
            files[source] = Some(
                OpenOptions::new()
                    .create(true)
                    .read(true)
                    .write(true)
                    .truncate(false)
                    .open(track_path(dir, source))
                    .map_err(|_| "无法创建声音素材")?,
            );
        }
        let cb = shared.clone();
        let errors = shared.clone();
        let tx = tx.clone();
        let mut diagnosed = false;
        let clock = Arc::new(OnceLock::<(cpal::StreamInstant, Instant)>::new());
        let callback_clock = clock.clone();
        let signal = Arc::new(Mutex::new(CaptureSignal::new(channels, rate)));
        signals.push((source, signal.clone()));
        let stream = d
            .build_input_stream_raw(
                config.into(),
                format,
                move |data, info| {
                    let now = Instant::now();
                    let stamp = info.timestamp();
                    let latency = stamp.callback.duration_since(stamp.capture);
                    let mut g = cb.lock().unwrap_or_else(|e| e.into_inner());
                    if !g.status.active || g.status.error.is_some() {
                        return;
                    }
                    let Some(start) = g.start else {
                        return;
                    };
                    let input = super::pcm::to_f32(data, format);
                    let count = input.len() / channels as usize;
                    if count == 0 || count > rate as usize {
                        g.fail("声音回调超过缓冲预算");
                        return;
                    }
                    // Stream::now 与 capture 属于同一 QPC 时基。只在启动时映射一次，
                    // 不能把每次回调到达时间减去 IAudioClock 的滞后值当成块首。
                    let Some(&(device_anchor, wall_anchor)) = callback_clock.get() else {
                        return;
                    };
                    let first = if stamp.capture < stamp.callback {
                        if stamp.capture >= device_anchor {
                            wall_anchor.checked_add(stamp.capture.duration_since(device_anchor))
                        } else {
                            wall_anchor.checked_sub(device_anchor.duration_since(stamp.capture))
                        }
                        .unwrap_or(now)
                    } else {
                        // 部分 loopback 端点没有有效 capture 时间；连续样本游标仍吸收回调抖动。
                        super::pcm::block_start(now, count, rate, latency)
                    };
                    let relative = if first >= start {
                        first.duration_since(start).as_secs_f64()
                    } else {
                        -start.duration_since(first).as_secs_f64()
                    };
                    let begin = (g.offset_ms as f64 / 1000.0 + relative) * RATE as f64;
                    let mut signal = signal.lock().unwrap_or_else(|e| e.into_inner());
                    match signal.push(&input, begin) {
                        Ok(Some(delta_frames)) => {
                            tracing::debug!(source, delta_frames, "录屏声音设备时间不连续")
                        }
                        Ok(None) => {}
                        Err(error) => {
                            g.fail(error);
                            return;
                        }
                    }
                    let blocks = signal.drain(g.offset_ms * 48, g.frame_end(now), |ms| {
                        g.muted_at(source, ms)
                    });
                    let (rms, peak) = blocks.iter().fold((0.0_f32, 0.0_f32), |(r, p), b| {
                        (r.max(b.level.0), p.max(b.level.1))
                    });
                    if !diagnosed {
                        tracing::debug!(
                            source,
                            input_frames = count,
                            raw_rms = levels(&input).0,
                            latency_ms = latency.as_secs_f64() * 1000.0,
                            begin,
                            upper = g.frame_end(now),
                            saved_samples = blocks.iter().map(|b| b.samples.len()).sum::<usize>(),
                            rms,
                            "录屏音频首块时钟与能量"
                        );
                        diagnosed = true;
                    }
                    g.meter_updated[source] = Some(now);
                    g.status.rms[source] = rms;
                    g.status.peak[source] = g.status.peak[source].max(peak);
                    if !g.preview {
                        for block in blocks {
                            if tx
                                .try_send(Packet {
                                    source,
                                    start: block.start,
                                    samples: block.samples,
                                })
                                .is_err()
                            {
                                g.fail("声音写入跟不上采集，已暂停全部来源");
                                break;
                            }
                        }
                    }
                },
                move |_| {
                    errors
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .fail("声音设备失效，请重试或选择替代设备");
                },
                None,
            )
            .map_err(|error| {
                tracing::error!(source, %error, rate, channels, ?format, "录屏声音设备打开失败");
                format!("无法打开声音设备：{error}")
            })?;
        let before = Instant::now();
        let device_now = stream.now();
        let midpoint = before + before.elapsed() / 2;
        let _ = clock.set((device_now, midpoint));
        stream.play().map_err(|error| {
            tracing::error!(source, %error, "录屏声音设备启动失败");
            format!("无法启动声音设备：{error}")
        })?;
        streams.push(stream);
    }
    let offset = {
        let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
        g.status.tracks = tracks.clone();
        g.offset_ms
    };
    manifest.complete = false;
    manifest.segments.push((offset, tracks.clone()));
    if let Some(dir) = &dir {
        write_manifest(dir, &manifest).map_err(|_| "声音检查点写入失败")?;
    }
    ready.send(Ok(())).map_err(|_| "录制已取消")?;
    let mut last_check = Instant::now();
    let mut last_commit = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(p) => {
                if let Some(file) = files[p.source].as_mut() {
                    if write_packet(file, p.start, &p.samples).is_err() {
                        shared
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .fail("声音素材写入失败，已暂停全部来源");
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
        if last_check.elapsed() > Duration::from_millis(500) {
            last_check = Instant::now();
            for t in &tracks {
                let selected = if t.source == 0 {
                    &options.microphone
                } else {
                    &options.system
                };
                if selected.is_none()
                    && default_device(&host, t.source)
                        .and_then(|d| d.id().ok())
                        .map(|id| id.to_string())
                        .as_deref()
                        != Some(&t.device_id)
                {
                    shared
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .fail("系统默认声音设备已变化，请核验设备后手动继续");
                }
            }
        }
        if last_commit.elapsed() > Duration::from_millis(250) {
            last_commit = Instant::now();
            if let Some(dir) = &dir {
                // 留 100ms 在途余量。先发布音轨完整前缀，再原子发布共同检查点。
                // 系统声音的正常无包区间为空洞静音；设备错误另记异常状态。
                let end = {
                    let g = shared.lock().unwrap_or_else(|e| e.into_inner());
                    if !g.status.active || g.status.error.is_some() {
                        continue;
                    }
                    g.time_ms(Instant::now())
                        .saturating_sub(100)
                        .min(MAX_AUDIO_MS)
                };
                for file in files.iter_mut().flatten() {
                    if file.metadata().map_err(|_| "声音素材状态读取失败")?.len()
                        < end * BYTES_PER_MS
                    {
                        file.set_len(end * BYTES_PER_MS)
                            .map_err(|_| "声音检查点扩展失败")?;
                    }
                    file.sync_data().map_err(|_| "声音检查点提交失败")?;
                }
                manifest.committed_ms = end;
                write_manifest(dir, &manifest).map_err(|_| "声音检查点写入失败")?;
            }
        }
    }
    // 先销毁所有流，再排空已门控样本；不遗留另一路独自采集。
    drop(streams);
    drop(tx);
    for p in rx.try_iter() {
        if let Some(file) = files[p.source].as_mut() {
            write_packet(file, p.start, &p.samples).map_err(|_| "声音尾块写入失败")?;
        }
    }
    if let Some(dir) = &dir {
        let g = shared.lock().unwrap_or_else(|e| e.into_inner());
        let end = g.offset_ms.min(MAX_AUDIO_MS);
        for (source, signal) in signals {
            let mut signal = signal.lock().unwrap_or_else(|e| e.into_inner());
            signal.finish();
            for block in signal.drain(0, end * 48, |ms| g.muted_at(source, ms)) {
                if let Some(file) = files[source].as_mut() {
                    write_packet(file, block.start, &block.samples)
                        .map_err(|_| "声音尾块写入失败")?;
                }
            }
        }
        for file in files.iter_mut().flatten() {
            file.set_len(end * BYTES_PER_MS)
                .map_err(|_| "声音尾部提交失败")?;
            file.sync_data().map_err(|_| "声音素材提交失败")?;
        }
        manifest.committed_ms = end;
        manifest.muted = Default::default();
        for source in 0..2 {
            let mut muted_start = None;
            for &(at, muted) in &g.mute_edges[source] {
                if muted {
                    muted_start = Some(at);
                } else if let Some(start) = muted_start.take() {
                    manifest.muted[source].push((start, at));
                }
            }
            if let Some(start) = muted_start {
                manifest.muted[source].push((start, end));
            }
        }
        manifest.complete = g.status.error.is_none();
        write_manifest(dir, &manifest).map_err(|_| "声音检查点提交失败")?;
    }
    Ok(())
}
fn check_privacy() -> Result<(), String> {
    use winreg::{
        RegKey,
        enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE},
    };
    for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        if let Ok(key) = RegKey::predef(hive).open_subkey(r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone") {
            if key.get_value::<String, _>("Value").ok().as_deref() == Some("Deny") { return Err("Windows 已禁止麦克风访问，请在隐私设置中允许桌面应用访问".into()); }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "recording_tests.rs"]
mod tests;
