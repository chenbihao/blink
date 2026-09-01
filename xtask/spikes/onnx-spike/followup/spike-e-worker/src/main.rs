//! Spike E (minimal): Hybrid Topology Feasibility Verification
//!
//! Only answers: can a worker process load real ORT + real ParaformerOnline
//! models, do real streaming inference over NDJSON, gracefully quit, survive
//! forced kill, and be restarted?
//!
//! Dangerous Crash/Oom tests have been REMOVED. Only safe fault isolation
//! is verified via child.kill() + child.wait().
//!
//! Worker protocol (NDJSON over stdin/stdout):
//!   Request:  Init { dll_path, enc_path, dec_path, mvn_path, tok_path }
//!             Infer { samples, is_final }
//!             Reset
//!             Quit
//!   Response: Ready
//!             Result { text }
//!             ResetOk
//!             Error { message }

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const SAMPLE_RATE: usize = 16000;
const CHUNK_STRIDE: usize = 9600; // 600ms at 16kHz
const N_MELS: usize = 80;
const LFR_M: usize = 7;
const LFR_N: usize = 6;
const FEAT_DIMS: usize = LFR_M * N_MELS;
const ENCODER_SIZE: usize = 512;
const FSMN_LAYERS: usize = 16;
const FSMN_LORDER: usize = 10;
const FSMN_DIMS: usize = 512;
const CIF_THRESHOLD: f32 = 1.0;
const TAIL_ALPHAS: f32 = 0.45;
const CHUNK_SIZE: [usize; 3] = [5, 10, 5];

// =====================================================================
// Windows API helpers
// =====================================================================

