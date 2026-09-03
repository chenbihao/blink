#!/usr/bin/env python3
"""Spike 07C: FSMN-VAD Rust Parity — Python Oracle

Establishes ground-truth frame scores, cache states, and segment boundaries
from the FSMN-VAD ONNX model, for Rust parity comparison.

Outputs:
  - oracle_data.json: per-chunk input features, frame scores, cache snapshots, segments
  - oracle_summary.json: aggregate summary for GO/NO-GO
"""
import os, sys, time, json, wave, math, traceback
import numpy as np

sys.stderr.reconfigure(encoding="utf-8", errors="replace")
sys.stdout.reconfigure(encoding="utf-8", errors="replace")
import warnings; warnings.filterwarnings("ignore")

try:
    import onnxruntime as ort
except:
    print("ERROR: onnxruntime not available"); sys.exit(1)
# librosa not required — we compute mel filterbank manually

# ─── Paths ──────────────────────────────────────────────────────────────────
S = os.path.dirname(os.path.abspath(__file__))
F = os.path.dirname(S); O = os.path.dirname(F)
MD = os.path.join(O, "models")
VAD_MD = os.path.join(MD, "fsmn-vad-onnx-v2")
RD = os.path.join(F, "results")
os.makedirs(RD, exist_ok=True)

# ─── Constants (from config.yaml + spike_d_models.py) ──────────────────────
SR = 16000
N_MELS = 80
FRAME_LENGTH_MS = 25
FRAME_SHIFT_MS = 10
VAD_FRAME_LENGTH = 400   # samples (25ms @ 16kHz)
VAD_FRAME_SHIFT = 160    # samples (10ms @ 16kHz)
SPLICE_LEN = 5           # lfr_m from config.yaml
VAD_CACHE_LAYERS = 4     # fsmn_layers from config.yaml
VAD_CACHE_DIM = 128      # proj_dim from config.yaml
VAD_CACHE_LORDER = 19    # from ONNX graph: [1, 128, 19, 1] (config.yaml says 20 but model uses 19)
VAD_INPUT_DIM = 200       # SPLICE_LEN * N_MELS / 2 (spliced 5×80 flattened, but model input is 400, then splice→200?)
# Actually from config: input_dim=400, splice 400 400
# From spike_d_models.py: sp flattens 5×80=400, but model input_dim=400
# Wait - re-reading spike_d_models.py:
#   _sp: f[i:i+5].flatten() → 5×80=400
#   But model input_dim in config = 400
# And CMVN means/vars are 400-dim (from am.mvn)
# So input to ONNX is (1, T, 400)

MAX_END_SILENCE_MS = 800
LOOKBACK_START_MS = 200
LOOKAHEAD_END_MS = 100
FRAME_IN_MS = 10

# ─── Mel filterbank (manual implementation matching librosa Slaney) ───────

def _hz_to_mel(hz):
    """Convert Hz to Mel (Slaney formula, matching librosa)."""
    f_min = 0.0
    f_sp = 200.0 / 3
    min_log_hz = 1000.0
    min_log_mel = (min_log_hz - f_min) / f_sp
    logstep = np.log(6.4) / 27.0
    if hz >= min_log_hz:
        return min_log_mel + np.log(hz / min_log_hz) / logstep
    return (hz - f_min) / f_sp

def _mel_to_hz(mel):
    """Convert Mel to Hz (Slaney formula, matching librosa)."""
    f_min = 0.0
    f_sp = 200.0 / 3
    min_log_hz = 1000.0
    min_log_mel = (min_log_hz - f_min) / f_sp
    logstep = np.log(6.4) / 27.0
    if mel >= min_log_mel:
        return min_log_hz * np.exp(logstep * (mel - min_log_mel))
    return f_min + f_sp * mel

