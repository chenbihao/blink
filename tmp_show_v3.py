import json

d = json.load(open(r'd:\Projects\Coding\blink\target\gate-chunk-fix-v3\gate_report.json', 'r', encoding='utf-8'))

for c in d['combinations']:
    print(f"\n=== {c['name']} (path={c['path_type']}) ===")
    if c['stt_engine'] == 'paraformer_onnx' and c.get('cer_stats'):
        print(f"  CER: mean={c['cer_stats']['mean']:.4f} p50={c['cer_stats']['p50']:.4f}")
    if c.get('rtf_stats'):
        print(f"  RTF: mean={c['rtf_stats']['mean']:.3f} p50={c['rtf_stats']['p50']:.3f}")
    if c.get('first_partial_stats'):
        print(f"  first_partial: mean={c['first_partial_stats']['mean']:.0f}ms p50={c['first_partial_stats']['p50']:.0f}ms")
    if c.get('final_after_release_stats'):
        print(f"  final_after_release: mean={c['final_after_release_stats']['mean']:.0f}ms p50={c['final_after_release_stats']['p50']:.0f}ms")
    for s in c['samples']:
        print(f"  [{s['sample_id']}] CER={s['cer']:.4f} RTF={s['rtf']:.3f} first_partial={s.get('first_partial_ms','-')}ms final={s.get('final_after_release_ms','-')}ms")
        print(f"    ref: {s['reference_raw'][:60]}")
        print(f"    hyp: {s['hypothesis_raw'][:60]}")