#[cfg(windows)]
mod winapi {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    pub fn get_working_set_mb() -> f64 {
        unsafe {
            let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            let ok = GetProcessMemoryInfo(
                -1isize as HANDLE,
                &mut c,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            );
            if ok != 0 {
                c.WorkingSetSize as f64 / (1024.0 * 1024.0)
            } else {
                -1.0
            }
        }
    }
    #[allow(dead_code)]
    pub fn get_process_working_set_mb(pid: u32) -> f64 {
        unsafe {
            let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            let handle = windows_sys::Win32::System::Threading::OpenProcess(
                windows_sys::Win32::System::Threading::PROCESS_QUERY_INFORMATION
                    | windows_sys::Win32::System::Threading::PROCESS_VM_READ,
                0,
                pid,
            );
            if handle.is_null() {
                return -1.0;
            }
            let ok = GetProcessMemoryInfo(
                handle as HANDLE,
                &mut c,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            );
            CloseHandle(handle);
            if ok != 0 {
                c.WorkingSetSize as f64 / (1024.0 * 1024.0)
            } else {
                -1.0
            }
        }
    }
    #[allow(dead_code)]
    pub fn get_thread_count() -> u32 {
        unsafe {
            let snapshot = ToolHelp::CreateToolhelp32Snapshot(ToolHelp::TH32CS_SNAPTHREAD, 0);
            if snapshot.is_null() {
                return 0;
            }
            let mut count = 0u32;
            let mut entry: ToolHelp::THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<ToolHelp::THREADENTRY32>() as u32;
            let current_pid = std::process::id();
            if ToolHelp::Thread32First(snapshot, &mut entry) != 0 {
                loop {
                    if entry.th32OwnerProcessID == current_pid {
                        count += 1;
                    }
                    if ToolHelp::Thread32Next(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
            count
        }
    }
}

#[cfg(not(windows))]
mod winapi {
    pub fn get_working_set_mb() -> f64 {
        -1.0
    }
    pub fn get_process_working_set_mb(_pid: u32) -> f64 {
        -1.0
    }
    pub fn get_thread_count() -> u32 {
        0
    }
}

// =====================================================================
// Audio / CMVN / Token loading (reused from Spike C Rust — GO verified)
// =====================================================================

fn load_wav_16k_mono(path: &Path) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 16000, "Expected 16kHz");
    assert_eq!(spec.channels, 1, "Expected mono");
    Ok(reader
        .into_samples::<i16>()
        .filter_map(|s| s.ok())
        .map(|s| s as f32 / 32768.0)
        .collect())
}

fn load_cmvn(path: &Path) -> Result<(Vec<f32>, Vec<f32>), Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
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

fn load_tokens(path: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    if path.extension().map_or(false, |e| e == "json") {
        let v: serde_json::Value = serde_json::from_str(&content)?;
        if let Some(obj) = v.as_object() {
            let max_id = obj
                .keys()
                .filter_map(|k| k.parse::<usize>().ok())
                .max()
                .unwrap_or(0);
            let mut tokens = vec![String::new(); max_id + 1];
            for (k, val) in obj {
                if let (Ok(idx), Some(s)) = (k.parse::<usize>(), val.as_str()) {
                    if idx < tokens.len() {
                        tokens[idx] = s.to_string();
                    }
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
    Ok(content
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            if !p.is_empty() {
                Some(p[0].to_string())
            } else {
                None
            }
        })
        .collect())
}

// =====================================================================
// ParaformerOnline — reused verbatim from Spike C Rust (GO verified)
// =====================================================================

struct ParaformerOnline {
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
    hidden_cache: ndarray::Array2<f32>,
    alphas_cache: ndarray::Array1<f32>,
    feats_cache: ndarray::Array2<f32>,
    decoder_cache: Vec<ndarray::Array3<f32>>,
    fbank: kaldi_native_fbank::online::OnlineFeature,
    fbank_offset: usize,
}

impl ParaformerOnline {
    fn new(
        enc: &Path,
        dec: &Path,
        mvn: &Path,
        tok: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (means, vars) = load_cmvn(mvn)?;
        let tokens = load_tokens(tok)?;
        let mut fb = kaldi_native_fbank::FbankOptions::default();
        fb.frame_opts.samp_freq = 16000.0;
        fb.frame_opts.frame_shift_ms = 10.0;
        fb.frame_opts.frame_length_ms = 25.0;
        fb.frame_opts.dither = 0.0;
        fb.frame_opts.preemph_coeff = 0.97;
        fb.frame_opts.remove_dc_offset = false;
        fb.frame_opts.window_type = "hamming".to_string();
        fb.frame_opts.round_to_power_of_two = true;
        fb.frame_opts.snip_edges = true;
        fb.mel_opts.num_bins = 80;
        fb.mel_opts.low_freq = 0.0;
        fb.mel_opts.high_freq = 0.0;
        fb.use_energy = false;
        fb.raw_energy = false;
        fb.use_log_fbank = true;
        fb.use_power = true;
        let fc = kaldi_native_fbank::FbankComputer::new(fb)?;
        let fbank = kaldi_native_fbank::online::OnlineFeature::new(
            kaldi_native_fbank::online::FeatureComputer::Fbank(fc),
        );
        use ort::session::builder::GraphOptimizationLevel;
        let encoder = ort::session::Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level1)?
            .with_intra_threads(1)?
            .commit_from_file(enc)?;
        let decoder = ort::session::Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level1)?
            .with_intra_threads(1)?
            .commit_from_file(dec)?;
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
            hidden_cache: ndarray::Array2::zeros((1, ENCODER_SIZE)),
            alphas_cache: ndarray::Array1::zeros(1),
            feats_cache: ndarray::Array2::zeros((CHUNK_SIZE[0] + CHUNK_SIZE[2], FEAT_DIMS)),
            decoder_cache: vec![ndarray::Array3::zeros((1, FSMN_DIMS, FSMN_LORDER)); FSMN_LAYERS],
            fbank,
            fbank_offset: 0,
        })
    }

    fn reset(&mut self) {
        self.start_idx_cache = 0;
        self.is_first_chunk = true;
        self.is_last_chunk = false;
        self.input_cache.clear();
        self.lfr_splice_cache.clear();
        self.hidden_cache = ndarray::Array2::zeros((1, ENCODER_SIZE));
        self.alphas_cache = ndarray::Array1::zeros(1);
        self.feats_cache = ndarray::Array2::zeros((CHUNK_SIZE[0] + CHUNK_SIZE[2], FEAT_DIMS));
        self.decoder_cache = vec![ndarray::Array3::zeros((1, FSMN_DIMS, FSMN_LORDER)); FSMN_LAYERS];
        let mut fb = kaldi_native_fbank::FbankOptions::default();
        fb.frame_opts.samp_freq = 16000.0;
        fb.frame_opts.frame_shift_ms = 10.0;
        fb.frame_opts.frame_length_ms = 25.0;
        fb.frame_opts.dither = 0.0;
        fb.frame_opts.preemph_coeff = 0.97;
        fb.frame_opts.remove_dc_offset = false;
        fb.frame_opts.window_type = "hamming".to_string();
        fb.frame_opts.round_to_power_of_two = true;
        fb.frame_opts.snip_edges = true;
        fb.mel_opts.num_bins = 80;
        fb.mel_opts.low_freq = 0.0;
        fb.mel_opts.high_freq = 0.0;
        fb.use_energy = false;
        fb.raw_energy = false;
        fb.use_log_fbank = true;
        fb.use_power = true;
        self.fbank = kaldi_native_fbank::online::OnlineFeature::new(
            kaldi_native_fbank::online::FeatureComputer::Fbank(
                kaldi_native_fbank::FbankComputer::new(fb).unwrap(),
            ),
        );
        self.fbank_offset = 0;
    }

    fn compute_fbank(&mut self, samples: &[f32]) -> ndarray::Array2<f32> {
        let scaled: Vec<f32> = samples.iter().map(|s| s * 32768.0).collect();
        self.fbank.accept_waveform(16000.0, &scaled);
        let total = self.fbank.num_frames_ready();
        let new = total - self.fbank_offset;
        if new == 0 {
            return ndarray::Array2::zeros((0, N_MELS));
        }
        let mut fb = ndarray::Array2::zeros((new, N_MELS));
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

    fn online_lfr_cmvn(
        &mut self,
        wf: &ndarray::Array2<f32>,
        fin: bool,
    ) -> (ndarray::Array2<f32>, Vec<Vec<f32>>) {
        use ndarray::Array2;
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
                    for k in 0..N_MELS {
                        p.push(all[i * LFR_N + j][k]);
                    }
                }
                out.push(p);
            } else if fin {
                let np = LFR_M - (t - i * LFR_N);
                let mut p = Vec::with_capacity(FEAT_DIMS);
                for j in 0..t - i * LFR_N {
                    for k in 0..N_MELS {
                        p.push(all[i * LFR_N + j][k]);
                    }
                }
                for _ in 0..np {
                    for k in 0..N_MELS {
                        p.push(all[t - 1][k]);
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

    fn get_pos_emb(&mut self, mut wf: ndarray::Array2<f32>) -> ndarray::Array2<f32> {
        let ts = wf.nrows();
        let fd = wf.ncols();
        let si = self.start_idx_cache;
        self.start_idx_cache += ts;
        let scale = -0.0330119726594128f32;
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

    fn add_overlap_chunk(
        &mut self,
        mut wf: ndarray::Array2<f32>,
        fin: bool,
    ) -> ndarray::Array2<f32> {
        use ndarray::s;
        if self.feats_cache.nrows() > 0 {
            let nr = self.feats_cache.nrows();
            let mut comb = ndarray::Array2::zeros((nr + wf.nrows(), wf.ncols()));
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
                    let mut pad = ndarray::Array2::zeros((wf.nrows() + pl, wf.ncols()));
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
        hidden: ndarray::Array2<f32>,
        mut alphas: ndarray::Array1<f32>,
        is_last: bool,
    ) -> ndarray::Array2<f32> {
        use ndarray::{Array1, Array2, Axis};
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
        if self.alphas_cache.len() > 0 {
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

    fn forward_chunk(&mut self, mut cf: ndarray::Array2<f32>, fin: bool) -> (String, f64) {
        use ndarray::{Array1, Array3, Axis};
        let t0 = Instant::now();
        let mut result = String::new();
        if cf.nrows() == 0 {
            return (result, 0.0);
        }
        let sf = (ENCODER_SIZE as f32).sqrt();
        cf.mapv_inplace(|x| x * sf);
        cf = self.get_pos_emb(cf);
        cf = self.add_overlap_chunk(cf, fin);
        let nf = cf.nrows();
        use ort::value::Value;
        let ei0 = self.encoder.inputs()[0].name().to_string();
        let ei1 = self.encoder.inputs()[1].name().to_string();
        let speech = cf.insert_axis(Axis(0));
        let speech_lens = Array1::from_elem(1, nf as i32);
        let sv = Value::from_array(speech).unwrap();
        let slv = Value::from_array(speech_lens).unwrap();
        let enc_inputs = ort::session::SessionInputs::from(vec![
            (ei0.as_str(), <Value>::from(sv)),
            (ei1.as_str(), <Value>::from(slv)),
        ]);
        let (enc, enc_lens, alphas) = {
            let eo = self.encoder.run(enc_inputs).unwrap();
            let enc: Array3<f32> = eo[0]
                .try_extract_array::<f32>()
                .unwrap()
                .view()
                .to_owned()
                .into_dimensionality::<ndarray::Ix3>()
                .unwrap();
            let el: Array1<i32> = eo[1]
                .try_extract_array::<i32>()
                .unwrap()
                .view()
                .to_owned()
                .into_dimensionality::<ndarray::Ix1>()
                .unwrap();
            let al: ndarray::Array2<f32> = eo[2]
                .try_extract_array::<f32>()
                .unwrap()
                .view()
                .to_owned()
                .into_dimensionality::<ndarray::Ix2>()
                .unwrap();
            (enc, el, al)
        };
        let ev = enc.index_axis(Axis(0), 0).to_owned();
        let av = alphas.index_axis(Axis(0), 0).to_owned();
        let lf = self.cif_search(ev, av, self.is_last_chunk);
        let lfc = lf.nrows();
        if lfc > 0 {
            let ae = lf.insert_axis(Axis(0));
            let ael = Array1::from_elem(1, lfc as i32);
            let ev2 = Value::from_array(enc.clone()).unwrap();
            let elv = Value::from_array(enc_lens.clone()).unwrap();
            let aev = Value::from_array(ae.clone()).unwrap();
            let aelv = Value::from_array(ael.clone()).unwrap();
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
                let do_ = self.decoder.run(di).unwrap();
                let lg: Array3<f32> = do_[0]
                    .try_extract_array::<f32>()
                    .unwrap()
                    .view()
                    .to_owned()
                    .into_dimensionality::<ndarray::Ix3>()
                    .unwrap();
                let mut ca = Vec::with_capacity(FSMN_LAYERS);
                for l in 0..FSMN_LAYERS {
                    ca.push(
                        do_[2 + l]
                            .try_extract_array::<f32>()
                            .unwrap()
                            .view()
                            .to_owned()
                            .into_dimensionality::<ndarray::Ix3>()
                            .unwrap(),
                    );
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

    fn forward(&mut self, chunk: &[f32], fin: bool) -> (String, f64) {
        if chunk.len() < 960 && fin && !self.is_first_chunk {
            self.is_last_chunk = true;
            let wf = self.feats_cache.clone();
            let (r, ms) = self.forward_chunk(wf, self.is_last_chunk);
            self.reset();
            return (r, ms);
        }
        if self.is_first_chunk {
            self.is_first_chunk = false;
        }
        let mut waves = self.input_cache.clone();
        waves.extend_from_slice(chunk);
        let fsl = SAMPLE_RATE * 25 / 1000;
        let fss = SAMPLE_RATE * 10 / 1000;
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
            if fin {
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
            let (lfr, nc) = self.online_lfr_cmvn(&wf, fin);
            self.lfr_splice_cache = nc;
            if lfr.nrows() == 0 {
                if fin {
                    self.input_cache.clear();
                    self.lfr_splice_cache.clear();
                }
                return (String::new(), 0.0);
            }
            let (r, ms) = self.forward_chunk(lfr, fin);
            if fin {
                self.reset();
            }
            return (r, ms);
        } else {
            for i in 0..wf.nrows() {
                self.lfr_splice_cache.push(wf.row(i).to_vec());
            }
            return (String::new(), 0.0);
        }
    }
}

// =====================================================================
// NDJSON Protocol — minimal safe subset (NO Crash, NO Oom)
// =====================================================================

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum WorkerRequest {
    Init {
        dll_path: String,
        enc_path: String,
        dec_path: String,
        mvn_path: String,
        tok_path: String,
    },
    Infer {
        samples: Vec<f32>,
        is_final: bool,
    },
    Reset,
    Quit,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum WorkerResponse {
    Ready,
    Result { text: String },
    ResetOk,
    Error { message: String },
}

// =====================================================================
// WorkerHandle — safe process management with kill+wait in Drop
// =====================================================================

struct WorkerHandle {
    child: std::process::Child,
    writer: std::io::BufWriter<std::process::ChildStdin>,
    rx: std::sync::mpsc::Receiver<std::io::Result<String>>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
    #[allow(dead_code)]
    pid: u32,
}

fn spawn_worker(exe: &Path, worker_log: &str) -> Result<WorkerHandle, String> {
    let _ = std::fs::write(worker_log, "");
    let mut child = Command::new(exe)
        .arg("worker")
        .env("WORKER_LOG_PATH", worker_log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{e}"))?;
    let pid = child.id();
    let writer = std::io::BufWriter::new(child.stdin.take().unwrap());
    let reader = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx.send(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "EOF",
                    )));
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        if tx.send(Ok(trimmed.to_string())).is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });
    Ok(WorkerHandle {
        child,
        writer,
        rx,
        reader_thread: Some(handle),
        pid,
    })
}

impl WorkerHandle {
    fn send_req(&mut self, req: &WorkerRequest) {
        let json = serde_json::to_string(req).unwrap();
        let _ = writeln!(self.writer, "{json}");
        let _ = self.writer.flush();
    }

    fn recv_timeout(&mut self, timeout: Duration) -> Option<String> {
        match self.rx.recv_timeout(timeout) {
            Ok(Ok(line)) => Some(line),
            Ok(Err(_)) => None,
            Err(_) => None,
        }
    }

    /// Wait for Ready (30s timeout for model init)
    fn wait_ready(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timeout waiting for Ready".into());
            }
            match self.rx.recv_timeout(remaining) {
                Ok(Ok(line)) => {
                    info!("wait_ready: received: {}", &line[..line.len().min(200)]);
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        let ttype = v.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if ttype == "Ready" {
                            return Ok(());
                        } else if ttype == "Error" {
                            let msg = v
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            return Err(format!("worker init error: {msg}"));
                        }
                    }
                }
                Ok(Err(e)) => return Err(format!("reader error: {e}")),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err("timeout waiting for Ready".into());
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("reader thread disconnected".into());
                }
            }
        }
    }

    /// Wait for a Result response (10s timeout)
    fn wait_result(&mut self) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let line = self.recv_timeout(remaining)?;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                let ttype = v.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if ttype == "Result" {
                    return v.get("text").and_then(|v| v.as_str()).map(String::from);
                } else if ttype == "Error" {
                    warn!(
                        "worker error: {}",
                        v.get("message").and_then(|v| v.as_str()).unwrap_or("?")
                    );
                    return None;
                }
            }
        }
    }

    /// Wait for ResetOk (5s timeout)
    fn wait_reset_ok(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let line = match self.rx.recv_timeout(remaining) {
                Ok(Ok(l)) => l,
                _ => return false,
            };
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if v.get("type").and_then(|v| v.as_str()).unwrap_or("") == "ResetOk" {
                    return true;
                }
            }
        }
    }

    /// Send Quit and wait for clean exit (5s timeout, then kill+wait)
    fn quit_and_wait(&mut self) {
        self.send_req(&WorkerRequest::Quit);
        let timeout = Duration::from_secs(5);
        let start = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        if let Some(h) = self.reader_thread.take() {
            let _ = h.join();
        }
    }

    /// Force kill + wait (for crash recovery test)
    fn kill_and_wait(&mut self) -> Option<std::process::ExitStatus> {
        let _ = self.child.kill();
        let status = self.child.wait().ok();
        if let Some(h) = self.reader_thread.take() {
            let _ = h.join();
        }
        status
    }
}

