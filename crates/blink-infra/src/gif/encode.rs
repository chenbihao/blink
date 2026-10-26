//! GIF 流式编码器（0.28.7，phase §3.6「导出期」）。
//!
//! 导出期才做调色板量化与 GIF 编码（录制期素材是快速无损压缩）：
//! - **全局调色板**：对全部导出帧的（缩放后）RGBA 采样训练 NeuQuant 256 色，
//!   演示 UI 场景（有限色彩）质量足够且每帧不带局部表，文件更小；
//! - **重复帧合并**：相邻帧像素完全相同 → 只累加 delay，不重复写帧数据
//!   （静止画面 GIF 体积的主要来源）；
//! - **dirty-rect**：与前一帧比较索引缓冲求变化包围盒，只写子矩形帧
//!   （GIF 子矩形语义 + disposal=Keep，解码端画布自然累积）；
//! - **流式临时成品**：逐帧写 `<目标>.tmp` → 完成后用 `gif` crate 独立解码
//!   验证（帧数/逻辑尺寸/总时长）→ rename 提交；取消/失败删除 tmp 不留伪成品；
//! - delay 已由 domain `ExportPlan` 量化（1/100s，clamp ≥2）。

use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use gif::{DisposalMethod, Encoder, Frame, Repeat};
use serde::Serialize;

use super::spool::SpoolReader;

/// 编码计划（infra 侧独立定义——不依赖 domain；由 app 层从
/// `blink_domain::gif::Timeline::export_plan` 冻结快照转换）。
#[derive(Debug, Clone, PartialEq)]
pub struct EncodePlan {
    pub src_w: u32,
    pub src_h: u32,
    /// None = 无限循环。
    pub loop_count: Option<u16>,
    /// 量化后的逐帧 delay（1/100s）。
    pub frames: Vec<EncodeFrame>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct EncodeFrame {
    pub src_idx: u32,
    /// 量化后的 delay，单位 1/100 秒。
    pub delay_cs: u32,
}

/// 质量预设（NeuQuant 采样密度：数值越小质量越好越慢）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportQuality {
    High,
    Medium,
    Low,
}

impl ExportQuality {
    fn sample_fac(self) -> i32 {
        match self {
            ExportQuality::High => 3,
            ExportQuality::Medium => 10,
            ExportQuality::Low => 24,
        }
    }
}

/// 导出选项。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExportOptions {
    /// 目标最大宽度（None = 原始尺寸）；等比缩放。
    pub max_width: Option<u32>,
    pub quality: ExportQuality,
    /// 覆盖已存在文件。
    pub overwrite: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            max_width: None,
            quality: ExportQuality::Medium,
            overwrite: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("目标文件已存在: {0}")]
    FileExists(PathBuf),
    #[error("导出已取消")]
    Cancelled,
    #[error("素材帧读取失败: 帧 {0}")]
    FrameRead(u32),
    #[error("成品验证失败: {0}")]
    VerifyFailed(String),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("GIF 编码失败: {0}")]
    Encode(String),
}

/// 导出结果统计。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportStats {
    /// 输出尺寸。
    pub width: u32,
    pub height: u32,
    /// 素材帧数（plan 帧数）。
    pub source_frames: u32,
    /// 实际写入 GIF 的帧数（重复帧合并后）。
    pub written_frames: u32,
    /// 重复帧合并省去的帧数。
    pub merged_duplicate_frames: u32,
    /// 成品字节。
    pub bytes: u64,
    /// 素材原始字节（压缩前）。
    pub source_raw_bytes: u64,
    /// 编码耗时 ms。
    pub encode_ms: u64,
}

impl From<gif::EncodingError> for ExportError {
    fn from(e: gif::EncodingError) -> Self {
        ExportError::Encode(e.to_string())
    }
}

/// 导出阶段（进度回调与事件用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportStage {
    /// 调色板训练·帧解码阶段（可量化进度 x/y）。
    Preparing,
    /// 调色板训练·NeuQuant 量化阶段（不可量化，UI 显示不定态文案）。
    Quantizing,
    /// 逐帧编码写盘。
    Encoding,
}

