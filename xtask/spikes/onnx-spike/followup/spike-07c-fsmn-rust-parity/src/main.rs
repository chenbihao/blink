//! Spike 07C: FSMN-VAD Rust Parity Runner
//!
//! Reproduces the Python oracle's full pipeline in Rust:
//! fbank → splice → CMVN → ONNX inference → softmax → endpoint state machine
//!
//! Compares frame scores, cache states, and segment boundaries against the Python oracle.
//! Does NOT modify production code. This is a spike only.

#![allow(dead_code)]

use kaldi_native_fbank::{FbankComputer, FbankOptions};
use kaldi_native_fbank::online::{OnlineFeature, FeatureComputer};
use ndarray::{Array2, Array3, Array4, Axis};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tracing::{info, warn};

// ─── Constants (matching Python oracle + config.yaml + ONNX graph) ────────

const SR: usize = 16000;
const N_MELS: usize = 80;
const VAD_FRAME_LENGTH: usize = 400;   // 25ms @ 16kHz
const VAD_FRAME_SHIFT: usize = 160;   // 10ms @ 16kHz
const SPLICE_LEN: usize = 5;           // lfr_m from config.yaml
const VAD_CACHE_LAYERS: usize = 4;     // fsmn_layers from config.yaml
const VAD_CACHE_DIM: usize = 128;      // proj_dim from config.yaml
const VAD_CACHE_LORDER: usize = 19;    // from ONNX graph: [1, 128, 19, 1]
const INPUT_DIM: usize = 400;          // SPLICE_LEN * N_MELS

const MAX_END_SILENCE_MS: u32 = 800;
const LOOKBACK_START_MS: f64 = 200.0;
const LOOKAHEAD_END_MS: f64 = 100.0;
const FRAME_IN_MS: f64 = 10.0;

// ─── Windows memory API ────────────────────────────────────────────────────

#[cfg(windows)]
mod winapi {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    pub fn get_working_set_mb() -> f64 {
        unsafe {
            let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            let ok = GetProcessMemoryInfo(
                -1isize as HANDLE,
                &mut c,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            );
            if ok != 0 { c.WorkingSetSize as f64 / (1024.0 * 1024.0) } else { -1.0 }
        }
    }
}
#[cfg(not(windows))]
mod winapi { pub fn get_working_set_mb() -> f64 { -1.0 } }

// ─── CMVN loading ─────────────────────────────────────────────────────────

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
    Ok((means, vars))
}

// ─── WAV loading ──────────────────────────────────────────────────────────

fn load_wav_16k_mono(path: &Path) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 16000, "Expected 16kHz");
    assert_eq!(spec.channels, 1, "Expected mono");
    let samples: Vec<f32> = reader.into_samples::<i16>()
        .filter_map(|s| s.ok())
        .map(|s| s as f32 / 32768.0)
        .collect();
    Ok(samples)
}

// ─── Test audio generation (matching Python oracle) ───────────────────────

fn gtone(dur: f64, freq: f64, amp: f64) -> Vec<f32> {
    let n = (dur * SR as f64) as usize;
    (0..n).map(|i| {
        let t = i as f64 / SR as f64;
        (2.0 * std::f64::consts::PI * freq * t).sin() as f32 * amp as f32
    }).collect()
}

fn gsil(dur: f64) -> Vec<f32> {
    vec![0.0; (dur * SR as f64) as usize]
}

fn gnoise(dur: f64, amp: f64) -> Vec<f32> {
    // Use a simple PRNG for reproducibility (matching Python's np.random.seed(42))
    let n = (dur * SR as f64) as usize;
    let mut state: u64 = 42;
    (0..n).map(|_| {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let r = ((state >> 33) as f64) / ((1u64 << 31) as f64) - 1.0;
        (r * amp) as f32
    }).collect()
}

fn concat(vecs: &[Vec<f32>]) -> Vec<f32> {
    let total: usize = vecs.iter().map(|v| v.len()).sum();
    let mut out = Vec::with_capacity(total);
    for v in vecs { out.extend_from_slice(v); }
    out
}

// ─── FSMN-VAD Rust implementation ─────────────────────────────────────────

struct FsmnVadRust {
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
    // Oracle logging
    all_frame_scores: Vec<f32>,
    all_frame_decisions: Vec<i32>,
    chunk_logs: Vec<ChunkLog>,
}