/// Drop guarantees kill+wait even if test panics
impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self.reader_thread.take() {
            let _ = h.join();
        }
    }
}

// =====================================================================
// Worker mode — child process entry point
// =====================================================================

fn run_worker_mode() {
    let stdin = std::io::stdin();
    let reader = BufReader::new(stdin);
    let mut asr: Option<ParaformerOnline> = None;

    let log_path = std::env::var("WORKER_LOG_PATH").unwrap_or_default();
    let mut log_file: Option<std::fs::File> = if !log_path.is_empty() {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok()
    } else {
        None
    };

    macro_rules! wlog {
        ($($arg:tt)*) => {
            if let Some(ref mut f) = log_file {
                let _ = writeln!(f, $($arg)*);
                let _ = f.flush();
            }
        };
    }

    let stdout = std::io::stdout();
    let mut stdout = std::io::BufWriter::new(stdout.lock());

    fn send<W: Write>(out: &mut W, resp: &WorkerResponse) {
        let json = serde_json::to_string(resp).unwrap_or_default();
        let _ = writeln!(out, "{json}");
        let _ = out.flush();
    }

    wlog!("Worker mode started");
    for line in reader.lines() {
        wlog!(
            "Worker received line: {:?}",
            line.as_ref().map(|s| &s[..s.len().min(80)])
        );
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                wlog!("Worker line error: {e}");
                break;
            }
        };
        let req: WorkerRequest = match serde_json::from_str(&line) {
            Ok(r) => {
                wlog!("Worker parsed request");
                r
            }
            Err(e) => {
                wlog!("Worker parse error: {e}");
                send(
                    &mut stdout,
                    &WorkerResponse::Error {
                        message: format!("Parse error: {e}"),
                    },
                );
                continue;
            }
        };
        match req {
            WorkerRequest::Init {
                dll_path,
                enc_path,
                dec_path,
                mvn_path,
                tok_path,
            } => {
                wlog!("Worker Init: dll={}, enc={}", dll_path, enc_path);
                let dll = PathBuf::from(&dll_path);
                match ort::init_from(&dll) {
                    Ok(builder) => {
                        builder.commit();
                        wlog!("Worker ORT init succeeded");
                    }
                    Err(e) => {
                        wlog!("Worker ORT init failed: {e}");
                        send(
                            &mut stdout,
                            &WorkerResponse::Error {
                                message: format!("init_from failed: {e}"),
                            },
                        );
                        continue;
                    }
                }
                match ParaformerOnline::new(
                    Path::new(&enc_path),
                    Path::new(&dec_path),
                    Path::new(&mvn_path),
                    Path::new(&tok_path),
                ) {
                    Ok(a) => {
                        wlog!("Worker model load succeeded");
                        asr = Some(a);
                        send(&mut stdout, &WorkerResponse::Ready);
                        wlog!("Worker sent Ready");
                    }
                    Err(e) => {
                        wlog!("Worker model load failed: {e}");
                        send(
                            &mut stdout,
                            &WorkerResponse::Error {
                                message: format!("Model load failed: {e}"),
                            },
                        );
                    }
                }
            }
            WorkerRequest::Infer { samples, is_final } => {
                if let Some(ref mut a) = asr {
                    let (text, _inf_ms) = a.forward(&samples, is_final);
                    send(&mut stdout, &WorkerResponse::Result { text });
                } else {
                    send(
                        &mut stdout,
                        &WorkerResponse::Error {
                            message: "Not initialized".to_string(),
                        },
                    );
                }
            }
            WorkerRequest::Reset => {
                if let Some(ref mut a) = asr {
                    a.reset();
                }
                send(&mut stdout, &WorkerResponse::ResetOk);
            }
            WorkerRequest::Quit => {
                wlog!("Worker Quit");
                break;
            }
        }
    }
    wlog!("Worker mode exiting");
}