impl ExportStage {
    pub fn as_str(self) -> &'static str {
        match self {
            ExportStage::Preparing => "preparing",
            ExportStage::Quantizing => "quantizing",
            ExportStage::Encoding => "encoding",
        }
    }
}

/// 进度回调：`(阶段, 已完成帧, 总帧)`。
pub type ProgressFn<'a> = dyn Fn(ExportStage, u32, u32) + Send + 'a;

/// 导出 GIF 到 `out_path`（含验证与原子提交）。
///
/// `plan` 由 domain `Timeline::export_plan` 冻结（revision 快照）；编码期间
/// 编辑操作不影响本次导出。
pub fn export_gif(
    reader: &SpoolReader,
    plan: &EncodePlan,
    opts: &ExportOptions,
    out_path: &Path,
    cancel: &AtomicBool,
    progress: &ProgressFn<'_>,
) -> Result<ExportStats, ExportError> {
    let t0 = std::time::Instant::now();
    if out_path.exists() && !opts.overwrite {
        return Err(ExportError::FileExists(out_path.to_path_buf()));
    }
    if plan.frames.is_empty() {
        return Err(ExportError::VerifyFailed("导出计划为空".into()));
    }
    // GIF 逻辑尺寸上限 u16。
    let (out_w, out_h) = target_size(plan.src_w, plan.src_h, opts.max_width);
    if out_w == 0 || out_h == 0 || out_w > u16::MAX as u32 || out_h > u16::MAX as u32 {
        return Err(ExportError::VerifyFailed(format!("输出尺寸非法: {out_w}x{out_h}")));
    }
    let total = plan.frames.len() as u32;
    tracing::debug!(
        out = %out_path.display(),
        out_w, out_h, total,
        quality = ?opts.quality,
        max_width = ?opts.max_width,
        "GIF 导出开始"
    );

    let tmp_path = out_path.with_extension("gif.tmp");
    // 清理上次残留 tmp
    let _ = fs::remove_file(&tmp_path);

    let result = encode_to_tmp(reader, plan, opts, &tmp_path, out_w, out_h, cancel, progress);
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = fs::remove_file(&tmp_path);
        return Err(ExportError::Cancelled);
    }
    let stats = match result {
        Ok(s) => s,
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
    };
    // 独立解码验证：帧数、逻辑尺寸、总 delay。
    let t_verify = std::time::Instant::now();
    let verify = verify_gif(&tmp_path, total_delay_cs(plan), out_w, out_h);
    tracing::debug!(
        verify_ms = t_verify.elapsed().as_millis() as u64,
        "GIF 导出：独立解码验证完成"
    );
    if let Err(e) = verify {
        let _ = fs::remove_file(&tmp_path);
        return Err(ExportError::VerifyFailed(e));
    }
    // 原子提交：tmp → 目标（overwrite 时先删旧文件）。
    if out_path.exists() {
        fs::remove_file(out_path)?;
    }
    fs::rename(&tmp_path, out_path)?;
    let bytes = fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
    Ok(ExportStats {
        width: out_w,
        height: out_h,
        source_frames: total,
        written_frames: stats.written,
        merged_duplicate_frames: total - stats.written,
        bytes,
        source_raw_bytes: stats.raw_bytes,
        encode_ms: t0.elapsed().as_millis() as u64,
    })
}

struct EncodeStats {
    written: u32,
    raw_bytes: u64,
}

