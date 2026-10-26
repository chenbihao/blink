//! GIF 录制流水线性能基准（0.28.7，phase §3.7 候选测试矩阵）。
//!
//! 用法：`cargo run -p blink-infra --release --example gif_pipeline_bench [-- --full]`
//!
//! 三部分：
//! - **A 压缩微基准**：静止/渐变/高熵三种内容 × 800×600 / 1280×720，
//!   各 30 帧 zstd-1 压缩 + 随机解压，报吞吐与压缩比（不假定压缩率）。
//! - **B 采集微基准**（Windows）：真实 GDI BitBlt + 光标叠加单帧耗时
//!   （屏幕中央 1280×720），量化热路径预算。
//! - **C 流水线整体**：合成帧源 15fps 持续变化画面全流水线（队列→压缩落盘→
//!   停止排空），报队列峰值/重复合并/总写入。`--full` 跑 30s，默认 10s。
//!
//! 输出为 Markdown 表（记入 phase 实现记录）。

use std::time::Instant;

use blink_infra::gif::{self as infra_gif};

/// 内容模式。
enum Content {
    Static(u8),
    /// 平滑渐变（帧间缓慢变化——典型 UI 动画）。
    Gradient(u32),
    /// 高熵伪随机（最坏情况，不假定压缩率的验证）。
    Noisy(u64),
}

impl Content {
    fn frame(&self, w: u32, h: u32, t: u32) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        match self {
            Content::Static(base) => {
                for px in v.chunks_exact_mut(4) {
                    px[0] = *base;
                    px[1] = base.wrapping_add(10);
                    px[2] = base.wrapping_add(20);
                    px[3] = 255;
                }
            }
            Content::Gradient(speed) => {
                for (i, px) in v.chunks_exact_mut(4).enumerate() {
                    let wave = ((i as u32 / 97) + t * speed) as u8;
                    px[0] = wave;
                    px[1] = wave.wrapping_mul(3).wrapping_add(7);
                    px[2] = 255 - wave;
                    px[3] = 255;
                }
            }
            Content::Noisy(seed) => {
                // xorshift64*：快速确定性伪随机（不密码学，只造高熵）
                let mut s = seed ^ (t as u64).wrapping_mul(0x9E3779B97F4A7C15);
                for chunk in v.chunks_exact_mut(8) {
                    s ^= s >> 12;
                    s ^= s << 25;
                    s ^= s >> 27;
                    let bits = s.wrapping_mul(0x2545F4914F6CDD1D);
                    chunk.copy_from_slice(&bits.to_le_bytes());
                }
            }
        }
        v
    }
    fn name(&self) -> &'static str {
        match self {
            Content::Static(_) => "静止",
            Content::Gradient(_) => "渐变(UI 动画)",
            Content::Noisy(_) => "高熵(最坏)",
        }
    }
}

fn bench_compress() {
    println!("## A. zstd-1 压缩微基准（30 帧）\n");
    println!("| 内容 | 尺寸 | 原始/帧 | 压缩/帧 | 压缩比 | 压缩吞吐 | 随机解压/帧 |");
    println!("|---|---|---|---|---|---|---|");
    for (w, h) in [(800u32, 600u32), (1280, 720)] {
        for content in [Content::Static(40), Content::Gradient(3), Content::Noisy(0xA5A5)] {
            let frames: Vec<Vec<u8>> =
                (0..30).map(|t| content.frame(w, h, t)).collect();
            let t0 = Instant::now();
            let mut total_out = 0u64;
            let compressed: Vec<Vec<u8>> = frames
                .iter()
                .map(|f| {
                    let c = zstd_compress(f);
                    total_out += c.len() as u64;
                    c
                })
                .collect();
            let enc_ms = t0.elapsed().as_millis() as u64;
            let t1 = Instant::now();
            for c in &compressed {
                let _ = zstd::stream::decode_all(&c[..]).unwrap();
            }
            let dec_ms = t1.elapsed().as_millis() as u64;
            let raw = frames[0].len() as u64;
            println!(
                "| {} | {}×{} | {} | {} | {:.2}× | {:.0} MB/s | {:.2} ms |",
                content.name(),
                w,
                h,
                raw,
                total_out / 30,
                raw as f64 * 30.0 / total_out as f64,
                (raw as f64 * 30.0 / 1048576.0) / (enc_ms as f64 / 1000.0),
                dec_ms as f64 / 30.0,
            );
        }
    }
}