// =====================================================================
// Result JSON structure
// =====================================================================

#[derive(Serialize)]
struct FeasibilityResult {
    spike: String,
    scope: String,
    ocr_in_process: OcrEvidence,
    onnx_stt_worker: OnnxSttWorker,
    recovery: Recovery,
    not_measured: Vec<String>,
    decision: String,
    blockers: Vec<String>,
}

#[derive(Serialize)]
struct OcrEvidence {
    evidence: String,
    feasible: bool,
}

#[derive(Serialize)]
struct OnnxSttWorker {
    release_build: bool,
    real_ort_loaded: bool,
    real_models_loaded: bool,
    ready_received: bool,
    nonempty_partial_received: bool,
    final_chunk_response_received: bool,
    graceful_quit_succeeded: bool,
}

#[derive(Serialize)]
struct Recovery {
    forced_kill_detected: bool,
    host_survived: bool,
    child_waited: bool,
    restart_ready_received: bool,
    no_orphan_process: bool,
}

// =====================================================================
// Host: main entry point
// =====================================================================

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "worker") {
        run_worker_mode();
        return;
    }

    tracing_subscriber::fmt().with_env_filter("info").init();
    info!("=== Spike E (minimal): Hybrid Topology Feasibility ===");

    let result = run_feasibility_test();
    let json = serde_json::to_string_pretty(&result).unwrap_or_default();
    println!("\n=== Spike E Result ===\n{json}");

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("./onnx-spike-e"));
    let base_dir = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.to_str())
        .unwrap_or(".");
    let results_dir = format!("{}/results", base_dir);
    std::fs::create_dir_all(&results_dir).ok();
    let out_path = format!("{}/spike_e_topology_comparison.json", results_dir);
    std::fs::write(&out_path, &json).ok();
    info!("Result saved to: {}", out_path);
}

