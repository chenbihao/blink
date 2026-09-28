#!/usr/bin/env python3
"""0.24.6 Spike — phase 6c：few-shot 变体系统测试（qwen3.5-0.8b）。

Q2（英文 3 示例）已测：0.521 且引发"模仿回答"退化（模型直接回答输入而非贴标签）。
本实验补齐未测变体（示例全部为 dataset 外新样本，无答案泄露）：
  F1 中文为主 5 示例（每类 1 个，中文句直接演示"中文→ask_ai"目标映射）
  F2 每类 2 例共 10 示例（加强覆盖）
  F3 英文 3 示例复跑（= Q2 对照，确认基线可复现）
结果写 results/phase6c-fewshot.json。
"""
import json
import os
import statistics
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
BASE = "http://127.0.0.1:1234"
MODEL = "qwen3.5-0.8b"
LABELS = ("translate", "ask_ai", "open_url", "open_path", "none")

SYSTEM = (
    "You are the intent classifier of a launcher app. The user's UI language is Chinese "
    "(zh-CN). Classify the user input into exactly one label: translate, ask_ai, open_url, "
    "open_path, none.\n"
    "translate = the input is natural-language text in a language foreign to the user's UI "
    "language (i.e. not Chinese), or an explicit translation request.\n"
    "ask_ai = the input is Chinese natural-language text addressed to an assistant.\n"
    "open_url = the input is a web URL.\n"
    "open_path = the input is a file system path.\n"
    "none = app name, search keyword, code identifier, command, email, UUID, color, IP.\n"
    "Reply with ONLY the label, nothing else."
)

# dataset 外新样本
F1 = [
    ("今天心情不错想出去走走", "ask_ai"),
    ("life is like a box of chocolates you never know what you gonna get", "translate"),
    ("https://www.wikipedia.org", "open_url"),
    ("C:\\Windows\\System32\\cmd.exe", "open_path"),
    ("firefox", "none"),
]
F2 = F1 + [
    ("这道菜在家怎么做才好吃", "ask_ai"),
    ("never gonna give you up never gonna let you down", "translate"),
    ("https://mail.example.com:8443/inbox", "open_url"),
    ("\\\\nas\\media\\movies\\movie.mkv", "open_path"),
    ("steam", "none"),
]
F3 = [
    ("the weather is really nice today so let's go hiking this afternoon", "translate"),
    ("帮我写一封给客户的道歉邮件", "ask_ai"),
    ("chrome", "none"),
]


def log(m):
    print(m, flush=True)


def classify(text, shots):
    messages = [{"role": "system", "content": SYSTEM}]
    for u, a in shots:
        messages.append({"role": "user", "content": u})
        messages.append({"role": "assistant", "content": a})
    messages.append({"role": "user", "content": text})
    body = {"model": MODEL, "temperature": 0, "max_tokens": 24,
            "reasoning_effort": "none", "messages": messages}
    req = urllib.request.Request(BASE + "/v1/chat/completions",
                                 data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=120) as r:
        data = json.loads(r.read())
    ms = (time.perf_counter() - t0) * 1000
    content = (data["choices"][0]["message"].get("content") or "").strip().lower()
    if content in LABELS:
        got = content
    else:
        got = next((l for l in LABELS if l in content), "unparsed")
    return got, ms, content


def run(cases, shots, tag):
    rows, times, unparsed = [], [], 0
    for c in cases:
        got, ms, raw = classify(c["input"], shots)
        times.append(ms)
        if got == "unparsed":
            unparsed += 1
        rows.append({**c, "got": got, "raw": raw[:40], "match": got == c["expected"],
                     "ms": round(ms, 1)})
    by_cat = {}
    for r_ in rows:
        by_cat.setdefault(r_["category"], []).append(r_["match"])
    acc = {c: round(sum(v) / len(v), 2) for c, v in by_cat.items()}
    s = sorted(times)
    block = {"tag": tag, "overall": round(sum(r_["match"] for r_ in rows) / len(rows), 3),
             "by_category": acc, "unparsed": unparsed,
             "latency_ms_p50": round(s[len(s) // 2], 1), "rows": rows}
    log(f"  [{tag}] overall={block['overall']} unparsed={unparsed} "
        f"p50={block['latency_ms_p50']}ms | " + " ".join(f"{k}={v}" for k, v in acc.items()))
    return block


def main():
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except AttributeError:
        pass
    with open(os.path.join(HERE, "dataset.json"), encoding="utf-8") as f:
        cases = json.load(f)["cases"]
    results = {"date": time.strftime("%Y-%m-%d %H:%M:%S"), "model": MODEL,
               "baseline": "Q1 zero-shot 0.562 (natural_zh 0.0)"}
    results["F1_zh_5shot"] = run(cases, F1, "F1 zh-dominant 5-shot")
    results["F2_10shot"] = run(cases, F2, "F2 per-class 10-shot")
    results["F3_en_3shot"] = run(cases, F3, "F3 en 3-shot (Q2 replay)")
    out = os.path.join(HERE, "results", "phase6c-fewshot.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump(results, f, ensure_ascii=False, indent=1)
    log(f"results -> {out}")


if __name__ == "__main__":
    main()