def _mel_filterbank(sr, n_fft, n_mels, fmin, fmax):
    """Create mel filterbank matching librosa.filters.mel with htk=False (Slaney)."""
    fft_freqs = np.fft.rfftfreq(n_fft, 1.0 / sr)
    mel_min = _hz_to_mel(fmin)
    mel_max = _hz_to_mel(fmax)
    mel_points = np.linspace(mel_min, mel_max, n_mels + 2)
    hz_points = np.array([_mel_to_hz(m) for m in mel_points])
    weights = np.zeros((n_mels, len(fft_freqs)), dtype=np.float32)
    fdiff = np.diff(hz_points)
    ramps = hz_points.reshape(-1, 1) - fft_freqs.reshape(1, -1)
    for i in range(n_mels):
        lower = -ramps[i] / fdiff[i]
        upper = ramps[i + 2] / fdiff[i + 1]
        weights[i] = np.maximum(0, np.minimum(lower, upper))
    # Slaney normalization
    enorm = 2.0 / (hz_points[2:n_mels + 2] - hz_points[:n_mels])
    weights *= enorm.reshape(-1, 1)
    return weights

# ─── Audio utils ──────────────────────────────────────────────────────────

def gtone(d, f=440, a=0.1, sr=SR):
    n = int(d * sr); t = np.arange(n) / sr
    return (np.sin(2 * np.pi * f * t) * a).astype(np.float32)

def gsil(d, sr=SR):
    return np.zeros(int(d * sr), dtype=np.float32)

def gnoise(d, a=0.02, sr=SR):
    return (np.random.randn(int(d * sr)) * a).astype(np.float32)

def load_wav_16k_mono(p):
    w = wave.open(p, "rb")
    assert w.getframerate() == 16000
    assert w.getnchannels() == 1
    return np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16).astype(np.float32) / 32768.0

# ─── FSMN-VAD ONNX Oracle ──────────────────────────────────────────────────