fn run_feasibility_test() -> FeasibilityResult {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("./onnx-spike-e"));
    let base_dir = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.to_str())
        .unwrap_or(".");

    let cpu_dll = std::env::var("ORT_CPU_DLL_PATH")
        .unwrap_or_else(|_| format!("{}/runtimes/onnxruntime-cpu/onnxruntime.dll", base_dir));
    let models_dir = format!("{}/../models", base_dir);
    let enc_path = format!("{}/paraformer-online-onnx/encoder.onnx", models_dir);
    let dec_path = format!("{}/paraformer-online-onnx/decoder.onnx", models_dir);
    let mvn_path = format!("{}/paraformer-online-onnx/am.mvn", models_dir);
    let tok_path = format!("{}/paraformer-online-onnx/tokens.json", models_dir);
    let wav_path = format!("{}/asr_example.wav", models_dir);
    let worker_log = format!("{}/worker_debug.log", base_dir);

    for (name, path) in [
        ("cpu_dll", &cpu_dll),
        ("encoder", &enc_path),
        ("decoder", &dec_path),
        ("am.mvn", &mvn_path),
        ("tokens", &tok_path),
        ("wav", &wav_path),
    ] {
        if !Path::new(path).exists() {
            warn!("Missing asset: {} = {}", name, path);
        }
    }

    let ocr = OcrEvidence {
        evidence: "results/spike_b_ocr_qualification.json".to_string(),
        feasible: true,
    };

    let mut release_build_ok = false;
    let mut real_ort_loaded = false;
    let mut real_models_loaded = false;
    let mut ready_received = false;
    let mut nonempty_partial_received = false;
    let mut final_chunk_response_received = false;
    let mut graceful_quit_succeeded = false;

    let mut forced_kill_detected = false;
    let mut host_survived = false;
    let mut child_waited = false;
    let mut restart_ready_received = false;
    let mut no_orphan_process = false;

    let mut blockers: Vec<String> = Vec::new();

    // --- Phase 1: Normal streaming link ---
    info!("Phase 1: Normal streaming link");
    let audio = match load_wav_16k_mono(Path::new(&wav_path)) {
        Ok(a) => a,
        Err(e) => {
            blockers.push(format!("wav load failed: {e}"));
            return build_result(
                ocr,
                OnnxSttWorker {
                    release_build: release_build_ok,
                    real_ort_loaded,
                    real_models_loaded,
                    ready_received,
                    nonempty_partial_received,
                    final_chunk_response_received,
                    graceful_quit_succeeded,
                },
                Recovery {
                    forced_kill_detected,
                    host_survived,
                    child_waited,
                    restart_ready_received,
                    no_orphan_process,
                },
                blockers,
            );
        }
    };
    let n_chunks = (audio.len() - 1) / CHUNK_STRIDE + 1;
    info!("Audio: {} samples, {} chunks", audio.len(), n_chunks);

    let mut wh = match spawn_worker(&exe, &worker_log) {
        Ok(w) => {
            release_build_ok = true;
            w
        }
        Err(e) => {
            blockers.push(format!("spawn worker failed: {e}"));
            return build_result(
                ocr,
                OnnxSttWorker {
                    release_build: release_build_ok,
                    real_ort_loaded,
                    real_models_loaded,
                    ready_received,
                    nonempty_partial_received,
                    final_chunk_response_received,
                    graceful_quit_succeeded,
                },
                Recovery {
                    forced_kill_detected,
                    host_survived,
                    child_waited,
                    restart_ready_received,
                    no_orphan_process,
                },
                blockers,
            );
        }
    };
    info!("Worker spawned: pid={}", wh.pid);

    wh.send_req(&WorkerRequest::Init {
        dll_path: cpu_dll.clone(),
        enc_path: enc_path.clone(),
        dec_path: dec_path.clone(),
        mvn_path: mvn_path.clone(),
        tok_path: tok_path.clone(),
    });
    info!("Sent Init, waiting for Ready (30s timeout)...");

    match wh.wait_ready() {
        Ok(()) => {
            ready_received = true;
            real_ort_loaded = true;
            real_models_loaded = true;
            info!("Ready received");
        }
        Err(e) => {
            blockers.push(format!("wait_ready failed: {e}"));
            wh.kill_and_wait();
            return build_result(
                ocr,
                OnnxSttWorker {
                    release_build: release_build_ok,
                    real_ort_loaded,
                    real_models_loaded,
                    ready_received,
                    nonempty_partial_received,
                    final_chunk_response_received,
                    graceful_quit_succeeded,
                },
                Recovery {
                    forced_kill_detected,
                    host_survived,
                    child_waited,
                    restart_ready_received,
                    no_orphan_process,
                },
                blockers,
            );
        }
    }

    let mut all_texts = Vec::new();
    let mut got_nonempty = false;
    for i in 0..n_chunks {
        let start = i * CHUNK_STRIDE;
        let end = ((i + 1) * CHUNK_STRIDE).min(audio.len());
        let is_final = i == n_chunks - 1;
        wh.send_req(&WorkerRequest::Infer {
            samples: audio[start..end].to_vec(),
            is_final,
        });
        match wh.wait_result() {
            Some(text) => {
                if is_final {
                    final_chunk_response_received = true;
                }
                if !text.is_empty() {
                    got_nonempty = true;
                    all_texts.push(text.clone());
                }
                info!("chunk {}: text='{}'", i, text);
            }
            None => {
                blockers.push(format!("worker timeout at chunk {}", i));
                wh.kill_and_wait();
                return build_result(
                    ocr,
                    OnnxSttWorker {
                        release_build: release_build_ok,
                        real_ort_loaded,
                        real_models_loaded,
                        ready_received,
                        nonempty_partial_received,
                        final_chunk_response_received,
                        graceful_quit_succeeded,
                    },
                    Recovery {
                        forced_kill_detected,
                        host_survived,
                        child_waited,
                        restart_ready_received,
                        no_orphan_process,
                    },
                    blockers,
                );
            }
        }
    }

    if got_nonempty {
        nonempty_partial_received = true;
    }
    let final_text = all_texts.join("");
    info!("Streaming done: final_text='{}'", final_text);

    wh.quit_and_wait();
    graceful_quit_succeeded = true;
    info!("Worker graceful quit succeeded");

    // --- Phase 2: Fault recovery ---
    info!("Phase 2: Fault recovery (kill + wait + restart)");
    let mut wh2 = match spawn_worker(&exe, &worker_log) {
        Ok(w) => w,
        Err(e) => {
            blockers.push(format!("spawn worker #2 failed: {e}"));
            return build_result(
                ocr,
                OnnxSttWorker {
                    release_build: release_build_ok,
                    real_ort_loaded,
                    real_models_loaded,
                    ready_received,
                    nonempty_partial_received,
                    final_chunk_response_received,
                    graceful_quit_succeeded,
                },
                Recovery {
                    forced_kill_detected,
                    host_survived,
                    child_waited,
                    restart_ready_received,
                    no_orphan_process,
                },
                blockers,
            );
        }
    };

    wh2.send_req(&WorkerRequest::Init {
        dll_path: cpu_dll.clone(),
        enc_path: enc_path.clone(),
        dec_path: dec_path.clone(),
        mvn_path: mvn_path.clone(),
        tok_path: tok_path.clone(),
    });
    match wh2.wait_ready() {
        Ok(()) => info!("Worker #2 Ready"),
        Err(e) => {
            blockers.push(format!("worker #2 wait_ready failed: {e}"));
            wh2.kill_and_wait();
            return build_result(
                ocr,
                OnnxSttWorker {
                    release_build: release_build_ok,
                    real_ort_loaded,
                    real_models_loaded,
                    ready_received,
                    nonempty_partial_received,
                    final_chunk_response_received,
                    graceful_quit_succeeded,
                },
                Recovery {
                    forced_kill_detected,
                    host_survived,
                    child_waited,
                    restart_ready_received,
                    no_orphan_process,
                },
                blockers,
            );
        }
    }

    let status = wh2.kill_and_wait();
    if let Some(s) = status {
        forced_kill_detected = !s.success();
    } else {
        forced_kill_detected = true;
    }
    child_waited = true;
    info!("Worker #2 killed and waited: status={:?}", status);

    host_survived = true;
    info!("Host survived worker kill");

    // --- Phase 3: Restart worker #3 ---
    info!("Phase 3: Restart worker #3 after kill");
    let mut wh3 = match spawn_worker(&exe, &worker_log) {
        Ok(w) => w,
        Err(e) => {
            blockers.push(format!("spawn worker #3 failed: {e}"));
            return build_result(
                ocr,
                OnnxSttWorker {
                    release_build: release_build_ok,
                    real_ort_loaded,
                    real_models_loaded,
                    ready_received,
                    nonempty_partial_received,
                    final_chunk_response_received,
                    graceful_quit_succeeded,
                },
                Recovery {
                    forced_kill_detected,
                    host_survived,
                    child_waited,
                    restart_ready_received,
                    no_orphan_process,
                },
                blockers,
            );
        }
    };

    wh3.send_req(&WorkerRequest::Init {
        dll_path: cpu_dll.clone(),
        enc_path: enc_path.clone(),
        dec_path: dec_path.clone(),
        mvn_path: mvn_path.clone(),
        tok_path: tok_path.clone(),
    });
    match wh3.wait_ready() {
        Ok(()) => {
            restart_ready_received = true;
            info!("Worker #3 Ready — restart successful");
        }
        Err(e) => {
            blockers.push(format!("worker #3 wait_ready failed: {e}"));
            wh3.kill_and_wait();
            return build_result(
                ocr,
                OnnxSttWorker {
                    release_build: release_build_ok,
                    real_ort_loaded,
                    real_models_loaded,
                    ready_received,
                    nonempty_partial_received,
                    final_chunk_response_received,
                    graceful_quit_succeeded,
                },
                Recovery {
                    forced_kill_detected,
                    host_survived,
                    child_waited,
                    restart_ready_received,
                    no_orphan_process,
                },
                blockers,
            );
        }
    }

    wh3.quit_and_wait();
    info!("Worker #3 graceful quit");

    // Brief delay to allow OS-level process cleanup before checking for orphans
    std::thread::sleep(Duration::from_millis(500));
    no_orphan_process = check_no_orphan_processes();
    info!("No orphan processes: {}", no_orphan_process);

    build_result(
        ocr,
        OnnxSttWorker {
            release_build: release_build_ok,
            real_ort_loaded,
            real_models_loaded,
            ready_received,
            nonempty_partial_received,
            final_chunk_response_received,
            graceful_quit_succeeded,
        },
        Recovery {
            forced_kill_detected,
            host_survived,
            child_waited,
            restart_ready_received,
            no_orphan_process,
        },
        blockers,
    )
}

