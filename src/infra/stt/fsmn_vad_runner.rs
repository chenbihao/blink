//! FSMN-VAD ONNX 生产 runner（0.22.9 Handoff 07D）。
//!
//! 从 07C spike 的 `FsmnVadRust` 提取并生产化：
//! - fbank（kaldi-native-fbank OnlineFeature）
//! - splice（LFR_M=5 拼接）
//! - CMVN 归一化
//! - ONNX 推理（4 层 cache 增量更新）
//! - softmax → frame decision → 3-frame smoothing
//! - endpoint state machine（segment 检测）
//! - reset / cache 清空 / generation 隔离
//!
//! ## 设计铁则
//!
//! - 复用 07C 已验证的 runner 逻辑，不重新手写另一套算法
//! - ORT Session 不是 `Sync`，推理在专用 blocking executor 上执行
//! - `forward` 是同步阻塞调用（在工作线程上），不阻塞 audio callback
//! - reset 清空 feature/cache/endpoint state
//!
//! ## 07C Parity 验证
//!
//! 1363 帧 max_diff < 0.006，5x reset + reprocess 完全一致，
//! 3 trial × 3 scenario 交替完全一致。
//!
//! production gate 前此模块不被主二进制调用（auto 解析到 EnergyVad），
//! dead_code 在此是预期的。

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use kaldi_native_fbank::online::{FeatureComputer, OnlineFeature};
use kaldi_native_fbank::{FbankComputer, FbankOptions};
use ndarray::{Array2, Array3, Array4, Axis};
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Value;
use tracing::{info, warn};

// ─── 常量（匹配 07C spike + config.yaml + ONNX graph）──────────────────────

const SR: usize = 16000;
const N_MELS: usize = 80;
const VAD_FRAME_LENGTH: usize = 400; // 25ms @ 16kHz
const VAD_FRAME_SHIFT: usize = 160; // 10ms @ 16kHz
const SPLICE_LEN: usize = 5; // lfr_m from config.yaml
const VAD_CACHE_LAYERS: usize = 4; // fsmn_layers from config.yaml
const VAD_CACHE_DIM: usize = 128; // proj_dim from config.yaml
const VAD_CACHE_LORDER: usize = 19; // from ONNX graph: [1, 128, 19, 1]
const INPUT_DIM: usize = 400; // SPLICE_LEN * N_MELS

const MAX_END_SILENCE_MS: u32 = 800;
const LOOKBACK_START_MS: f64 = 200.0;
const LOOKAHEAD_END_MS: f64 = 100.0;
const FRAME_IN_MS: f64 = 10.0;

// ─── CMVN loading ─────────────────────────────────────────────────────────

/// 从 am.mvn 文件加载 CMVN means 和 vars。
///
/// 与 07C spike 和 `paraformer_runner::load_cmvn` 相同的解析逻辑。
/// FSMN-VAD 的 am.mvn 只有 80 维（N_MELS），不需要 LFR_M 扩展。
pub fn load_cmvn(path: &Path) -> Result<(Vec<f32>, Vec<f32>), String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("CMVN 读取失败: {e}"))?;
    let lines: Vec<&str> = content.lines().collect();
    let mut means = Vec::new();
    let mut vars = Vec::new();
    for i in 0..lines.len() {
        let items: Vec<&str> = lines[i].split_whitespace().collect();
        if items.is_empty() {
            continue;
        }
        if items[0] == "<AddShift>" && i + 1 < lines.len() {
            let next: Vec<&str> = lines[i + 1].split_whitespace().collect();
            if !next.is_empty() && next[0] == "<LearnRateCoef>" {
                means = next[3..next.len() - 1]
                    .iter()
                    .filter_map(|s| s.parse().ok())
                    .collect();
            }
        } else if items[0] == "<Rescale>" && i + 1 < lines.len() {
            let next: Vec<&str> = lines[i + 1].split_whitespace().collect();
            if !next.is_empty() && next[0] == "<LearnRateCoef>" {
                vars = next[3..next.len() - 1]
                    .iter()
                    .filter_map(|s| s.parse().ok())
                    .collect();
            }
        }
    }
    Ok((means, vars))
}

// ─── FsmnVadRunner ───────────────────────────────────────────────────────