fn encode_to_tmp(
    reader: &SpoolReader,
    plan: &EncodePlan,
    opts: &ExportOptions,
    tmp_path: &Path,
    out_w: u32,
    out_h: u32,
    cancel: &AtomicBool,
    progress: &ProgressFn<'_>,
) -> Result<EncodeStats, ExportError> {
    // ── 1. 调色板训练：解码一遍导出帧的（缩放后）像素，NeuQuant 生成全局
    //    256 色表；量化器保留用于逐帧索引映射。此阶段是导出前半段的耗时主体
    //    （素材帧随机解码 + 缩放），进度经 Preparing 阶段回调上报。
    let sample_fac = opts.quality.sample_fac();
    let (palette, nq) = train_quantizer(reader, plan, out_w, out_h, sample_fac, cancel, progress)?;

    let file = BufWriter::with_capacity(1 << 20, File::create(tmp_path)?);
    let mut encoder = Encoder::new(file, out_w as u16, out_h as u16, &palette)?;
    encoder.set_repeat(match plan.loop_count {
        None => Repeat::Infinite,
        Some(n) => Repeat::Finite(n),
    })?;

    let mut prev_indexed: Vec<u8> = Vec::new();
    let mut cur_indexed: Vec<u8> = Vec::new();
    let mut written = 0u32;
    let mut raw_bytes = 0u64;
    let total = plan.frames.len() as u32;
    let mut done = 0u32;

    // 重复帧合并：pending = 尚未写出的当前索引帧及其累计 delay。
    let mut pending: Option<(Vec<u8>, u32)> = None;

    macro_rules! flush_pending {
        () => {
            if let Some((buffer, delay_cs)) = pending.take() {
                write_one_frame(&mut encoder, &buffer, &mut prev_indexed, out_w, out_h, delay_cs)?;
                written += 1;
            }
        };
    }

    for pf in &plan.frames {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ExportError::Cancelled);
        }
        let frame_t0 = std::time::Instant::now();
        let bgra = reader
            .read_frame(pf.src_idx)
            .map_err(|_| ExportError::FrameRead(pf.src_idx))?;
        raw_bytes += bgra.len() as u64;
        let scaled = resize_bgra(&bgra, plan.src_w, plan.src_h, out_w, out_h);
        cur_indexed.clear();
        cur_indexed.reserve(scaled.len() / 4);
        for px in scaled.chunks_exact(4) {
            // scaled 是 RGBA（resize 输出统一 RGBA）；index_of 要求 4 字节输入
            cur_indexed.push(nq.index_of(px) as u8);
        }
        match pending.take() {
            Some((buf, delay)) if buf == cur_indexed[..] => {
                // 重复帧：累加 delay，不写帧
                pending = Some((buf, delay.saturating_add(pf.delay_cs).min(u16::MAX as u32)));
            }
            Some((buf, delay)) => {
                write_one_frame(&mut encoder, &buf, &mut prev_indexed, out_w, out_h, delay)?;
                written += 1;
                pending = Some((cur_indexed.clone(), pf.delay_cs));
            }
            None => {
                pending = Some((cur_indexed.clone(), pf.delay_cs));
            }
        }
        done += 1;
        tracing::trace!(
            src_idx = pf.src_idx,
            delay_cs = pf.delay_cs,
            encode_frame_ms = frame_t0.elapsed().as_millis() as u64,
            "GIF 导出：编码帧完成"
        );
        progress(ExportStage::Encoding, done, total);
    }
    flush_pending!();
    drop(encoder); // flush encoder + writer
    Ok(EncodeStats { written, raw_bytes })
}

/// 写一帧（dirty-rect：与 prev 求变化包围盒，只写子矩形；首帧全量）。
fn write_one_frame<W: std::io::Write>(
    encoder: &mut Encoder<W>,
    indexed: &[u8],
    prev: &mut Vec<u8>,
    w: u32,
    h: u32,
    delay_cs: u32,
) -> Result<(), ExportError> {
    let rect = if prev.len() == indexed.len() && !prev.is_empty() {
        dirty_rect(indexed, prev, w, h).unwrap_or((0, 0, w, h))
    } else {
        (0, 0, w, h)
    };
    let (rx, ry, rw, rh) = rect;
    let mut buffer = Vec::with_capacity((rw * rh) as usize);
    for row in 0..rh {
        let y = ry + row;
        let start = (y * w + rx) as usize;
        buffer.extend_from_slice(&indexed[start..start + rw as usize]);
    }
    encoder.write_frame(&Frame {
        delay: delay_cs.min(u16::MAX as u32) as u16,
        dispose: DisposalMethod::Keep,
        transparent: None,
        needs_user_input: false,
        top: ry as u16,
        left: rx as u16,
        width: rw as u16,
        height: rh as u16,
        interlaced: false,
        palette: None,
        buffer: std::borrow::Cow::Owned(buffer),
    })?;
    // 编码后该帧成为「前一帧」画布状态
    if prev.len() != indexed.len() {
        prev.clear();
        prev.extend_from_slice(indexed);
    } else {
        prev.copy_from_slice(indexed);
    }
    Ok(())
}