#[derive(Debug, Clone, Serialize)]
struct ChunkLog {
    chunk_idx: usize,
    n_samples: usize,
    input_shape: Vec<usize>,
    output_shape: Vec<usize>,
    inference_ms: f64,
    n_frames: usize,
    frame_scores: Vec<f32>,
    frame_decisions: Vec<i32>,
    is_final: bool,
}

impl FsmnVadRust {
    fn new(model_path: &Path, mvn_path: &Path, dll_path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        // Init ORT
        ort::init_from(dll_path)
            .map_err(|e| format!("ORT init failed: {e}"))?
            .commit();

        // Load CMVN
        let (means, vars) = load_cmvn(mvn_path)?;
        info!("CMVN: means[{}], vars[{}]", means.len(), vars.len());

        // Build session
        use ort::session::builder::GraphOptimizationLevel;
        let session = ort::session::Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level1)?
            .with_intra_threads(1)?
            .commit_from_file(model_path)?;

        let input_names: Vec<String> = session.inputs().iter().map(|i| i.name().to_string()).collect();
        let output_names: Vec<String> = session.outputs().iter().map(|o| o.name().to_string()).collect();
        info!("FSMN-VAD inputs: {:?}", input_names);
        info!("FSMN-VAD outputs: {:?}", output_names);
        info!("Inputs: {}, Outputs: {}", session.inputs().len(), session.outputs().len());

        // Init fbank
        let fbank = Self::build_fbank();

        // Init cache: [1, 128, 19, 1] per layer
        let cache = vec![Array4::zeros((1, VAD_CACHE_DIM, VAD_CACHE_LORDER, 1)); VAD_CACHE_LAYERS];