/// FSMN-VAD ONNX 推理 runner。
///
/// 一次只承载一个 active stream。`reset` 清空所有 cache 和状态。
/// 由专用 blocking executor 上的工作线程调用。
pub struct FsmnVadRunner {
    session: ort::session::Session,
    means: Vec<f32>,
    vars: Vec<f32>,
    input_names: Vec<String>,
    output_names: Vec<String>,
    // State
    cache: Vec<Array4<f32>>,
    input_cache: Vec<f32>,
    segments: Vec<(f64, f64)>,
    current_start: Option<f64>,
    total_samples: usize,
    silence_frames: u32,
    in_speech: bool,
    // Fbank
    fbank: OnlineFeature,
    fbank_offset: usize,
}

/// 单次推理的产出。
#[derive(Debug, Clone)]
pub struct FsmnVadOutput {
    /// 本次 chunk 检测到的端点事件（空 = 无事件）。
    pub events: Vec<(String, f64)>,
    /// 本次推理耗时（毫秒）。
    pub inference_ms: f64,
    /// 本次产出的帧数。
    pub n_frames: usize,
}

impl FsmnVadRunner {
    /// 创建 runner——加载 ORT Session、CMVN。
    ///
    /// **ORT DLL 必须已通过 `ort::init_from` 初始化**。
    pub fn new(model_path: &Path, mvn_path: &Path) -> Result<Self, String> {
        let (means, vars) = load_cmvn(mvn_path)?;
        info!(
            means_len = means.len(),
            vars_len = vars.len(),
            "FSMN-VAD runner: CMVN loaded"
        );

        let session = ort::session::Session::builder()
            .map_err(|e| format!("ORT builder 创建失败: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level1)
            .map_err(|e| format!("设置 optimization level 失败: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| format!("设置 intra_threads 失败: {e}"))?
            .commit_from_file(model_path)
            .map_err(|e| format!("FSMN-VAD Session commit_from_file 失败: {e}"))?;

        let input_names: Vec<String> = session
            .inputs()
            .iter()
            .map(|i| i.name().to_string())
            .collect();
        let output_names: Vec<String> = session
            .outputs()
            .iter()
            .map(|o| o.name().to_string())
            .collect();
        info!(
            inputs = input_names.len(),
            outputs = output_names.len(),
            "FSMN-VAD runner: Session created"
        );

        let fbank = Self::build_fbank();
        let cache = vec![Array4::zeros((1, VAD_CACHE_DIM, VAD_CACHE_LORDER, 1)); VAD_CACHE_LAYERS];

        Ok(Self {
            session,
            means,
            vars,
            input_names,
            output_names,
            cache,
            input_cache: Vec::new(),
            segments: Vec::new(),
            current_start: None,
            total_samples: 0,
            silence_frames: 0,
            in_speech: false,
            fbank,
            fbank_offset: 0,
        })
    }

    /// 构建 kaldi-native-fbank 的 FbankOptions（FSMN-VAD 专用配置）。
    ///
    /// 与 07C spike 完全一致：snip_edges=true（Kaldi convention）。
    fn build_fbank() -> OnlineFeature {
        let mut opts = FbankOptions::default();
        opts.frame_opts.samp_freq = 16000.0;
        opts.frame_opts.frame_shift_ms = 10.0;
        opts.frame_opts.frame_length_ms = 25.0;
        opts.frame_opts.dither = 0.0;
        opts.frame_opts.preemph_coeff = 0.97;
        opts.frame_opts.remove_dc_offset = false;
        opts.frame_opts.window_type = "hamming".to_string();
        opts.frame_opts.round_to_power_of_two = true;
        opts.frame_opts.snip_edges = true; // Kaldi convention (no center padding)
        opts.mel_opts.num_bins = 80;
        opts.mel_opts.low_freq = 0.0;
        opts.mel_opts.high_freq = 0.0; // 0 = Nyquist = 8000
        opts.use_energy = false;
        opts.raw_energy = false;
        opts.use_log_fbank = true;
        opts.use_power = true;
        let computer = FbankComputer::new(opts).expect("fbank options 不变，不会失败");
        OnlineFeature::new(FeatureComputer::Fbank(computer))
    }

    /// 重置所有流状态——清空 cache、fbank offset、endpoint 状态。
    ///
    /// 新 generation 不得看到上一 generation 的任何 cache/segment。
    pub fn reset(&mut self) {
        self.cache = vec![Array4::zeros((1, VAD_CACHE_DIM, VAD_CACHE_LORDER, 1)); VAD_CACHE_LAYERS];
        self.input_cache.clear();
        self.segments.clear();
        self.current_start = None;
        self.total_samples = 0;
        self.silence_frames = 0;
        self.in_speech = false;
        let fbank_computer = FbankComputer::new(Self::default_fbank_options())
            .expect("fbank options 不变，不会失败");
        self.fbank = OnlineFeature::new(FeatureComputer::Fbank(fbank_computer));
        self.fbank_offset = 0;
    }

    /// 返回默认 fbank options（与 build_fbank 一致，用于 reset 重建）。
    fn default_fbank_options() -> FbankOptions {
        let mut opts = FbankOptions::default();
        opts.frame_opts.samp_freq = 16000.0;
        opts.frame_opts.frame_shift_ms = 10.0;
        opts.frame_opts.frame_length_ms = 25.0;
        opts.frame_opts.dither = 0.0;
        opts.frame_opts.preemph_coeff = 0.97;
        opts.frame_opts.remove_dc_offset = false;
        opts.frame_opts.window_type = "hamming".to_string();
        opts.frame_opts.round_to_power_of_two = true;
        opts.frame_opts.snip_edges = true;
        opts.mel_opts.num_bins = 80;
        opts.mel_opts.low_freq = 0.0;
        opts.mel_opts.high_freq = 0.0;
        opts.use_energy = false;
        opts.raw_energy = false;
        opts.use_log_fbank = true;
        opts.use_power = true;
        opts
    }

    /// 处理一段 PCM 音频样本，返回端点事件和推理耗时。
    ///
    /// 与 07C spike 的 `process` 方法逻辑完全一致。
    /// `is_final` 为 true 时进行 final flush。
    pub fn forward(&mut self, samples: &[f32], is_final: bool) -> FsmnVadOutput {
        let mut events = Vec::new();

        // Accumulate with leftover
        let mut s = std::mem::take(&mut self.input_cache);
        s.extend_from_slice(samples);

        if s.len() < VAD_FRAME_LENGTH {
            self.input_cache = s;
            // final flush: 如果 in_speech，闭合未关闭的 segment
            if is_final && self.in_speech {
                if let Some(start) = self.current_start {
                    let end_t = self.total_samples as f64 / SR as f64;
                    self.segments.push((start, end_t));
                    events.push(("end".to_string(), end_t));
                }
                self.in_speech = false;
                self.current_start = None;
            }
            return FsmnVadOutput {
                events,
                inference_ms: 0.0,
                n_frames: 0,
            };
        }

        let nf = (s.len() - VAD_FRAME_LENGTH) / VAD_FRAME_SHIFT + 1;
        if nf < 1 {
            self.input_cache = s;
            return FsmnVadOutput {
                events,
                inference_ms: 0.0,
                n_frames: 0,
            };
        }

        let us = (nf - 1) * VAD_FRAME_SHIFT + VAD_FRAME_LENGTH;
        let wav_data: Vec<f32> = s[..us].to_vec();
        self.input_cache = s[us..].to_vec();

        // Fbank
        let fb = self.compute_fbank(&wav_data);
        if fb.nrows() < SPLICE_LEN {
            if is_final && self.in_speech {
                if let Some(start) = self.current_start {
                    let end_t = self.total_samples as f64 / SR as f64;
                    self.segments.push((start, end_t));
                    events.push(("end".to_string(), end_t));
                }
                self.in_speech = false;
                self.current_start = None;
            }
            return FsmnVadOutput {
                events,
                inference_ms: 0.0,
                n_frames: 0,
            };
        }

        // Splice
        let sp = self.splice(&fb);
        if sp.nrows() == 0 {
            if is_final && self.in_speech {
                if let Some(start) = self.current_start {
                    let end_t = self.total_samples as f64 / SR as f64;
                    self.segments.push((start, end_t));
                    events.push(("end".to_string(), end_t));
                }
                self.in_speech = false;
                self.current_start = None;
            }
            return FsmnVadOutput {
                events,
                inference_ms: 0.0,
                n_frames: 0,
            };
        }

        // CMVN
        let ft = self.apply_cmvn(&sp);
        let sp_in = ft.insert_axis(Axis(0)); // (1, T, 400)

        // Build ORT inputs
        let speech_val = match Value::from_array(sp_in.clone()) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "FSMN-VAD: speech Value 构造失败");
                return FsmnVadOutput {
                    events,
                    inference_ms: 0.0,
                    n_frames: 0,
                };
            }
        };

        let mut feed_pairs: Vec<(String, Value)> = Vec::new();
        feed_pairs.push((self.input_names[0].clone(), Value::from(speech_val)));
        for i in 0..VAD_CACHE_LAYERS {
            let cache_val = match Value::from_array(self.cache[i].clone()) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "FSMN-VAD: cache[{i}] Value 构造失败");
                    return FsmnVadOutput {
                        events,
                        inference_ms: 0.0,
                        n_frames: 0,
                    };
                }
            };
            feed_pairs.push((self.input_names[1 + i].clone(), Value::from(cache_val)));
        }
        let inputs = ort::session::SessionInputs::from(feed_pairs);

        // Run inference
        let t0 = std::time::Instant::now();
        let outputs = match self.session.run(inputs) {
            Ok(o) => o,
            Err(e) => {
                warn!(error = %e, "FSMN-VAD: ORT run 失败");
                return FsmnVadOutput {
                    events,
                    inference_ms: t0.elapsed().as_secs_f64() * 1000.0,
                    n_frames: 0,
                };
            }
        };
        let inf_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // Extract outputs
        let (logits, new_caches) = {
            let logits: Array3<f32> = match outputs[0]
                .try_extract_array::<f32>()
                .map(|a| a.view().to_owned().into_dimensionality::<ndarray::Ix3>())
            {
                Ok(Ok(a)) => a,
                _ => {
                    warn!("FSMN-VAD: logits 提取失败");
                    return FsmnVadOutput {
                        events,
                        inference_ms: inf_ms,
                        n_frames: 0,
                    };
                }
            };
            let mut caches = Vec::with_capacity(VAD_CACHE_LAYERS);
            for l in 0..VAD_CACHE_LAYERS {
                match outputs[1 + l]
                    .try_extract_array::<f32>()
                    .map(|a| a.view().to_owned().into_dimensionality::<ndarray::Ix4>())
                {
                    Ok(Ok(a)) => caches.push(a),
                    _ => {
                        warn!("FSMN-VAD: cache[{l}] 提取失败");
                        return FsmnVadOutput {
                            events,
                            inference_ms: inf_ms,
                            n_frames: 0,
                        };
                    }
                }
            }
            (logits, caches)
        };

        // Update cache
        self.cache = new_caches;

        // Frame-level scores
        let logp = logits.index_axis(Axis(0), 0); // (T, output_dim)
        let n_frames = logp.nrows();
        let output_dim = logp.ncols();

        let mut frame_scores = Vec::with_capacity(n_frames);
        let mut frame_decisions = Vec::with_capacity(n_frames);

        for i in 0..n_frames {
            // Softmax
            let mut max_val = f32::MIN;
            for j in 0..output_dim {
                let v = logp[[i, j]];
                if v > max_val {
                    max_val = v;
                }
            }
            let mut sum = 0.0f32;
            let mut probs = vec![0.0f32; output_dim];
            for (j, prob) in probs.iter_mut().enumerate().take(output_dim) {
                *prob = (logp[[i, j]] - max_val).exp();
                sum += *prob;
            }
            for prob in probs.iter_mut().take(output_dim) {
                *prob /= sum;
            }
            // speech_prob = 1 - silence_prob (silence_pdf_ids = [0])
            let spp = 1.0 - probs[0];
            let dec = if spp > 0.5 { 1 } else { 0 };
            frame_scores.push(spp);
            frame_decisions.push(dec);
        }

        // Smoothing (3-frame majority)
        let sm = {
            let mut sm = frame_decisions.clone();
            for i in 1..n_frames.saturating_sub(1) {
                let sum: i32 = frame_decisions[i - 1] + frame_decisions[i] + frame_decisions[i + 1];
                sm[i] = if sum >= 2 { 1 } else { 0 };
            }
            sm
        };

        // Endpoint state machine
        let frame_shift_s = VAD_FRAME_SHIFT as f64 / SR as f64;
        let chunk_start_s = self.total_samples as f64 / SR as f64;

        for (fi, &is_speech) in sm.iter().enumerate() {
            let ft_s = chunk_start_s + fi as f64 * frame_shift_s;

            if is_speech == 1 {
                self.silence_frames = 0;
                if !self.in_speech {
                    self.in_speech = true;
                    self.current_start = Some((ft_s - LOOKBACK_START_MS / 1000.0).max(0.0));
                }
            } else {
                self.silence_frames += 1;
                if self.in_speech {
                    let sil_ms = self.silence_frames as f64 * FRAME_IN_MS;
                    if sil_ms >= MAX_END_SILENCE_MS as f64 {
                        let end_t = ft_s + LOOKAHEAD_END_MS / 1000.0;
                        if let Some(start) = self.current_start {
                            self.segments.push((start, end_t));
                            events.push(("end".to_string(), end_t));
                        }
                        self.in_speech = false;
                        self.current_start = None;
                        self.silence_frames = 0;
                    }
                }
            }
        }

        self.total_samples += us;

        // Final flush
        if is_final && self.in_speech {
            if let Some(start) = self.current_start {
                let end_t = self.total_samples as f64 / SR as f64;
                self.segments.push((start, end_t));
                events.push(("end".to_string(), end_t));
            }
            self.in_speech = false;
            self.current_start = None;
        }

        FsmnVadOutput {
            events,
            inference_ms: inf_ms,
            n_frames,
        }
    }

    // ── 内部方法（与 07C spike 逻辑一致）──────────────────────────────

    fn compute_fbank(&mut self, samples: &[f32]) -> Array2<f32> {
        let scaled: Vec<f32> = samples.iter().map(|s| s * 32768.0).collect();
        self.fbank.accept_waveform(16000.0, &scaled);
        let total = self.fbank.num_frames_ready();
        let new_frames = total - self.fbank_offset;
        if new_frames == 0 {
            return Array2::zeros((0, N_MELS));
        }
        let mut fbank = Array2::zeros((new_frames, N_MELS));
        for i in 0..new_frames {
            let idx = self.fbank_offset + i;
            if let Some(frame) = self.fbank.get_frame(idx) {
                for j in 0..N_MELS.min(frame.len()) {
                    fbank[[i, j]] = frame[j];
                }
            }
        }
        self.fbank_offset = total;
        fbank
    }

    fn splice(&self, f: &Array2<f32>) -> Array2<f32> {
        let n_frames = f.nrows();
        if n_frames < SPLICE_LEN {
            return Array2::zeros((0, INPUT_DIM));
        }
        let n_spliced = n_frames - SPLICE_LEN + 1;
        let mut out = Array2::zeros((n_spliced, INPUT_DIM));
        for i in 0..n_spliced {
            for j in 0..SPLICE_LEN {
                for k in 0..N_MELS {
                    out[[i, j * N_MELS + k]] = f[[i + j, k]];
                }
            }
        }
        out
    }

    fn apply_cmvn(&self, f: &Array2<f32>) -> Array2<f32> {
        let mut out = f.clone();
        for i in 0..out.nrows() {
            for j in 0..INPUT_DIM.min(self.means.len()) {
                out[[i, j]] = (out[[i, j]] + self.means[j]) * self.vars[j];
            }
        }
        out
    }

    /// 获取已检测的 segments（诊断用）。
    #[allow(dead_code)]
    pub fn segments(&self) -> &[(f64, f64)] {
        &self.segments
    }
}

