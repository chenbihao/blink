//! Spike C2: ParaformerOnline — Rust ort Implementation
//!
//! Minimal Rust port of the Python oracle (spike_c2_paraformer_online.py).
//! Implements: fbank (kaldi-native-fbank) -> LFR -> CMVN -> pos_emb -> encoder -> CIF -> decoder -> greedy
//! Goal: prove Rust can load encoder/decoder ONNX and do chunk-by-chunk streaming.

#![allow(dead_code)]

use kaldi_native_fbank::{FbankComputer, FbankOptions};
use kaldi_native_fbank::online::{OnlineFeature, FeatureComputer};
use ndarray::{Array1, Array2, Array3, Axis, s};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tracing::{info, warn};

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
const CHUNK_STRIDE_SAMPLES: usize = 9600;

#[cfg(windows)]
mod winapi {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    pub fn get_working_set_mb() -> f64 {
        unsafe {
            let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            let ok = GetProcessMemoryInfo(-1isize as HANDLE, &mut c, std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32);
            if ok != 0 { c.WorkingSetSize as f64 / (1024.0 * 1024.0) } else { -1.0 }
        }
    }
}
#[cfg(not(windows))]
mod winapi { pub fn get_working_set_mb() -> f64 { -1.0 } }

fn load_cmvn(path: &Path) -> Result<(Vec<f32>, Vec<f32>), Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    let mut means = Vec::new();
    let mut vars = Vec::new();
    for i in 0..lines.len() {
        let items: Vec<&str> = lines[i].split_whitespace().collect();
        if items.is_empty() { continue; }
        if items[0] == "<AddShift>" && i + 1 < lines.len() {
            let next: Vec<&str> = lines[i+1].split_whitespace().collect();
            if !next.is_empty() && next[0] == "<LearnRateCoef>" {
                means = next[3..next.len()-1].iter().filter_map(|s| s.parse().ok()).collect();
            }
        } else if items[0] == "<Rescale>" && i + 1 < lines.len() {
            let next: Vec<&str> = lines[i+1].split_whitespace().collect();
            if !next.is_empty() && next[0] == "<LearnRateCoef>" {
                vars = next[3..next.len()-1].iter().filter_map(|s| s.parse().ok()).collect();
            }
        }
    }
    let target = FEAT_DIMS;
    if means.len() < target {
        let orig = means.clone();
        means = Vec::with_capacity(target);
        while means.len() < target { means.extend(&orig); }
        means.truncate(target);
        let orig_v = vars.clone();
        vars = Vec::with_capacity(target);
        while vars.len() < target { vars.extend(&orig_v); }
        vars.truncate(target);
    }
    Ok((means, vars))
}

fn load_tokens(path: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    if path.extension().map_or(false, |e| e == "json") {
        let v: serde_json::Value = serde_json::from_str(&content)?;
        if let Some(obj) = v.as_object() {
            let max_id = obj.keys().filter_map(|k| k.parse::<usize>().ok()).max().unwrap_or(0);
            let mut tokens = vec![String::new(); max_id + 1];
            for (k, val) in obj {
                if let (Ok(idx), Some(s)) = (k.parse::<usize>(), val.as_str()) {
                    if idx < tokens.len() { tokens[idx] = s.to_string(); }
                }
            }
            return Ok(tokens);
        } else if let Some(arr) = v.as_array() {
            return Ok(arr.iter().filter_map(|x| x.as_str().map(String::from)).collect());
        }
    }
    // token.txt format: "token id"
    let mut tokens = Vec::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if !parts.is_empty() { tokens.push(parts[0].to_string()); }
    }
    Ok(tokens)
}

fn load_wav_16k_mono(path: &Path) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 16000, "Expected 16kHz");
    assert_eq!(spec.channels, 1, "Expected mono");
    let samples: Vec<f32> = reader.into_samples::<i16>().filter_map(|s| s.ok())
        .map(|s| s as f32 / 32768.0).collect();
    Ok(samples)
}

