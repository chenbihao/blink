#!/usr/bin/env python3
"""Spike D: VAD+ASR 对比矩阵 — 主入口

Combos: A=EnergyVad+ParaformerOnline, B=FSMN-VAD+ParaformerOnline,
C=EnergyVad+ParaformerOffline, D=FSMN-VAD+ParaformerOffline,
E=FSMN-VAD+SenseVoice(N/A), F=FSMN-VAD+ParaformerOnline(dup B)
GGUF Nano = NOT_MEASURED (C++ worker)
"""
import os,sys,time,json,traceback,gc
import numpy as np
sys.stderr.reconfigure(encoding="utf-8",errors="replace")
sys.stdout.reconfigure(encoding="utf-8",errors="replace")
import warnings;warnings.filterwarnings("ignore")

from spike_d_models import (
    build_corpus, EV, FV, PO, PF,
    eval_vad, cer, run_combo,
    SR, CSS
)

S=os.path.dirname(os.path.abspath(__file__))
F=os.path.dirname(S);O=os.path.dirname(F)
MD=os.path.join(O,"models");RD=os.path.join(F,"results")
os.makedirs(RD,exist_ok=True)
VAD_MD=os.path.join(MD,"fsmn-vad-onnx-v2")
PO_MD=os.path.join(MD,"paraformer-online-onnx")
PF_MD=os.path.join(MD,"paraformer-zh-onnx")

