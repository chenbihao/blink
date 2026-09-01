//! Windows 平台音频采集实现（cpal / WASAPI）。
//!
//! 使用 [cpal](https://github.com/RustAudio/cpal) 库通过 WASAPI 采集麦克风 PCM 数据，
//! 替换原 waveIn (winmm) 手写 FFI 实现。
//!
//! ## 为什么换掉 waveIn
//!
//! waveIn 是 Windows 3.1 时代的遗留 API，在 Windows 10/11 麦克风隐私设置下
//! `waveInOpen` 仍返回成功但采集到的数据**全是零**（静默拦截，无错误码）。
//! cpal 使用 WASAPI（Vista+ 现代 API），正确处理麦克风隐私权限——
//! 权限不足时返回明确错误而非静默给零数据。
//!
//! ## 数据流
//!
//! ```text
//! WASAPI → cpal 回调 → f32 PCM → downmix → 重采样 16kHz → channel → VoiceService
//! ```
//!
//! ## 线程模型
//!
//! cpal 的 `Stream` 在 Windows 上不实现 `Send`（WASAPI 线程亲和性），
//! 因此所有 cpal 对象（`Host`/`Device`/`Stream`）的生命周期局限于采集线程。
//! 跨线程仅传递 `Arc<AtomicBool>`（停止信号）和 `UnboundedSender`（数据管道），均 `Send`。
//!
//! ## 默认设备
//!
//! cpal 通过 `GetDefaultAudioEndpoint(eCapture, eConsole)` 获取系统默认输入设备。
//! 注意：某些虚拟声卡驱动（如 VR/远程桌面）可能劫持此 API，导致返回的默认设备
//! 与 mmsys.cpl 面板显示的不一致。设置页设备列表会标注当前默认设备名称供用户参考。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::{AudioCapture, AudioChunk, AudioError, AudioFormat};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Data, Error as CpalError, InputCallbackInfo, SampleFormat, StreamConfig};

// ── 采集器 ──────────────────────────────────────────────────────────────────

/// cpal 采集器：通过 WASAPI 采集麦克风音频。
///
/// `device_name = None` 表示使用系统默认输入设备。
pub struct CpalCapture {
    capturing: Arc<AtomicBool>,
    device_name: Option<String>,
}

impl CpalCapture {
    pub fn new(device_name: Option<String>) -> Self {
        Self {
            capturing: Arc::new(AtomicBool::new(false)),
            device_name,
        }
    }
}

impl Default for CpalCapture {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Drop for CpalCapture {
    fn drop(&mut self) {
        // 通知采集线程退出。Drop 后 capture 线程会在最多 50ms 内检测到并 drop Stream。
        self.capturing.store(false, Ordering::SeqCst);
    }
}

impl AudioCapture for CpalCapture {
    fn start(
        &mut self,
        format: AudioFormat,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<AudioChunk>, AudioError> {
        if self.capturing.load(Ordering::SeqCst) {
            return Err(AudioError::Io("already capturing".into()));
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AudioChunk>();
        self.capturing.store(true, Ordering::SeqCst);

        let capturing = self.capturing.clone();
        let device_name = self.device_name.clone();
        let target_format = format;

        std::thread::Builder::new()
            .name("blink-audio-capture".into())
            .spawn(move || {
                capture_thread(capturing, tx, device_name, target_format);
            })
            .map_err(|e| AudioError::Io(format!("failed to spawn capture thread: {e}")))?;

        Ok(rx)
    }

    fn stop(&mut self) {
        self.capturing.store(false, Ordering::SeqCst);
    }

    fn is_capturing(&self) -> bool {
        self.capturing.load(Ordering::SeqCst)
    }
}

// ── 采集线程 ────────────────────────────────────────────────────────────────

/// cpal 采集线程。所有 cpal 对象（Device、Stream）的生命周期局限于此线程。
fn capture_thread(
    capturing: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::UnboundedSender<AudioChunk>,
    device_name: Option<String>,
    target_format: AudioFormat,
) {
    let host = cpal::default_host();

    // ── 枚举所有输入设备（诊断用）──
    log_device_list(&host);

    // ── 获取设备 ──
    let device = match find_device(&host, device_name.as_deref()) {
        Some(d) => d,
        None => {
            tracing::error!("cpal: 无可用音频输入设备");
            return;
        }
    };

    let dev_name = device.to_string();

    // ── 获取设备原生配置 ──
    let supported_config = match device.default_input_config() {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(%e, "cpal: 获取设备配置失败");
            return;
        }
    };

    let in_sample_rate = supported_config.sample_rate();
    let in_channels = supported_config.channels();
    let sample_format = supported_config.sample_format();

    tracing::debug!(
        device = %dev_name,
        sample_rate = in_sample_rate,
        channels = in_channels,
        format = ?sample_format,
        target_rate = target_format.sample_rate,
        "cpal: 设备配置"
    );

    let stream_config: StreamConfig = supported_config.into();
    let target_rate = target_format.sample_rate;
    let target_channels = target_format.channels;

    // ── 检查 Windows 麦克风隐私权限 ──
    // 即使 WASAPI 打开设备成功，如果隐私设置禁止了麦克风访问，
    // Windows 会静默返回零数据（不报错）。这是 OS 级行为，不是 cpal 的问题。
    check_microphone_privacy();

    // ── 构建输入流（统一回调，build_input_stream_raw）──
    // 使用 build_input_stream_raw 统一回调类型，所有采样格式在回调内通过
    // convert_to_f32 统一转换，避免 match-on-format 产生的多个重复闭包分支。
    let cb_capturing = capturing.clone();
    let cb_tx = tx.clone();

    let stream = match device.build_input_stream_raw(
        stream_config,
        sample_format,
        move |data: &Data, _: &InputCallbackInfo| {
            if !cb_capturing.load(Ordering::Relaxed) {
                return;
            }
            let f32_data = convert_to_f32(data, sample_format);
            process_and_send(
                &f32_data,
                in_channels,
                in_sample_rate,
                target_channels,
                target_rate,
                target_format,
                &cb_tx,
            );
        },
        on_stream_error,
        None,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(%e, "cpal: 构建输入流失败");
            return;
        }
    };

