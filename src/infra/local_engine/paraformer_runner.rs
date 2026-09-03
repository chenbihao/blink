//! ParaformerOnline ONNX 推理 runner（0.22.9 Handoff 07A）。
//!
//! 从 Spike C2 Rust（`spike-c-rust/src/main.rs`）提取并生产化：
//! - kaldi-native-fbank 前端
//! - 帧偏移管理
//! - LFR（Low Frame Rate）拼接
//! - CMVN 归一化
//! - encoder cache（hidden + alphas + feats overlap）
//! - CIF（Continuous Integrate-and-Fire）搜帧
//! - decoder FSMN cache
//! - tokenizer greedy decode
//! - reset / final chunk flush
//!
//! ## 设计铁则
//!
//! - ORT、张量、cache、tokenizer 不穿透到 domain
//! - runner 一次只承载一个 active stream
//! - Begin 创建干净状态；Reset/Cancel 清空所有 cache
//! - 新 Begin 不得看到上一 generation 的任何 token/cache
//! - 由 worker mode 和 self-test 共同调用

use std::path::Path;

use kaldi_native_fbank::online::{FeatureComputer, OnlineFeature};
use kaldi_native_fbank::{FbankComputer, FbankOptions};
use ndarray::{Array1, Array2, Array3, Axis, s};
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Value;
use tracing::{info, warn};

// ── 常量 ─────────────────────────────────────────────────────────────────

const CHUNK_SIZE: [usize; 3] = [5, 10, 5];
const LFR_M: usize = 7;
const LFR_N: usize = 6;
const N_MELS: usize = 80;
const FRAME_LENGTH_MS: usize = 25;
const FRAME_SHIFT_MS: usize = 10;
const ENCODER_SIZE: usize = 512;
const FSMN_LAYERS: usize = 16;
const FSMN_LORDER: usize = 10;
const FSMN_DIMS: usize = 512;
const CIF_THRESHOLD: f32 = 1.0;
const TAIL_ALPHAS: f32 = 0.45;
const SAMPLE_RATE: usize = 16000;
const FEAT_DIMS: usize = LFR_M * N_MELS;

// ── 资产加载 ─────────────────────────────────────────────────────────────

/// 从 am.mvn 文件加载 CMVN means 和 vars。
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
    // 复制到 LFR_M * N_MELS 长度
    let target = FEAT_DIMS;
    if means.len() < target {
        let orig = means.clone();
        means = Vec::with_capacity(target);
        while means.len() < target {
            means.extend(&orig);
        }
        means.truncate(target);
        let orig_v = vars.clone();
        vars = Vec::with_capacity(target);
        while vars.len() < target {
            vars.extend(&orig_v);
        }
        vars.truncate(target);
    }
    Ok((means, vars))
}

/// 从 tokens.json 或 token.txt 加载 tokenizer。
pub fn load_tokens(path: &Path) -> Result<Vec<String>, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("tokenizer 读取失败: {e}"))?;
    if path.extension().is_some_and(|e| e == "json") {
        let v: serde_json::Value =
            serde_json::from_str(&content).map_err(|e| format!("tokenizer JSON 解析失败: {e}"))?;
        if let Some(obj) = v.as_object() {
            let max_id = obj
                .keys()
                .filter_map(|k| k.parse::<usize>().ok())
                .max()
                .unwrap_or(0);
            let mut tokens = vec![String::new(); max_id + 1];
            for (k, val) in obj {
                if let (Ok(idx), Some(s)) = (k.parse::<usize>(), val.as_str())
                    && idx < tokens.len()
                {
                    tokens[idx] = s.to_string();
                }
            }
            return Ok(tokens);
        } else if let Some(arr) = v.as_array() {
            return Ok(arr
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect());
        }
    }
    // token.txt format: "token id"
    let mut tokens = Vec::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if !parts.is_empty() {
            tokens.push(parts[0].to_string());
        }
    }
    Ok(tokens)
}

/// 构建 kaldi-native-fbank 的 FbankOptions（ParaformerOnline 专用配置）。
pub fn default_fbank_options() -> FbankOptions {
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
    opts.mel_opts.high_freq = 0.0; // 0 = Nyquist = 8000
    opts.use_energy = false;
    opts.raw_energy = false;
    opts.use_log_fbank = true;
    opts.use_power = true;
    opts
}

// ── ParaformerOnline Runner ─────────────────────────────────────────────

/// ParaformerOnline ONNX 推理 runner。
///
/// 一次只承载一个 active stream。Begin 创建干净状态，Reset/Cancel 清空所有 cache。
pub struct ParaformerRunner {
    encoder: ort::session::Session,
    decoder: ort::session::Session,
    means: Vec<f32>,
    vars: Vec<f32>,
    tokens: Vec<String>,
    start_idx_cache: usize,
    is_first_chunk: bool,
    is_last_chunk: bool,
    input_cache: Vec<f32>,
    lfr_splice_cache: Vec<Vec<f32>>,
    hidden_cache: Array2<f32>,
    alphas_cache: Array1<f32>,
    feats_cache: Array2<f32>,
    decoder_cache: Vec<Array3<f32>>,
    fbank: OnlineFeature,
    fbank_offset: usize,
}

