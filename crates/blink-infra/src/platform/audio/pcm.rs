//! WASAPI 原始 PCM 转换与回调时钟边界，语音输入及录屏共用。
use cpal::{Data, FromSample, SampleFormat, SizedSample};
use std::time::{Duration, Instant};

pub fn supported(format: SampleFormat) -> bool {
    matches!(
        format,
        SampleFormat::F32
            | SampleFormat::F64
            | SampleFormat::I8
            | SampleFormat::I16
            | SampleFormat::I24
            | SampleFormat::I32
            | SampleFormat::I64
            | SampleFormat::U8
            | SampleFormat::U16
            | SampleFormat::U32
            | SampleFormat::U64
    )
}

fn convert<T: SizedSample>(data: &Data) -> Vec<f32>
where
    f32: FromSample<T>,
{
    convert_samples(data.as_slice::<T>().unwrap_or_default())
}

fn convert_samples<T: SizedSample>(samples: &[T]) -> Vec<f32>
where
    f32: FromSample<T>,
{
    samples
        .iter()
        .map(|&sample| {
            let sample = sample.to_sample::<f32>();
            if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            }
        })
        .collect()
}

pub fn to_f32(data: &Data, format: SampleFormat) -> Vec<f32> {
    match format {
        SampleFormat::F32 => convert::<f32>(data),
        SampleFormat::F64 => convert::<f64>(data),
        SampleFormat::I8 => convert::<i8>(data),
        SampleFormat::I16 => convert::<i16>(data),
        SampleFormat::I24 => convert::<cpal::I24>(data),
        SampleFormat::I32 => convert::<i32>(data),
        SampleFormat::I64 => convert::<i64>(data),
        SampleFormat::U8 => convert::<u8>(data),
        SampleFormat::U16 => convert::<u16>(data),
        SampleFormat::U32 => convert::<u32>(data),
        SampleFormat::U64 => convert::<u64>(data),
        _ => Vec::new(),
    }
}

/// 回调交付的是一个完整音频块。部分 WASAPI loopback 设备报告零延迟，
/// 此时不能把块首映射为 now，否则整块落在未来并被隐私/时钟门控丢弃。
pub fn block_start(now: Instant, frames: usize, rate: u32, latency: Duration) -> Instant {
    let duration = Duration::from_secs_f64(frames as f64 / rate.max(1) as f64);
    now.checked_sub(latency.max(duration)).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zero_latency_maps_the_whole_block_before_callback_time() {
        let now = Instant::now();
        assert_eq!(
            now.duration_since(block_start(now, 480, 48_000, Duration::ZERO)),
            Duration::from_millis(10)
        );
        assert_eq!(
            now.duration_since(block_start(now, 480, 48_000, Duration::from_millis(25))),
            Duration::from_millis(25)
        );
    }
    #[test]
    fn input_and_recording_share_signed_unsigned_and_float_conversion() {
        assert_eq!(convert_samples(&[i16::MIN, 0, 16384]), [-1.0, 0.0, 0.5]);
        assert_eq!(convert_samples(&[0u16, 32768, 49152]), [-1.0, 0.0, 0.5]);
        assert_eq!(convert_samples(&[f32::NAN, 2.0, -0.5]), [0.0, 1.0, -0.5]);
    }
}