    if let Err(e) = stream.play() {
        tracing::error!(%e, "cpal: 启动输入流失败");
        return;
    }

    tracing::debug!("cpal: 采集已启动");

    // 保持线程存活——Stream 必须不被 drop 才能持续采集
    while capturing.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }

    // Stream 在此 drop，WASAPI 自动停止采集
    drop(stream);
    tracing::debug!("cpal: 采集已停止");
}

// ── 辅助函数 ────────────────────────────────────────────────────────────────

/// 将 cpal 原始 `Data` 转换为 f32 归一化样本。
///
/// `build_input_stream_raw` 回调接收 `&Data`（动态类型），
/// 此函数根据 `SampleFormat` 将其统一转换为 `Vec<f32>`。
fn convert_to_f32(data: &Data, format: SampleFormat) -> Vec<f32> {
    match format {
        SampleFormat::F32 => data.as_slice::<f32>().unwrap_or_default().to_vec(),
        SampleFormat::I16 => data
            .as_slice::<i16>()
            .unwrap_or_default()
            .iter()
            .map(|&s| s as f32 / 32768.0)
            .collect(),
        SampleFormat::U16 => data
            .as_slice::<u16>()
            .unwrap_or_default()
            .iter()
            .map(|&s| (s as f32 - 32768.0) / 32768.0)
            .collect(),
        SampleFormat::I32 => data
            .as_slice::<i32>()
            .unwrap_or_default()
            .iter()
            .map(|&s| s as f32 / 2_147_483_648.0)
            .collect(),
        SampleFormat::U32 => data
            .as_slice::<u32>()
            .unwrap_or_default()
            .iter()
            .map(|&s| (s as f32 - 2_147_483_648.0) / 2_147_483_648.0)
            .collect(),
        _ => {
            // 未知格式（I8/U8/I64/U64 等），尝试 f32 回退
            tracing::warn!(?format, "cpal: 未知采样格式，尝试以 f32 解析");
            data.as_slice::<f32>().unwrap_or_default().to_vec()
        }
    }
}

/// 枚举所有输入设备并打印诊断日志（含默认设备标记）。
fn log_device_list(host: &cpal::Host) {
    let default_name = host.default_input_device().map(|d| d.to_string());

    if let Ok(devices) = host.input_devices() {
        let mut device_list: Vec<String> = Vec::new();
        for d in devices {
            let name = d.to_string();
            let is_default = default_name.as_deref() == Some(&name);
            device_list.push(if is_default {
                format!("[默认] {}", name)
            } else {
                format!("       {}", name)
            });
        }
        tracing::trace!(
            count = device_list.len(),
            "cpal: 可用输入设备列表:\n{}",
            device_list.join("\n")
        );
    }
}

/// cpal 音频流错误回调。
fn on_stream_error(err: CpalError) {
    tracing::error!(%err, "cpal: 音频流错误");
}

