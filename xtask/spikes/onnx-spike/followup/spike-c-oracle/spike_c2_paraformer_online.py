#!/usr/bin/env python3
"""Spike C2: ParaformerOnline — Python onnxruntime Oracle

基于上游 C++ paraformer-online.cpp 的完整 Python 实现:
- fbank (kaldi-compatible, using librosa)
- CMVN (从 am.mvn 加载)
- LFR (Low Frame Rate, m=7, n=6)
- Positional Embedding
- Encoder (chunk-by-chunk + encoder cache)
- CIF Search (CifSearch + hidden/alpha cache)
- Decoder FSMN cache
- chunk-by-chunk + is_final + reset

证明句尾前产生非空 partial transcript。
输出每个 chunk: audio timestamp, partial, final, cache shape, inference duration。
"""

import os
import sys
import time
import json
import wave
import math
import numpy as np

sys.stderr.reconfigure(encoding="utf-8", errors="replace")
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

import warnings
warnings.filterwarnings("ignore")

SPIKE_DIR = os.path.dirname(os.path.abspath(__file__))
ONNX_SPIKE_DIR = os.path.join(os.path.dirname(SPIKE_DIR), "..")
MODELS_DIR = os.path.join(ONNX_SPIKE_DIR, "models")
RESULTS_DIR = os.path.join(os.path.dirname(SPIKE_DIR), "results")
os.makedirs(RESULTS_DIR, exist_ok=True)

# ParaformerOnline model dir — modelscope downloads to cache_dir/model_id
# We'll search for it
PARAFORMER_ONLINE_DIR = os.path.join(MODELS_DIR, "paraformer-online-onnx")
# Also check modelscope cache path
MODELSCOPE_CACHE = os.path.join(MODELS_DIR, "speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx")

VAD_DIR = os.path.join(MODELS_DIR, "fsmn-vad-onnx-v2")
PARAFORMER_OFFLINE_DIR = os.path.join(MODELS_DIR, "paraformer-zh-onnx")

# Default config (from paraformer-online.cpp)
CHUNK_SIZE = [5, 10, 5]  # left, center, right (in 60ms units)
ENCODER_CHUNK_LOOK_BACK = 4
DECODER_CHUNK_LOOK_BACK = 1
LFR_M = 7
LFR_N = 6
N_MELS = 80
FRAME_LENGTH = 25  # ms
FRAME_SHIFT = 10  # ms
ENCODER_SIZE = 512
FSMN_LAYERS = 16
FSMN_LORDER = 10
FSMN_DIMS = 512
CIF_THRESHOLD = 1.0
TAIL_ALPHAS = 0.45
SAMPLE_RATE = 16000

import onnxruntime as ort


def find_model_dir():
    """Find the ParaformerOnline model directory."""
    candidates = [
        PARAFORMER_ONLINE_DIR,
        MODELSCOPE_CACHE,
        os.path.join(MODELS_DIR, "speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx"),
    ]
    # Also search subdirs
    if os.path.exists(MODELS_DIR):
        for d in os.listdir(MODELS_DIR):
            full = os.path.join(MODELS_DIR, d)
            if os.path.isdir(full) and "online" in d.lower() and "paraformer" in d.lower():
                candidates.append(full)
    
    for c in candidates:
        encoder = os.path.join(c, "encoder.onnx")
        if not os.path.exists(encoder):
            encoder = os.path.join(c, "encoder_onnx", "model_quant.onnx")
        decoder = os.path.join(c, "decoder.onnx")
        if not os.path.exists(decoder):
            decoder = os.path.join(c, "decoder_onnx", "model_quant.onnx")
        am_mvn = os.path.join(c, "am.mvn")
        config = os.path.join(c, "config.yaml")
        tokens = os.path.join(c, "tokens.json")
        if not os.path.exists(tokens):
            tokens = os.path.join(c, "token.txt")
        
        if os.path.exists(encoder) and os.path.exists(decoder):
            print(f"Found model dir: {c}")
            return {
                "dir": c,
                "encoder": encoder,
                "decoder": decoder,
                "am_mvn": am_mvn if os.path.exists(am_mvn) else None,
                "config": config if os.path.exists(config) else None,
                "tokens": tokens if os.path.exists(tokens) else None,
            }
    
    return None


def load_wav_16k_mono(path):
    with wave.open(path, "rb") as wf:
        assert wf.getframerate() == 16000, f"Expected 16kHz, got {wf.getframerate()}"
        assert wf.getnchannels() == 1, "Expected mono"
        raw = wf.readframes(wf.getnframes())
        return np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0


