//! 只在内存抓屏，不保存像素。用法：cargo run -p blink-infra --example scroll_capture_bench -- 1244 789 24
use std::time::{Duration, Instant};

fn p95(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[((values.len() as f64 * 0.95).ceil() as usize).saturating_sub(1)]
}

fn main() -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |index: usize, default: u32| {
        args.get(index)
            .and_then(|arg| arg.parse().ok())
            .unwrap_or(default)
    };
    let (width, height, count) = (number(0, 1244), number(1, 789), number(2, 24));
    if count == 0 || count > 120 || width == 0 || height == 0 || width > 4096 || height > 2160 {
        return Err("参数超出基准范围".into());
    }
    let mut captures = Vec::new();
    let mut conversions = Vec::new();
    let mut reused_conversions = Vec::new();
    let mut continuous_captures = Vec::new();
    for _ in 0..count {
        let started = Instant::now();
        let mut rgba = blink_infra::platform::screenshot::capture_region(0, 0, width, height)?;
        let captured = started.elapsed().as_secs_f64() * 1000.0;
        let original = rgba.clone();
        let converting = Instant::now();
        for chunk in rgba.chunks_exact_mut(4) {
            let px = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let rb = px & 0x00FF00FF;
            let ga = px & 0xFF00FF00;
            chunk.copy_from_slice(&(ga | (rb << 16) | (rb >> 16)).to_ne_bytes());
        }
        std::hint::black_box(&rgba);
        captures.push(captured);
        conversions.push(converting.elapsed().as_secs_f64() * 1000.0);
        let converting = Instant::now();
        blink_infra::platform::screenshot::swap_rgba_bgra_in_place(&mut rgba);
        std::hint::black_box(&rgba);
        reused_conversions.push(converting.elapsed().as_secs_f64() * 1000.0);
        if rgba != original {
            return Err("快速转换与原路径像素结果不一致".into());
        }
        let started = Instant::now();
        let current =
            blink_infra::platform::screenshot::capture_region_continuous(0, 0, width, height)?;
        continuous_captures.push(started.elapsed().as_secs_f64() * 1000.0);
        if current.len() != original.len() {
            return Err("连续捕获像素尺寸不一致".into());
        }
        std::hint::black_box(current);
        std::thread::sleep(Duration::from_millis(50));
    }
    let capture_mean = captures.iter().sum::<f64>() / count as f64;
    let convert_mean = conversions.iter().sum::<f64>() / count as f64;
    let reused_mean = reused_conversions.iter().sum::<f64>() / count as f64;
    tracing::info!(
        width,
        height,
        count,
        capture_mean_ms = capture_mean,
        capture_p95_ms = p95(&mut captures),
        continuous_capture_mean_ms = continuous_captures.iter().sum::<f64>() / count as f64,
        continuous_capture_p95_ms = p95(&mut continuous_captures),
        convert_mean_ms = convert_mean,
        convert_p95_ms = p95(&mut conversions),
        reused_convert_mean_ms = reused_mean,
        reused_convert_p95_ms = p95(&mut reused_conversions),
        "scroll capture benchmark (pixels not persisted)"
    );
    Ok(())
}
