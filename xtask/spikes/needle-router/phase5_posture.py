#!/usr/bin/env python3
"""0.24.6 Needle Spike — phase 5 姿势归因实验。

问题：phase 3 的三个结构性失败（语义错位 / 中文语境误判 / confidence 分层失效），
归因是"模型能力不足"还是"使用姿势不对"？两个姿势变量：

- 工具形状：v1 单工具+enum（意图分类语义，基线 0.375）vs v2 单工具+enum（建议面语义
  重写）vs v3 五工具零参（Needle 原生 tool-selection 训练分布）
- system：minimal（含用户语言）vs none（仅日期）

组合矩阵（每组 48 样本，期望标签同 dataset.json）：
  E1 v2+minimal  E2 v3+minimal  E3 v3+none  （E0 = phase3 v1+minimal = 0.375 基线）

对赢家组合追加：--forced 对照（消除空调用）、confidence 分层、warm 延迟。

结果写 results/phase5-posture.json。
"""
import json
import os
import socket
import statistics
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ASSETS = os.path.join(HERE, ".assets")
RESULTS = os.path.join(HERE, "results")

NAME_TO_INTENT = {"suggest_translation": "translate", "ask_ai": "ask_ai",
                  "open_url": "open_url", "open_path": "open_path",
                  "no_intent": "none", "route_intent": "enum-tool"}