/// FSMN-VAD runner 的构建配置。
///
/// 资产路径由 deployment 系统提供，不在 runner 内部做 asset-lock 校验。
#[derive(Debug, Clone)]
pub struct FsmnVadRunnerConfig {
    /// ONNX 模型文件路径（model_quant.onnx）。
    pub model_path: PathBuf,
    /// am.mvn 文件路径（CMVN 归一化参数）。
    pub mvn_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_config_fields_present() {
        let config = FsmnVadRunnerConfig {
            model_path: PathBuf::from("model_quant.onnx"),
            mvn_path: PathBuf::from("am.mvn"),
        };
        assert!(!config.model_path.exists()); // just check struct
        assert!(!config.model_path.as_os_str().is_empty());
    }

    #[test]
    fn load_cmvn_missing_file_returns_err() {
        let result = load_cmvn(Path::new("nonexistent.mvn"));
        assert!(result.is_err());
    }

    /// 验证 CMVN 解析逻辑——使用合成 am.mvn 内容。
    #[test]
    fn load_cmvn_parses_synthetic() {
        let content = "<AddShift> 80 80
<LearnRateCoef> 0 [ -0.1 -0.2 -0.3 ]
<Rescale> 80 80
<LearnRateCoef> 1 [ 0.5 0.6 0.7 ]";
        let dir = std::env::temp_dir();
        let path = dir.join("test_fsmn_cmvn.mvn");
        std::fs::write(&path, content).unwrap();

        let (means, vars) = load_cmvn(&path).unwrap();
        assert_eq!(means, vec![-0.1, -0.2, -0.3]);
        assert_eq!(vars, vec![0.5, 0.6, 0.7]);

        let _ = std::fs::remove_file(&path);
    }
}