fn zstd_compress(data: &[u8]) -> Vec<u8> {
    infra_gif::spool::compress_frame(data).unwrap()
}

#[cfg(target_os = "windows")]
fn bench_capture() {
    println!("\n## B. GDI 采集 + 光标叠加（屏幕中央 1280×720，20 帧平均）\n");
    let mut source = infra_gif::GdiFrameSource::new(400, 200, 1280, 720, true);
    // 预热（thread_local GDI 缓存首帧建位图）
    let _ = infra_gif::FrameSource::capture(&mut source);
    let t0 = Instant::now();
    let n = 20;
    for _ in 0..n {
        let _ = infra_gif::FrameSource::capture(&mut source);
    }
    let per = t0.elapsed().as_millis() as f64 / n as f64;
    println!("- 单帧（BitBlt + 光标）：**{per:.1} ms**（≈{:.0} fps 上限）", 1000.0 / per);
}

#[cfg(not(target_os = "windows"))]
fn bench_capture() {}

struct BenchSource {
    w: u32,
    h: u32,
    tick: u32,
    content: Content,
}

impl infra_gif::FrameSource for BenchSource {
    fn capture(&mut self) -> Result<Vec<u8>, String> {
        self.tick += 1;
        Ok(self.content.frame(self.w, self.h, self.tick))
    }
    fn frame_bytes(&self) -> usize {
        (self.w as usize) * (self.h as usize) * 4
    }
}

fn bench_pipeline(full: bool) {
    let seconds = if full { 30 } else { 10 };
    println!(
        "\n## C. 全流水线（1280×720 · 15fps · 持续变化 · {seconds}s 真实墙钟）\n"
    );
    let root = std::env::temp_dir().join(format!("blink-gif-bench-{}", infra_gif::generate_session_id()));
    let meta = infra_gif::SessionMeta {
        spool_version: infra_gif::SPOOL_VERSION,
        id: "bench".into(),
        region: infra_gif::CaptureRegion { x: 0, y: 0, w: 1280, h: 720 },
        fps: 15,
        show_cursor: false,
        created_at: 0,
        state: "recording".into(),
        ts_ms: Vec::new(),
        last_dur_ms: 66,
        spool_bytes: 0,
        stop_reason: None,
    };
    let writer = infra_gif::SpoolWriter::create(&root, meta).unwrap();
    let (ctl, rx) = infra_gif::spawn_pipeline(
        BenchSource { w: 1280, h: 720, tick: 0, content: Content::Gradient(3) },
        writer,
        15,
        infra_gif::RecordingBudget::default(),
    );
    ctl.command(infra_gif::CaptureCommand::Start);
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    ctl.command(infra_gif::CaptureCommand::Stop);
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    println!("- 落盘帧数：**{}**（{}s × 15fps 期望 {}）", outcome.frames, seconds, seconds * 15);
    println!("- 重复帧合并：{}", outcome.duplicates_merged);
    println!("- 队列峰值：**{} 帧**（cap 32）", outcome.peak_queued);
    println!("- 压缩素材：**{:.1} MB**（原始 ≈ {:.1} MB）", outcome.spool_bytes as f64 / 1048576.0, outcome.frames as f64 * 1280.0 * 720.0 * 4.0 / 1048576.0);
    println!("- 过载停止：{}", outcome.overloaded);
    println!("- 采集端累计耗时：{} ms", outcome.capture_cost_ms);
    let _ = std::fs::remove_dir_all(&root);
}

fn main() {
    let full = std::env::args().any(|a| a == "--full");
    println!("# GIF 流水线基准（{}）\n", if cfg!(debug_assertions) { "debug 构建——数值仅相对参考" } else { "release" });
    bench_compress();
    bench_capture();
    bench_pipeline(full);
}