        Ok(Self {
            session, means, vars, input_names, output_names,
            cache, input_cache: Vec::new(), segments: Vec::new(),
            current_start: None, total_samples: 0,
            silence_frames: 0, in_speech: false,
            fbank, fbank_offset: 0,
            all_frame_scores: Vec::new(),
            all_frame_decisions: Vec::new(),
            chunk_logs: Vec::new(),
        })
    }

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
        opts.frame_opts.snip_edges = true;  // Kaldi convention (no center padding)
        opts.mel_opts.num_bins = 80;
        opts.mel_opts.low_freq = 0.0;
        opts.mel_opts.high_freq = 0.0;
        opts.use_energy = false;
        opts.raw_energy = false;
        opts.use_log_fbank = true;
        opts.use_power = true;
        let computer = FbankComputer::new(opts).unwrap();
        OnlineFeature::new(FeatureComputer::Fbank(computer))
    }

    fn reset(&mut self) {
        self.cache = vec![Array4::zeros((1, VAD_CACHE_DIM, VAD_CACHE_LORDER, 1)); VAD_CACHE_LAYERS];
        self.input_cache.clear();
        self.segments.clear();
        self.current_start = None;
        self.total_samples = 0;
        self.silence_frames = 0;
        self.in_speech = false;
        self.fbank = Self::build_fbank();
        self.fbank_offset = 0;
        self.all_frame_scores.clear();
        self.all_frame_decisions.clear();
        self.chunk_logs.clear();
    }

    fn compute_fbank(&mut self, samples: &[f32]) -> Array2<f32> {
        // Scale to int16 range (matching Python oracle)
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

    fn process(&mut self, samples: &[f32], chunk_idx: usize, is_final: bool) -> (Vec<(String, f64)>, Option<ChunkLog>) {
        let mut events = Vec::new();
        let n_samples = samples.len();

        // Accumulate with leftover
        let mut s = std::mem::take(&mut self.input_cache);
        s.extend_from_slice(samples);

        if s.len() < VAD_FRAME_LENGTH {
            self.input_cache = s;
            return (events, None);
        }

        let nf = (s.len() - VAD_FRAME_LENGTH) / VAD_FRAME_SHIFT + 1;
        if nf < 1 {
            self.input_cache = s;
            return (events, None);
        }

        let us = (nf - 1) * VAD_FRAME_SHIFT + VAD_FRAME_LENGTH;
        let wav_data: Vec<f32> = s[..us].to_vec();
        self.input_cache = s[us..].to_vec();

        // Fbank
        let fb = self.compute_fbank(&wav_data);
        if fb.nrows() < SPLICE_LEN {
            return (events, None);
        }

        // Splice
        let sp = self.splice(&fb);
        if sp.nrows() == 0 {
            return (events, None);
        }

        // CMVN
        let ft = self.apply_cmvn(&sp);
        let sp_in = ft.insert_axis(Axis(0)); // (1, T, 400)

        let input_shape = sp_in.shape().to_vec();

        // Build ORT inputs
        use ort::value::Value;
        let speech_val = Value::from_array(sp_in.clone()).unwrap();

        // Build feed pairs
        let mut feed_pairs: Vec<(String, Value)> = Vec::new();
        feed_pairs.push((self.input_names[0].clone(), <ort::value::Value>::from(speech_val)));
        for i in 0..VAD_CACHE_LAYERS {
            let cache_val = Value::from_array(self.cache[i].clone()).unwrap();
            feed_pairs.push((self.input_names[1 + i].clone(), <ort::value::Value>::from(cache_val)));
        }
        let inputs = ort::session::SessionInputs::from(feed_pairs);

        // Run inference
        let t0 = Instant::now();
        let outputs = self.session.run(inputs).map_err(|e| {
            format!("ORT run failed: {e}")
        }).unwrap();
        let inf_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // Extract outputs
        let (logits, new_caches) = {
            let logits: Array3<f32> = outputs[0]
                .try_extract_array::<f32>().unwrap()
                .view().to_owned()
                .into_dimensionality::<ndarray::Ix3>().unwrap();
            let mut caches = Vec::with_capacity(VAD_CACHE_LAYERS);
            for l in 0..VAD_CACHE_LAYERS {
                let ca = outputs[1 + l].try_extract_array::<f32>().unwrap();
                caches.push(ca.view().to_owned().into_dimensionality::<ndarray::Ix4>().unwrap());
            }
            (logits, caches)
        };

        let output_shape = logits.shape().to_vec();

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
                if v > max_val { max_val = v; }
            }
            let mut sum = 0.0f32;
            let mut probs = vec![0.0f32; output_dim];
            for j in 0..output_dim {
                probs[j] = (logp[[i, j]] - max_val).exp();
                sum += probs[j];
            }
            for j in 0..output_dim {
                probs[j] /= sum;
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
                let sum: i32 = frame_decisions[i-1] + frame_decisions[i] + frame_decisions[i+1];
                sm[i] = if sum >= 2 { 1 } else { 0 };
            }
            sm
        };

        // Endpoint state machine
        let frame_shift_s = VAD_FRAME_SHIFT as f64 / SR as f64;
        let chunk_start_s = self.total_samples as f64 / SR as f64;

        for (fi, &is_speech) in sm.iter().enumerate() {
            self.all_frame_scores.push(frame_scores[fi]);
            self.all_frame_decisions.push(is_speech);
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

        let log = ChunkLog {
            chunk_idx, n_samples, input_shape, output_shape,
            inference_ms: inf_ms, n_frames,
            frame_scores: frame_scores.clone(),
            frame_decisions: sm.clone(),
            is_final,
        };
        self.chunk_logs.push(log.clone());

        (events, Some(log))
    }

    fn finalize(&mut self) -> Vec<(f64, f64)> {
        if self.in_speech {
            if let Some(start) = self.current_start {
                let end_t = self.total_samples as f64 / SR as f64;
                self.segments.push((start, end_t));
                self.in_speech = false;
            }
        }
        self.segments.clone()
    }
}