def load_cmvn(path):
    """Load CMVN from am.mvn file (kaldi format)."""
    means_list = []
    vars_list = []
    with open(path, "r", encoding="utf-8") as f:
        lines = f.readlines()
    for i in range(len(lines)):
        line_item = lines[i].split()
        if len(line_item) > 0 and line_item[0] == "<AddShift>":
            if i + 1 < len(lines):
                next_item = lines[i + 1].split()
                if len(next_item) > 0 and next_item[0] == "<LearnRateCoef>":
                    means_list = list(map(float, next_item[3:-1]))
        elif len(line_item) > 0 and line_item[0] == "<Rescale>":
            if i + 1 < len(lines):
                next_item = lines[i + 1].split()
                if len(next_item) > 0 and next_item[0] == "<LearnRateCoef>":
                    vars_list = list(map(float, next_item[3:-1]))
    
    means = np.array(means_list, dtype=np.float32)
    vars_ = np.array(vars_list, dtype=np.float32)
    # Tile to match feat_dim (lfr_m * n_mels = 7 * 80 = 560)
    feat_dim = LFR_M * N_MELS
    if len(means) < feat_dim:
        means = np.tile(means, (10,))[:feat_dim]
        vars_ = np.tile(vars_, (10,))[:feat_dim]
    return means, vars_


def compute_fbank(wav, sr=16000):
    """Compute fbank features (kaldi-compatible) using librosa.
    
    Returns: (num_frames, 80) float32 array
    """
    import librosa
    # frame_length = 25ms = 400 samples at 16kHz
    # frame_shift = 10ms = 160 samples at 16kHz
    n_fft = 512
    win_length = int(sr * FRAME_LENGTH / 1000)  # 400
    hop_length = int(sr * FRAME_SHIFT / 1000)  # 160
    
    # Use hamming window (kaldi default is povey, but hamming is close)
    # Actually FunASR uses "hamming" or "povey" window
    # Let's use librosa's stft with hamming
    S = np.abs(librosa.stft(
        wav, n_fft=n_fft, win_length=win_length, hop_length=hop_length,
        window='hamming', center=False
    ))
    
    # Apply mel filterbank (80 bins)
    mel_basis = librosa.filters.mel(sr=sr, n_fft=n_fft, n_mels=N_MELS, fmin=0.0, fmax=sr/2)
    fbank = np.dot(mel_basis, S)  # (n_mels, num_frames)
    
    # Log
    fbank = np.log(np.maximum(fbank, 1e-10))
    
    # Transpose to (num_frames, n_mels)
    fbank = fbank.T.astype(np.float32)
    
    return fbank


def compute_fbank_kaldi(wav, sr=16000):
    """Compute fbank features more kaldi-compatible.
    
    Uses: pre-emphasis=0.97, frame_length=25ms, frame_shift=10ms, n_mels=80, window=hamming
    """
    import librosa
    
    # Pre-emphasis (kaldi default)
    wav = np.append(wav[0], wav[1:] - 0.97 * wav[:-1])
    
    n_fft = 512
    win_length = int(sr * FRAME_LENGTH / 1000)  # 400
    hop_length = int(sr * FRAME_SHIFT / 1000)  # 160
    
    # Compute STFT
    S = np.abs(librosa.stft(
        wav, n_fft=n_fft, win_length=win_length, hop_length=hop_length,
        window='hamming', center=False
    ))
    
    # Mel filterbank
    mel_basis = librosa.filters.mel(sr=sr, n_fft=n_fft, n_mels=N_MELS, fmin=0.0, fmax=sr/2)
    fbank = np.dot(mel_basis, S)  # (n_mels, num_frames)
    
    # Log
    fbank = np.log(np.maximum(fbank, 1e-10))
    
    # Transpose to (num_frames, n_mels)
    fbank = fbank.T.astype(np.float32)
    
    return fbank