/// 求两索引缓冲的变化包围盒；完全相同返回 None。
fn dirty_rect(cur: &[u8], prev: &[u8], w: u32, _h: u32) -> Option<(u32, u32, u32, u32)> {
    let stride = w as usize;
    let mut min_x = usize::MAX;
    let mut max_x = 0usize;
    let mut min_y = usize::MAX;
    let mut max_y = 0usize;
    for (y, (crow, prow)) in cur
        .chunks_exact(stride)
        .zip(prev.chunks_exact(stride))
        .enumerate()
    {
        if crow == prow {
            continue;
        }
        min_y = min_y.min(y);
        max_y = max_y.max(y);
        for (x, (a, b)) in crow.iter().zip(prow.iter()).enumerate() {
            if a != b {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
            }
        }
    }
    if min_x == usize::MAX {
        return None;
    }
    Some((
        min_x as u32,
        min_y as u32,
        (max_x - min_x + 1) as u32,
        (max_y - min_y + 1) as u32,
    ))
}

/// 训练全局调色板：解码全部导出帧的缩放后 RGBA → NeuQuant（256 色）。
/// 返回 `(palette_rgb, quantizer)`；量化器复用于逐帧 `index_of` 映射，保证
/// 索引与调色板一致。
fn train_quantizer(
    reader: &SpoolReader,
    plan: &EncodePlan,
    out_w: u32,
    out_h: u32,
    sample_fac: i32,
    cancel: &AtomicBool,
    progress: &ProgressFn<'_>,
) -> Result<(Vec<u8>, color_quant::NeuQuant), ExportError> {
    let t0 = std::time::Instant::now();
    let total = plan.frames.len() as u32;
    let mut pixels = Vec::new();
    for (i, pf) in plan.frames.iter().enumerate() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ExportError::Cancelled);
        }
        let frame_t0 = std::time::Instant::now();
        let bgra = reader
            .read_frame(pf.src_idx)
            .map_err(|_| ExportError::FrameRead(pf.src_idx))?;
        let scaled = resize_bgra(&bgra, plan.src_w, plan.src_h, out_w, out_h);
        // 训练像素抽稀：每 4 像素取 1（NeuQuant 对 UI 演示场景足够；
        // 全量输入只增加汇总与量化耗时，质量差异不可感知）
        pixels.extend(scaled.chunks_exact(4).step_by(4).flat_map(|px| px.to_vec()));
        tracing::trace!(
            src_idx = pf.src_idx,
            decode_resize_ms = frame_t0.elapsed().as_millis() as u64,
            "GIF 导出：调色板训练帧完成"
        );
        progress(ExportStage::Preparing, i as u32 + 1, total);
    }
    let t_train_pixels = t0.elapsed();
    // 量化阶段单独上报（这是「x/x 卡住」的那段——计算本身不可量化进度）
    progress(ExportStage::Quantizing, total, total);
    let nq = color_quant::NeuQuant::new(sample_fac, 256, &pixels);
    tracing::debug!(
        total,
        pixel_bytes = pixels.len(),
        sample_fac,
        pixels_ms = t_train_pixels.as_millis() as u64,
        quantize_ms = t0.elapsed().as_millis() as u64 - t_train_pixels.as_millis() as u64,
        palette_colors = nq.color_map_rgb().len() / 3,
        "GIF 导出：调色板训练完成"
    );
    Ok((nq.color_map_rgb(), nq))
}