class FsmnVadOracle:
    """FSMN-VAD streaming ONNX with full oracle logging."""

    def __init__(self, model_dir):
        mp = os.path.join(model_dir, "model_quant.onnx")
        vp = os.path.join(model_dir, "am.mvn")

        so = ort.SessionOptions()
        so.intra_op_num_threads = 1
        so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        self.session = ort.InferenceSession(mp, sess_options=so, providers=["CPUExecutionProvider"])

        self.input_names = [i.name for i in self.session.get_inputs()]
        self.output_names = [o.name for o in self.session.get_outputs()]
        self.means, self.vars = self._load_cmvn(vp)

        print(f"FSMN-VAD inputs: {self.input_names}")
        print(f"FSMN-VAD outputs: {self.output_names}")
        print(f"Input shapes: {[i.shape for i in self.session.get_inputs()]}")
        print(f"Output shapes: {[o.shape for o in self.session.get_outputs()]}")
        print(f"CMVN: means[{len(self.means)}], vars[{len(self.vars)}]")

        self.reset()

    def _load_cmvn(self, path):
        m, v_ = [], []
        with open(path, "r", encoding="utf-8") as f:
            ls = f.readlines()
        for i in range(len(ls)):
            it = ls[i].split()
            if not it: continue
            if it[0] == "<AddShift>" and i + 1 < len(ls):
                ni = ls[i+1].split()
                if ni and ni[0] == "<LearnRateCoef>":
                    m = list(map(float, ni[3:-1]))
            elif it[0] == "<Rescale>" and i + 1 < len(ls):
                ni = ls[i+1].split()
                if ni and ni[0] == "<LearnRateCoef>":
                    v_ = list(map(float, ni[3:-1]))
        return np.array(m, dtype=np.float32), np.array(v_, dtype=np.float32)

    def reset(self):
        self.cache = [np.zeros((1, VAD_CACHE_DIM, VAD_CACHE_LORDER, 1), dtype=np.float32)
                      for _ in range(VAD_CACHE_LAYERS)]
        self.input_cache = np.array([], dtype=np.float32)
        self.segments = []
        self.current_start = None
        self.total_samples = 0
        self.silence_frames = 0
        self.past_speech_frames = 0
        self.in_speech = False
        # Online fbank state (matches Rust kaldi-native-fbank OnlineFeature)
        self.fbank_wave = np.array([], dtype=np.float32)  # accumulated waveform
        self.fbank_offset = 0  # frames already extracted
        # Oracle logging
        self.all_frame_scores = []
        self.all_cache_snapshots = []
        self.all_input_features = []
        self.chunk_log = []

    # Pre-computed constants for fbank
    _WINDOW = None
    _MEL_FB = None

    @classmethod
    def _init_fbank_consts(cls):
        if cls._WINDOW is None:
            cls._WINDOW = np.hamming(VAD_FRAME_LENGTH)
            cls._MEL_FB = _mel_filterbank(SR, 512, N_MELS, 0.0, SR/2)

    def _compute_fbank(self, wav):
        """Online 80-mel fbank with pre-emphasis, hamming window, snip_edges=True.

        Matches Rust kaldi-native-fbank OnlineFeature: accumulates waveform
        across calls, only produces new frames when enough samples arrive.
        Pre-emphasis is applied per-frame (matching kaldi ProcessWindow),
        not globally on the entire waveform.
        """
        self._init_fbank_consts()

        # Scale and accumulate
        scaled = wav * 32768
        self.fbank_wave = np.concatenate([self.fbank_wave, scaled])

        win_length = VAD_FRAME_LENGTH  # 400
        hop_length = VAD_FRAME_SHIFT   # 160
        n_fft = 512

        w = self.fbank_wave
        if len(w) < win_length:
            return np.zeros((0, N_MELS), dtype=np.float32)

        # Total frames available
        total_frames = 1 + (len(w) - win_length) // hop_length
        new_frames = total_frames - self.fbank_offset

        if new_frames <= 0:
            return np.zeros((0, N_MELS), dtype=np.float32)

        fb = np.zeros((new_frames, N_MELS), dtype=np.float32)
        for i in range(new_frames):
            idx = self.fbank_offset + i
            start = idx * hop_length
            frame = w[start:start + win_length].copy()
            # Per-frame pre-emphasis (matching kaldi ProcessWindow):
            # y[i] = x[i] - preemph * x[i-1]  for i > 0
            # First sample: y[0] = x[0] (no pre-emphasis on first)
            # NOTE: kaldi uses remove_dc_offset before pre-emphasis, but
            # our config has remove_dc_offset=false, matching Rust.
            preemph = 0.97
            frame_pe = np.empty_like(frame)
            frame_pe[0] = frame[0]
            frame_pe[1:] = frame[1:] - preemph * frame[:-1]
            # Apply window
            windowed = frame_pe * self._WINDOW
            # Zero-pad to n_fft
            padded = np.zeros(n_fft, dtype=np.float32)
            padded[:win_length] = windowed
            # FFT -> power spectrum
            spectrum = np.fft.rfft(padded)
            power = np.abs(spectrum) ** 2
            # Mel filterbank
            mel_energy = np.dot(self._MEL_FB, power)
            fb[i] = np.log(np.maximum(mel_energy, 1e-10))

        self.fbank_offset = total_frames
        return fb.astype(np.float32)  # (new_frames, 80)

    def _splice(self, f):
        if len(f) < SPLICE_LEN:
            return np.zeros((0, 400), dtype=np.float32)
        return np.array([f[i:i+SPLICE_LEN].flatten() for i in range(len(f) - SPLICE_LEN + 1)],
                        dtype=np.float32)

    def _apply_cmvn(self, f):
        return (f + self.means) * self.vars

    def process(self, samples, chunk_idx=0, is_final=False):
        """Process a chunk of audio samples, return (events, oracle_data)."""
        events = []
        oracle = {"chunk_idx": chunk_idx, "n_samples": len(samples), "is_final": is_final}

        # Accumulate with leftover
        if len(self.input_cache) > 0:
            s = np.concatenate([self.input_cache, samples])
        else:
            s = samples
        self.input_cache = np.array([], dtype=np.float32)

        if len(s) < VAD_FRAME_LENGTH:
            self.input_cache = s
            return events, oracle

        nf = (len(s) - VAD_FRAME_LENGTH) // VAD_FRAME_SHIFT + 1
        if nf < 1:
            self.input_cache = s
            return events, oracle

        us = (nf - 1) * VAD_FRAME_SHIFT + VAD_FRAME_LENGTH
        wav_data = s[:us]
        self.input_cache = s[us:]

        # Fbank
        fb = self._compute_fbank(wav_data)
        sp = self._splice(fb)
        if len(sp) == 0:
            return events, oracle

        ft = self._apply_cmvn(sp)
        sp_in = ft[np.newaxis, :, :]  # (1, T, 400)

        oracle["input_shape"] = list(sp_in.shape)
        oracle["input_features"] = sp_in[0].tolist()

        # Build feeds
        feeds = {self.input_names[0]: sp_in}
        for i in range(VAD_CACHE_LAYERS):
            feeds[self.input_names[1 + i]] = self.cache[i]

        # Run inference
        t0 = time.perf_counter()
        outputs = self.session.run(self.output_names, feeds)
        inf_ms = (time.perf_counter() - t0) * 1000.0

        logits = outputs[0]  # (1, T, output_dim)
        new_caches = [outputs[1 + i] for i in range(VAD_CACHE_LAYERS)]

        oracle["output_shape"] = list(logits.shape)
        oracle["logits"] = logits[0].tolist()
        oracle["inference_ms"] = round(inf_ms, 3)
        oracle["cache_before"] = [c.tolist() for c in self.cache]
        oracle["cache_after"] = [c.tolist() for c in new_caches]

        # Update cache
        self.cache = new_caches

        # Frame-level scores
        lp = logits[0]  # (T, output_dim)
        pr = np.exp(lp - np.max(lp, axis=-1, keepdims=True))
        pr = pr / np.sum(pr, axis=-1, keepdims=True)
        spp = 1.0 - pr[:, 0]  # speech probability = 1 - silence_prob
        dec = (spp > 0.5).astype(int)

        # Smoothing (3-frame majority)
        sm = dec.copy()
        for i in range(1, len(dec) - 1):
            sm[i] = 1 if np.sum(dec[i-1:i+2]) >= 2 else 0

        oracle["frame_scores"] = spp.tolist()
        oracle["frame_decisions"] = sm.tolist()

        # Endpoint state machine
        frame_shift_s = VAD_FRAME_SHIFT / SR
        chunk_start_s = self.total_samples / SR

        for fi, is_speech in enumerate(sm):
            self.all_frame_scores.append(float(spp[fi]))
            ft_s = chunk_start_s + fi * frame_shift_s

            if is_speech:
                self.silence_frames = 0
                self.past_speech_frames += 1
                if not self.in_speech:
                    self.in_speech = True
                    self.current_start = max(0, ft_s - LOOKBACK_START_MS / 1000)
            else:
                self.past_speech_frames = 0
                self.silence_frames += 1
                if self.in_speech:
                    sil_ms = self.silence_frames * FRAME_IN_MS
                    if sil_ms >= MAX_END_SILENCE_MS:
                        end_t = ft_s + LOOKAHEAD_END_MS / 1000
                        if self.current_start is not None:
                            self.segments.append((self.current_start, end_t))
                            events.append(("end", end_t))
                        self.in_speech = False
                        self.current_start = None
                        self.silence_frames = 0

        self.total_samples += us
        oracle["segments_so_far"] = list(self.segments)
        oracle["chunk_total_ms"] = round(inf_ms, 3)

        self.chunk_log.append(oracle)
        return events, oracle

    def finalize(self):
        if self.in_speech and self.current_start is not None:
            end_t = self.total_samples / SR
            self.segments.append((self.current_start, end_t))
            self.in_speech = False
        return list(self.segments)