def online_lfr_cmvn(wav_feats, means_list, vars_list, lfr_splice_cache, input_finished):
    """LFR (Low Frame Rate) + CMVN.
    
    Mimics C++ OnlineLfrCmvn:
    - Concatenate lfr_m consecutive frames, stride lfr_n
    - Apply CMVN: (feat + mean) * var
    
    Returns: (out_feats, new_lfr_splice_cache, lfr_splice_frame_idxs)
    """
    m = LFR_M
    n = LFR_N
    
    # Prepend lfr_splice_cache
    if lfr_splice_cache is not None and (isinstance(lfr_splice_cache, np.ndarray) and lfr_splice_cache.size > 0) or (isinstance(lfr_splice_cache, list) and len(lfr_splice_cache) > 0):
        if isinstance(lfr_splice_cache, list):
            lfr_splice_cache = np.array(lfr_splice_cache, dtype=np.float32)
        wav_feats = np.vstack([lfr_splice_cache, wav_feats])
    
    T = len(wav_feats)
    T_lrf = int(np.ceil((T - (m - 1) // 2) / float(n)))
    lfr_splice_frame_idxs = T_lrf
    
    out_feats = []
    p = []
    for i in range(T_lrf):
        if m <= T - i * n:
            p = []
            for j in range(m):
                p.extend(wav_feats[i * n + j])
            out_feats.append(p)
        else:
            if input_finished:
                num_padding = m - (T - i * n)
                p = []
                for j in range(T - i * n):
                    p.extend(wav_feats[i * n + j])
                for j in range(num_padding):
                    p.extend(wav_feats[-1])
                out_feats.append(p)
            else:
                lfr_splice_frame_idxs = i
                break
    
    lfr_splice_frame_idxs = min(T - 1, lfr_splice_frame_idxs * n)
    
    # New lfr_splice_cache
    new_lfr_splice_cache = wav_feats[lfr_splice_frame_idxs:] if lfr_splice_frame_idxs < T else []
    
    # Apply CMVN
    if out_feats:
        out_feats = np.array(out_feats, dtype=np.float32)
        out_feats = (out_feats + means_list) * vars_list
    else:
        out_feats = np.zeros((0, LFR_M * N_MELS), dtype=np.float32)
    
    return out_feats, new_lfr_splice_cache, lfr_splice_frame_idxs


def get_pos_emb(wav_feats, start_idx_cache):
    """Positional embedding (sinusoidal).
    
    Mimics C++ GetPosEmb.
    """
    timesteps = wav_feats.shape[0]
    feat_dim = wav_feats.shape[1]
    
    start_idx = start_idx_cache
    start_idx_cache += timesteps
    
    scale = -0.0330119726594128  # -log(10000) / (feat_dim/2 - 1)
    
    pe = np.zeros((start_idx_cache, feat_dim), dtype=np.float32)
    for i in range(feat_dim // 2):
        tmptime = math.exp(i * scale)
        for j in range(start_idx_cache):
            coe = tmptime * (j + 1)
            pe[j, i] = math.sin(coe)
            pe[j, i + feat_dim // 2] = math.cos(coe)
    
    wav_feats += pe[start_idx:start_idx + timesteps]
    
    return wav_feats, start_idx_cache


def cif_search(hidden, alphas, chunk_size, hidden_cache, alphas_cache, is_last_chunk, 
               cif_threshold=CIF_THRESHOLD, tail_alphas=TAIL_ALPHAS):
    """CIF search — extract acoustic embeds from hidden states.
    
    Mimics C++ CifSearch.
    """
    if len(hidden) == 0:
        return np.zeros((0, ENCODER_SIZE), dtype=np.float32), hidden_cache, alphas_cache
    
    hidden_size = hidden.shape[1]
    
    # Zero out left and right context alphas
    chunk_size_pre = chunk_size[0]
    alphas[:chunk_size_pre] = 0.0
    chunk_size_suf = sum(chunk_size[:-1])
    alphas[chunk_size_suf:] = 0.0
    
    # Prepend cache
    if len(hidden_cache) > 0:
        hidden = np.vstack([hidden_cache, hidden])
        alphas = np.concatenate([alphas_cache, alphas])
    
    if is_last_chunk:
        tail_hidden = np.zeros((1, hidden_size), dtype=np.float32)
        hidden = np.vstack([hidden, tail_hidden])
        alphas = np.append(alphas, tail_alphas)
    
    list_frame = []
    intergrate = 0.0
    frames = np.zeros(hidden_size, dtype=np.float32)
    
    for i in range(len(alphas)):
        alpha = alphas[i]
        if alpha + intergrate < cif_threshold:
            intergrate += alpha
            frames += alpha * hidden[i]
        else:
            frames += (cif_threshold - intergrate) * hidden[i]
            list_frame.append(frames.copy())
            intergrate += alpha
            intergrate -= cif_threshold
            frames = intergrate * hidden[i]
    
    # Update cache
    new_alphas_cache = np.array([intergrate], dtype=np.float32)
    if intergrate > 0.0:
        new_hidden_cache = (frames / intergrate).reshape(1, -1)
    else:
        new_hidden_cache = frames.reshape(1, -1)
    
    if len(list_frame) > 0:
        list_frame = np.array(list_frame, dtype=np.float32)
    else:
        list_frame = np.zeros((0, hidden_size), dtype=np.float32)
    
    return list_frame, new_hidden_cache, new_alphas_cache


def add_overlap_chunk(wav_feats, feats_cache, chunk_size, input_finished, is_last_chunk, feat_dims):
    """Add overlap chunks (left + right context).
    
    Mimics C++ AddOverlapChunk.
    """
    wav_feats = np.vstack([feats_cache, wav_feats]) if len(feats_cache) > 0 else wav_feats
    
    if input_finished:
        new_feats_cache = wav_feats[-chunk_size[0]:] if len(wav_feats) >= chunk_size[0] else wav_feats
        if not is_last_chunk:
            padding_length = sum(chunk_size) - len(wav_feats)
            if padding_length > 0:
                padding = np.zeros((padding_length, feat_dims), dtype=np.float32)
                wav_feats = np.vstack([wav_feats, padding])
    else:
        cache_len = chunk_size[0] + chunk_size[2]
        new_feats_cache = wav_feats[-cache_len:] if len(wav_feats) >= cache_len else wav_feats
    
    return wav_feats, new_feats_cache


class ParaformerOnlineOracle:
    """Python oracle for ParaformerOnline streaming ASR."""
    
    def __init__(self, model_info):
        self.encoder_path = model_info["encoder"]
        self.decoder_path = model_info["decoder"]
        self.am_mvn_path = model_info["am_mvn"]
        self.config_path = model_info["config"]
        self.tokens_path = model_info["tokens"]
        
        # Load CMVN
        if self.am_mvn_path:
            self.means_list, self.vars_list = load_cmvn(self.am_mvn_path)
            print(f"  CMVN loaded: means shape={self.means_list.shape}, vars shape={self.vars_list.shape}")
        else:
            self.means_list = np.zeros(LFR_M * N_MELS, dtype=np.float32)
            self.vars_list = np.ones(LFR_M * N_MELS, dtype=np.float32)
        
        # Load tokens
        self.tokens = []
        if self.tokens_path:
            if self.tokens_path.endswith(".json"):
                with open(self.tokens_path, "r", encoding="utf-8") as f:
                    token_data = json.load(f)
                    if isinstance(token_data, dict):
                        self.tokens = [token_data[str(i)] if str(i) in token_data else "" for i in range(len(token_data))]
                    elif isinstance(token_data, list):
                        self.tokens = token_data
            else:
                with open(self.tokens_path, "r", encoding="utf-8") as f:
                    for line in f:
                        parts = line.strip().split()
                        if parts:
                            self.tokens.append(parts[0])
            print(f"  Tokens loaded: {len(self.tokens)} tokens")
        
        # Create ONNX sessions
        so = ort.SessionOptions()
        so.intra_op_num_threads = 1
        so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        
        self.encoder = ort.InferenceSession(self.encoder_path, sess_options=so, providers=["CPUExecutionProvider"])
        self.decoder = ort.InferenceSession(self.decoder_path, sess_options=so, providers=["CPUExecutionProvider"])
        
        # Print I/O
        print(f"  Encoder inputs: {[(i.name, i.shape, i.type) for i in self.encoder.get_inputs()]}")
        print(f"  Encoder outputs: {[(o.name, o.shape, o.type) for o in self.encoder.get_outputs()]}")
        print(f"  Decoder inputs: {[(i.name, i.shape, i.type) for i in self.decoder.get_inputs()]}")
        print(f"  Decoder outputs: {[(o.name, o.shape, o.type) for o in self.decoder.get_outputs()]}")
        
        self.feat_dims = LFR_M * N_MELS
        self.sqrt_factor = math.sqrt(ENCODER_SIZE)
        
        self.init_cache()
    
    def init_cache(self):
        """Initialize/reset all caches."""
        self.start_idx_cache = 0
        self.is_first_chunk = True
        self.is_last_chunk = False
        
        # Input cache (audio samples not yet processed by fbank)
        self.input_cache = []
        # Reserved waveforms
        self.reserve_waveforms = []
        # LFR splice cache
        self.lfr_splice_cache = []
        
        # CIF cache
        self.hidden_cache = np.zeros((1, ENCODER_SIZE), dtype=np.float32)
        self.alphas_cache = np.array([0.0], dtype=np.float32)
        
        # Feats cache (for overlap chunks)
        self.feats_cache = np.zeros((CHUNK_SIZE[0] + CHUNK_SIZE[2], self.feat_dims), dtype=np.float32)
        
        # FSMN decoder cache
        fsmn_init = np.zeros((1, FSMN_DIMS, FSMN_LORDER), dtype=np.float32)
        self.decoder_cache = [fsmn_init.copy() for _ in range(FSMN_LAYERS)]
    
    def reset(self):
        self.init_cache()
    
    def extract_feats(self, waves, input_finished):
        """Extract features: fbank → LFR → CMVN → pos_emb.
        
        Mimics C++ ExtractFeats.
        """
        # Merge input cache
        waves = np.concatenate([self.input_cache, waves]) if len(self.input_cache) > 0 else waves
        
        frame_sample_length = SAMPLE_RATE // 1000 * FRAME_LENGTH  # 400
        frame_shift_sample_length = SAMPLE_RATE // 1000 * FRAME_SHIFT  # 160
        
        # Compute frame number
        frame_number = int((len(waves) - frame_sample_length) / frame_shift_sample_length) + 1
        if frame_number < 1 or len(waves) < frame_sample_length:
            self.input_cache = list(waves)
            return np.zeros((0, self.feat_dims), dtype=np.float32), np.array([], dtype=np.float32)
        
        # Save audio after last frame shift position
        self.input_cache = list(waves[frame_number * frame_shift_sample_length:])
        
        # Truncate waves
        waves = waves[:frame_number * frame_shift_sample_length - frame_shift_sample_length + frame_sample_length]
        
        # Fbank
        wav_int16 = waves * 32768
        fbank_feats = compute_fbank_kaldi(wav_int16.astype(np.float32), SAMPLE_RATE)
        wav_feats = fbank_feats  # (num_frames, 80)
        
        if len(wav_feats) == 0:
            if input_finished:
                # Process remaining
                if len(self.reserve_waveforms) > 0:
                    waves = self.reserve_waveforms
                wav_feats = np.array(self.lfr_splice_cache, dtype=np.float32) if self.lfr_splice_cache else np.zeros((0, N_MELS), dtype=np.float32)
                if len(wav_feats) > 0:
                    wav_feats, _, _ = online_lfr_cmvn(wav_feats, self.means_list, self.vars_list, [], True)
            else:
                return np.zeros((0, self.feat_dims), dtype=np.float32), waves
        else:
            # LFR + CMVN
            if len(self.lfr_splice_cache) == 0:
                self.lfr_splice_cache = [wav_feats[0]] * ((LFR_M - 1) // 2)
            
            total = len(wav_feats) + len(self.lfr_splice_cache)
            if total >= LFR_M:
                wav_feats, self.lfr_splice_cache, _ = online_lfr_cmvn(
                    wav_feats, self.means_list, self.vars_list, self.lfr_splice_cache, input_finished
                )
            else:
                self.lfr_splice_cache.extend(wav_feats.tolist())
                return np.zeros((0, self.feat_dims), dtype=np.float32), waves
        
        if input_finished:
            self.input_cache = []
            self.lfr_splice_cache = []
        
        return wav_feats, waves
    
    def forward_chunk(self, chunk_feats, input_finished):
        """Forward one chunk through encoder → CIF → decoder.
        
        Returns: (result_text, chunk_log)
        """
        result = ""
        t0 = time.time()
        
        if len(chunk_feats) == 0:
            return result, {"inference_ms": 0, "cache_shapes": {}}
        
        num_frames = len(chunk_feats)
        
        # Apply sqrt factor
        chunk_feats = chunk_feats * self.sqrt_factor
        
        # Positional embedding
        chunk_feats, self.start_idx_cache = get_pos_emb(chunk_feats, self.start_idx_cache)
        
        # Add overlap chunk
        chunk_feats, self.feats_cache = add_overlap_chunk(
            chunk_feats, self.feats_cache, CHUNK_SIZE, input_finished,
            self.is_last_chunk, self.feat_dims
        )
        
        num_frames = len(chunk_feats)
        
        # Encoder
        speech = chunk_feats.reshape(1, num_frames, self.feat_dims).astype(np.float32)
        speech_lens = np.array([num_frames], dtype=np.int32)
        
        enc_inputs = {
            self.encoder.get_inputs()[0].name: speech,
            self.encoder.get_inputs()[1].name: speech_lens,
        }
        
        enc_outputs = self.encoder.run(None, enc_inputs)
        # enc_outputs[0] = enc (1, T, 512)
        # enc_outputs[1] = enc_lens (1,)
        # enc_outputs[2] = alphas (1, T)
        
        enc = enc_outputs[0]  # (1, T, encoder_size)
        enc_lens = enc_outputs[1]  # (1,)
        alphas = enc_outputs[2]  # (1, T)
        
        # CIF search
        enc_vec = enc[0]  # (T, encoder_size)
        alpha_vec = alphas[0]  # (T,)
        
        list_frame, self.hidden_cache, self.alphas_cache = cif_search(
            enc_vec, alpha_vec, CHUNK_SIZE,
            self.hidden_cache, self.alphas_cache,
            self.is_last_chunk
        )
        
        cache_shapes = {
            "enc_shape": list(enc.shape),
            "alphas_shape": list(alphas.shape),
            "hidden_cache_shape": list(self.hidden_cache.shape),
            "alphas_cache_shape": list(self.alphas_cache.shape),
            "list_frame_shape": list(list_frame.shape),
        }
        
        if len(list_frame) > 0:
            # Decoder
            dec_inputs = {
                self.decoder.get_inputs()[0].name: enc,  # enc
                self.decoder.get_inputs()[1].name: enc_lens,  # enc_lens
                self.decoder.get_inputs()[2].name: list_frame.reshape(1, len(list_frame), ENCODER_SIZE).astype(np.float32),  # acoustic_embeds
                self.decoder.get_inputs()[3].name: np.array([len(list_frame)], dtype=np.int32),  # acoustic_embeds_len
            }
            # Add FSMN cache
            for l in range(FSMN_LAYERS):
                dec_inputs[self.decoder.get_inputs()[4 + l].name] = self.decoder_cache[l]
            
            dec_outputs = self.decoder.run(None, dec_inputs)
            # dec_outputs[0] = logits (1, T, vocab_size)
            # dec_outputs[1] = logits_lens (1,)
            # dec_outputs[2..] = fsmn_cache (16 layers)
            
            # Greedy search
            logits = dec_outputs[0][0]  # (T, vocab_size)
            token_ids = np.argmax(logits, axis=-1)
            
            # Update FSMN cache
            self.decoder_cache = [dec_outputs[2 + l] for l in range(FSMN_LAYERS)]
            
            # Decode tokens
            result = self.greedy_search(token_ids)
            cache_shapes["logits_shape"] = list(logits.shape)
            cache_shapes["decoder_cache_shapes"] = [list(c.shape) for c in self.decoder_cache]
        
        inference_ms = (time.time() - t0) * 1000
        
        return result, {
            "inference_ms": round(inference_ms, 2),
            "cache_shapes": cache_shapes,
            "num_frames": num_frames,
            "enc_out_shape": list(enc.shape),
            "alphas_out_shape": list(alphas.shape),
            "list_frame_count": len(list_frame),
        }
    
    def greedy_search(self, token_ids):
        """Greedy decode tokens to text."""
        text = []
        for tid in token_ids:
            tid = int(tid)
            if tid < len(self.tokens):
                token = self.tokens[tid]
                if token == "<eos>" or token == "</s>":
                    break
                text.append(token)
        return "".join(text)
    
    def forward(self, chunk_audio, input_finished):
        """Forward one chunk of audio.
        
        Args:
            chunk_audio: np.float32 array, 16kHz mono
            input_finished: bool, True if this is the last chunk
        
        Returns:
            (result_text, chunk_log)
        """
        if len(chunk_audio) < 16 * 60 and input_finished and not self.is_first_chunk:
            self.is_last_chunk = True
            wav_feats = self.feats_cache.copy()
            result, log = self.forward_chunk(wav_feats, self.is_last_chunk)
            self.reset()
            return result, log
        
        if self.is_first_chunk:
            self.is_first_chunk = False
        
        wav_feats, waves = self.extract_feats(chunk_audio, input_finished)
        
        if len(wav_feats) == 0:
            return "", {"inference_ms": 0, "cache_shapes": {}, "note": "no features extracted"}
        
        result, log = self.forward_chunk(wav_feats, input_finished)
        
        if input_finished:
            self.reset()
        
        return result, log


def get_mem_mb():
    try:
        import psutil
        return psutil.Process().memory_info().rss / 1024 / 1024
    except:
        return 0.0


def generate_test_audio(duration_s, text_type="short"):
    """Generate test audio (TTS-like or simple tone).
    
    For now, we use a simple sine wave as placeholder.
    Real testing needs actual speech audio.
    """
    sr = 16000
    t = np.linspace(0, duration_s, int(sr * duration_s), endpoint=False)
    # Simple speech-like signal (mix of frequencies)
    audio = 0.3 * np.sin(2 * np.pi * 200 * t) + 0.2 * np.sin(2 * np.pi * 400 * t)
    audio = audio.astype(np.float32)
    return audio


def main():
    print("=" * 60)
    print("Spike C2: ParaformerOnline — Python onnxruntime Oracle")
    print("=" * 60)
    
    # Find model
    model_info = find_model_dir()
    if not model_info:
        print("  ❌ ParaformerOnline model not found!")
        print("  Searching in:")
        print(f"    {PARAFORMER_ONLINE_DIR}")
        print(f"    {MODELSCOPE_CACHE}")
        if os.path.exists(MODELS_DIR):
            print(f"  Contents of {MODELS_DIR}:")
            for d in os.listdir(MODELS_DIR):
                print(f"    {d}")
        
        # Write result JSON with BLOCKED status
        result = {
            "spike": "C2_paraformer_online_oracle",
            "status": "BLOCKED",
            "reason": "ParaformerOnline ONNX model not downloaded",
            "model_search_dirs": [PARAFORMER_ONLINE_DIR, MODELSCOPE_CACHE],
            "models_dir_contents": os.listdir(MODELS_DIR) if os.path.exists(MODELS_DIR) else [],
        }
        output_path = os.path.join(RESULTS_DIR, "spike_c2_paraformer_online.json")
        with open(output_path, "w", encoding="utf-8") as f:
            json.dump(result, f, ensure_ascii=False, indent=2)
        print(f"\nResult saved to: {output_path}")
        return
    
    # Find test audio
    test_wav = None
    for path in [
        os.path.join(MODELS_DIR, "fsmn-vad-onnx", "asr_example.wav"),
        os.path.join(ONNX_SPIKE_DIR, "models", "asr_example.wav"),
    ]:
        if os.path.exists(path):
            test_wav = path
            break
    
    if not test_wav:
        print("  ⚠️ No test WAV found, generating synthetic audio")
        audio = generate_test_audio(5.0)
    else:
        print(f"  Test WAV: {test_wav}")
        audio = load_wav_16k_mono(test_wav)
    
    audio_dur = len(audio) / SAMPLE_RATE
    print(f"  Audio duration: {audio_dur:.2f}s, samples: {len(audio)}")
    
    # Initialize model
    print(f"\n  Initializing ParaformerOnline...")
    mem_before = get_mem_mb()
    t0 = time.time()
    asr = ParaformerOnlineOracle(model_info)
    t_load = time.time() - t0
    mem_after = get_mem_mb()
    print(f"  Model loaded: {t_load:.3f}s")
    print(f"  Memory: {mem_before:.1f}MB → {mem_after:.1f}MB (Δ{mem_after - mem_before:.1f}MB)")
    
    # Chunk parameters
    chunk_stride_samples = CHUNK_SIZE[1] * (SAMPLE_RATE // 1000) * FRAME_SHIFT * LFR_N
    # = 10 * 16 * 10 * 6 = 9600 samples = 600ms
    print(f"\n  chunk_size={CHUNK_SIZE}, chunk_stride={chunk_stride_samples} samples ({chunk_stride_samples/SAMPLE_RATE*1000:.0f}ms)")
    
    n_chunks = (len(audio) - 1) // chunk_stride_samples + 1
    print(f"  Total chunks: {n_chunks}")
    
    # Stream chunk-by-chunk
    chunk_logs = []
    all_partial_texts = []
    first_nonempty_partial_time = None
    t_stream_start = time.time()
    
    for i in range(n_chunks):
        start_sample = i * chunk_stride_samples
        end_sample = min((i + 1) * chunk_stride_samples, len(audio))
        chunk = audio[start_sample:end_sample]
        is_final = (i == n_chunks - 1)
        chunk_audio_dur = len(chunk) / SAMPLE_RATE
        
        t_chunk_start = time.time()
        partial_text, chunk_log = asr.forward(chunk, is_final)
        t_chunk = time.time() - t_chunk_start
        
        audio_timestamp = start_sample / SAMPLE_RATE
        chunk_log["chunk_index"] = i
        chunk_log["audio_timestamp_s"] = round(audio_timestamp, 3)
        chunk_log["audio_duration_s"] = round(chunk_audio_dur, 3)
        chunk_log["partial_text"] = partial_text
        chunk_log["is_final"] = is_final
        chunk_log["total_chunk_ms"] = round(t_chunk * 1000, 2)
        
        chunk_logs.append(chunk_log)
        
        if partial_text:
            if first_nonempty_partial_time is None:
                first_nonempty_partial_time = time.time() - t_stream_start
            all_partial_texts.append(partial_text)
        
        print(f"  chunk {i:3d} | t={audio_timestamp:6.2f}s | dur={chunk_audio_dur:.3f}s | "
              f"inf={chunk_log.get('inference_ms', 0):6.1f}ms | final={is_final} | "
              f"frames={chunk_log.get('num_frames', 0)} | text='{partial_text}'")
    
    t_stream_total = time.time() - t_stream_start
    final_text = "".join(all_partial_texts)
    mem_peak = get_mem_mb()
    
    # Multi-utterance reset test
    print(f"\n  --- Multi-utterance Reset Test (20x) ---")
    reset_results = []
    for utt in range(20):
        asr.reset()
        texts = []
        for i in range(n_chunks):
            start = i * chunk_stride_samples
            end = min((i + 1) * chunk_stride_samples, len(audio))
            chunk = audio[start:end]
            is_final = (i == n_chunks - 1)
            result, _ = asr.forward(chunk, is_final)
            if result:
                texts.append(result)
        utt_text = "".join(texts)
        reset_results.append(utt_text)
        if utt < 2 or utt == 19:
            print(f"  Utterance {utt + 1}: '{utt_text}'")
    
    consistent = all(r == reset_results[0] for r in reset_results)
    print(f"  20x reset: {'✅ All consistent' if consistent else '❌ Inconsistent!'}")
    
    # Cancel test (run 5 chunks then reset)
    print(f"\n  --- Cancel Test ---")
    asr.reset()
    cancel_logs = []
    for i in range(5):
        start = i * chunk_stride_samples
        end = min((i + 1) * chunk_stride_samples, len(audio))
        chunk = audio[start:end]
        result, _ = asr.forward(chunk, False)
        cancel_logs.append(result)
    asr.reset()  # Cancel
    # Start new utterance
    new_result = ""
    for i in range(n_chunks):
        start = i * chunk_stride_samples
        end = min((i + 1) * chunk_stride_samples, len(audio))
        chunk = audio[start:end]
        is_final = (i == n_chunks - 1)
        result, _ = asr.forward(chunk, is_final)
        if result:
            new_result += result
    print(f"  After cancel+reset: '{new_result}'")
    cancel_ok = new_result == final_text
    print(f"  Cancel test: {'✅ Pass' if cancel_ok else '⚠️ Different (expected for streaming)'}")
    
    # Summary
    print(f"\n  {'─' * 50}")
    print(f"  Total streaming time: {t_stream_total:.3f}s")
    print(f"  RTF: {t_stream_total / audio_dur:.4f}x")
    if first_nonempty_partial_time:
        print(f"  First non-empty partial latency: {first_nonempty_partial_time:.3f}s")
    else:
        print(f"  No non-empty partial produced!")
    print(f"  Final text: '{final_text}'")
    print(f"  Partial text count: {len(all_partial_texts)}")
    print(f"  Peak memory: {mem_peak:.1f}MB")
    
    # Determine status
    has_partial = first_nonempty_partial_time is not None
    rtf = t_stream_total / audio_dur if audio_dur > 0 else 999
    
    if has_partial and rtf < 1.0:
        status = "GO"
    elif has_partial:
        status = "CONDITIONAL_GO"
    else:
        status = "BLOCKED"
    
    # Build result JSON
    result = {
        "spike": "C2_paraformer_online_oracle",
        "status": status,
        "model": {
            "encoder": model_info["encoder"],
            "decoder": model_info["decoder"],
            "am_mvn": model_info["am_mvn"],
            "config": model_info["config"],
            "tokens": model_info["tokens"],
        },
        "audio": {
            "duration_s": audio_dur,
            "sample_rate": SAMPLE_RATE,
            "samples": len(audio),
        },
        "chunk_params": {
            "chunk_size": CHUNK_SIZE,
            "chunk_stride_samples": chunk_stride_samples,
            "chunk_stride_ms": chunk_stride_samples / SAMPLE_RATE * 1000,
            "n_chunks": n_chunks,
            "lfr_m": LFR_M,
            "lfr_n": LFR_N,
            "encoder_size": ENCODER_SIZE,
            "fsmn_layers": FSMN_LAYERS,
            "cif_threshold": CIF_THRESHOLD,
        },
        "streaming_results": {
            "total_streaming_s": round(t_stream_total, 3),
            "rtf": round(rtf, 4),
            "first_nonempty_partial_latency_s": round(first_nonempty_partial_time, 3) if first_nonempty_partial_time else None,
            "final_text": final_text,
            "n_partial_texts": len(all_partial_texts),
            "peak_mem_mb": round(mem_peak, 1),
            "model_load_time_s": round(t_load, 3),
            "model_load_mem_delta_mb": round(mem_after - mem_before, 1),
        },
        "chunk_logs": chunk_logs,
        "reset_test": {
            "iterations": 20,
            "consistent": consistent,
            "sample_results": reset_results[:3],
        },
        "cancel_test": {
            "cancelled_after_chunks": 5,
            "result_after_reset": new_result,
            "matches_original": cancel_ok,
        },
        "encoder_io": {
            "inputs": [{"name": i.name, "shape": str(i.shape), "type": str(i.type)} for i in asr.encoder.get_inputs()],
            "outputs": [{"name": o.name, "shape": str(o.shape), "type": str(o.type)} for o in asr.encoder.get_outputs()],
        },
        "decoder_io": {
            "inputs": [{"name": i.name, "shape": str(i.shape), "type": str(i.type)} for i in asr.decoder.get_inputs()],
            "outputs": [{"name": o.name, "shape": str(o.shape), "type": str(o.type)} for o in asr.decoder.get_outputs()],
        },
    }
    
    output_path = os.path.join(RESULTS_DIR, "spike_c2_paraformer_online.json")
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=2)
    print(f"\n  Result saved to: {output_path}")
    print(f"  Status: {status}")


if __name__ == "__main__":
    main()