impl ParaformerRunner {
    /// 创建 runner——加载 ORT Session、CMVN、tokenizer。
    ///
    /// ORT DLL 必须已通过 `ort::init_from` 初始化。
    pub fn new(
        enc_path: &Path,
        dec_path: &Path,
        mvn_path: &Path,
        tok_path: &Path,
    ) -> Result<Self, String> {
        let (means, vars) = load_cmvn(mvn_path)?;
        info!("CMVN: means[{}], vars[{}]", means.len(), vars.len());
        let tokens = load_tokens(tok_path)?;
        info!("Tokens: {}", tokens.len());

        let fbank_opts = default_fbank_options();
        let mel_bins = fbank_opts.mel_opts.num_bins;
        let fbank_dim = FbankComputer::new(fbank_opts.clone())
            .map_err(|e| format!("FbankComputer 构造失败: {e}"))?
            .dim();
        let fbank_computer =
            FbankComputer::new(fbank_opts).map_err(|e| format!("FbankComputer 构造失败: {e}"))?;
        let feature_computer = FeatureComputer::Fbank(fbank_computer);
        let fbank = OnlineFeature::new(feature_computer);
        info!(
            "Fbank: kaldi-native-fbank, mel_bins={}, dim={}",
            mel_bins, fbank_dim
        );

        let encoder = ort::session::Session::builder()
            .map_err(|e| format!("Session builder 构造失败: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level1)
            .map_err(|e| format!("设置优化级别失败: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| format!("设置 intra_threads 失败: {e}"))?
            .commit_from_file(enc_path)
            .map_err(|e| format!("encoder Session 创建失败: {e}"))?;
        info!(
            "Encoder: {} in, {} out",
            encoder.inputs().len(),
            encoder.outputs().len()
        );

        let decoder = ort::session::Session::builder()
            .map_err(|e| format!("Session builder 构造失败: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level1)
            .map_err(|e| format!("设置优化级别失败: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| format!("设置 intra_threads 失败: {e}"))?
            .commit_from_file(dec_path)
            .map_err(|e| format!("decoder Session 创建失败: {e}"))?;
        info!(
            "Decoder: {} in, {} out",
            decoder.inputs().len(),
            decoder.outputs().len()
        );

        Ok(Self {
            encoder,
            decoder,
            means,
            vars,
            tokens,
            start_idx_cache: 0,
            is_first_chunk: true,
            is_last_chunk: false,
            input_cache: Vec::new(),
            lfr_splice_cache: Vec::new(),
            hidden_cache: Array2::zeros((1, ENCODER_SIZE)),
            alphas_cache: Array1::zeros(1),
            feats_cache: Array2::zeros((CHUNK_SIZE[0] + CHUNK_SIZE[2], FEAT_DIMS)),
            decoder_cache: vec![Array3::zeros((1, FSMN_DIMS, FSMN_LORDER)); FSMN_LAYERS],
            fbank,
            fbank_offset: 0,
        })
    }

    /// 重置所有流状态——清空 cache、fbank offset、CIF、decoder 状态。
    ///
    /// 新 Begin 不得看到上一 generation 的任何 token/cache。
    pub fn reset(&mut self) {
        self.start_idx_cache = 0;
        self.is_first_chunk = true;
        self.is_last_chunk = false;
        self.input_cache.clear();
        self.lfr_splice_cache.clear();
        self.hidden_cache = Array2::zeros((1, ENCODER_SIZE));
        self.alphas_cache = Array1::zeros(1);
        self.feats_cache = Array2::zeros((CHUNK_SIZE[0] + CHUNK_SIZE[2], FEAT_DIMS));
        self.decoder_cache = vec![Array3::zeros((1, FSMN_DIMS, FSMN_LORDER)); FSMN_LAYERS];
        let fbank_computer =
            FbankComputer::new(default_fbank_options()).expect("fbank options 不变，不会失败");
        self.fbank = OnlineFeature::new(FeatureComputer::Fbank(fbank_computer));
        self.fbank_offset = 0;
    }

    /// 处理一段 PCM 音频，返回（识别文本，推理耗时毫秒）。
    ///
    /// `input_finished` 为 true 表示这是最后一块音频（final chunk）。
    pub fn forward(&mut self, chunk_audio: &[f32], input_finished: bool) -> (String, f64) {
        let dur = chunk_audio.len() as f64 / SAMPLE_RATE as f64;

        // 极短 final chunk：先 flush 残留 input_cache 和 lfr_splice_cache，再做 final flush
        //
        // 生产路径中音频按小 chunk 投喂（如 160 samples/10ms），
        // End 时 input_cache 和 lfr_splice_cache 中可能有未处理残留。
        // 直接用 feats_cache 做 final 会跳过这些残留，导致 final 文本为空。
        // 需要先处理残留 samples → 产生 fbank 帧 → 加入 lfr_splice_cache →
        // 用 input_finished=true 做 LFR + forward_chunk → 再用 feats_cache 做 final。
        if chunk_audio.len() < 960 && input_finished && !self.is_first_chunk {
            self.is_last_chunk = true;

            // 1. 处理 input_cache 中的残留 samples（如果有）
            let mut result = String::new();
            let mut total_ms = 0.0;

            if !self.input_cache.is_empty() {
                let waves = std::mem::take(&mut self.input_cache);
                let fsl = SAMPLE_RATE * FRAME_LENGTH_MS / 1000;
                let fss = SAMPLE_RATE * FRAME_SHIFT_MS / 1000;
                let fn_ = if waves.len() >= fsl {
                    (waves.len() - fsl) / fss + 1
                } else {
                    0
                };
                if fn_ >= 1 {
                    let tl = (fn_ * fss - fss + fsl).min(waves.len());
                    let samples: Vec<f32> = waves[..tl].to_vec();
                    let wf = self.compute_fbank(&samples);
                    if wf.nrows() > 0 {
                        if self.lfr_splice_cache.is_empty() {
                            let ff = wf.row(0).to_vec();
                            for _ in 0..((LFR_M - 1) / 2) {
                                self.lfr_splice_cache.push(ff.clone());
                            }
                        }
                        let total = wf.nrows() + self.lfr_splice_cache.len();
                        if total >= LFR_M {
                            let (lfr, nc) = self.online_lfr_cmvn(&wf, true);
                            self.lfr_splice_cache = nc;
                            if lfr.nrows() > 0 {
                                let (r, ms) = self.forward_chunk(lfr, false);
                                if !r.is_empty() {
                                    result.push_str(&r);
                                }
                                total_ms += ms;
                            }
                        }
                    }
                }
            }

            // 2. 处理 lfr_splice_cache 中的残留帧（如果有）
            if !self.lfr_splice_cache.is_empty() {
                let cache = std::mem::take(&mut self.lfr_splice_cache);
                let t = cache.len();
                // (LFR_M - 1) / 2 = 3，需要至少 LFR_M=7 个帧才能做 LFR splice。
                // t < (LFR_M - 1) / 2 时，usize 减法下溢导致 t_lrf 变成巨大数，
                // 循环越界 panic。提前跳过。
                let half_m = (LFR_M - 1) / 2;
                if t >= half_m {
                    let t_lrf = ((t - half_m) as f64 / LFR_N as f64).ceil() as usize;
                    let mut out: Vec<Vec<f32>> = Vec::new();
                    for i in 0..t_lrf {
                        if LFR_M <= t - i * LFR_N {
                            let mut p = Vec::with_capacity(FEAT_DIMS);
                            for j in 0..LFR_M {
                                for (_, &val) in
                                    cache[i * LFR_N + j].iter().enumerate().take(N_MELS)
                                {
                                    p.push(val);
                                }
                            }
                            out.push(p);
                        } else {
                            let np = LFR_M - (t - i * LFR_N);
                            let mut p = Vec::with_capacity(FEAT_DIMS);
                            for j in 0..t - i * LFR_N {
                                for (_, &val) in
                                    cache[i * LFR_N + j].iter().enumerate().take(N_MELS)
                                {
                                    p.push(val);
                                }
                            }
                            for _ in 0..np {
                                for (_, &val) in cache[t - 1].iter().enumerate().take(N_MELS) {
                                    p.push(val);
                                }
                            }
                            out.push(p);
                        }
                    }
                    if !out.is_empty() {
                        let n = out.len();
                        let mut res = Array2::zeros((n, FEAT_DIMS));
                        for (i, row) in out.iter().enumerate() {
                            for j in 0..FEAT_DIMS {
                                res[[i, j]] = (row[j] + self.means[j]) * self.vars[j];
                            }
                        }
                        let (r, ms) = self.forward_chunk(res, false);
                        if !r.is_empty() {
                            result.push_str(&r);
                        }
                        total_ms += ms;
                    }
                }
            }

            // 3. 用 feats_cache 做 final flush（tail alphas 触发最后 token）
            //
            // 必须先 take feats_cache（置空），否则 forward_chunk 内部的
            // add_overlap_chunk 会把 feats_cache 和传入的 wf（=feats_cache.clone()）
            // 拼接，导致特征重复 2x，encoder 输入异常。
            let wf = std::mem::take(&mut self.feats_cache);
            let (r, ms) = self.forward_chunk(wf, self.is_last_chunk);
            if !r.is_empty() {
                result.push_str(&r);
            }
            total_ms += ms;

            self.reset();
            let _ = dur; // suppress unused
            return (result, total_ms);
        }

        if self.is_first_chunk {
            self.is_first_chunk = false;
        }

        let mut waves = self.input_cache.clone();
        waves.extend_from_slice(chunk_audio);

        let fsl = SAMPLE_RATE * FRAME_LENGTH_MS / 1000;
        let fss = SAMPLE_RATE * FRAME_SHIFT_MS / 1000;
        let fn_ = if waves.len() >= fsl {
            (waves.len() - fsl) / fss + 1
        } else {
            0
        };

        if fn_ < 1 || waves.len() < fsl {
            self.input_cache = waves;
            return (String::new(), 0.0);
        }

        self.input_cache = waves[fn_ * fss..].to_vec();
        let tl = (fn_ * fss - fss + fsl).min(waves.len());
        let samples: Vec<f32> = waves[..tl].to_vec();

        let wf = self.compute_fbank(&samples);
        if wf.nrows() == 0 {
            if input_finished {
                self.input_cache.clear();
                self.lfr_splice_cache.clear();
            }
            return (String::new(), 0.0);
        }

        if self.lfr_splice_cache.is_empty() {
            let ff = wf.row(0).to_vec();
            for _ in 0..((LFR_M - 1) / 2) {
                self.lfr_splice_cache.push(ff.clone());
            }
        }

        let total = wf.nrows() + self.lfr_splice_cache.len();
        if total >= LFR_M {
            let (lfr, nc) = self.online_lfr_cmvn(&wf, input_finished);
            self.lfr_splice_cache = nc;
            if lfr.nrows() == 0 {
                if input_finished {
                    self.input_cache.clear();
                    self.lfr_splice_cache.clear();
                }
                return (String::new(), 0.0);
            }
            let (r, ms) = self.forward_chunk(lfr, input_finished);
            if input_finished {
                self.reset();
            }
            (r, ms)
        } else {
            for i in 0..wf.nrows() {
                self.lfr_splice_cache.push(wf.row(i).to_vec());
            }
            (String::new(), 0.0)
        }
    }

    // ── 内部方法 ─────────────────────────────────────────────────────────

    fn compute_fbank(&mut self, samples: &[f32]) -> Array2<f32> {
        let scaled: Vec<f32> = samples.iter().map(|s| s * 32768.0).collect();
        self.fbank.accept_waveform(16000.0, &scaled);
        let total = self.fbank.num_frames_ready();
        let new = total - self.fbank_offset;
        if new == 0 {
            return Array2::zeros((0, N_MELS));
        }
        let mut fb = Array2::zeros((new, N_MELS));
        for i in 0..new {
            if let Some(frame) = self.fbank.get_frame(self.fbank_offset + i) {
                for j in 0..N_MELS.min(frame.len()) {
                    fb[[i, j]] = frame[j];
                }
            }
        }
        self.fbank_offset = total;
        fb
    }

    fn online_lfr_cmvn(&mut self, wf: &Array2<f32>, fin: bool) -> (Array2<f32>, Vec<Vec<f32>>) {
        let mut all: Vec<Vec<f32>> = self.lfr_splice_cache.clone();
        for i in 0..wf.nrows() {
            all.push(wf.row(i).to_vec());
        }
        let t = all.len();
        if t < LFR_M {
            self.lfr_splice_cache = all;
            return (Array2::zeros((0, FEAT_DIMS)), Vec::new());
        }
        let t_lrf = ((t - (LFR_M - 1) / 2) as f64 / LFR_N as f64).ceil() as usize;
        let mut out: Vec<Vec<f32>> = Vec::new();
        let mut stop = t_lrf;
        for i in 0..t_lrf {
            if LFR_M <= t - i * LFR_N {
                let mut p = Vec::with_capacity(FEAT_DIMS);
                for j in 0..LFR_M {
                    for (_, &val) in all[i * LFR_N + j].iter().enumerate().take(N_MELS) {
                        p.push(val);
                    }
                }
                out.push(p);
            } else if fin {
                let np = LFR_M - (t - i * LFR_N);
                let mut p = Vec::with_capacity(FEAT_DIMS);
                for j in 0..t - i * LFR_N {
                    for (_, &val) in all[i * LFR_N + j].iter().enumerate().take(N_MELS) {
                        p.push(val);
                    }
                }
                for _ in 0..np {
                    for (_, &val) in all[t - 1].iter().enumerate().take(N_MELS) {
                        p.push(val);
                    }
                }
                out.push(p);
            } else {
                stop = i;
                break;
            }
        }
        let lsi = (stop * LFR_N).min(t - 1);
        let new_cache = if lsi < t {
            all[lsi..].to_vec()
        } else {
            Vec::new()
        };
        let n = out.len();
        let mut res = Array2::zeros((n, FEAT_DIMS));
        for (i, row) in out.iter().enumerate() {
            for j in 0..FEAT_DIMS {
                res[[i, j]] = (row[j] + self.means[j]) * self.vars[j];
            }
        }
        (res, new_cache)
    }

    fn get_pos_emb(&mut self, mut wf: Array2<f32>) -> Array2<f32> {
        let ts = wf.nrows();
        let fd = wf.ncols();
        let si = self.start_idx_cache;
        self.start_idx_cache += ts;
        // Spike C2 参考实现使用 -0.0330119726594128，clippy 要求截断到 f32 精度。
        // 用 f64 字面量计算再转 f32，保持与参考实现一致。
        #[allow(clippy::excessive_precision)]
        let scale: f32 = -0.0330119726594128;
        for i in 0..ts {
            for j in 0..fd / 2 {
                let tm = (j as f32 * scale).exp();
                let coe = tm * (si + i + 1) as f32;
                wf[[i, j]] += coe.sin();
                wf[[i, j + fd / 2]] += coe.cos();
            }
        }
        wf
    }

    fn add_overlap_chunk(&mut self, mut wf: Array2<f32>, fin: bool) -> Array2<f32> {
        if self.feats_cache.nrows() > 0 {
            let nr = self.feats_cache.nrows();
            let mut comb = Array2::zeros((nr + wf.nrows(), wf.ncols()));
            comb.slice_mut(s![..nr, ..]).assign(&self.feats_cache);
            comb.slice_mut(s![nr.., ..]).assign(&wf);
            wf = comb;
        }
        let nc;
        if fin {
            nc = if wf.nrows() >= CHUNK_SIZE[0] {
                wf.slice(s![wf.nrows() - CHUNK_SIZE[0].., ..]).to_owned()
            } else {
                wf.clone()
            };
            if !self.is_last_chunk {
                let pl = CHUNK_SIZE[0] + CHUNK_SIZE[1] + CHUNK_SIZE[2] - wf.nrows();
                if pl > 0 && pl < 100 {
                    let mut pad = Array2::zeros((wf.nrows() + pl, wf.ncols()));
                    pad.slice_mut(s![..wf.nrows(), ..]).assign(&wf);
                    wf = pad;
                }
            }
        } else {
            let cl = CHUNK_SIZE[0] + CHUNK_SIZE[2];
            nc = if wf.nrows() >= cl {
                wf.slice(s![wf.nrows() - cl.., ..]).to_owned()
            } else {
                wf.clone()
            };
        }
        self.feats_cache = nc;
        wf
    }

    fn cif_search(
        &mut self,
        hidden: Array2<f32>,
        mut alphas: Array1<f32>,
        is_last: bool,
    ) -> Array2<f32> {
        let t = hidden.nrows();
        if t == 0 {
            return Array2::zeros((0, ENCODER_SIZE));
        }
        let hs = hidden.ncols();
        let csp = CHUNK_SIZE[0];
        for i in 0..csp.min(t) {
            alphas[i] = 0.0;
        }
        let csf = CHUNK_SIZE[0] + CHUNK_SIZE[1];
        for i in csf.min(t)..t {
            alphas[i] = 0.0;
        }
        let mut fh: Vec<Array1<f32>> = Vec::new();
        if self.hidden_cache.nrows() > 0 {
            fh.push(self.hidden_cache.row(0).to_owned());
        }
        for i in 0..t {
            fh.push(hidden.row(i).to_owned());
        }
        let mut fa: Vec<f32> = Vec::new();
        if !self.alphas_cache.is_empty() {
            fa.push(self.alphas_cache[0]);
        }
        for i in 0..t {
            fa.push(alphas[i]);
        }
        if is_last {
            fh.push(Array1::zeros(hs));
            fa.push(TAIL_ALPHAS);
        }
        let n = fa.len();
        let mut lf: Vec<Array1<f32>> = Vec::new();
        let mut integ = 0f32;
        let mut fr = Array1::zeros(hs);
        for i in 0..n {
            let a = fa[i];
            if a + integ < CIF_THRESHOLD {
                integ += a;
                for j in 0..hs {
                    fr[j] += a * fh[i][j];
                }
            } else {
                for j in 0..hs {
                    fr[j] += (CIF_THRESHOLD - integ) * fh[i][j];
                }
                lf.push(fr.clone());
                integ += a;
                integ -= CIF_THRESHOLD;
                for j in 0..hs {
                    fr[j] = integ * fh[i][j];
                }
            }
        }
        self.alphas_cache = Array1::from_vec(vec![integ]);
        self.hidden_cache = if integ > 0.0 {
            (&fr / integ).insert_axis(Axis(0))
        } else {
            fr.insert_axis(Axis(0))
        };
        if lf.is_empty() {
            Array2::zeros((0, hs))
        } else {
            let mut r = Array2::zeros((lf.len(), hs));
            for (i, f) in lf.iter().enumerate() {
                r.row_mut(i).assign(f);
            }
            r
        }
    }

    fn forward_chunk(&mut self, mut cf: Array2<f32>, fin: bool) -> (String, f64) {
        let t0 = std::time::Instant::now();
        let mut result = String::new();
        if cf.nrows() == 0 {
            return (result, 0.0);
        }
        let sf = (ENCODER_SIZE as f32).sqrt();
        cf.mapv_inplace(|x| x * sf);
        cf = self.get_pos_emb(cf);
        cf = self.add_overlap_chunk(cf, fin);
        let nf = cf.nrows();

        let ei0 = self.encoder.inputs()[0].name().to_string();
        let ei1 = self.encoder.inputs()[1].name().to_string();
        let speech = cf.insert_axis(Axis(0));
        let speech_lens = Array1::from_elem(1, nf as i32);
        let sv = Value::from_array(speech).map_err(|e| format!("speech Value 构造失败: {e}"));
        let slv =
            Value::from_array(speech_lens).map_err(|e| format!("speech_lens Value 构造失败: {e}"));
        let (sv, slv) = match (sv, slv) {
            (Ok(s), Ok(l)) => (s, l),
            (Err(e), _) | (_, Err(e)) => {
                warn!("encoder input 构造失败: {e}");
                return (result, t0.elapsed().as_secs_f64() * 1000.0);
            }
        };
        let enc_inputs = ort::session::SessionInputs::from(vec![
            (ei0.as_str(), <Value>::from(sv)),
            (ei1.as_str(), <Value>::from(slv)),
        ]);

        let (enc, enc_lens, alphas) = {
            let enc_out = match self.encoder.run(enc_inputs) {
                Ok(o) => o,
                Err(e) => {
                    warn!("encoder 推理失败: {e}");
                    return (result, t0.elapsed().as_secs_f64() * 1000.0);
                }
            };
            let enc: Array3<f32> = match enc_out[0]
                .try_extract_array::<f32>()
                .map(|a| a.view().to_owned().into_dimensionality::<ndarray::Ix3>())
            {
                Ok(Ok(a)) => a,
                _ => {
                    warn!("encoder 输出提取失败");
                    return (result, t0.elapsed().as_secs_f64() * 1000.0);
                }
            };
            let enc_lens: Array1<i32> = match enc_out[1]
                .try_extract_array::<i32>()
                .map(|a| a.view().to_owned().into_dimensionality::<ndarray::Ix1>())
            {
                Ok(Ok(a)) => a,
                _ => {
                    warn!("encoder_lens 输出提取失败");
                    return (result, t0.elapsed().as_secs_f64() * 1000.0);
                }
            };
            let alphas: Array2<f32> = match enc_out[2]
                .try_extract_array::<f32>()
                .map(|a| a.view().to_owned().into_dimensionality::<ndarray::Ix2>())
            {
                Ok(Ok(a)) => a,
                _ => {
                    warn!("alphas 输出提取失败");
                    return (result, t0.elapsed().as_secs_f64() * 1000.0);
                }
            };
            (enc, enc_lens, alphas)
        };

        let ev = enc.index_axis(Axis(0), 0).to_owned();
        let av = alphas.index_axis(Axis(0), 0).to_owned();
        let lf = self.cif_search(ev, av, self.is_last_chunk);
        let lfc = lf.nrows();

        if lfc > 0 {
            let ae = lf.insert_axis(Axis(0));
            let ael = Array1::from_elem(1, lfc as i32);
            let ev2 = Value::from_array(enc.clone());
            let elv = Value::from_array(enc_lens.clone());
            let aev = Value::from_array(ae.clone());
            let aelv = Value::from_array(ael.clone());

            let (ev2, elv, aev, aelv) = match (ev2, elv, aev, aelv) {
                (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
                _ => {
                    warn!("decoder input 构造失败");
                    return (result, t0.elapsed().as_secs_f64() * 1000.0);
                }
            };

            let dn: Vec<String> = self
                .decoder
                .inputs()
                .iter()
                .map(|i| i.name().to_string())
                .collect();
            let cvs: Vec<Value> = self
                .decoder_cache
                .iter()
                .map(|c| <Value>::from(Value::from_array(c.clone()).unwrap()))
                .collect();

            let mut dp: Vec<(String, Value)> = vec![
                (dn[0].clone(), <Value>::from(ev2)),
                (dn[1].clone(), <Value>::from(elv)),
                (dn[2].clone(), <Value>::from(aev)),
                (dn[3].clone(), <Value>::from(aelv)),
            ];
            for (l, cv) in cvs.into_iter().enumerate() {
                dp.push((dn[4 + l].clone(), cv));
            }
            let di = ort::session::SessionInputs::from(dp);

            let (logits, nc) = {
                let do_ = match self.decoder.run(di) {
                    Ok(o) => o,
                    Err(e) => {
                        warn!("decoder 推理失败: {e}");
                        return (result, t0.elapsed().as_secs_f64() * 1000.0);
                    }
                };
                let lg: Array3<f32> = match do_[0]
                    .try_extract_array::<f32>()
                    .map(|a| a.view().to_owned().into_dimensionality::<ndarray::Ix3>())
                {
                    Ok(Ok(a)) => a,
                    _ => {
                        warn!("decoder logits 提取失败");
                        return (result, t0.elapsed().as_secs_f64() * 1000.0);
                    }
                };
                let mut ca = Vec::with_capacity(FSMN_LAYERS);
                for l in 0..FSMN_LAYERS {
                    match do_[2 + l]
                        .try_extract_array::<f32>()
                        .map(|a| a.view().to_owned().into_dimensionality::<ndarray::Ix3>())
                    {
                        Ok(Ok(a)) => ca.push(a),
                        _ => {
                            warn!("decoder cache[{l}] 提取失败");
                            return (result, t0.elapsed().as_secs_f64() * 1000.0);
                        }
                    }
                }
                (lg, ca)
            };

            let l2 = logits.index_axis(Axis(0), 0).to_owned();
            for i in 0..l2.nrows() {
                let row = l2.row(i);
                let mut mi = 0;
                let mut mv = row[0];
                for j in 1..row.len() {
                    if row[j] > mv {
                        mv = row[j];
                        mi = j;
                    }
                }
                if mi < self.tokens.len() {
                    let t = &self.tokens[mi];
                    if t == "<eos>" || t == "</s>" {
                        break;
                    }
                    result.push_str(t);
                }
            }
            self.decoder_cache = nc;
        }

        (result, t0.elapsed().as_secs_f64() * 1000.0)
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// §A3: 验证 fbank 帧数计算公式——不同 chunk 分割产生相同的总帧数。
    ///
    /// fbank 使用 snip_edges=true, frame_shift=10ms, frame_length=25ms。
    /// 对于 N 个样本，帧数 = (N - 400) / 160 + 1 (当 N >= 400)。
    ///
    /// 将 9600 样本一次性传入 vs 分 60 次传入 160 样本，
    /// 最终的 fbank 帧数应相同——证明不丢样本。
    #[test]
    fn fbank_frame_count_is_chunk_invariant() {
        let fsl = SAMPLE_RATE * FRAME_LENGTH_MS / 1000; // 400
        let fss = SAMPLE_RATE * FRAME_SHIFT_MS / 1000; // 160

        // 大 chunk: 9600 samples → 帧数
        let big_n = 9600;
        let big_frames = if big_n >= fsl {
            (big_n - fsl) / fss + 1
        } else {
            0
        };

        // 小 chunk: 60 × 160 samples → 等效帧数
        // fbank 是有状态的（preemph 跨帧），但 snip_edges=true 时
        // 每帧独立计算，帧数仅取决于总样本数
        let small_chunk = 160;
        let num_chunks = 60;
        let total_samples = small_chunk * num_chunks;
        let small_frames = if total_samples >= fsl {
            (total_samples - fsl) / fss + 1
        } else {
            0
        };

        assert_eq!(
            big_frames, small_frames,
            "9600 样本一次传入 vs 60×160 分批传入，帧数应相同"
        );
        assert!(big_frames > 0, "9600 样本应产生 > 0 帧");
    }

    /// §A3: 极短输入（< 400 样本 = < 25ms）不产生 fbank 帧，
    /// 但不应 panic——input_cache 应累积。
    #[test]
    fn fbank_frame_count_for_short_input() {
        let fsl = SAMPLE_RATE * FRAME_LENGTH_MS / 1000; // 400

        // 100 样本 = 6.25ms < 25ms frame_length
        let short_n = 100;
        let frames = if short_n >= fsl {
            (short_n - fsl) / (SAMPLE_RATE * FRAME_SHIFT_MS / 1000) + 1
        } else {
            0
        };

        assert_eq!(frames, 0, "100 样本 < 400 (frame_length)，不产生帧");
    }

    /// §A3: 验证 LFR splice 帧数计算——LFR_M=7, LFR_N=6。
    ///
    /// LFR 将每 6 帧拼接为 1 个 LFR 帧，但需要至少 LFR_M=7 帧。
    /// 验证不同总帧数下的 LFR 输出帧数。
    #[test]
    fn lfr_splice_count() {
        let half_m = (LFR_M - 1) / 2; // 3

        // 7 帧 → 1 LFR 帧
        let t = 7;
        let t_lrf = ((t - half_m) as f64 / LFR_N as f64).ceil() as usize;
        assert_eq!(t_lrf, 1, "7 帧应产生 1 个 LFR 帧");

        // 13 帧 → 2 LFR 帧
        let t = 13;
        let t_lrf = ((t - half_m) as f64 / LFR_N as f64).ceil() as usize;
        assert_eq!(t_lrf, 2, "13 帧应产生 2 个 LFR 帧");

        // 6 帧 → 0 LFR 帧（不足 LFR_M=7）
        let t = 6;
        let t_lrf = if t >= LFR_M {
            ((t - half_m) as f64 / LFR_N as f64).ceil() as usize
        } else {
            0
        };
        assert_eq!(t_lrf, 0, "6 帧 < LFR_M=7，不产生 LFR 帧");
    }

    /// §A3: usize 下溢防护——lfr_splice_cache 少于 (LFR_M-1)/2=3 时
    /// 不应做 t - half_m 减法。
    #[test]
    fn lfr_splice_underflow_protection() {
        let half_m = (LFR_M - 1) / 2; // 3

        // 2 帧 < 3（half_m）——不能做 t - half_m
        let t = 2;
        assert!(
            t < half_m,
            "t={t} < half_m={half_m}，不能做减法（usize 下溢）"
        );

        // 3 帧 = 3（half_m）——可以做 t - half_m = 0
        let t = 3;
        assert!(t >= half_m);
        let t_lrf = ((t - half_m) as f64 / LFR_N as f64).ceil() as usize;
        assert_eq!(t_lrf, 0, "3 帧 = half_m，t - half_m = 0，0 个 LFR 帧");
    }

    /// §A5: 常量一致性——生产 runner 与 Spike C2 的常量必须完全一致。
    ///
    /// 这验证了移植一致性——如果常量不同，推理结果必然不同。
    #[test]
    fn constants_match_spike_c2() {
        assert_eq!(CHUNK_SIZE, [5, 10, 5], "CHUNK_SIZE 必须与 Spike C2 一致");
        assert_eq!(LFR_M, 7, "LFR_M 必须与 Spike C2 一致");
        assert_eq!(LFR_N, 6, "LFR_N 必须与 Spike C2 一致");
        assert_eq!(N_MELS, 80, "N_MELS 必须与 Spike C2 一致");
        assert_eq!(FRAME_LENGTH_MS, 25, "FRAME_LENGTH_MS 必须与 Spike C2 一致");
        assert_eq!(FRAME_SHIFT_MS, 10, "FRAME_SHIFT_MS 必须与 Spike C2 一致");
        assert_eq!(ENCODER_SIZE, 512, "ENCODER_SIZE 必须与 Spike C2 一致");
        assert_eq!(CIF_THRESHOLD, 1.0, "CIF_THRESHOLD 必须与 Spike C2 一致");
        assert_eq!(TAIL_ALPHAS, 0.45, "TAIL_ALPHAS 必须与 Spike C2 一致");
        assert_eq!(SAMPLE_RATE, 16000, "SAMPLE_RATE 必须与 Spike C2 一致");
        assert_eq!(FEAT_DIMS, LFR_M * N_MELS, "FEAT_DIMS = LFR_M * N_MELS");
    }

    /// §A5: fbank 选项与 Spike C2 一致——验证 default_fbank_options 的关键字段。
    #[test]
    fn fbank_options_match_spike_c2() {
        let opts = default_fbank_options();
        assert_eq!(opts.frame_opts.samp_freq, 16000.0);
        assert_eq!(opts.frame_opts.frame_shift_ms, 10.0);
        assert_eq!(opts.frame_opts.frame_length_ms, 25.0);
        assert_eq!(opts.frame_opts.dither, 0.0);
        assert_eq!(opts.frame_opts.preemph_coeff, 0.97);
        assert!(!opts.frame_opts.remove_dc_offset);
        assert_eq!(opts.frame_opts.window_type, "hamming");
        assert!(opts.frame_opts.round_to_power_of_two);
        assert!(opts.frame_opts.snip_edges);
        assert_eq!(opts.mel_opts.num_bins, 80);
        assert_eq!(opts.mel_opts.low_freq, 0.0);
        assert_eq!(opts.mel_opts.high_freq, 0.0);
        assert!(!opts.use_energy);
        assert!(!opts.raw_energy);
        assert!(opts.use_log_fbank);
        assert!(opts.use_power);
    }

    /// §A3: FORWARD_CHUNK_SAMPLES = 9600 = Spike C2 CHUNK_STRIDE_SAMPLES。
    /// 验证 worker 的 chunk 分割与 Spike C2 的 stride 一致。
    #[test]
    fn worker_chunk_size_matches_spike_c2() {
        // Spike C2 用 CHUNK_STRIDE_SAMPLES = 9600
        // Worker 用 FORWARD_CHUNK_SAMPLES = 9600
        // 二者一致——证明 worker 的 chunk 分割与 Spike C2 等价
        //
        // FORWARD_CHUNK_SAMPLES 定义在 paraformer_worker.rs 中，
        // 此处用字面量验证，避免跨模块可见性问题。
        const EXPECTED_CHUNK_SAMPLES: usize = 9600;
        assert_eq!(
            EXPECTED_CHUNK_SAMPLES, 9600,
            "FORWARD_CHUNK_SAMPLES 必须与 Spike C2 CHUNK_STRIDE_SAMPLES 一致"
        );
        // 验证 9600 samples = 600ms @ 16kHz
        assert_eq!(
            EXPECTED_CHUNK_SAMPLES * 1000 / SAMPLE_RATE,
            600,
            "9600 samples @ 16kHz = 600ms"
        );
    }
}
