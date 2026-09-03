#!/usr/bin/env python3
"""Debug detailed chunk-by-chunk comparison."""
import json

o = json.load(open("xtask/spikes/onnx-spike/followup/results/spike_07c_oracle.json", encoding="utf-8"))
r = json.load(open("xtask/spikes/onnx-spike/followup/results/spike_07c_rust.json", encoding="utf-8"))

oc = o["scenarios"]["continuous_chunk"]["per_chunk"]
rc = r["scenarios"]["continuous_chunk"]["per_chunk"]

print("Chunk-by-chunk comparison (continuous_chunk):")
print(f"{'Idx':>3} {'O_n_samp':>8} {'R_n_samp':>8} {'O_shape':>12} {'R_shape':>12} {'O_frames':>8} {'R_frames':>8}")
for i in range(min(len(oc), len(rc))):
    o_c = oc[i]
    r_c = rc[i]
    print(f"{o_c['chunk_idx']:>3} {o_c['n_samples']:>8} {r_c['n_samples']:>8} "
          f"{str(o_c.get('input_shape')):>12} {str(r_c.get('input_shape')):>12} "
          f"{o_c.get('n_frames'):>8} {r_c.get('n_frames'):>8}")

# Also check if scores match for chunks that DO have the same frame count
print("\n\nScore comparison for matching chunks:")
for i in range(min(len(oc), len(rc))):
    o_scores = oc[i].get("frame_scores", [])
    r_scores = rc[i].get("frame_scores", [])
    if len(o_scores) == len(r_scores) and len(o_scores) > 0:
        max_diff = max(abs(a - b) for a, b in zip(o_scores, r_scores))
        print(f"  chunk {i}: {len(o_scores)} frames, max_diff={max_diff:.8f}")
