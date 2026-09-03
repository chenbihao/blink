#!/usr/bin/env python3
"""Spike 07C: Compare Python oracle vs Rust runner for parity validation."""
import json, sys, os

R = os.path.dirname(os.path.abspath(__file__))
F = os.path.dirname(R)
RD = os.path.join(F, "results")

oracle_path = os.path.join(RD, "spike_07c_oracle.json")
rust_path   = os.path.join(RD, "spike_07c_rust.json")

with open(oracle_path, "r", encoding="utf-8") as f:
    oracle = json.load(f)
with open(rust_path, "r", encoding="utf-8") as f:
    rust = json.load(f)

print("=" * 70)
print("Spike 07C: Python vs Rust Parity Report")
print("=" * 70)

# ─── Model info ────────────────────────────────────────────────────────────
print("\n[1] Model Info (from Oracle):")
mi = oracle.get("model_info", {})
print(f"  Inputs:  {mi.get('input_names')}")
print(f"  Outputs: {mi.get('output_names')}")
print(f"  Input shapes:  {mi.get('input_shapes')}")
print(f"  Output shapes: {mi.get('output_shapes')}")
cfg = mi.get("config", {})
print(f"  Config: n_mels={cfg.get('n_mels')}, frame_len={cfg.get('frame_length_ms')}ms, "
      f"frame_shift={cfg.get('frame_shift_ms')}ms, splice={cfg.get('splice_len')}, "
      f"cache_layers={cfg.get('cache_layers')}, cache_dim={cfg.get('cache_dim')}, "
      f"cache_lorder={cfg.get('cache_lorder')}, input_dim={cfg.get('input_dim')}")

# ─── Scenario-by-scenario comparison ───────────────────────────────────────
print("\n[2] Scenario Parity:")
all_ok = True

for name in oracle.get("scenarios", {}):
    o = oracle["scenarios"][name]
    r = rust.get("scenarios", {}).get(name)
    if r is None:
        print(f"\n  --- {name} --- MISSING in Rust!")
        all_ok = False
        continue

    print(f"\n  --- {name} ---")
    print(f"    Audio:   oracle={o['audio_duration_s']}s / rust={r['audio_duration_s']}s")
    print(f"    Samples: oracle={o['n_samples']} / rust={r['n_samples']}")
    print(f"    Chunks:  oracle={o['n_inference_chunks']} / rust={r['n_inference_chunks']}")
    print(f"    Frames:  oracle={o['n_frames']} / rust={r['n_frames']}")

    # Segment comparison
    o_segs = o["detected_segments"]
    r_segs = r["detected_segments"]
    seg_match = (o_segs == r_segs)
    if not seg_match:
        print(f"    Segments MISMATCH:")
        print(f"      oracle: {o_segs}")
        print(f"      rust:   {r_segs}")
        all_ok = False
    else:
        print(f"    Segments: MATCH ({len(o_segs)} segments)")

    # Frame scores comparison
    o_scores = o["frame_scores"]
    r_scores = r["frame_scores"]

    if len(o_scores) != len(r_scores):
        print(f"    Frame count MISMATCH: oracle={len(o_scores)} vs rust={len(r_scores)}")
        all_ok = False
    else:
        max_diff = 0.0
        max_idx = -1
        for i, (a, b) in enumerate(zip(o_scores, r_scores)):
            d = abs(a - b)
            if d > max_diff:
                max_diff = d
                max_idx = i
        # Also compute mean abs diff
        mean_diff = sum(abs(a - b) for a, b in zip(o_scores, r_scores)) / len(o_scores)
        # Count how many exceed tolerance
        tolerance = 0.01  # 1e-2
        n_exceed = sum(1 for a, b in zip(o_scores, r_scores) if abs(a - b) > tolerance)

        print(f"    Frame scores: {len(o_scores)} frames")
        print(f"      max_diff = {max_diff:.8f} (frame {max_idx})")
        print(f"      mean_diff = {mean_diff:.8f}")
        print(f"      exceeding 0.01 tol: {n_exceed}/{len(o_scores)}")

        if max_diff > 0.1:
            print(f"      STATUS: MISMATCH (max_diff > 0.1)")
            all_ok = False
        elif max_diff > 0.01:
            print(f"      STATUS: CLOSE (max_diff 0.01-0.1)")
        else:
            print(f"      STATUS: MATCH (max_diff < 0.01)")

    # Per-chunk comparison
    o_chunks = o.get("per_chunk", [])
    r_chunks = r.get("per_chunk", [])
    if len(o_chunks) != len(r_chunks):
        print(f"    Chunk count MISMATCH: oracle={len(o_chunks)} vs rust={len(r_chunks)}")
        all_ok = False
    else:
        chunk_ok = True
        for ci, (oc, rc) in enumerate(zip(o_chunks, r_chunks)):
            o_inf = oc.get("inference_ms", 0)
            r_inf = rc.get("inference_ms", 0)
            o_shape = oc.get("input_shape")
            r_shape = rc.get("input_shape")
            if o_shape != r_shape:
                print(f"    Chunk {ci} input_shape MISMATCH: {o_shape} vs {r_shape}")
                chunk_ok = False
                all_ok = False
            o_fs = oc.get("frame_scores", [])
            r_fs = rc.get("frame_scores", [])
            if len(o_fs) != len(r_fs):
                print(f"    Chunk {ci} frame count MISMATCH: {len(o_fs)} vs {len(r_fs)}")
                chunk_ok = False
                all_ok = False
            else:
                cmax = max(abs(a - b) for a, b in zip(o_fs, r_fs)) if o_fs else 0
                if cmax > 0.1:
                    print(f"    Chunk {ci} score max_diff={cmax:.8f}")
                    chunk_ok = False
                    all_ok = False
        if chunk_ok:
            print(f"    Per-chunk: {len(o_chunks)} chunks all match")

