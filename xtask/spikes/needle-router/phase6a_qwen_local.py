#!/usr/bin/env python3
"""0.24.6 Spike — phase 6a：LM Studio 本地 qwen3.5-0.8b 路由对照实验。

与 Needle（phase 3/5）同一 48 样本数据集、同一期望标签，两种姿势：
  Q1 zero-shot：规则描述 system prompt（建议面语义 + 中文语境，与 Needle v2 对齐）
  Q2 few-shot：同 system + 3 个示例对（中文句→ask_ai / 英文长句→translate / 应用名→none）

reasoning_effort=none 关闭 qwen3.5 默认 thinking（否则 0.8B 在此任务上 reasoning 失控，
单请求可烧 2000 token / 20s）。temperature=0。结果写 results/phase6a-qwen-local.json。
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
    "language (i.e. not Chinese), so the launcher suggests translating it to Chinese; or an "
    "explicit translation request (翻译 / fanyi / translate ...).\n"
    "ask_ai = the input is Chinese natural-language text addressed to an assistant "
    "(question, request, instruction).\n"
    "open_url = the input is a web URL.\n"
    "open_path = the input is a file system path (Windows C:\\dir\\file.ext, UNC "
    "\\\\server\\share, or unix /usr/bin).\n"
    "none = app name, search keyword, code identifier, shell command, email, UUID, color "
    "code, IP, or anything meant for the search box.\n"
    "Reply with ONLY the label, nothing else."
)

FEW_SHOT = [
    ("the weather is really nice today so let's go hiking this afternoon", "translate"),
    ("帮我写一封给客户的道歉邮件", "ask_ai"),
    ("chrome", "none"),
]


def log(m):
    print(m, flush=True)


def classify(text, fewshot=False):
    messages = [{"role": "system", "content": SYSTEM}]
    if fewshot:
        for u, a in FEW_SHOT:
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
    # 解析：优先精确匹配，否则在文本里找第一个出现的 label
    if content in LABELS:
        got = content
    else:
        got = next((l for l in LABELS if l in content), "unparsed:" + content[:20])
    usage = data.get("usage", {})
    return got, ms, usage.get("completion_tokens", 0), \
        (usage.get("completion_tokens_details") or {}).get("reasoning_tokens", 0)


def run(cases, fewshot, tag):
    rows, times = [], []
    for c in cases:
        got, ms, ctok, rtok = classify(c["input"], fewshot)
        times.append(ms)
        rows.append({**c, "got": got, "match": got == c["expected"],
                     "ms": round(ms, 1), "completion_tokens": ctok, "reasoning_tokens": rtok})
    by_cat = {}
    for r_ in rows:
        by_cat.setdefault(r_["category"], []).append(r_["match"])
    acc = {c: round(sum(v) / len(v), 2) for c, v in by_cat.items()}
    s = sorted(times)
    p = lambda q: s[min(int(len(s) * q), len(s) - 1)]
    block = {"tag": tag, "overall": round(sum(r_["match"] for r_ in rows) / len(rows), 3),
             "by_category": acc,
             "latency_ms": {"p50": round(p(0.5), 1), "p95": round(p(0.95), 1),
                            "max": round(s[-1], 1)},
             "rows": rows}
    log(f"  [{tag}] overall={block['overall']} p50={block['latency_ms']['p50']}ms "
        f"p95={block['latency_ms']['p95']}ms | " +
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
               "endpoint": BASE, "reasoning_effort": "none",
               "baseline_note": "Needle3-121m 十组姿势最高 0.396（decision.md §七）"}
    results["Q1_zero_shot"] = run(cases, False, "Q1 zero-shot")
    results["Q2_few_shot"] = run(cases, True, "Q2 few-shot(3)")
    out = os.path.join(HERE, "results", "phase6a-qwen-local.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump(results, f, ensure_ascii=False, indent=1)
    log(f"results -> {out}")


if __name__ == "__main__":
    main()
