#!/usr/bin/env python3
"""0.24.6 Spike — phase 6d：结构化输出 + JSON few-shot 组合测试（qwen3.5-0.8b）。

对策目标：F1 的 unparsed 任务边界破坏（5/48 脱离标签输出）。三组对照：
  G1 schema 强制 + JSON 模板 system + zero-shot
  G2 schema 强制 + JSON 模板 system + 中文 5 示例（assistant 回复为标准 JSON）
  G3 无 schema（裸解码）+ 同 G2 JSON few-shot —— 分离"grammar 强制"与"示例教格式"贡献
LM Studio 实测支持 response_format json_schema（constrained decoding）；json_object 不支持。
结果写 results/phase6d-structured.json。
"""
import json
import os
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
BASE = "http://127.0.0.1:1234"
MODEL = "qwen3.5-0.8b"
LABELS = ("translate", "ask_ai", "open_url", "open_path", "none")

SCHEMA = {"type": "json_schema", "json_schema": {
    "name": "intent_class", "strict": True,
    "schema": {"type": "object",
               "properties": {"intent": {"type": "string",
                                          "enum": list(LABELS)}},
               "required": ["intent"], "additionalProperties": False}}}

SYSTEM = (
    "You are the intent classifier of the Blink launcher. The user's UI language is "
    "Chinese (zh-CN).\n"
    "Classify the user input into exactly one intent:\n"
    "- translate: natural-language text in a language foreign to the user's UI language "
    "(not Chinese), or an explicit translation request (翻译 / fanyi / translate).\n"
    "- ask_ai: Chinese natural-language text addressed to an assistant (question, "
    "request, instruction).\n"
    "- open_url: a web URL.\n"
    "- open_path: a file system path.\n"
    "- none: app name, search keyword, code identifier, shell command, email, UUID, "
    "color code, IP.\n"
    'Output MUST be a JSON object exactly like {"intent": "<label>"} — no other text.'
)

SHOTS = [
    ("今天心情不错想出去走走", '{"intent": "ask_ai"}'),
    ("life is like a box of chocolates you never know what you gonna get",
     '{"intent": "translate"}'),
    ("https://www.wikipedia.org", '{"intent": "open_url"}'),
    ("C:\\Windows\\System32\\cmd.exe", '{"intent": "open_path"}'),
    ("firefox", '{"intent": "none"}'),
]


def log(m):
    print(m, flush=True)


def classify(text, shots, use_schema):
    messages = [{"role": "system", "content": SYSTEM}]
    for u, a in shots:
        messages.append({"role": "user", "content": u})
        messages.append({"role": "assistant", "content": a})
    messages.append({"role": "user", "content": text})
    body = {"model": MODEL, "temperature": 0, "max_tokens": 48,
            "reasoning_effort": "none", "messages": messages}
    if use_schema:
        body["response_format"] = SCHEMA
    req = urllib.request.Request(BASE + "/v1/chat/completions",
                                 data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=120) as r:
        data = json.loads(r.read())
    ms = (time.perf_counter() - t0) * 1000
    content = (data["choices"][0]["message"].get("content") or "").strip()
    try:
        obj = json.loads(content)
        got = obj.get("intent", "unparsed")
        if got not in LABELS:
            got = "unparsed"
    except (json.JSONDecodeError, ValueError):
        got = "unparsed"
    return got, ms, content[:60]


def run(cases, shots, use_schema, tag):
    rows, times = [], []
    for c in cases:
        got, ms, raw = classify(c["input"], shots, use_schema)
        times.append(ms)
        rows.append({**c, "got": got, "raw": raw, "match": got == c["expected"],
                     "ms": round(ms, 1)})
    by_cat = {}
    for r_ in rows:
        by_cat.setdefault(r_["category"], []).append(r_["match"])
    acc = {c: round(sum(v) / len(v), 2) for c, v in by_cat.items()}
    s = sorted(times)
    unparsed = sum(1 for r_ in rows if r_["got"] == "unparsed")
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
               "baseline": "F1 few-shot 0.646 (unparsed 5/48)"}
    results["G1_schema_zeroshot"] = run(cases, [], True, "G1 schema+0shot")
    results["G2_schema_zh5shot"] = run(cases, SHOTS, True, "G2 schema+zh5shot-JSON")
    results["G3_noschema_zh5shot"] = run(cases, SHOTS, False, "G3 noschema+zh5shot-JSON")
    out = os.path.join(HERE, "results", "phase6d-structured.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump(results, f, ensure_ascii=False, indent=1)
    log(f"results -> {out}")


if __name__ == "__main__":
    main()