#[derive(Debug, Clone, Serialize)]
struct ChunkLog {
    inference_ms: f64, num_frames: usize, enc_out_shape: Vec<usize>,
    alphas_out_shape: Vec<usize>, list_frame_count: usize, chunk_index: usize,
    audio_timestamp_s: f64, audio_duration_s: f64, partial_text: String,
    is_final: bool, total_chunk_ms: f64,
}

struct ParaformerOnline {
    encoder: ort::session::Session,
    decoder: ort::session::Session,
    means: Vec<f32>, vars: Vec<f32>, tokens: Vec<String>,
    start_idx_cache: usize, is_first_chunk: bool, is_last_chunk: bool,
    input_cache: Vec<f32>, lfr_splice_cache: Vec<Vec<f32>>,
    hidden_cache: Array2<f32>, alphas_cache: Array1<f32>,
    feats_cache: Array2<f32>,
    decoder_cache: Vec<Array3<f32>>,
    fbank: OnlineFeature,
    fbank_offset: usize,
}

impl ParaformerOnline {
    fn new(enc_path: &Path, dec_path: &Path, mvn_path: &Path, tok_path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let (means, vars) = load_cmvn(mvn_path)?;
        info!("CMVN: means[{}], vars[{}]", means.len(), vars.len());
        let tokens = load_tokens(tok_path)?;
        info!("Tokens: {}", tokens.len());

        // Configure kaldi-native-fbank for ParaformerOnline
        // Match: preemph=0.97, hamming window, 80 mel bins, fmin=0, fmax=8000
        let mut fbank_opts = FbankOptions::default();
        fbank_opts.frame_opts.samp_freq = 16000.0;
        fbank_opts.frame_opts.frame_shift_ms = 10.0;
        fbank_opts.frame_opts.frame_length_ms = 25.0;
        fbank_opts.frame_opts.dither = 0.0;
        fbank_opts.frame_opts.preemph_coeff = 0.97;
        fbank_opts.frame_opts.remove_dc_offset = false;  // Python oracle doesn't do DC removal
        fbank_opts.frame_opts.window_type = "hamming".to_string();
        fbank_opts.frame_opts.round_to_power_of_two = true;  // n_fft = 512 (next pow2 of 400)
        fbank_opts.frame_opts.snip_edges = true;
        fbank_opts.mel_opts.num_bins = 80;
        fbank_opts.mel_opts.low_freq = 0.0;
        fbank_opts.mel_opts.high_freq = 0.0;  // 0 means use Nyquist = 8000
        fbank_opts.use_energy = false;
        fbank_opts.raw_energy = false;
        fbank_opts.use_log_fbank = true;
        fbank_opts.use_power = true;

        let mel_bins = fbank_opts.mel_opts.num_bins;
        let fbank_dim = FbankComputer::new(fbank_opts.clone())?.dim();
        let fbank_computer = FbankComputer::new(fbank_opts)?;
        let feature_computer = FeatureComputer::Fbank(fbank_computer);
        let fbank = OnlineFeature::new(feature_computer);
        info!("Fbank: kaldi-native-fbank, mel_bins={}, dim={}", mel_bins, fbank_dim);

        use ort::session::builder::GraphOptimizationLevel;
        let encoder = ort::session::Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level1)?
            .with_intra_threads(1)?
            .commit_from_file(enc_path)?;
        info!("Encoder: {} in, {} out", encoder.inputs().len(), encoder.outputs().len());
        let decoder = ort::session::Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level1)?
            .with_intra_threads(1)?
            .commit_from_file(dec_path)?;
        info!("Decoder: {} in, {} out", decoder.inputs().len(), decoder.outputs().len());

        Ok(Self {
            encoder, decoder, means, vars, tokens,
            start_idx_cache: 0, is_first_chunk: true, is_last_chunk: false,
            input_cache: Vec::new(), lfr_splice_cache: Vec::new(),
            hidden_cache: Array2::zeros((1, ENCODER_SIZE)),
            alphas_cache: Array1::zeros(1),
            feats_cache: Array2::zeros((CHUNK_SIZE[0]+CHUNK_SIZE[2], FEAT_DIMS)),
            decoder_cache: vec![Array3::zeros((1, FSMN_DIMS, FSMN_LORDER)); FSMN_LAYERS],
            fbank,
            fbank_offset: 0,
        })
    }

    fn reset(&mut self) {
        self.start_idx_cache = 0; self.is_first_chunk = true; self.is_last_chunk = false;
        self.input_cache.clear(); self.lfr_splice_cache.clear();
        self.hidden_cache = Array2::zeros((1, ENCODER_SIZE));
        self.alphas_cache = Array1::zeros(1);
        self.feats_cache = Array2::zeros((CHUNK_SIZE[0]+CHUNK_SIZE[2], FEAT_DIMS));
        self.decoder_cache = vec![Array3::zeros((1, FSMN_DIMS, FSMN_LORDER)); FSMN_LAYERS];
        // Reset fbank by recreating it
        let mut fbank_opts = FbankOptions::default();
        fbank_opts.frame_opts.samp_freq = 16000.0;
        fbank_opts.frame_opts.frame_shift_ms = 10.0;
        fbank_opts.frame_opts.frame_length_ms = 25.0;
        fbank_opts.frame_opts.dither = 0.0;
        fbank_opts.frame_opts.preemph_coeff = 0.97;
        fbank_opts.frame_opts.remove_dc_offset = false;
        fbank_opts.frame_opts.window_type = "hamming".to_string();
        fbank_opts.frame_opts.round_to_power_of_two = true;
        fbank_opts.frame_opts.snip_edges = true;
        fbank_opts.mel_opts.num_bins = 80;
        fbank_opts.mel_opts.low_freq = 0.0;
        fbank_opts.mel_opts.high_freq = 0.0;
        fbank_opts.use_energy = false;
        fbank_opts.raw_energy = false;
        fbank_opts.use_log_fbank = true;
        fbank_opts.use_power = true;
        let fbank_computer = FbankComputer::new(fbank_opts).unwrap();
        self.fbank = OnlineFeature::new(FeatureComputer::Fbank(fbank_computer));
        self.fbank_offset = 0;
    }

    /// Compute fbank features using kaldi-native-fbank
    fn compute_fbank_knf(&mut self, samples: &[f32]) -> Array2<f32> {
        // Python oracle scales samples to int16 range before fbank: wav_int16 = waves * 32768
        // kaldi-native-fbank handles pre-emphasis internally (preemph_coeff=0.97)
        // We scale to match Python oracle's input range for CMVN compatibility
        let scaled: Vec<f32> = samples.iter().map(|s| s * 32768.0).collect();
        self.fbank.accept_waveform(16000.0, &scaled);
        let total_frames = self.fbank.num_frames_ready();
        let new_frames = total_frames - self.fbank_offset;
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
        self.fbank_offset = total_frames;
        fbank
    }

    fn online_lfr_cmvn(&mut self, wav_feats: &Array2<f32>, input_finished: bool) -> (Array2<f32>, Vec<Vec<f32>>) {
        let mut all: Vec<Vec<f32>> = self.lfr_splice_cache.clone();
        for i in 0..wav_feats.nrows() { all.push(wav_feats.row(i).to_vec()); }
        let t = all.len();
        if t < LFR_M { self.lfr_splice_cache = all; return (Array2::zeros((0, FEAT_DIMS)), Vec::new()); }
        let t_lrf = ((t-(LFR_M-1)/2) as f64/LFR_N as f64).ceil() as usize;
        let mut out: Vec<Vec<f32>> = Vec::new();
        let mut stop = t_lrf;
        for i in 0..t_lrf {
            if LFR_M <= t-i*LFR_N {
                let mut p = Vec::with_capacity(FEAT_DIMS);
                for j in 0..LFR_M { for k in 0..N_MELS { p.push(all[i*LFR_N+j][k]); } }
                out.push(p);
            } else if input_finished {
                let np = LFR_M-(t-i*LFR_N);
                let mut p = Vec::with_capacity(FEAT_DIMS);
                for j in 0..t-i*LFR_N { for k in 0..N_MELS { p.push(all[i*LFR_N+j][k]); } }
                for _ in 0..np { for k in 0..N_MELS { p.push(all[t-1][k]); } }
                out.push(p);
            } else { stop = i; break; }
        }
        let lsi = (stop*LFR_N).min(t-1);
        let new_cache = if lsi < t { all[lsi..].to_vec() } else { Vec::new() };
        let n = out.len();
        let mut res = Array2::zeros((n, FEAT_DIMS));
        for (i, row) in out.iter().enumerate() { for j in 0..FEAT_DIMS { res[[i,j]] = (row[j]+self.means[j])*self.vars[j]; } }
        (res, new_cache)
    }

    fn get_pos_emb(&mut self, mut wf: Array2<f32>) -> Array2<f32> {
        let ts = wf.nrows(); let fd = wf.ncols();
        let si = self.start_idx_cache; self.start_idx_cache += ts;
        let scale = -0.0330119726594128f32;
        for i in 0..ts { for j in 0..fd/2 {
            let tm = (j as f32*scale).exp();
            let coe = tm*(si+i+1) as f32;
            wf[[i,j]] += coe.sin(); wf[[i,j+fd/2]] += coe.cos();
        }}
        wf
    }

    fn add_overlap_chunk(&mut self, mut wf: Array2<f32>, input_finished: bool) -> Array2<f32> {
        if self.feats_cache.nrows() > 0 {
            let nr = self.feats_cache.nrows();
            let mut comb = Array2::zeros((nr+wf.nrows(), wf.ncols()));
            comb.slice_mut(s![..nr, ..]).assign(&self.feats_cache);
            comb.slice_mut(s![nr.., ..]).assign(&wf);
            wf = comb;
        }
        let nc;
        if input_finished {
            nc = if wf.nrows() >= CHUNK_SIZE[0] { wf.slice(s![wf.nrows()-CHUNK_SIZE[0].., ..]).to_owned() } else { wf.clone() };
            if !self.is_last_chunk {
                let pl = CHUNK_SIZE[0]+CHUNK_SIZE[1]+CHUNK_SIZE[2]-wf.nrows();
                if pl > 0 && pl < 100 {
                    let mut pad = Array2::zeros((wf.nrows()+pl, wf.ncols()));
                    pad.slice_mut(s![..wf.nrows(), ..]).assign(&wf);
                    wf = pad;
                }
            }
        } else {
            let cl = CHUNK_SIZE[0]+CHUNK_SIZE[2];
            nc = if wf.nrows() >= cl { wf.slice(s![wf.nrows()-cl.., ..]).to_owned() } else { wf.clone() };
        }
        self.feats_cache = nc;
        wf
    }

    fn cif_search(&mut self, hidden: Array2<f32>, mut alphas: Array1<f32>, is_last: bool) -> Array2<f32> {
        let t = hidden.nrows();
        if t == 0 { return Array2::zeros((0, ENCODER_SIZE)); }
        let hs = hidden.ncols();
        let csp = CHUNK_SIZE[0];
        for i in 0..csp.min(t) { alphas[i] = 0.0; }
        let csf = CHUNK_SIZE[0]+CHUNK_SIZE[1];
        for i in csf.min(t)..t { alphas[i] = 0.0; }
        let mut fh: Vec<Array1<f32>> = Vec::new();
        if self.hidden_cache.nrows() > 0 { fh.push(self.hidden_cache.row(0).to_owned()); }
        for i in 0..t { fh.push(hidden.row(i).to_owned()); }
        let mut fa: Vec<f32> = Vec::new();
        if self.alphas_cache.len() > 0 { fa.push(self.alphas_cache[0]); }
        for i in 0..t { fa.push(alphas[i]); }
        if is_last { fh.push(Array1::zeros(hs)); fa.push(TAIL_ALPHAS); }
        let n = fa.len();
        let mut lf: Vec<Array1<f32>> = Vec::new();
        let mut integ = 0f32; let mut fr = Array1::zeros(hs);
        for i in 0..n {
            let a = fa[i];
            if a+integ < CIF_THRESHOLD {
                integ += a;
                for j in 0..hs { fr[j] += a*fh[i][j]; }
            } else {
                for j in 0..hs { fr[j] += (CIF_THRESHOLD-integ)*fh[i][j]; }
                lf.push(fr.clone());
                integ += a; integ -= CIF_THRESHOLD;
                for j in 0..hs { fr[j] = integ*fh[i][j]; }
            }
        }
        self.alphas_cache = Array1::from_vec(vec![integ]);
        self.hidden_cache = if integ > 0.0 { (&fr/integ).insert_axis(Axis(0)) } else { fr.insert_axis(Axis(0)) };
        if lf.is_empty() { Array2::zeros((0, hs)) }
        else {
            let mut r = Array2::zeros((lf.len(), hs));
            for (i, f) in lf.iter().enumerate() { r.row_mut(i).assign(f); }
            r
        }
    }

    fn forward_chunk(&mut self, mut cf: Array2<f32>, input_finished: bool) -> (String, f64, usize, Vec<usize>, Vec<usize>, usize) {
        let t0 = Instant::now();
        let mut result = String::new();
        if cf.nrows() == 0 { return (result, 0.0, 0, vec![], vec![], 0); }
        let sf = (ENCODER_SIZE as f32).sqrt();
        cf.mapv_inplace(|x| x*sf);
        cf = self.get_pos_emb(cf);
        cf = self.add_overlap_chunk(cf, input_finished);
        let nf = cf.nrows();
        use ort::value::Value;
        // Extract input names first to avoid borrowing conflicts
        let enc_in0 = self.encoder.inputs()[0].name().to_string();
        let enc_in1 = self.encoder.inputs()[1].name().to_string();
        let speech = cf.insert_axis(Axis(0));
        let speech_lens = ndarray::Array1::from_elem(1, nf as i32);
        let sv = Value::from_array(speech.clone()).unwrap();
        let slv = Value::from_array(speech_lens.clone()).unwrap();
        let enc_inputs = ort::session::SessionInputs::from(vec![
            (enc_in0.as_str(), <ort::value::Value>::from(sv)),
            (enc_in1.as_str(), <ort::value::Value>::from(slv)),
        ]);
        // Extract all needed data from enc_out in a scoped block
        let (enc, enc_lens, alphas, es, as_) = {
            let enc_out = self.encoder.run(enc_inputs).unwrap();
            let enc: Array3<f32> = enc_out[0].try_extract_array::<f32>().unwrap().view().to_owned().into_dimensionality::<ndarray::Ix3>().unwrap();
            let enc_lens: Array1<i32> = enc_out[1].try_extract_array::<i32>().unwrap().view().to_owned().into_dimensionality::<ndarray::Ix1>().unwrap();
            let alphas: Array2<f32> = enc_out[2].try_extract_array::<f32>().unwrap().view().to_owned().into_dimensionality::<ndarray::Ix2>().unwrap();
            let es = enc.shape().to_vec();
            let as_ = alphas.shape().to_vec();
            (enc, enc_lens, alphas, es, as_)
        }; // enc_out dropped here
        let ev = enc.index_axis(Axis(0), 0).to_owned();
        let av = alphas.index_axis(Axis(0), 0).to_owned();
        let lf = self.cif_search(ev, av, self.is_last_chunk);
        let lfc = lf.nrows();
        if lfc > 0 {
            let ae = lf.insert_axis(Axis(0));
            let ael = ndarray::Array1::from_elem(1, lfc as i32);
            // Build decoder inputs as owned Values
            let ev2 = Value::from_array(enc.clone()).unwrap();
            let elv = Value::from_array(enc_lens.clone()).unwrap();
            let aev = Value::from_array(ae.clone()).unwrap();
            let aelv = Value::from_array(ael.clone()).unwrap();
            let dec_input_names: Vec<String> = self.decoder.inputs().iter().map(|i| i.name().to_string()).collect();
            let cvs: Vec<Value> = self.decoder_cache.iter().map(|c| <ort::value::Value>::from(Value::from_array(c.clone()).unwrap())).collect();
            let mut dec_pairs: Vec<(String, Value)> = vec![
                (dec_input_names[0].clone(), <ort::value::Value>::from(ev2)),
                (dec_input_names[1].clone(), <ort::value::Value>::from(elv)),
                (dec_input_names[2].clone(), <ort::value::Value>::from(aev)),
                (dec_input_names[3].clone(), <ort::value::Value>::from(aelv)),
            ];
            for (l, cv) in cvs.into_iter().enumerate() {
                dec_pairs.push((dec_input_names[4 + l].clone(), cv));
            }
            let dec_inputs = ort::session::SessionInputs::from(dec_pairs);
            // Extract decoder outputs in scoped block
            let (logits, new_caches) = {
                let do_ = self.decoder.run(dec_inputs).unwrap();
                let logits: Array3<f32> = do_[0].try_extract_array::<f32>().unwrap().view().to_owned().into_dimensionality::<ndarray::Ix3>().unwrap();
                let mut caches = Vec::with_capacity(FSMN_LAYERS);
                for l in 0..FSMN_LAYERS {
                    let ca = do_[2+l].try_extract_array::<f32>().unwrap();
                    caches.push(ca.view().to_owned().into_dimensionality::<ndarray::Ix3>().unwrap());
                }
                (logits, caches)
            }; // do_ dropped here
            let l2 = logits.index_axis(Axis(0), 0).to_owned();
            for i in 0..l2.nrows() {
                let row = l2.row(i);
                let mut mi = 0; let mut mv = row[0];
                for j in 1..row.len() { if row[j] > mv { mv = row[j]; mi = j; } }
                if mi < self.tokens.len() {
                    let t = &self.tokens[mi];
                    if t == "<eos>" || t == "</s>" { break; }
                    result.push_str(t);
                }
            }
            self.decoder_cache = new_caches;
        }
        let ms = t0.elapsed().as_secs_f64()*1000.0;
        (result, ms, nf, es, as_, lfc)
    }

    fn forward(&mut self, chunk_audio: &[f32], input_finished: bool) -> (String, ChunkLog) {
        let dur = chunk_audio.len() as f64 / SAMPLE_RATE as f64;
        if chunk_audio.len() < 960 && input_finished && !self.is_first_chunk {
            self.is_last_chunk = true;
            let wf = self.feats_cache.clone();
            let (r, ms, nf, es, as_, lfc) = self.forward_chunk(wf, self.is_last_chunk);
            self.reset();
            return (r, ChunkLog { inference_ms: ms, num_frames: nf, enc_out_shape: es, alphas_out_shape: as_, list_frame_count: lfc, chunk_index: 0, audio_timestamp_s: 0.0, audio_duration_s: dur, partial_text: String::new(), is_final: true, total_chunk_ms: ms });
        }
        if self.is_first_chunk { self.is_first_chunk = false; }
        let mut waves = self.input_cache.clone();
        waves.extend_from_slice(chunk_audio);
        let fsl = SAMPLE_RATE*FRAME_LENGTH_MS/1000;
        let fss = SAMPLE_RATE*FRAME_SHIFT_MS/1000;
        let fn_ = if waves.len() >= fsl { (waves.len()-fsl)/fss+1 } else { 0 };
        if fn_ < 1 || waves.len() < fsl {
            self.input_cache = waves;
            return (String::new(), ChunkLog { inference_ms: 0.0, num_frames: 0, enc_out_shape: vec![], alphas_out_shape: vec![], list_frame_count: 0, chunk_index: 0, audio_timestamp_s: 0.0, audio_duration_s: dur, partial_text: String::new(), is_final: input_finished, total_chunk_ms: 0.0 });
        }
        self.input_cache = waves[fn_*fss..].to_vec();
        let tl = (fn_*fss-fss+fsl).min(waves.len());
        let samples: Vec<f32> = waves[..tl].to_vec();
        // Use kaldi-native-fbank instead of manual DFT
        let wf = self.compute_fbank_knf(&samples);
        if wf.nrows() == 0 {
            if input_finished { self.input_cache.clear(); self.lfr_splice_cache.clear(); }
            return (String::new(), ChunkLog { inference_ms: 0.0, num_frames: 0, enc_out_shape: vec![], alphas_out_shape: vec![], list_frame_count: 0, chunk_index: 0, audio_timestamp_s: 0.0, audio_duration_s: dur, partial_text: String::new(), is_final: input_finished, total_chunk_ms: 0.0 });
        }
        if self.lfr_splice_cache.is_empty() {
            let ff = wf.row(0).to_vec();
            for _ in 0..((LFR_M-1)/2) { self.lfr_splice_cache.push(ff.clone()); }
        }
        let total = wf.nrows() + self.lfr_splice_cache.len();
        if total >= LFR_M {
            let (lfr, nc) = self.online_lfr_cmvn(&wf, input_finished);
            self.lfr_splice_cache = nc;
            if lfr.nrows() == 0 {
                if input_finished { self.input_cache.clear(); self.lfr_splice_cache.clear(); }
                return (String::new(), ChunkLog { inference_ms: 0.0, num_frames: 0, enc_out_shape: vec![], alphas_out_shape: vec![], list_frame_count: 0, chunk_index: 0, audio_timestamp_s: 0.0, audio_duration_s: dur, partial_text: String::new(), is_final: input_finished, total_chunk_ms: 0.0 });
            }
            let (r, ms, nf, es, as_, lfc) = self.forward_chunk(lfr, input_finished);
            if input_finished { self.reset(); }
            return (r, ChunkLog { inference_ms: ms, num_frames: nf, enc_out_shape: es, alphas_out_shape: as_, list_frame_count: lfc, chunk_index: 0, audio_timestamp_s: 0.0, audio_duration_s: dur, partial_text: String::new(), is_final: input_finished, total_chunk_ms: ms });
        } else {
            for i in 0..wf.nrows() { self.lfr_splice_cache.push(wf.row(i).to_vec()); }
            return (String::new(), ChunkLog { inference_ms: 0.0, num_frames: 0, enc_out_shape: vec![], alphas_out_shape: vec![], list_frame_count: 0, chunk_index: 0, audio_timestamp_s: 0.0, audio_duration_s: dur, partial_text: String::new(), is_final: input_finished, total_chunk_ms: 0.0 });
        }
    }
}

fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();
    info!("=== Spike C2: ParaformerOnline Rust Implementation (kaldi-native-fbank) ===");

    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let models = base.join("../../models");
    let enc = models.join("paraformer-online-onnx/encoder.onnx");
    let dec = models.join("paraformer-online-onnx/decoder.onnx");
    let mvn = models.join("paraformer-online-onnx/am.mvn");
    let tok = models.join("paraformer-online-onnx/tokens.json");
    let wav_path = models.join("asr_example.wav");

    for (name, p) in [("encoder", &enc), ("decoder", &dec), ("am.mvn", &mvn), ("tokens", &tok), ("wav", &wav_path)] {
        if !p.exists() { warn!("Missing: {} = {}", name, p.display()); }
    }

    let mem_before = winapi::get_working_set_mb();
    let t0 = Instant::now();
    let mut asr = match ParaformerOnline::new(&enc, &dec, &mvn, &tok) {
        Ok(a) => a,
        Err(e) => { warn!("Failed to init: {e}"); return; }
    };
    let t_load = t0.elapsed().as_secs_f64();
    let mem_after = winapi::get_working_set_mb();
    info!("Model loaded: {:.3}s, mem: {:.1} -> {:.1}MB", t_load, mem_before, mem_after);

    let audio = match load_wav_16k_mono(&wav_path) {
        Ok(a) => a,
        Err(e) => { warn!("Failed to load wav: {e}"); return; }
    };
    let dur = audio.len() as f64 / SAMPLE_RATE as f64;
    info!("Audio: {:.3}s, {} samples", dur, audio.len());

    let n_chunks = (audio.len()-1) / CHUNK_STRIDE_SAMPLES + 1;
    info!("Chunks: {}, stride: {} samples ({}ms)", n_chunks, CHUNK_STRIDE_SAMPLES, CHUNK_STRIDE_SAMPLES*1000/SAMPLE_RATE);

    let mut chunk_logs = Vec::new();
    let mut all_texts = Vec::new();
    let mut first_partial: Option<f64> = None;
    let t_stream = Instant::now();

    for i in 0..n_chunks {
        let start = i * CHUNK_STRIDE_SAMPLES;
        let end = ((i+1)*CHUNK_STRIDE_SAMPLES).min(audio.len());
        let chunk = &audio[start..end];
        let is_final = i == n_chunks - 1;
        let ts = start as f64 / SAMPLE_RATE as f64;
        let tc = Instant::now();
        let (text, mut log) = asr.forward(chunk, is_final);
        let tcm = tc.elapsed().as_secs_f64() * 1000.0;
        log.chunk_index = i;
        log.audio_timestamp_s = ts;
        log.partial_text = text.clone();
        log.total_chunk_ms = tcm;
        if !text.is_empty() {
            if first_partial.is_none() { first_partial = Some(t_stream.elapsed().as_secs_f64()); }
            all_texts.push(text.clone());
        }
        info!("chunk {:3} | t={:6.2}s | inf={:6.1}ms | final={} | frames={} | text='{}'", i, ts, log.inference_ms, is_final, log.num_frames, text);
        chunk_logs.push(log);
    }

    let t_total = t_stream.elapsed().as_secs_f64();
    let final_text = all_texts.join("");
    let mem_peak = winapi::get_working_set_mb();
    let rtf = t_total / dur;

    // 20x reset test
    let mut reset_results = Vec::new();
    for _ in 0..20 {
        asr.reset();
        let mut texts = Vec::new();
        for i in 0..n_chunks {
            let s = i*CHUNK_STRIDE_SAMPLES;
            let e = ((i+1)*CHUNK_STRIDE_SAMPLES).min(audio.len());
            let (r, _) = asr.forward(&audio[s..e], i == n_chunks-1);
            if !r.is_empty() { texts.push(r); }
        }
        reset_results.push(texts.join(""));
    }
    let consistent = reset_results.iter().all(|r| r == &reset_results[0]);
    info!("20x reset: {}", if consistent { "consistent" } else { "INCONSISTENT" });

    // Cancel test
    asr.reset();
    for i in 0..5 {
        let s = i*CHUNK_STRIDE_SAMPLES;
        let e = ((i+1)*CHUNK_STRIDE_SAMPLES).min(audio.len());
        let _ = asr.forward(&audio[s..e], false);
    }
    asr.reset();
    let mut cancel_result = String::new();
    for i in 0..n_chunks {
        let s = i*CHUNK_STRIDE_SAMPLES;
        let e = ((i+1)*CHUNK_STRIDE_SAMPLES).min(audio.len());
        let (r, _) = asr.forward(&audio[s..e], i == n_chunks-1);
        if !r.is_empty() { cancel_result.push_str(&r); }
    }
    let cancel_ok = cancel_result == final_text;

    let status = if first_partial.is_some() && rtf < 1.0 { "GO" } else if first_partial.is_some() { "CONDITIONAL_GO" } else { "BLOCKED" };
    info!("Status: {}", status);
    info!("RTF: {:.4}, first partial: {:?}, text: '{}'", rtf, first_partial, final_text);

    let result = serde_json::json!({
        "spike": "C2_rust_implementation_knf",
        "status": status,
        "fbank_implementation": "kaldi-native-fbank",
        "streaming_results": {
            "total_streaming_s": (t_total * 1000.0).round() / 1000.0,
            "rtf": (rtf * 10000.0).round() / 10000.0,
            "first_nonempty_partial_latency_s": first_partial.map(|v| (v * 1000.0).round() / 1000.0),
            "final_text": final_text,
            "n_partial_texts": all_texts.len(),
            "peak_mem_mb": (mem_peak * 10.0).round() / 10.0,
            "model_load_time_s": (t_load * 1000.0).round() / 1000.0,
            "model_load_mem_delta_mb": ((mem_after - mem_before) * 10.0).round() / 10.0,
        },
        "chunk_logs": chunk_logs,
        "reset_test": { "iterations": 20, "consistent": consistent, "sample_results": reset_results.iter().take(3).cloned().collect::<Vec<_>>() },
        "cancel_test": { "cancelled_after_chunks": 5, "result_after_reset": cancel_result, "matches_original": cancel_ok },
    });

    let out = base.join("results/spike_c2_rust_knf.json");
    std::fs::create_dir_all(out.parent().unwrap()).ok();
    std::fs::write(&out, serde_json::to_string_pretty(&result).unwrap()).unwrap();
    info!("Result saved to: {}", out.display());
}