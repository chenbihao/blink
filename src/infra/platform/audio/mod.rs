//! 音频采集平台抽象层。
//!
//! 0.10 语音输入:hold-to-talk 期间采集麦克风音频,送给 STT engine 识别。
//!
//! ## 设计
//!
//! - **trait `AudioCapture`**:平台无关的音频采集接口
//! - **`AudioChunk`**:一段 PCM 音频数据(f32 归一化,16kHz 单声道)
//! - Windows 实现:`windows.rs`(cpal / WASAPI)
//!
//! ## 线程安全
//!
//! 采集在独立 tokio task 中运行,通过 channel 把 chunk 送给消费者(STT engine)。
//! hook 线程绝不参与采集(保证 P0 热键不阻塞)。

use std::fmt;

/// 音频格式参数。STT engine 期望的输入格式。
#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    /// 采样率(Hz)。STT 模型通常 16kHz。
    pub sample_rate: u32,
    /// 声道数。STT 通常单声道。
    pub channels: u16,
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            channels: 1,
        }
    }
}

/// 一段 PCM 音频数据。f32 归一化 [-1.0, 1.0]。
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// PCM 样本(f32,交错声道若 channels > 1)
    pub samples: Vec<f32>,
    /// 这段数据的格式
    #[allow(dead_code)]
    pub format: AudioFormat,
}

#[allow(dead_code)]
impl AudioChunk {
    /// 创建空 chunk。
    pub fn empty(format: AudioFormat) -> Self {
        Self {
            samples: Vec::new(),
            format,
        }
    }

    /// 样本数。
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 时长(秒)。
    pub fn duration_secs(&self) -> f64 {
        if self.format.channels == 0 {
            return 0.0;
        }
        self.samples.len() as f64 / (self.format.sample_rate as f64 * self.format.channels as f64)
    }
}

/// 音频采集错误。
#[derive(Debug)]
#[allow(dead_code)]
pub enum AudioError {
    /// 设备不可用
    DeviceUnavailable(String),
    /// 格式不支持
    UnsupportedFormat(String),
    /// 采集过程中 IO 错误
    Io(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::DeviceUnavailable(msg) => write!(f, "audio device unavailable: {msg}"),
            AudioError::UnsupportedFormat(msg) => write!(f, "unsupported audio format: {msg}"),
            AudioError::Io(msg) => write!(f, "audio IO error: {msg}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// 音频输入设备描述。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AudioDevice {
    /// 设备 ID(cpal 设备名称，用作唯一标识)
    pub id: String,
    /// 设备名称
    pub name: String,
    /// 通道数
    pub channels: u16,
    /// 是否为系统默认输入设备（cpal/WASAPI 认定的默认设备）
    pub is_default: bool,
}

/// 音频采集 trait。
///
/// 实现者负责打开麦克风设备、按指定格式采集 PCM 数据。
/// 采集到的 chunk 通过 channel 传递给消费者。
pub trait AudioCapture: Send {
    /// 开始采集。返回一个接收音频 chunk 的 channel receiver。
    /// 采集在内部线程/task 中运行,直到 `stop` 被调用。
    fn start(&mut self, format: AudioFormat) -> Result<tokio::sync::mpsc::UnboundedReceiver<AudioChunk>, AudioError>;

    /// 停止采集。内部线程退出,channel 关闭。
    fn stop(&mut self);

    /// 是否正在采集。
    #[allow(dead_code)]
    fn is_capturing(&self) -> bool;
}

/// 列出可用的音频输入设备。
#[cfg(target_os = "windows")]
pub fn list_input_devices() -> Vec<AudioDevice> {
    windows::list_input_devices()
}

/// 创建音频采集器(工厂函数)。
/// device_name = None 表示使用系统默认设备。
#[cfg(target_os = "windows")]
pub fn create_capture() -> Box<dyn AudioCapture> {
    Box::new(windows::CpalCapture::new(None))
}

/// 创建指定设备的音频采集器。
#[cfg(target_os = "windows")]
pub fn create_capture_with_device(device_name: String) -> Box<dyn AudioCapture> {
    Box::new(windows::CpalCapture::new(Some(device_name)))
}

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows;