def log(m):
    print(m, flush=True)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class Serve:
    def __init__(self, tools, system="system-minimal.txt", forced=False):
        self.port = free_port()
        cmd = [os.path.join(ASSETS, "needle.exe"),
               "--model", os.path.join(ASSETS, "needle3.cact"),
               "--tools", os.path.join(HERE, tools),
               "--system", os.path.join(HERE, system),
               "--serve", "--port", str(self.port)]
        if forced:
            cmd.append("--forced")
        env = dict(os.environ, NEEDLE_TELEMETRY="0", DO_NOT_TRACK="1")
        self.proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL, env=env)
        self.base = f"http://127.0.0.1:{self.port}"
        self.tools = tools

    def post(self, path, body, timeout=120):
        req = urllib.request.Request(self.base + path, data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json"})
        t0 = time.perf_counter()
        with urllib.request.urlopen(req, timeout=timeout) as r:
            data = json.loads(r.read())
        return (time.perf_counter() - t0) * 1000, data

    def wait_ready(self, timeout=30):
        t0 = time.time()
        while time.time() - t0 < timeout:
            if self.proc.poll() is not None:
                raise RuntimeError("serve exited early")
            try:
                self.post("/reset", {}, timeout=2)
                return
            except Exception:
                time.sleep(0.3)
        raise RuntimeError("serve not ready")

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def classify(sv, text):
    sv.post("/reset", {})
    _, r = sv.post("/complete", {"input": text})
    calls = r.get("function_calls") or []
    supp = r.get("suppressed_calls") or []
    if calls:
        c = calls[0]
        got = c["arguments"].get("intent") if "intent" in c.get("arguments", {}) \
            else NAME_TO_INTENT.get(c["name"], "unknown:" + c["name"])
        return got, r.get("confidence", 0.0), "call"
    if supp:
        c = supp[0]
        got = c["arguments"].get("intent") if "intent" in c.get("arguments", {}) \
            else NAME_TO_INTENT.get(c["name"], "unknown:" + c["name"])
        return got, r.get("confidence", 0.0), "suppressed"
    return "none", r.get("confidence", 0.0), "empty"


def run_matrix(sv, cases, tag):
    rows, empty_calls, times = [], 0, []
    for c in cases:
        t0 = time.perf_counter()
        got, conf, kind = classify(sv, c["input"])
        times.append((time.perf_counter() - t0) * 1000)
        if kind == "empty":
            empty_calls += 1
        rows.append({**c, "got": got, "confidence": round(conf, 3),
                     "call_kind": kind, "match": got == c["expected"]})
    by_cat = {}
    for r_ in rows:
        by_cat.setdefault(r_["category"], []).append(r_["match"])
    acc = {c: round(sum(v) / len(v), 2) for c, v in by_cat.items()}
    overall = round(sum(r_["match"] for r_ in rows) / len(rows), 3)
    match_conf = [r_["confidence"] for r_ in rows if r_["match"]]
    miss_conf = [r_["confidence"] for r_ in rows if not r_["match"]]
    block = {
        "tag": tag, "overall": overall, "by_category": acc,
        "empty_calls": empty_calls,
        "conf": {"match_mean": round(statistics.mean(match_conf), 3) if match_conf else None,
                 "mismatch_mean": round(statistics.mean(miss_conf), 3) if miss_conf else None},
        "latency_ms_p50": round(statistics.median(times), 1),
        "rows": rows,
    }
    log(f"  [{tag}] overall={overall} empty={empty_calls} p50={block['latency_ms_p50']}ms | " +
        " ".join(f"{k}={v}" for k, v in acc.items()))
    return block


def main():
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except AttributeError:
        pass
    with open(os.path.join(HERE, "dataset.json"), encoding="utf-8") as f:
        cases = json.load(f)["cases"]

    results = {"date": time.strftime("%Y-%m-%d %H:%M:%S"),
               "baseline_note": "E0 = phase3 v1(enum 意图语义)+minimal = 0.375"}

    # E1 v2 语义重写 + minimal
    sv = Serve("tools-v2-semantic.json", "system-minimal.txt")
    sv.wait_ready(); results["E1_v2_semantic_minimal"] = run_matrix(sv, cases, "E1 v2+minimal"); sv.stop()
    # E2 v3 多工具 + minimal
    sv = Serve("tools-v3-multitool.json", "system-minimal.txt")
    sv.wait_ready(); results["E2_v3_multitool_minimal"] = run_matrix(sv, cases, "E2 v3+minimal"); sv.stop()
    # E3 v3 多工具 + 无 system 语境
    sv = Serve("tools-v3-multitool.json", "system-none.txt")
    sv.wait_ready(); results["E3_v3_multitool_nosys"] = run_matrix(sv, cases, "E3 v3+nosys"); sv.stop()

    # 赢家 + --forced 对照
    candidates = [results["E1_v2_semantic_minimal"], results["E2_v3_multitool_minimal"],
                  results["E3_v3_multitool_nosys"]]
    best = max(candidates, key=lambda b: b["overall"])
    winner = {"E1 v2+minimal": ("tools-v2-semantic.json", "system-minimal.txt"),
              "E2 v3+minimal": ("tools-v3-multitool.json", "system-minimal.txt"),
              "E3 v3+nosys": ("tools-v3-multitool.json", "system-none.txt")}[best["tag"]]
    log(f"  winner: {best['tag']} ({best['overall']}) — rerun with --forced")
    sv = Serve(winner[0], winner[1], forced=True)
    sv.wait_ready()
    results["E4_winner_forced"] = run_matrix(sv, cases, "E4 winner+forced")
    sv.stop()

    # E5 中文语境一字一句强化 + forced（prompt 路线终审）
    sv = Serve("tools-v4-zhguard.json", "system-minimal.txt", forced=True)
    sv.wait_ready()
    results["E5_v4_zhguard_forced"] = run_matrix(sv, cases, "E5 v4-zhguard+forced")
    sv.stop()
    # E6 v3 多工具 + forced（两类修复姿势叠加检验）
    sv = Serve("tools-v3-multitool.json", "system-minimal.txt", forced=True)
    sv.wait_ready()
    results["E6_v3_multitool_forced"] = run_matrix(sv, cases, "E6 v3+forced")
    sv.stop()

    with open(os.path.join(RESULTS, "phase5-posture.json"), "w", encoding="utf-8") as f:
        json.dump(results, f, ensure_ascii=False, indent=1)
    log(f"\nresults -> {os.path.join(RESULTS, 'phase5-posture.json')}")


if __name__ == "__main__":
    main()