def main():
    print("="*60)
    print("Spike D: VAD+ASR Comparison Matrix")
    print("="*60)

    corpus=build_corpus()
    print(f"Corpus: {len(corpus)} items")
    for item in corpus:
        dur=len(item["audio"])/SR
        print(f"  {item['name']:30s} {dur:.1f}s  segs={item['segments']}")

    # Load models
    print("\n--- Loading FSMN-VAD ---")
    fv=FV(VAD_MD)
    print("\n--- Loading ParaformerOnline ---")
    po=PO(PO_MD)
    print("\n--- Loading Paraformer Offline ---")
    pf=PF(PF_MD)

    combos=[
        ("A","EnergyVad","paraformer_online",EV(),po),
        ("B","FSMN-VAD","paraformer_online",fv,po),
        ("C","EnergyVad","paraformer_offline",EV(),pf),
        ("D","FSMN-VAD","paraformer_offline",fv,pf),
        ("E","FSMN-VAD","sensevoice_onnx",fv,None),
        ("F","FSMN-VAD","paraformer_online",fv,po),
    ]

    all_results={}
    for cid,vtype,atype,vad,asr in combos:
        print(f"\n{'='*40}")
        print(f"Combo {cid}: {vtype} + {atype}")
        print(f"{'='*40}")
        if atype=="sensevoice_onnx":
            all_results[cid]={"status":"NOT_AVAILABLE","reason":"SenseVoice ONNX model not downloaded"}
            print("  SKIPPED: model not available");continue
        if atype=="gguf_nano":
            all_results[cid]={"status":"NOT_MEASURED","reason":"C++ worker, not tested in Python"}
            print("  SKIPPED: C++ worker");continue
        try:
            res=run_combo(cid,vtype,atype,corpus,vad,asr)
            all_results[cid]={"status":"MEASURED","results":res}
            for r in res:
                ve=r["vad_eval"]
                print(f"  {r['item']:30s} VAD: p={ve['p']:.2f} r={ve['r']:.2f} fa={ve['fa']} fr={ve['fr']} | ASR RTF={r['asr_rtf']:.3f} text='{r['asr_text'][:40]}'")
        except Exception as e:
            print(f"  ERROR: {e}")
            traceback.print_exc()
            all_results[cid]={"status":"ERROR","error":str(e)}

    # Aggregate
    print(f"\n{'='*60}")
    print("Aggregate Summary")
    print(f"{'='*60}")
    agg={}
    for cid,data in all_results.items():
        if data.get("status")!="MEASURED":
            agg[cid]={"status":data["status"]};continue
        res=data["results"]
        ps=[r["vad_eval"]["p"] for r in res if r["vad_eval"]["p"] is not None]
        rs=[r["vad_eval"]["r"] for r in res if r["vad_eval"]["r"] is not None]
        f1s=[2*p*r/(p+r) if(p+r)>0 else 0 for p,r in zip(ps,rs)]
        rtfs=[r["asr_rtf"] for r in res if r["asr_rtf"]>0]
        fas=[r["vad_eval"]["fa"] for r in res]
        frs=[r["vad_eval"]["fr"] for r in res]
        fps=[r["asr_first_partial_s"] for r in res if r["asr_first_partial_s"] is not None]
        agg[cid]={
            "vad_precision":round(float(np.mean(ps)),3) if ps else 0,
            "vad_recall":round(float(np.mean(rs)),3) if rs else 0,
            "vad_f1":round(float(np.mean(f1s)),3) if f1s else 0,
            "vad_total_fa":sum(fas),
            "vad_total_fr":sum(frs),
            "asr_rtf_avg":round(float(np.mean(rtfs)),4) if rtfs else 0,
            "asr_rtf_p95":round(float(np.percentile(rtfs,95)),4) if rtfs else 0,
            "asr_first_partial_avg_s":round(float(np.mean(fps)),3) if fps else None,
            "n_items":len(res),
        }
        print(f"  {cid}: P={agg[cid]['vad_precision']} R={agg[cid]['vad_recall']} F1={agg[cid]['vad_f1']} FA={agg[cid]['vad_total_fa']} FR={agg[cid]['vad_total_fr']} RTF={agg[cid]['asr_rtf_avg']}")

    # Key comparisons
    print(f"\n{'='*60}")
    print("Key Comparisons")
    print(f"{'='*60}")
    if "A" in agg and "B" in agg and agg["A"].get("status")=="MEASURED" and agg["B"].get("status")=="MEASURED":
        a,b=agg["A"],agg["B"]
        print(f"1. FSMN-VAD vs EnergyVad (with ParaformerOnline):")
        print(f"   F1: {b['vad_f1']} vs {a['vad_f1']}")
        print(f"   FA: {b['vad_total_fa']} vs {a['vad_total_fa']}")
        print(f"   FR: {b['vad_total_fr']} vs {a['vad_total_fr']}")
        f1_diff=b['vad_f1']-a['vad_f1']
        print(f"   FSMN-VAD {'显著优于' if f1_diff>0.05 else {'劣于' if f1_diff<-0.05 else '相当于'}}EnergyVad (F1差={f1_diff:+.3f})")
    if "B" in agg and "D" in agg and agg["B"].get("status")=="MEASURED" and agg["D"].get("status")=="MEASURED":
        b,d=agg["B"],agg["D"]
        print(f"2. ONNX ASR (streaming) vs GGUF Nano:")
        print(f"   RTF: {b['asr_rtf_avg']} (ONNX streaming) vs NOT_MEASURED (GGUF)")
        print(f"   ONNX streaming RTF < 1.0: {'是' if b['asr_rtf_avg']<1.0 else '否'}")
    if "C" in agg and "D" in agg:
        c,d=agg["C"],agg["D"]
        print(f"3. FSMN-VAD+GGUF是否已足够:")
        print(f"   EnergyVad F1={c.get('vad_f1','N/A')} vs FSMN-VAD F1={d.get('vad_f1','N/A')}")
    if "B" in agg and "D" in agg:
        b,d=agg["B"],agg["D"]
        print(f"4. 真流式收益:")
        if b.get('asr_first_partial_avg_s') and d.get('asr_rtf_avg'):
            print(f"   流式首partial={b['asr_first_partial_avg_s']}s vs 离线RTF={d['asr_rtf_avg']}")
            print(f"   流式 {'值得' if b['asr_first_partial_avg_s'] and b['asr_first_partial_avg_s']<1.0 else '不值得'} (首partial<1s)")

    # Save results
    output={
        "spike":"D_vad_asr_matrix",
        "timestamp":time.strftime("%Y-%m-%dT%H:%M:%S"),
        "combos_tested":[cid for cid in all_results],
        "combos_not_tested":{"E":"SenseVoice ONNX not downloaded","GGUF":"C++ worker, not Python"},
        "corpus_size":len(corpus),
        "corpus_items":[{"name":c["name"],"conditions":c["conditions"],"audio_dur_s":round(len(c["audio"])/SR,3),"segments":c["segments"]} for c in corpus],
        "per_combo":all_results,
        "aggregate":agg,
    }
    out_path=os.path.join(RD,"spike_d_vad_asr_matrix.json")
    with open(out_path,"w",encoding="utf-8") as f:
        json.dump(output,f,ensure_ascii=False,indent=2)
    print(f"\nResults saved to: {out_path}")
    print(f"\nSpike D complete.")

if __name__=="__main__":
    main()
