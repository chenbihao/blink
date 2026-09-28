#!/usr/bin/env python3
"""0.24.6 Spike — phase 6b：embedding 标签相似度路由（零训练零下载）。

用 LM Studio 现成 nomic-embed-text-v1.5（768 维）：样本 embedding 与 5 个标签
embedding 做 cosine 最近邻。两种标签姿势对照：
  A. 纯标签名（"translate"...）
  B. 标签名+英文描述（与 laya QUESTIONS / tools.json 语义对齐）
对比 Needle 0.396 / qwen3.5-0.8b 0.562。结果写 results/phase6b-embedding.json。
注意：nemic 以英文为主，中文样本表现弱是"模型选型"问题而非路线问题
（多语言 embedding 如 bge-m3 可后续对照，本次网络受限未下载）。
"""
import json
import math
import os
import statistics
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
BASE = "http://127.0.0.1:1234"
MODEL = "text-embedding-nomic-embed-text-v1.5"

LABELS = ("translate", "ask_ai", "open_url", "open_path", "none")
DESCS = {
    "translate": "natural-language text in a language foreign to the user's UI language Chinese, or an explicit translation request",
    "ask_ai": "Chinese natural-language text addressed to an assistant: a question, request, or instruction",
    "open_url": "a web URL such as https://example.com/path",
    "open_path": "a file system path such as C:\\dir\\file.ext or /usr/local/bin/tool",
    "none": "an app name, search keyword, code identifier, shell command, email, UUID, color code, or IP address",
}


def log(m):
    print(m, flush=True)


def embed(texts):
    body = {"model": MODEL, "input": texts}
    req = urllib.request.Request(BASE + "/v1/embeddings",
                                 data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=120) as r:
        data = json.loads(r.read())
    ms = (time.perf_counter() - t0) * 1000
    return [d["embedding"] for d in sorted(data["data"], key=lambda x: x["index"])], ms


def cos(a, b):
    dot = sum(x * y for x, y in zip(a, b))
    return dot / (math.sqrt(sum(x * x for x in a)) * math.sqrt(sum(y * y for y in b)))


def run(cases, label_texts, tag):
    label_vecs, _ = embed(label_texts)
    rows, times = [], []
    for c in cases:
        vecs, ms = embed([c["input"]])
        vec = vecs[0]
        times.append(ms)
        scores = sorted(((cos(vec, lv), l) for lv, l in zip(label_vecs, LABELS)), reverse=True)
        got = scores[0][1]
        rows.append({**c, "got": got, "match": got == c["expected"],
                     "ms": round(ms, 1),
                     "margin": round(scores[0][0] - scores[1][0], 4),
                     "top2": [[l, round(s, 3)] for s, l in scores[:2]]})
    by_cat = {}
    for r_ in rows:
        by_cat.setdefault(r_["category"], []).append(r_["match"])
    acc = {c: round(sum(v) / len(v), 2) for c, v in by_cat.items()}
    s = sorted(times)
    block = {"tag": tag, "overall": round(sum(r_["match"] for r_ in rows) / len(rows), 3),
             "by_category": acc,
             "latency_ms_p50": round(s[len(s) // 2], 1),
             "rows": rows}
    log(f"  [{tag}] overall={block['overall']} p50={block['latency_ms_p50']}ms | " +
        " ".join(f"{k}={v}" for k, v in acc.items()))
    return block


def main():
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except AttributeError:
        pass
    with open(os.path.join(HERE, "dataset.json"), encoding="utf-8") as f:
        cases = json.load(f)["cases"]
    results = {"date": time.strftime("%Y-%m-%d %H:%M:%S"), "model": MODEL,
               "baseline": "Needle 0.396 / qwen3.5-0.8b 0.562"}
    results["A_label_only"] = run(cases, list(LABELS), "A label-only")
    results["B_label_plus_desc"] = run(
        cases, [f"{l}: {DESCS[l]}" for l in LABELS], "B label+desc")
    out = os.path.join(HERE, "results", "phase6b-embedding.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump(results, f, ensure_ascii=False, indent=1)
    log(f"results -> {out}")


if __name__ == "__main__":
    main()