/// 目标尺寸（等比缩放到 max_width 内；不放大）。
fn target_size(src_w: u32, src_h: u32, max_width: Option<u32>) -> (u32, u32) {
    match max_width {
        Some(max_w) if src_w > max_w => {
            let scale = max_w as f64 / src_w as f64;
            (
                (src_w as f64 * scale).round() as u32,
                (src_h as f64 * scale).round() as u32,
            )
        }
        _ => (src_w, src_h),
    }
}

/// box 平均缩放 BGRA → RGBA（等比；短演示帧数有限，质量优先）。
/// 缩略图/预览（app 层）与导出编码共用。
pub fn resize_bgra(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        // 同尺寸：BGRA→RGBA 直转
        let mut out = src.to_vec();
        crate::platform::screenshot::swap_rgba_bgra_in_place(&mut out);
        return out;
    }
    let mut out = vec![0u8; (dst_w * dst_h * 4) as usize];
    let x_ratio = src_w as f64 / dst_w as f64;
    let y_ratio = src_h as f64 / dst_h as f64;
    for dy in 0..dst_h {
        let sy0 = (dy as f64 * y_ratio) as u32;
        let sy1 = (((dy + 1) as f64 * y_ratio).ceil() as u32).min(src_h).max(sy0 + 1);
        for dx in 0..dst_w {
            let sx0 = (dx as f64 * x_ratio) as u32;
            let sx1 = (((dx + 1) as f64 * x_ratio).ceil() as u32).min(src_w).max(sx0 + 1);
            let (mut b, mut g, mut r, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for sy in sy0..sy1 {
                let row = (sy * src_w) as usize;
                for sx in sx0..sx1 {
                    let i = (row + sx as usize) * 4;
                    b += src[i] as u64;
                    g += src[i + 1] as u64;
                    r += src[i + 2] as u64;
                    a += src[i + 3] as u64;
                    n += 1;
                }
            }
            let o = ((dy * dst_w + dx) as usize) * 4;
            out[o] = (r / n) as u8;
            out[o + 1] = (g / n) as u8;
            out[o + 2] = (b / n) as u8;
            out[o + 3] = (a / n) as u8;
        }
    }
    out
}

fn total_delay_cs(plan: &EncodePlan) -> u64 {
    plan.frames.iter().map(|f| f.delay_cs as u64).sum()
}