# ─── Test Scenarios ────────────────────────────────────────────────────────

def build_test_scenarios():
    """Build test scenarios covering all required validation cases."""
    np.random.seed(42)
    scenarios = []

    # 1. Continuous chunk (2s speech + 0.5s silence + 1s speech)
    a = np.concatenate([gtone(2, 220, 0.1), gsil(0.5), gtone(1, 330, 0.1)])
    scenarios.append(("continuous_chunk", a, [(0, 2), (2.5, 3.5)], "连续 chunk"))

    # 2. Short phrase (0.3s speech + 0.4s silence)
    a = np.concatenate([gtone(0.3, 440, 0.1), gsil(0.4)])
    scenarios.append(("short_phrase", a, [(0, 0.3)], "短句"))

    # 3. Mid-sentence pause (1s speech + 0.2s pause + 1s speech + 0.5s silence)
    a = np.concatenate([gtone(1, 220, 0.1), gsil(0.2), gtone(1, 330, 0.1), gsil(0.5)])
    scenarios.append(("mid_pause", a, [(0, 2.2)], "句中停顿"))

    # 4. Long silence (3s pure silence)
    scenarios.append(("long_silence", gsil(3), [], "长静音"))

    # 5. Pure noise (3s white noise)
    scenarios.append(("pure_noise", gnoise(3, 0.03), [], "纯噪声"))

    # 6. Real audio (if available)
    wav_path = os.path.join(MD, "asr_example.wav")
    if os.path.exists(wav_path):
        try:
            a = load_wav_16k_mono(wav_path)
            dur = len(a) / SR
            scenarios.append(("real_audio", a, [(0, min(dur, 5))], "真实音频"))
        except:
            pass

    return scenarios


