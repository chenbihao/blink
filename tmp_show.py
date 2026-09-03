import json

d = json.load(open(r'd:\Projects\Coding\blink\target\gate-pos-emb-fix-v2\gate_report.json', 'r', encoding='utf-8'))

for c in d['combinations']:
    if c['stt_engine'] == 'paraformer_onnx':
        for s in c['samples']:
            print(f"sample={s['sample_id']}")
            print(f"  ref={s['reference_raw'][:80]}")
            print(f"  hyp={s['hypothesis_raw'][:80]}")
            print(f"  CER={s['cer']:.3f}")
            print(f"  first_partial={s['first_partial_ms']}ms")
            print(f"  rtf={s['rtf']:.3f}")
            print()
    if 'gguf' in c['stt_engine']:
        for s in c['samples']:
            print(f"[{c['stt_engine']}] sample={s['sample_id']}")
            print(f"  ref={s['reference_raw'][:80]}")
            print(f"  hyp={s['hypothesis_raw'][:80]}")
            print(f"  CER={s['cer']:.3f}")
            print(f"  rtf={s['rtf']:.3f}")
            print()