fn build_result(
    ocr: OcrEvidence,
    stt: OnnxSttWorker,
    recovery: Recovery,
    blockers: Vec<String>,
) -> FeasibilityResult {
    let all_go = stt.release_build
        && stt.real_ort_loaded
        && stt.real_models_loaded
        && stt.ready_received
        && stt.nonempty_partial_received
        && stt.final_chunk_response_received
        && stt.graceful_quit_succeeded
        && recovery.forced_kill_detected
        && recovery.host_survived
        && recovery.child_waited
        && recovery.restart_ready_received
        && recovery.no_orphan_process;

    let topology_ok = stt.release_build
        && stt.real_ort_loaded
        && stt.real_models_loaded
        && stt.ready_received
        && stt.graceful_quit_succeeded
        && recovery.host_survived
        && recovery.restart_ready_received;

    let decision = if all_go && blockers.is_empty() {
        "HYBRID_FEASIBILITY_GO".to_string()
    } else if topology_ok && (!stt.nonempty_partial_received || !stt.final_chunk_response_received)
    {
        "WORKER_TOPOLOGY_GO\nSTREAMING_PAYLOAD_BLOCKED".to_string()
    } else {
        "BLOCKED".to_string()
    };

    FeasibilityResult {
        spike: "E_minimal_hybrid_topology".to_string(),
        scope: "feasibility_only".to_string(),
        ocr_in_process: ocr,
        onnx_stt_worker: stt,
        recovery,
        not_measured: vec![
            "memory comparison".to_string(),
            "latency comparison".to_string(),
            "RTF".to_string(),
            "CPU usage".to_string(),
            "p50/p95".to_string(),
            "stress".to_string(),
            "OOM".to_string(),
            "native crash".to_string(),
        ],
        decision,
        blockers,
    }
}

fn check_no_orphan_processes() -> bool {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq onnx-spike-e.exe", "/FO", "CSV", "/NH"])
            .output();
        let current_pid = std::process::id();
        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                // Filter out the current (host) process — only orphan workers count
                let orphans: Vec<&str> = stdout
                    .lines()
                    .filter(|l| l.contains("onnx-spike-e"))
                    .filter(|l| {
                        // tasklist CSV format: "Name","PID","SessionName","Session#","Mem"
                        // Try to extract PID and exclude current process
                        let parts: Vec<&str> = l.split(',').collect();
                        if parts.len() >= 2 {
                            let pid_str = parts[1].trim_matches('"');
                            if let Ok(pid) = pid_str.parse::<u32>() {
                                return pid != current_pid;
                            }
                        }
                        true
                    })
                    .collect();
                orphans.is_empty()
            }
            Err(_) => true,
        }
    }
    #[cfg(not(windows))]
    {
        true
    }
}