def run_oracle():
    """Run the Python oracle and save data for Rust parity comparison."""
    print("=" * 60)
    print("Spike 07C: FSMN-VAD Rust Parity — Python Oracle")
    print("=" * 60)

    fv = FsmnVadOracle(VAD_MD)

    scenarios = build_test_scenarios()
    print(f"\nScenarios: {len(scenarios)}")
    for name, audio, gt, desc in scenarios:
        dur = len(audio) / SR
        print(f"  {name:25s} {dur:.1f}s  gt={gt}")

    all_results = {}

    # --- Scenarios ---
    # Feed in 160-sample (10ms) chunks to match real-time audio callback.
    # The VAD accumulates internally until enough frames for a splice.
    for name, audio, gt, desc in scenarios:
        print(f"\n{'='*40}")
        print(f"Scenario: {name} ({desc})")
        print(f"{'='*40}")

        fv.reset()
        # Use 1600-sample (100ms) chunks - same as Spike D
        # Real-time 10ms feeding is tested separately for latency
        chunk_size = 1600  # 100ms @ 16kHz
        all_oracle_chunks = []
        chunk_t0 = time.perf_counter()

        for i in range(0, len(audio), chunk_size):
            chunk = audio[i:i+chunk_size]
            is_final = (i + chunk_size) >= len(audio)
            ci = i // chunk_size
            events, oracle = fv.process(chunk, ci, is_final)
            if oracle.get("input_shape"):
                all_oracle_chunks.append(oracle)

        final_segs = fv.finalize()
        total_ms = (time.perf_counter() - chunk_t0) * 1000.0

        # Collect frame scores
        all_scores = []
        for oc in all_oracle_chunks:
            all_scores.extend(oc.get("frame_scores", []))

        result = {
            "name": name,
            "description": desc,
            "audio_duration_s": round(len(audio) / SR, 3),
            "n_samples": len(audio),
            "ground_truth_segments": gt,
            "detected_segments": [[round(s, 4), round(e, 4)] for s, e in final_segs],
            "n_inference_chunks": len(all_oracle_chunks),
            "n_frames": len(all_scores),
            "frame_scores": [round(s, 6) for s in all_scores],
            "total_ms": round(total_ms, 3),
            "per_chunk": [{
                "chunk_idx": oc["chunk_idx"],
                "n_samples": oc["n_samples"],
                "input_shape": oc.get("input_shape"),
                "output_shape": oc.get("output_shape"),
                "inference_ms": oc.get("inference_ms", 0),
                "n_frames": len(oc.get("frame_scores", [])),
                "frame_scores": [round(s, 6) for s in oc.get("frame_scores", [])],
                "frame_decisions": oc.get("frame_decisions", []),
                "is_final": oc["is_final"],
            } for oc in all_oracle_chunks],
            "cache_final": [c.tolist() for c in fv.cache],
        }
        all_results[name] = result

        # Print summary
        print(f"  Segments: {final_segs}")
        print(f"  Frames: {len(all_scores)}")
        print(f"  Total: {total_ms:.1f}ms")

    # --- Reset test ---
    print(f"\n{'='*40}")
    print("Reset Test: 5x reset + reprocess scenario 1")
    print(f"{'='*40}")
    audio = scenarios[0][1]
    reset_results = []
    for trial in range(5):
        fv.reset()
        for i in range(0, len(audio), 160):
            fv.process(audio[i:i+160], i // 160, (i + 160) >= len(audio))
        segs = fv.finalize()
        reset_results.append(segs)
    consistent = all(s == reset_results[0] for s in reset_results)
    print(f"  5x reset consistent: {consistent}")

    # --- Multi-session test ---
    print(f"\n{'='*40}")
    print("Multi-session test: alternating scenarios")
    print(f"{'='*40}")
    multi_results = []
    for trial in range(3):
        for name, audio, gt, desc in scenarios[:3]:
            fv.reset()
            for i in range(0, len(audio), 160):
                fv.process(audio[i:i+160], i // 160, (i + 160) >= len(audio))
            segs = fv.finalize()
            multi_results.append({"trial": trial, "scenario": name, "segments": segs})

    # Check consistency
    first = {r["scenario"]: r["segments"] for r in multi_results if r["trial"] == 0}
    multi_ok = all({r["scenario"]: r["segments"] for r in multi_results if r["trial"] == t} == first
                   for t in range(1, 3))
    print(f"  Multi-session consistent: {multi_ok}")

    # --- Performance measurement ---
    print(f"\n{'='*40}")
    print("Performance: 100 chunks on real audio")
    print(f"{'='*40}")
    wav_path = os.path.join(MD, "asr_example.wav")
    if os.path.exists(wav_path):
        audio = load_wav_16k_mono(wav_path)
        fv.reset()
        timings = []
        for i in range(0, min(len(audio), 16000), 160):  # 1s
            chunk = audio[i:i+160]
            t0 = time.perf_counter()
            fv.process(chunk, i // 160, False)
            timings.append((time.perf_counter() - t0) * 1000.0)
        p50 = float(np.percentile(timings, 50))
        p95 = float(np.percentile(timings, 95))
        print(f"  Chunks: {len(timings)}, p50={p50:.3f}ms, p95={p95:.3f}ms")
        print(f"  Budget per chunk: {160/SR*1000:.0f}ms (10ms audio)")
        print(f"  Within budget: {'YES' if p95 < 10.0 else 'NO'}")
    else:
        p50 = p95 = 0
        print("  (no real audio available)")

    # --- Save oracle data ---
    output = {
        "spike": "07c_fsmn_vad_rust_parity_oracle",
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S"),
        "model_info": {
            "input_names": fv.input_names,
            "output_names": fv.output_names,
            "input_shapes": [list(i.shape) for i in fv.session.get_inputs()],
            "output_shapes": [list(o.shape) for o in fv.session.get_outputs()],
            "config": {
                "n_mels": N_MELS,
                "frame_length_ms": FRAME_LENGTH_MS,
                "frame_shift_ms": FRAME_SHIFT_MS,
                "splice_len": SPLICE_LEN,
                "cache_layers": VAD_CACHE_LAYERS,
                "cache_dim": VAD_CACHE_DIM,
                "cache_lorder": VAD_CACHE_LORDER,
                "input_dim": 400,
                "max_end_silence_ms": MAX_END_SILENCE_MS,
                "lookback_start_ms": LOOKBACK_START_MS,
                "lookahead_end_ms": LOOKAHEAD_END_MS,
                "frame_in_ms": FRAME_IN_MS,
            },
            "cmvn_dim": len(fv.means),
        },
        "scenarios": all_results,
        "reset_test": {"trials": 5, "consistent": consistent,
                       "sample_segments": reset_results[0] if reset_results else []},
        "multi_session_test": {"consistent": multi_ok, "n_trials": 3},
        "performance": {"p50_ms": round(p50, 3), "p95_ms": round(p95, 3),
                        "budget_ms": 10.0, "within_budget": p95 < 10.0},
    }

    out_path = os.path.join(RD, "spike_07c_oracle.json")
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(output, f, ensure_ascii=False, indent=2)
    print(f"\nOracle data saved to: {out_path}")
    print("Spike 07C oracle complete.")


if __name__ == "__main__":
    run_oracle()