/// 独立解码验证：帧数、逻辑尺寸、总 delay 与计划一致（重复帧合并会改变
/// 「GIF 帧数」，故只验证 delay 总和与尺寸；帧数 ≤ 计划数由合并保证）。
fn verify_gif(path: &Path, expect_delay_cs: u64, expect_w: u32, expect_h: u32) -> Result<(), String> {
    let file = BufReader::new(File::open(path).map_err(|e| e.to_string())?);
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = opts.read_info(file).map_err(|e| e.to_string())?;
    let (w, h) = (decoder.width() as u32, decoder.height() as u32);
    if (w, h) != (expect_w, expect_h) {
        return Err(format!("尺寸不符: {w}x{h} ≠ {expect_w}x{expect_h}"));
    }
    let mut frames = 0u32;
    let mut delay_sum = 0u64;
    while let Some(frame) = decoder
        .read_next_frame()
        .map_err(|e| format!("解码失败于帧 {frames}: {e}"))?
    {
        frames += 1;
        delay_sum += frame.delay as u64;
    }
    if frames == 0 {
        return Err("GIF 无帧".into());
    }
    if delay_sum != expect_delay_cs {
        return Err(format!("总时长不符: {delay_sum} ≠ {expect_delay_cs} (1/100s)"));
    }
    Ok(())
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gif::spool::{generate_session_id, CaptureRegion, SessionMeta, SpoolWriter};
    

    /// 合成 8x8 帧：底色 `base`，(2,2) 处一个 2x2 亮块当「运动目标」。
    fn synth_frame(w: u32, h: u32, base: u8, hot: bool) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            v.extend_from_slice(&[base, base, base, 255]);
        }
        if hot {
            for y in 2..4 {
                for x in 2..4 {
                    let i = ((y * w + x) * 4) as usize;
                    v[i] = 255;
                    v[i + 1] = 255;
                    v[i + 2] = 255;
                }
            }
        }
        v
    }

    fn build_session(w: u32, h: u32, frames: &[Vec<u8>], ts: &[u64]) -> (tempfile::TempDir, String) {
        let root = tempfile::tempdir().unwrap();
        let meta = SessionMeta {
            spool_version: crate::gif::spool::SPOOL_VERSION,
            id: generate_session_id(),
            region: CaptureRegion { x: 0, y: 0, w, h },
            fps: 10,
            show_cursor: false,
            created_at: 0,
            state: "recording".into(),
            ts_ms: Vec::new(),
            last_dur_ms: 100,
            spool_bytes: 0,
            stop_reason: None,
        };
        let id = meta.id.clone();
        let mut writer = SpoolWriter::create(root.path(), meta).unwrap();
        for (i, f) in frames.iter().enumerate() {
            writer.append_frame(ts[i], f).unwrap();
        }
        writer.finalize("user").unwrap();
        (root, id)
    }

    fn plan_from(frames: &[Vec<u8>], src_w: u32, src_h: u32, delays: &[u32]) -> EncodePlan {
        EncodePlan {
            src_w,
            src_h,
            loop_count: None,
            frames: frames
                .iter()
                .enumerate()
                .map(|(i, _)| EncodeFrame {
                    src_idx: i as u32,
                    delay_cs: delays[i].min(u32::MAX),
                })
                .collect(),
        }
    }

    #[test]
    fn export_verify_roundtrip_full_cycle() {
        let frames = vec![
            synth_frame(8, 8, 30, false),
            synth_frame(8, 8, 30, true),
            synth_frame(8, 8, 40, false),
        ];
        let (root, id) = build_session(8, 8, &frames, &[0, 100, 200]);
        let reader = SpoolReader::open(root.path(), &id).unwrap();
        let plan = plan_from(&frames, 8, 8, &[10, 10, 10]);
        let out = root.path().join("out.gif");
        let cancel = AtomicBool::new(false);
        let stats = export_gif(
            &reader,
            &plan,
            &ExportOptions::default(),
            &out,
            &cancel,
            &|_, _, _| {},
        )
        .unwrap();
        assert_eq!((stats.width, stats.height), (8, 8));
        assert_eq!(stats.written_frames, 3);
        assert_eq!(stats.merged_duplicate_frames, 0);
        assert!(stats.bytes > 0);
        assert!(out.exists());
        assert!(!out.with_extension("gif.tmp").exists(), "tmp 已提交清理");
    }

    #[test]
    fn duplicate_frames_merged_with_accumulated_delay() {
        let same = synth_frame(8, 8, 50, false);
        let frames = vec![same.clone(), same.clone(), same.clone()];
        let (root, id) = build_session(8, 8, &frames, &[0, 100, 200]);
        let reader = SpoolReader::open(root.path(), &id).unwrap();
        let plan = plan_from(&frames, 8, 8, &[10, 10, 10]);
        let out = root.path().join("dup.gif");
        let stats = export_gif(
            &reader,
            &plan,
            &ExportOptions::default(),
            &out,
            &AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
        assert_eq!(stats.written_frames, 1, "三帧相同合并为一帧");
        assert_eq!(stats.merged_duplicate_frames, 2);
        // 验证层：总 delay = 30 × 10ms
        let v = verify_gif(&out, 30, 8, 8);
        assert!(v.is_ok());
    }

    #[test]
    fn dirty_rect_composes_to_full_canvas_on_decode() {
        // 帧 0 全量（含亮块），帧 1 只移动亮块——dirty-rect 写子矩形，
        // 解码端以画布累积语义还原。验证：解出的帧 1 == 期望全帧。
        let frames = vec![
            synth_frame(8, 8, 30, true),
            {
                // 亮块移动到 (4,4)
                let mut v = synth_frame(8, 8, 30, false);
                for y in 4..6 {
                    for x in 4..6 {
                        let i = ((y * 8 + x) * 4) as usize;
                        v[i] = 255;
                        v[i + 1] = 255;
                        v[i + 2] = 255;
                    }
                }
                v
            },
        ];
        let (root, id) = build_session(8, 8, &frames, &[0, 100]);
        let reader = SpoolReader::open(root.path(), &id).unwrap();
        let plan = plan_from(&frames, 8, 8, &[10, 10]);
        let out = root.path().join("dirty.gif");
        export_gif(
            &reader,
            &plan,
            &ExportOptions::default(),
            &out,
            &AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();

        // 独立解码：帧 1 的画布内容应等于 synth(40 亮块@(4,4))
        let file = BufReader::new(File::open(&out).unwrap());
        let mut opts = gif::DecodeOptions::new();
        opts.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = opts.read_info(file).unwrap();
        let mut canvas = vec![0u8; 8 * 8 * 4];
        let mut frame_idx = 0;
        while let Some(f) = decoder.read_next_frame().unwrap() {
            // 把子矩形覆盖到画布（先拷出子矩形数据避免借用冲突）
            let mut sub = vec![0u8; f.buffer.len()];
            sub.copy_from_slice(&f.buffer);
            let (fl, ft, fw, fh) = (f.left, f.top, f.width, f.height);
            for row in 0..fh {
                for col in 0..fw {
                    let sx = (fl + col) as usize;
                    let sy = (ft + row) as usize;
                    let src = (row as usize * fw as usize + col as usize) * 4;
                    let dst = (sy * 8 + sx) * 4;
                    canvas[dst..dst + 4].copy_from_slice(&sub[src..src + 4]);
                }
            }
            if frame_idx == 1 {
                // 期望：底 30 无 (2,2) 亮块，(4,4) 有亮块
                let expect = {
                    let mut v = synth_frame(8, 8, 30, false);
                    for y in 4..6 {
                        for x in 4..6 {
                            let i = ((y * 8 + x) * 4) as usize;
                            v[i] = 255;
                            v[i + 1] = 255;
                            v[i + 2] = 255;
                        }
                    }
                    bgra_to_rgba(&v)
                };
                // 比较 RGB（canvas RGBA vs 期望 RGBA）
                for px in 0..64 {
                    let i = px * 4;
                    assert_eq!(
                        (canvas[i], canvas[i + 1], canvas[i + 2]),
                        (expect[i], expect[i + 1], expect[i + 2]),
                        "帧1 画布像素 {px} 不符（dirty-rect 合成错误）"
                    );
                }
            }
            frame_idx += 1;
        }
        assert_eq!(frame_idx, 2);
    }

    fn bgra_to_rgba(v: &[u8]) -> Vec<u8> {
        let mut o = v.to_vec();
        crate::platform::screenshot::swap_rgba_bgra_in_place(&mut o);
        o
    }

    #[test]
    fn existing_file_refused_without_overwrite() {
        let frames = vec![synth_frame(8, 8, 20, false)];
        let (root, id) = build_session(8, 8, &frames, &[0]);
        let out = root.path().join("exists.gif");
        std::fs::write(&out, b"old").unwrap();
        let reader = SpoolReader::open(root.path(), &id).unwrap();
        let plan = plan_from(&frames, 8, 8, &[10]);
        let err = export_gif(
            &reader,
            &plan,
            &ExportOptions::default(),
            &out,
            &AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap_err();
        assert!(matches!(err, ExportError::FileExists(_)));
        assert_eq!(std::fs::read(&out).unwrap(), b"old", "旧文件未被破坏");
        // overwrite 后成功
        export_gif(
            &reader,
            &plan,
            &ExportOptions {
                overwrite: true,
                ..Default::default()
            },
            &out,
            &AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
    }

    #[test]
    fn cancelled_export_leaves_no_artifact() {
        let frames: Vec<Vec<u8>> = (0..40)
            .map(|i| synth_frame(64, 64, (i * 3 % 200) as u8, i % 2 == 0))
            .collect();
        let ts: Vec<u64> = (0..40).map(|i| i as u64 * 100).collect();
        let (root, id) = build_session(64, 64, &frames, &ts);
        let reader = SpoolReader::open(root.path(), &id).unwrap();
        let plan = plan_from(&frames, 64, 64, &vec![10u32; 40]);
        let out = root.path().join("cancel.gif");
        let cancel = AtomicBool::new(false);
        let err = export_gif(
            &reader,
            &plan,
            &ExportOptions::default(),
            &out,
            &cancel,
            &|_stage, done, _| {
                if done >= 5 {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            },
        )
        .unwrap_err();
        assert!(matches!(err, ExportError::Cancelled));
        assert!(!out.exists(), "取消不留伪成品");
        assert!(!out.with_extension("gif.tmp").exists(), "tmp 已清理");
    }

    #[test]
    fn max_width_scales_down_proportionally() {
        let frames = vec![synth_frame(800, 600, 25, false)];
        let (root, id) = build_session(800, 600, &frames, &[0]);
        let reader = SpoolReader::open(root.path(), &id).unwrap();
        let plan = plan_from(&frames, 800, 600, &[10]);
        let out = root.path().join("scaled.gif");
        let stats = export_gif(
            &reader,
            &plan,
            &ExportOptions {
                max_width: Some(400),
                ..Default::default()
            },
            &out,
            &AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
        assert_eq!((stats.width, stats.height), (400, 300));
    }

    #[test]
    fn color_channels_preserved_through_palette() {
        // 纯红 BGRA 帧：导出解码后必须是红不是蓝（R/B 交换回归）
        let red = vec![0u8, 0, 255, 255]; // BGRA → R=255
        let frames = vec![red.repeat(64)]; // 8x8 = 64 px
        let (root, id) = build_session(8, 8, &frames, &[0]);
        let reader = SpoolReader::open(root.path(), &id).unwrap();
        let plan = plan_from(&frames, 8, 8, &[10]);
        let out = root.path().join("red.gif");
        export_gif(
            &reader,
            &plan,
            &ExportOptions::default(),
            &out,
            &AtomicBool::new(false),
            &|_, _, _| {},
        )
        .unwrap();
        let file = BufReader::new(File::open(&out).unwrap());
        let mut opts = gif::DecodeOptions::new();
        opts.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = opts.read_info(file).unwrap();
        let frame = decoder.read_next_frame().unwrap().expect("首帧");
        let buf = &frame.buffer;
        let (r, g, b) = (buf[0], buf[1], buf[2]);
        assert!(r > 200 && g < 60 && b < 60, "应为红色，实为 R={r} G={g} B={b}");
    }

    #[test]
    fn dirty_rect_full_change_and_equal_frames() {
        let a = vec![1u8; 16];
        let b = vec![1u8; 16];
        let c = vec![2u8; 16];
        assert_eq!(dirty_rect(&a, &b, 4, 4), None, "完全相同 → None");
        assert_eq!(dirty_rect(&a, &c, 4, 4), Some((0, 0, 4, 4)), "全变 → 全帧");
        // 单点变化
        let mut d = a.clone();
        d[2 * 4 + 3] = 9; // (x=3, y=2)
        assert_eq!(dirty_rect(&a, &d, 4, 4), Some((3, 2, 1, 1)));
    }

    #[test]
    fn target_size_never_upscales() {
        assert_eq!(target_size(800, 600, Some(1000)), (800, 600));
        assert_eq!(target_size(800, 600, Some(400)), (400, 300));
        assert_eq!(target_size(801, 601, Some(400)), (400, 300));
        assert_eq!(target_size(800, 600, None), (800, 600));
    }
}