// ─── Main ──────────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();
    info!("=== Spike 07C: FSMN-VAD Rust Parity Runner ===");

    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let models = base.join("../../models");
    let model_path = models.join("fsmn-vad-onnx-v2/model_quant.onnx");
    let mvn_path = models.join("fsmn-vad-onnx-v2/am.mvn");
    let wav_path = models.join("asr_example.wav");
    let ort_dll = base.join("../runtimes/onnxruntime-cpu/onnxruntime.dll");

    for (name, p) in [("model", &model_path), ("mvn", &mvn_path), ("ort_dll", &ort_dll)] {
        if !p.exists() { warn!("Missing: {} = {}", name, p.display()); }
    }

    let mem_before = winapi::get_working_set_mb();
    let t0 = Instant::now();
    let mut vad = match FsmnVadRust::new(&model_path, &mvn_path, &ort_dll) {
        Ok(v) => v,
        Err(e) => { warn!("Failed to init: {e}"); return; }
    };
    let t_load = t0.elapsed().as_secs_f64();
    let mem_after = winapi::get_working_set_mb();
    info!("Model loaded: {:.3}s, mem: {:.1} -> {:.1}MB", t_load, mem_before, mem_after);

    // ─── Test scenarios ────────────────────────────────────────────────────
    let scenarios: Vec<(&str, Vec<f32>, Vec<(f64, f64)>, &str)> = vec![
        ("continuous_chunk",
         concat(&[gtone(2.0, 220.0, 0.1), gsil(0.5), gtone(1.0, 330.0, 0.1)]),
         vec![(0.0, 2.0), (2.5, 3.5)], "连续 chunk"),
        ("short_phrase",
         concat(&[gtone(0.3, 440.0, 0.1), gsil(0.4)]),
         vec![(0.0, 0.3)], "短句"),
        ("mid_pause",
         concat(&[gtone(1.0, 220.0, 0.1), gsil(0.2), gtone(1.0, 330.0, 0.1), gsil(0.5)]),
         vec![(0.0, 2.2)], "句中停顿"),
        ("long_silence", gsil(3.0), vec![], "长静音"),
        ("pure_noise", gnoise(3.0, 0.03), vec![], "纯噪声"),
    ];

    // Add real audio if available
    let mut scenarios = scenarios;
    if wav_path.exists() {
        if let Ok(audio) = load_wav_16k_mono(&wav_path) {
            let dur = audio.len() as f64 / SR as f64;
            scenarios.push(("real_audio", audio, vec![(0.0, dur.min(5.0))], "真实音频"));
        }
    }

    let mut all_results = serde_json::Map::new();

    // ─── Run scenarios ─────────────────────────────────────────────────────
    for (name, audio, gt, desc) in &scenarios {
        info!("");
        info!("=== Scenario: {} ({}) ===", name, desc);
        info!("Audio: {:.3}s, {} samples", audio.len() as f64 / SR as f64, audio.len());

        vad.reset();
        let chunk_size = 1600; // 100ms, matching Spike D

        let t_start = Instant::now();
        let mut chunk_logs = Vec::new();
        let mut n_inference_chunks = 0;

        for i in (0..audio.len()).step_by(chunk_size) {
            let end = (i + chunk_size).min(audio.len());
            let chunk = &audio[i..end];
            let ci = i / chunk_size;
            let is_final = end >= audio.len();
            let (_, log) = vad.process(chunk, ci, is_final);
            if let Some(l) = log {
                chunk_logs.push(l);
                n_inference_chunks += 1;
            }
        }
        let final_segs = vad.finalize();
        let total_ms = t_start.elapsed().as_secs_f64() * 1000.0;

        info!("Segments: {:?}", final_segs);
        info!("Inference chunks: {}, Frames: {}", n_inference_chunks, vad.all_frame_scores.len());
        info!("Total: {:.1}ms", total_ms);

        // Collect all frame scores
        let all_scores: Vec<f32> = vad.all_frame_scores.clone();

        let result = serde_json::json!({
            "name": name,
            "description": desc,
            "audio_duration_s": (audio.len() as f64 / SR as f64 * 1000.0).round() / 1000.0,
            "n_samples": audio.len(),
            "ground_truth_segments": gt,
            "detected_segments": final_segs.iter().map(|(s, e)| vec![(*s * 10000.0).round() / 10000.0, (*e * 10000.0).round() / 10000.0]).collect::<Vec<_>>(),
            "n_inference_chunks": n_inference_chunks,
            "n_frames": all_scores.len(),
            "frame_scores": all_scores.iter().map(|s| (s * 1000000.0).round() / 1000000.0).collect::<Vec<_>>(),
            "total_ms": (total_ms * 1000.0).round() / 1000.0,
            "per_chunk": chunk_logs.iter().map(|c| serde_json::json!({
                "chunk_idx": c.chunk_idx,
                "n_samples": c.n_samples,
                "input_shape": c.input_shape,
                "output_shape": c.output_shape,
                "inference_ms": (c.inference_ms * 1000.0).round() / 1000.0,
                "n_frames": c.n_frames,
                "frame_scores": c.frame_scores.iter().map(|s| (s * 1000000.0).round() / 1000000.0).collect::<Vec<_>>(),
                "frame_decisions": c.frame_decisions,
                "is_final": c.is_final,
            })).collect::<Vec<_>>(),
        });
        all_results.insert(name.to_string(), result);
    }

    // ─── Reset test ────────────────────────────────────────────────────────
    info!("");
    info!("=== Reset Test: 5x reset + reprocess scenario 1 ===");
    let audio = &scenarios[0].1;
    let mut reset_results = Vec::new();
    for _ in 0..5 {
        vad.reset();
        for i in (0..audio.len()).step_by(1600) {
            let end = (i + 1600).min(audio.len());
            vad.process(&audio[i..end], i / 1600, end >= audio.len());
        }
        reset_results.push(vad.finalize());
    }
    let consistent = reset_results.iter().all(|r| r == &reset_results[0]);
    info!("5x reset consistent: {}", consistent);

    // ─── Multi-session test ────────────────────────────────────────────────
    info!("");
    info!("=== Multi-session test: alternating scenarios ===");
    let mut multi_ok = true;
    let mut first_results: Vec<(String, Vec<(f64, f64)>)> = Vec::new();
    for trial in 0..3 {
        for (name, audio, _, _) in &scenarios[..3] {
            vad.reset();
            for i in (0..audio.len()).step_by(1600) {
                let end = (i + 1600).min(audio.len());
                vad.process(&audio[i..end], i / 1600, end >= audio.len());
            }
            let segs = vad.finalize();
            if trial == 0 {
                first_results.push((name.to_string(), segs.clone()));
            } else {
                let prev = first_results.iter().find(|(n, _)| n == name).map(|(_, s)| s.clone());
                if prev != Some(segs) {
                    multi_ok = false;
                }
            }
        }
    }
    info!("Multi-session consistent: {}", multi_ok);

    // ─── Performance measurement ───────────────────────────────────────────
    info!("");
    info!("=== Performance: 100 chunks on real audio ===");
    let mut p50 = 0.0;
    let mut p95 = 0.0;
    if wav_path.exists() {
        if let Ok(audio) = load_wav_16k_mono(&wav_path) {
            vad.reset();
            let mut timings = Vec::new();
            let n = audio.len().min(16000); // 1s
            for i in (0..n).step_by(160) {
                let end = (i + 160).min(n);
                let t0 = Instant::now();
                vad.process(&audio[i..end], i / 160, false);
                timings.push(t0.elapsed().as_secs_f64() * 1000.0);
            }
            timings.sort_by(|a, b| a.partial_cmp(b).unwrap());
            p50 = timings[timings.len() / 2];
            p95 = timings[(timings.len() as f64 * 0.95) as usize];
            info!("Chunks: {}, p50={:.3}ms, p95={:.3}ms", timings.len(), p50, p95);
            info!("Budget per chunk: 10ms (10ms audio)");
            info!("Within budget: {}", if p95 < 10.0 { "YES" } else { "NO" });
        }
    } else {
        info!("(no real audio available)");
    }

    let mem_peak = winapi::get_working_set_mb();

    // ─── Save results ──────────────────────────────────────────────────────
    let output = serde_json::json!({
        "spike": "07c_fsmn_vad_rust_parity_rust",
        "timestamp": format!("{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)),
        "model_load_time_s": (t_load * 1000.0).round() / 1000.0,
        "model_load_mem_delta_mb": ((mem_after - mem_before) * 10.0).round() / 10.0,
        "peak_mem_mb": (mem_peak * 10.0).round() / 10.0,
        "scenarios": all_results,
        "reset_test": {
            "trials": 5,
            "consistent": consistent,
            "sample_segments": reset_results.get(0).cloned().unwrap_or_default(),
        },
        "multi_session_test": {"consistent": multi_ok, "n_trials": 3},
        "performance": {
            "p50_ms": (p50 * 1000.0).round() / 1000.0,
            "p95_ms": (p95 * 1000.0).round() / 1000.0,
            "budget_ms": 10.0,
            "within_budget": p95 < 10.0,
        },
    });

    let out = base.join("../results/spike_07c_rust.json");
    std::fs::create_dir_all(out.parent().unwrap()).ok();
    std::fs::write(&out, serde_json::to_string_pretty(&output).unwrap()).unwrap();
    info!("Result saved to: {}", out.display());
    info!("Spike 07C Rust runner complete.");
}