/// 检查 Windows 麦克风隐私设置。
///
/// Windows 10 1703+ 引入了麦克风隐私控制。当应用未获授权时，
/// WASAPI 仍可打开设备并返回「成功」，但采集数据**全是零**。
/// 这是 OS 级静默拦截行为，不是 cpal 或 WASAPI 的 bug。
///
/// 检查注册表：
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone`
/// Value = "Allow" / "Deny"
fn check_microphone_privacy() {
    use winreg::RegKey;
    use winreg::enums::*;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key_path = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";

    match hkcu.open_subkey(key_path) {
        Ok(key) => {
            let value: String = key.get_value("Value").unwrap_or_default();
            tracing::info!(
                mic_privacy = %value,
                "Windows 麦克风隐私设置"
            );
            if value != "Allow" {
                tracing::error!(
                    "⚠️ Windows 麦克风隐私设置为 '{}',不是 'Allow'！\n\
                     这会导致 WASAPI 静默返回零数据（不报错）。\n\
                     修复方法：设置 → 隐私和安全性 → 麦克风 → 允许应用访问麦克风",
                    value
                );
            }
        }
        Err(e) => {
            tracing::warn!(%e, "无法读取麦克风隐私注册表项（可能非 Windows 或权限不足）");
        }
    }

    // 检查桌面应用（NonPackaged）的麦克风权限
    let nonpackaged_path = format!(r"{}\NonPackaged", key_path);
    if let Ok(nonpackaged) = hkcu.open_subkey(&nonpackaged_path) {
        let mut denied_apps = Vec::new();
        for subkey_name in nonpackaged.enum_keys().flatten() {
            if let Ok(subkey) = nonpackaged.open_subkey(&subkey_name) {
                let val: String = subkey.get_value("Value").unwrap_or_default();
                if val != "Allow" {
                    // subkey_name 是应用的 SID，用 chars 截断避免 UTF-8 边界 panic
                    let short: String = subkey_name.chars().take(40).collect();
                    denied_apps.push(format!("{}={}", short, val));
                }
            }
        }
        if !denied_apps.is_empty() {
            tracing::trace!(
                denied = ?denied_apps,
                "部分桌面应用被禁止访问麦克风"
            );
        }
    }
}

/// 查找音频输入设备。按名称查找，找不到则回退到默认设备。
fn find_device(host: &cpal::Host, name: Option<&str>) -> Option<cpal::Device> {
    match name {
        Some(name) => {
            tracing::debug!(%name, "cpal: 按名称查找设备");
            if let Ok(devices) = host.input_devices() {
                for device in devices {
                    let dev_name = device.to_string();
                    if dev_name == name {
                        tracing::debug!(%dev_name, "cpal: 设备名称匹配成功");
                        return Some(device);
                    }
                }
            }
            tracing::warn!(
                requested = %name,
                "cpal: 指定设备未找到，回退到默认设备。\
                 这通常是因为设备名称不完全匹配（检查上方设备列表中的名称）"
            );
            host.default_input_device()
        }
        None => {
            tracing::info!("cpal: 未指定设备名（None），使用系统默认设备");
            host.default_input_device()
        }
    }
}

/// 处理音频数据：downmix → 重采样 → 发送。
fn process_and_send(
    data: &[f32],
    in_channels: u16,
    in_rate: u32,
    out_channels: u16,
    out_rate: u32,
    format: AudioFormat,
    tx: &tokio::sync::mpsc::UnboundedSender<AudioChunk>,
) {
    let mono = downmix(data, in_channels, out_channels);
    let resampled = resample(&mono, in_rate, out_rate);
    let chunk = AudioChunk {
        samples: resampled,
        format,
    };
    let _ = tx.send(chunk);
}

/// 多声道 → 目标声道数（取平均）。
///
/// STT 模型期望单声道。若设备原生多声道（如立体声），按帧取平均降混。
fn downmix(input: &[f32], in_channels: u16, out_channels: u16) -> Vec<f32> {
    if in_channels == out_channels {
        return input.to_vec();
    }
    if out_channels == 1 && in_channels > 1 {
        let ch = in_channels as usize;
        input
            .chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect()
    } else {
        input.to_vec()
    }
}

/// 线性插值重采样。
///
/// 设备原生采样率（如 48kHz）→ STT 目标采样率（如 16kHz）。
/// 线性插值对语音足够，无需引入重采样库依赖。
fn resample(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let output_len = ((input.len() as f64) * ratio) as usize;
    if output_len == 0 {
        return Vec::new();
    }
    let mut output = Vec::with_capacity(output_len);
    for i in 0..output_len {
        let src_pos = i as f64 / ratio;
        let idx0 = src_pos.floor() as usize;
        let idx1 = (idx0 + 1).min(input.len() - 1);
        let frac = (src_pos - idx0 as f64) as f32;
        output.push(input[idx0] * (1.0 - frac) + input[idx1] * frac);
    }
    output
}

// ── 设备枚举 ────────────────────────────────────────────────────────────────

/// 列出可用的音频输入设备。
///
/// 使用 cpal 的 WASAPI 后端枚举设备，返回设备名称作为 ID
/// （cpal 的 `Device` 不暴露稳定数值 ID，名称是唯一标识）。
/// 标注 `is_default` 以便前端在「系统默认」选项旁显示实际设备名。
pub fn list_input_devices() -> Vec<super::AudioDevice> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().map(|d| d.to_string());
    let mut devices = Vec::new();

    if let Ok(input_devices) = host.input_devices() {
        for device in input_devices {
            let name = device.to_string();
            let channels = device
                .default_input_config()
                .map(|c| c.channels())
                .unwrap_or(1);
            let is_default = default_name.as_deref() == Some(&name);
            devices.push(super::AudioDevice {
                id: name.clone(),
                name,
                channels,
                is_default,
            });
        }
    }

    devices
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── 纯逻辑测试 ──

    #[test]
    fn resample_identity() {
        let input = vec![0.5, 0.3, -0.2, 0.8];
        let output = resample(&input, 16000, 16000);
        assert_eq!(output, input);
    }

    #[test]
    fn resample_downsample() {
        // 48000 → 16000, ratio = 1/3
        let input = vec![0.0, 0.1, 0.2, 0.3, 0.4, 0.5];
        let output = resample(&input, 48000, 16000);
        assert_eq!(output.len(), 2); // 6 * (16000/48000) = 2
    }

    #[test]
    fn resample_upsample() {
        // 16000 → 48000, ratio = 3
        let input = vec![0.0, 0.5];
        let output = resample(&input, 16000, 48000);
        assert_eq!(output.len(), 6); // 2 * 3 = 6
    }

    #[test]
    fn resample_preserves_extremes() {
        // 重采样后端点应保持
        let input = vec![1.0, 0.5, -1.0];
        let output = resample(&input, 48000, 16000);
        assert!(!output.is_empty());
        assert!((output[0] - 1.0).abs() < 0.01, "首个样本应接近 1.0");
    }

    #[test]
    fn downmix_stereo_to_mono() {
        let input = vec![0.2, 0.4, 0.6, 0.8]; // 2 frames stereo
        let output = downmix(&input, 2, 1);
        // f32 除法有精度误差，用近似比较
        assert_eq!(output.len(), 2);
        assert!((output[0] - 0.3).abs() < 1e-6, "got {}", output[0]);
        assert!((output[1] - 0.7).abs() < 1e-6, "got {}", output[1]);
    }

    #[test]
    fn downmix_mono_passthrough() {
        let input = vec![0.5, 0.3];
        let output = downmix(&input, 1, 1);
        assert_eq!(output, input);
    }

    #[test]
    fn downmix_empty() {
        let output = downmix(&[], 2, 1);
        assert!(output.is_empty());
    }

    // ── 集成测试（依赖系统音频设备，可跳过）──

    /// 验证 cpal 采集能实际收到音频数据。
    ///
    /// 打开默认输入设备，采集 3 秒，断言至少收到一个非空 chunk。
    ///
    /// **此测试依赖真实音频硬件**——需要可用的麦克风设备且隐私权限已授予。
    /// 在 CI、远程桌面、无麦克风环境会失败，因此标记 `#[ignore]`。
    /// 手动运行：`cargo test --bin blink cpal_capture_returns_chunks -- --ignored --nocapture`
    #[test]
    #[ignore = "依赖真实音频输入设备，需手动运行：--ignored --nocapture"]
    fn cpal_capture_returns_chunks() {
        let mut capture = CpalCapture::new(None);
        let format = AudioFormat::default();
        let mut rx = capture.start(format).expect("capture.start 失败");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv()).await
        });

        capture.stop();

        match result {
            Ok(Some(chunk)) => {
                assert!(!chunk.samples.is_empty(), "音频数据为空");
                let sum_sq: f64 = chunk
                    .samples
                    .iter()
                    .map(|s| (*s as f64) * (*s as f64))
                    .sum();
                let rms = (sum_sq / chunk.samples.len() as f64).sqrt();
                eprintln!(
                    "收到 {} 个样本 ({:.1}ms), RMS = {:.6}, sample_rate = {}",
                    chunk.samples.len(),
                    chunk.samples.len() as f64 / chunk.format.sample_rate as f64 * 1000.0,
                    rms,
                    chunk.format.sample_rate,
                );
            }
            Ok(None) => panic!("channel 在 3 秒内关闭，未收到任何数据"),
            Err(_) => panic!("3 秒内未收到任何音频数据（cpal 采集可能卡死）"),
        }
    }
}
