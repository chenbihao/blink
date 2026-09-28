#!/usr/bin/env python3
"""0.24.6 Spike — phase 7：laya 决策模型（BERT 系非自回归）路由对照。

laya-multilingual（mmBERT-base 322M，100+ 语言）choice 任务跑同一 48 样本，
标签语义与 tools.json 对齐（建议面语义）。对比 Needle 0.396 / qwen3.5-0.8b 0.562。

需先：python -m venv .venv && .venv/Scripts/pip install laya
结果写 results/phase7-laya.json。
"""
import json
import os
import statistics
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))

QUESTIONS = {
    "intent": {
        "type": "choice",
        "instructions": (
            "Classify the launcher input text `request` into exactly one intent. "
            "The launcher user's UI language is Chinese (zh-CN): Chinese text is the "
            "user's own language."
        ),
        "criteria": {
            "translate": (
                "natural-language text (a full sentence or phrase, not a URL/path/"
                "identifier) written in a language foreign to the user's UI language "
                "Chinese, or an explicit translation request (翻译 / fanyi / translate)"
            ),
            "ask_ai": (
                "Chinese natural-language text addressed to an assistant: a question, "
                "request, or instruction written in Chinese"
            ),
            "open_url": "a web URL (http/https, host or host/path, optionally port)",
            "open_path": (
                "a file system path: Windows absolute like C:\\dir\\file.ext, UNC "
                "\\\\server\\share, or unix /usr/local/bin/tool"
            ),
            "none": (
                "an app name, search keyword, code identifier, shell command, email, "
                "UUID, color code, IP, or anything meant for the search box"
            ),
        },
    }
}


def log(m):
    print(m, flush=True)


def main():
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except AttributeError:
        pass
    sys.path.insert(0, os.path.join(HERE, ".venv", "Lib", "site-packages"))
    from laya import Router

    t0 = time.time()
    router = Router(default="multilingual")
    log(f"router ready in {time.time()-t0:.1f}s")

    # 单发探测：输出结构与延迟
    r = router.predict(state="chrome", questions=QUESTIONS)
    log(f"probe output: {json.dumps(r, ensure_ascii=False)[:600]}")

    with open(os.path.join(HERE, "dataset.json"), encoding="utf-8") as f:
        cases = json.load(f)["cases"]

    rows, times = [], []
    for c in cases:
        t1 = time.perf_counter()
        r = router.predict(state=c["input"], questions=QUESTIONS)
        ms = (time.perf_counter() - t1) * 1000
        times.append(ms)
        ans = (r.get("answers") or {}).get("intent") or {}
        got = ans.get("choice") or ans.get("value") or str(ans)[:40]
        probs = ans.get("probabilities") or ans.get("probs")
        top_conf = ans.get("confidence") or (max(probs.values()) if isinstance(probs, dict) else None)
        rows.append({**c, "got": got, "match": got == c["expected"],
                     "ms": round(ms, 1), "confidence": top_conf,
                     "probs": probs})
    by_cat = {}
    for r_ in rows:
        by_cat.setdefault(r_["category"], []).append(r_["match"])
    acc = {c: round(sum(v) / len(v), 2) for c, v in by_cat.items()}
    s = sorted(times)
    p = lambda q: s[min(int(len(s) * q), len(s) - 1)]
    match_conf = [r_["confidence"] for r_ in rows if r_["match"] and r_["confidence"]]
    miss_conf = [r_["confidence"] for r_ in rows if not r_["match"] and r_["confidence"]]
    results = {
        "date": time.strftime("%Y-%m-%d %H:%M:%S"), "model": "laya-multilingual (mmBERT-base 322M)",
        "overall": round(sum(r_["match"] for r_ in rows) / len(rows), 3),
        "by_category": acc,
        "latency_ms": {"p50": round(p(0.5), 1), "p95": round(p(0.95), 1), "max": round(s[-1], 1)},
        "conf_distribution": {
            "match_mean": round(statistics.mean(match_conf), 3) if match_conf else None,
            "mismatch_mean": round(statistics.mean(miss_conf), 3) if miss_conf else None},
        "rows": rows,
    }
    log(f"\n[laya-multilingual] overall={results['overall']} "
        f"p50={results['latency_ms']['p50']}ms p95={results['latency_ms']['p95']}ms | " +
        " ".join(f"{k}={v}" for k, v in acc.items()))
    log(f"conf match/miss: {results['conf_distribution']}")
    out = os.path.join(HERE, "results", "phase7-laya.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump(results, f, ensure_ascii=False, indent=1)
    log(f"results -> {out}")


if __name__ == "__main__":
    main()