# ─── Reset & Multi-session ─────────────────────────────────────────────────
print("\n[3] Reset Test:")
o_reset = oracle.get("reset_test", {})
r_reset = rust.get("reset_test", {})
print(f"  Oracle: consistent={o_reset.get('consistent')}, trials={o_reset.get('trials')}")
print(f"  Rust:   consistent={r_reset.get('consistent')}, trials={r_reset.get('trials')}")
if o_reset.get('consistent') != r_reset.get('consistent'):
    all_ok = False
    print("  STATUS: MISMATCH")
else:
    print("  STATUS: MATCH")

print("\n[4] Multi-session Test:")
o_ms = oracle.get("multi_session_test", {})
r_ms = rust.get("multi_session_test", {})
print(f"  Oracle: consistent={o_ms.get('consistent')}, trials={o_ms.get('n_trials')}")
print(f"  Rust:   consistent={r_ms.get('consistent')}, trials={r_ms.get('n_trials')}")
if o_ms.get('consistent') != r_ms.get('consistent'):
    all_ok = False
    print("  STATUS: MISMATCH")
else:
    print("  STATUS: MATCH")

# ─── Performance ────────────────────────────────────────────────────────────
print("\n[5] Performance:")
o_perf = oracle.get("performance", {})
r_perf = rust.get("performance", {})
print(f"  Oracle: p50={o_perf.get('p50_ms')}ms, p95={o_perf.get('p95_ms')}ms, budget={o_perf.get('budget_ms')}ms, within={o_perf.get('within_budget')}")
print(f"  Rust:   p50={r_perf.get('p50_ms')}ms, p95={r_perf.get('p95_ms')}ms, budget={r_perf.get('budget_ms')}ms, within={r_perf.get('within_budget')}")

# ─── Memory ────────────────────────────────────────────────────────────────
print("\n[6] Memory (Rust only):")
print(f"  Model load time: {rust.get('model_load_time_s')}s")
print(f"  Model load mem delta: {rust.get('model_load_mem_delta_mb')}MB")
print(f"  Peak working set: {rust.get('peak_mem_mb')}MB")

# ─── Verdict ────────────────────────────────────────────────────────────────
print("\n" + "=" * 70)
if all_ok:
    print("VERDICT: GO — Rust achieves parity with Python oracle")
else:
    print("VERDICT: REVIEW — Some mismatches detected, see details above")
print("=" * 70)
