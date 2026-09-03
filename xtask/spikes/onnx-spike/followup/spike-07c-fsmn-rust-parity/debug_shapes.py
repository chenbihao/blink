#!/usr/bin/env python3
"""Debug shape mismatches between Python oracle and Rust runner."""
import json

o = json.load(open("xtask/spikes/onnx-spike/followup/results/spike_07c_oracle.json", encoding="utf-8"))
r = json.load(open("xtask/spikes/onnx-spike/followup/results/spike_07c_rust.json", encoding="utf-8"))

oc = o["scenarios"]["continuous_chunk"]["per_chunk"]
rc = r["scenarios"]["continuous_chunk"]["per_chunk"]

print("Oracle chunk shapes (first 5):")
for c in oc[:5]:
    print(f"  chunk {c['chunk_idx']}: input={c.get('input_shape')}, n_frames={c.get('n_frames')}")

print("\nRust chunk shapes (first 5):")
for c in rc[:5]:
    print(f"  chunk {c['chunk_idx']}: input={c.get('input_shape')}, n_frames={c.get('n_frames')}")

# Check splice behavior
# Python: librosa.stft with center=True produces ceil(len(w)/hop)+1 frames
# Rust: kaldi-native-fbank with snip_edges=true produces 1 + (len-512)//160 frames (if len >= 512)
# Then splice reduces by SPLICE_LEN-1 = 4

# For chunk 0: 1600 samples accumulated -> 1920 (with input_cache)
# Actually Python feeds 1600-sample chunks
# Python fbank: ceil(1600/160)+1 = 11 frames -> splice -> 11-4 = 7? No...
# Let's trace the actual input_shape

# Python chunk 0: input_shape = [1, 6, 400] -> 6 spliced frames = 10 fbank frames
# Rust chunk 0: input_shape = [1, 4, 400] -> 4 spliced frames = 8 fbank frames

# Difference is 2 fbank frames = 2 * 10ms = 20ms

# For chunk 1 (1600 samples):
# Python: [1, 7, 400] -> 11 fbank -> splice -> 7
# Rust: [1, 6, 400] -> 10 fbank -> splice -> 6

# Difference is 1 fbank frame = 10ms

# This is the classic center=True vs snip_edges=true difference
# librosa center=True pads by n_fft//2 = 256 on each side
# So for 1600 samples: effective = 1600 + 512 = 2112 (padded)
# frames = 1 + (2112 - 512) // 160 = 1 + 1600//160 = 1 + 10 = 11
# kaldi snip_edges: frames = 1 + (1600 - 400) // 160 = 1 + 1200//160 = 1 + 7 = 8
# Wait, that gives 8 vs 11. But actual is 10 vs 8 (spliced 6 vs 4)

# Let me check what wav_data length each chunk actually processes
# Python: accumulates input_cache + chunk, then computes usable frames
# For chunk 0: 1600 samples -> nf = (1600-400)//160 + 1 = 8 -> us = 7*160+400 = 1520
# Wait, that's wrong. nf = (len(s) - FRAME_LENGTH) // FRAME_SHIFT + 1
# = (1600 - 400) // 160 + 1 = 1200//160 + 1 = 7 + 1 = 8
# us = (8-1)*160 + 400 = 1120 + 400 = 1520

# So Python feeds 1520 samples to fbank -> with center=True:
# frames = 1 + (1520 + 512 - 512) // 160 = 1 + 1520//160 = 1 + 9.5 -> ceil? 
# librosa: frames = 1 + (len - win_length) // hop  with center padding
# Actually: center=True pads to len + n_fft, then frames = 1 + (padded_len - n_fft) // hop
# = 1 + (1520 + 512 - 512) // 160 = 1 + 1520 // 160 = 1 + 9 = 10? 
# No: 1520/160 = 9.5, so 1 + 9 = 10 frames -> splice -> 10-4 = 6. Yes!

# Rust: feeds 1520 samples to kaldi-native-fbank with snip_edges=true:
# frames = 1 + (1520 - 400) // 160 = 1 + 1120//160 = 1 + 7 = 8
# splice -> 8-4 = 4. Yes!

# So the difference is center=True (librosa) vs snip_edges=true (kaldi)
# center=True gives 10 fbank frames from 1520 samples
# snip_edges=true gives 8 fbank frames from 1520 samples
# Difference = 2 frames per chunk

# For subsequent chunks (input_cache is empty), same pattern but only 1 frame diff
# because input_cache accumulates differently

print("\n=== Root cause analysis ===")
print("Python librosa: center=True (pads n_fft//2 on each side)")
print("Rust kaldi-native-fbank: snip_edges=true (no padding, clips at edges)")
print()
print("For 1520 samples (first chunk usable):")
print(f"  librosa frames: 1 + (1520 + 512 - 512) // 160 = {1 + (1520 + 512 - 512) // 160}")
print(f"  kaldi frames:   1 + (1520 - 400) // 160 = {1 + (1520 - 400) // 160}")
print(f"  splice: librosa {10-4} vs kaldi {8-4}")
print()
print("Fix: Set snip_edges=false in Rust fbank, OR set center=False in Python librosa")
print("Best: align both to same convention (snip_edges=false matches librosa center=True)")
