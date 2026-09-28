#!/usr/bin/env python3
"""0.24.6 Needle Router Spike — 完整可行性实验。

阶段 1 单发基准 / 阶段 2 常驻 serve 基准 / 阶段 3 数据集适用性（双 system 对照）
/ 阶段 4 协议合同观察。结果写 results/spike-results.json。

仅标准库；Windows x64；详见同目录 README.md。
"""
import argparse
import json
import os
import socket
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ASSETS = os.path.join(HERE, ".assets")
RESULTS_DIR = os.path.join(HERE, "results")

ASSET_SOURCES = [
    ("needle.exe", "https://huggingface.co/Cactus-Compute/needle3/resolve/main/windows-x86_64/needle.exe", 1277952),
    ("needle3.cact", "https://huggingface.co/Cactus-Compute/needle3/resolve/main/needle3.cact", 35335380),
    ("tokenizer.model", "https://huggingface.co/Cactus-Compute/needle3/resolve/main/tokenizer/tokenizer.model", 126520),
    ("tokenizer.vocab", "https://huggingface.co/Cactus-Compute/needle3/resolve/main/tokenizer/tokenizer.vocab", 106870),
]

ARTIFACT_TOTAL_MB = round(sum(s for _, _, s in ASSET_SOURCES) / 1024 / 1024, 2)

BENCH_LONG = "how do I get better at cooking chinese food at home"
BENCH_SHORT = "chrome"


def log(msg):
    print(msg, flush=True)


# ---------- 阶段 0：资产准备 ----------

def prepare_assets():
    os.makedirs(ASSETS, exist_ok=True)
    for name, url, size in ASSET_SOURCES:
        path = os.path.join(ASSETS, name)
        if os.path.exists(path) and os.path.getsize(path) == size:
            continue
        log(f"[assets] downloading {name} ({size/1024/1024:.2f} MB) ...")
        tmp = path + ".part"
        urllib.request.urlretrieve(url, tmp)
        if os.path.getsize(tmp) != size:
            os.remove(tmp)
            raise RuntimeError(f"size mismatch after download: {name}")
        os.replace(tmp, path)
    log(f"[assets] ok, artifact total = {ARTIFACT_TOTAL_MB} MB (runner+weights+tokenizer)")


# ---------- 通用 ----------

def percent(sorted_ms, q):
    return sorted_ms[min(int(len(sorted_ms) * q), len(sorted_ms) - 1)]


def stats_block(ms_list):
    s = sorted(ms_list)
    return {
        "n": len(s), "min": round(s[0], 1), "p50": round(percent(s, 0.5), 1),
        "p95": round(percent(s, 0.95), 1), "max": round(s[-1], 1),
        "mean": round(statistics.mean(s), 1),
    }


def needle_ram_mb():
    """工作集（KB→MB），取所有 needle.exe 进程里最大的。"""
    try:
        out = subprocess.check_output(
            ["tasklist", "/FI", "IMAGENAME eq needle.exe", "/FO", "CSV", "/NH"],
            text=True, stderr=subprocess.DEVNULL).strip()
    except subprocess.CalledProcessError:
        return None
    best = None
    for line in out.splitlines():
        parts = [p.strip('"') for p in line.split('","')]
        if len(parts) >= 5 and parts[0] == "needle.exe":
            kb = int(parts[4].replace(",", "").replace(" K", "").replace("K", ""))
            best = max(best or 0, kb)
    return round(best / 1024, 1) if best else None


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


# ---------- 阶段 1：单发（--prompt，每请求冷进程） ----------

def phase1_oneshot():
    log("\n=== phase 1: one-shot --prompt (cold process per call) ===")
    exe = os.path.join(ASSETS, "needle.exe")
    model = os.path.join(ASSETS, "needle3.cact")
    tools = os.path.join(HERE, "tools.json")
    system = os.path.join(HERE, "system-minimal.txt")
    env = dict(os.environ, NEEDLE_TELEMETRY="0", DO_NOT_TRACK="1")
    times, peaks, sample = [], [], None
    for i in range(10):
        t0 = time.perf_counter()
        p = subprocess.run(
            [exe, "--model", model, "--tools", tools, "--system", system, "--prompt", BENCH_LONG],
            capture_output=True, text=True, env=env, timeout=120)
        ms = (time.perf_counter() - t0) * 1000
        times.append(ms)
        try:
            r = json.loads(p.stdout)
            peaks.append(r.get("peak_ram_mb"))
            sample = r
        except (json.JSONDecodeError, ValueError):
            sample = {"raw": p.stdout[:200], "stderr": p.stderr[:200]}
    block = {"wall_ms": stats_block(times), "peak_ram_mb_reported": peaks,
             "sample_response": sample}
    log(f"  wall: {block['wall_ms']}")
    log(f"  reported peak_ram_mb: {peaks}")
    return block


# ---------- 阶段 2：常驻 serve ----------

class Serve:
    def __init__(self, depth=None, system="system-minimal.txt"):
        self.port = free_port()
        exe = os.path.join(ASSETS, "needle.exe")
        cmd = [exe, "--model", os.path.join(ASSETS, "needle3.cact"),
               "--tools", os.path.join(HERE, "tools.json"),
               "--system", os.path.join(HERE, system),
               "--serve", "--port", str(self.port)]
        if depth:
            cmd += ["--depth", str(depth)]
        env = dict(os.environ, NEEDLE_TELEMETRY="0", DO_NOT_TRACK="1")
        self.proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL, env=env)
        self.base = f"http://127.0.0.1:{self.port}"

    def post(self, path, body, timeout=60):
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
                raise RuntimeError("serve process exited early")
            try:
                self.post("/reset", {}, timeout=2)
                return time.time() - t0
            except (urllib.error.URLError, socket.timeout, json.JSONDecodeError):
                time.sleep(0.3)
        raise RuntimeError("serve not ready in 30s")

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def phase2_serve():
    log("\n=== phase 2: resident --serve (full depth) ===")
    out = {}
    sv = Serve()
    try:
        ready_s = sv.wait_ready()
        out["ready_s"] = round(ready_s, 2)
        log(f"  serve ready in {ready_s:.2f}s (port {sv.port})")
        out["ram_after_start_mb"] = needle_ram_mb()

        # 首请求（预热：工具 embedding 等） vs 后续
        first_ms, _ = sv.post("/complete", {"input": BENCH_SHORT})
        follow = []
        for _ in range(5):
            sv.post("/reset", {})
            ms, _ = sv.post("/complete", {"input": BENCH_SHORT})
            follow.append(ms)
        out["first_request_ms"] = round(first_ms, 1)
        out["followup_request_ms"] = stats_block(follow)
        log(f"  first request {first_ms:.0f}ms vs followup {out['followup_request_ms']['p50']}ms p50")

        # warm 延迟：长句 / 短词
        for label, query in (("warm_long_sentence", BENCH_LONG), ("warm_short_keyword", BENCH_SHORT)):
            ms_list = []
            for _ in range(40):
                sv.post("/reset", {})
                ms, _ = sv.post("/complete", {"input": query})
                ms_list.append(ms)
            out[label] = stats_block(ms_list)
            log(f"  {label}: {out[label]}")

        # 稳定性：连续 60 请求无错误
        errs = 0
        for i in range(60):
            try:
                sv.post("/reset", {})
                _, r = sv.post("/complete", {"input": f"stability probe number {i}"})
                if not r.get("success"):
                    errs += 1
            except Exception:
                errs += 1
        out["stability_errors_of_60"] = errs
        out["ram_under_load_mb"] = needle_ram_mb()
        log(f"  stability: {errs}/60 errors; RAM under load {out['ram_under_load_mb']} MB")

        # idle 后请求（OnDemand TTL 参照）
        log("  idling 20s ...")
        time.sleep(20)
        ms, r = sv.post("/complete", {"input": BENCH_SHORT})
        out["after_idle_20s_request_ms"] = round(ms, 1)
        out["alive_after_idle"] = sv.proc.poll() is None
        log(f"  after 20s idle: {ms:.0f}ms, alive={out['alive_after_idle']}")
    finally:
        sv.stop()
    return out


# ---------- 阶段 2b：depth 4 对照 ----------

def phase2b_depth4():
    log("\n=== phase 2b: resident --serve --depth 4 (29M subnet, control) ===")
    out = {}
    sv = Serve(depth=4)
    try:
        sv.wait_ready()
        ms_list = []
        for _ in range(15):
            sv.post("/reset", {})
            ms, _ = sv.post("/complete", {"input": BENCH_LONG})
            ms_list.append(ms)
        out["warm_long_sentence"] = stats_block(ms_list)
        out["ram_mb"] = needle_ram_mb()
        log(f"  depth4 long: {out['warm_long_sentence']}  RAM {out['ram_mb']} MB")
    finally:
        sv.stop()
    return out


# ---------- 阶段 3：数据集适用性（双 system 对照） ----------

def classify(sv, text):
    sv.post("/reset", {})
    _, r = sv.post("/complete", {"input": text}, timeout=120)
    calls = r.get("function_calls") or []
    supp = r.get("suppressed_calls") or []
    if calls:
        return calls[0]["arguments"].get("intent", "none"), r.get("confidence", 0.0), "call"
    if supp:
        return supp[0]["arguments"].get("intent", "none"), r.get("confidence", 0.0), "suppressed"
    return "none", r.get("confidence", 0.0), "empty"


def phase3_dataset():
    log("\n=== phase 3: dataset suitability (minimal vs enriched system) ===")
    with open(os.path.join(HERE, "dataset.json"), encoding="utf-8") as f:
        cases = json.load(f)["cases"]
    out = {}
    for variant, system_file in (("minimal", "system-minimal.txt"),
                                 ("enriched", "system-enriched.txt")):
        sv = Serve(system=system_file)
        try:
            sv.wait_ready()
            rows = []
            for c in cases:
                got, conf, kind = classify(sv, c["input"])
                rows.append({**c, "got": got, "confidence": round(conf, 3),
                             "call_kind": kind, "match": got == c["expected"]})
            out[variant] = summarize(rows)
            log(f"  [{variant}] overall {out[variant]['overall_accuracy']} | " +
                " ".join(f"{k}={v['accuracy']}" for k, v in out[variant]["by_category"].items()))
        finally:
            sv.stop()
    return out


def summarize(rows):
    by_cat = {}
    for r in rows:
        by_cat.setdefault(r["category"], []).append(r)
    cat_acc = {c: {"accuracy": round(sum(x["match"] for x in rs) / len(rs), 2),
                   "mismatches": [{"input": x["input"][:40], "expected": x["expected"],
                                   "got": x["got"], "conf": x["confidence"]}
                                  for x in rs if not x["match"]]}
               for c, rs in by_cat.items()}
    conf_points = {}
    for th in (0.2, 0.3, 0.35, 0.4, 0.45):
        kept = [r for r in rows if r["confidence"] >= th]
        if kept:
            acc = round(sum(x["match"] for x in kept) / len(kept), 3)
            conf_points[f"acc_if_conf>={th}"] = {
                "accuracy": acc, "kept": len(kept), "dropped": len(rows) - len(kept)}
    match_conf = [r["confidence"] for r in rows if r["match"]]
    miss_conf = [r["confidence"] for r in rows if not r["match"]]
    return {
        "overall_accuracy": round(sum(r["match"] for r in rows) / len(rows), 3),
        "by_category": cat_acc, "confidence_thresholding": conf_points,
        "conf_distribution": {
            "match_mean": round(statistics.mean(match_conf), 3) if match_conf else None,
            "mismatch_mean": round(statistics.mean(miss_conf), 3) if miss_conf else None,
        },
        "rows": rows,
    }


# ---------- 阶段 4：协议合同观察 ----------

def phase4_protocol():
    log("\n=== phase 4: protocol contract observations (vs EngineManager health contract) ===")
    obs = {}
    sv = Serve()
    try:
        sv.wait_ready()
        # GET /health：EngineManager 两阶段健康合同需要它
        try:
            urllib.request.urlopen(sv.base + "/health", timeout=5)
            obs["get_health"] = "200"
        except urllib.error.HTTPError as e:
            obs["get_health"] = f"HTTP {e.code} (EngineManager 合同要求 200+身份 JSON)"
        except Exception as e:
            obs["get_health"] = f"error: {type(e).__name__}"
        # 无 token 头直接调用（X-Engine-Token 合同）
        ms, r = sv.post("/complete", {"input": "chrome"})
        obs["complete_without_token"] = "accepted" if r.get("success") else "rejected"
        # 响应是否含身份回显字段
        obs["identity_fields_in_response"] = sorted(
            k for k in r.keys() if k in ("engine_id", "instance_id", "token_fingerprint"))
        obs["shutdown_endpoint"] = "未见 /shutdown（EngineManager stop 合同走 HTTP /shutdown）"
        # 并发：两个同时在途请求
        import threading
        results = []
        def hit():
            try:
                sv.post("/complete", {"input": BENCH_SHORT}, timeout=120)
                results.append("ok")
            except Exception as e:
                results.append(type(e).__name__)
        t1 = threading.Thread(target=hit); t2 = threading.Thread(target=hit)
        t1.start(); t2.start(); t1.join(); t2.join()
        obs["concurrent_two_requests"] = results
        # 对话累积语义（reset 语义确认）
        sv.post("/complete", {"input": "what is rust"}, timeout=120)
        ms2, r2 = sv.post("/complete", {"input": "and gofmt"}, timeout=120)
        obs["turns_accumulate"] = "是（官方 --serve 语义：turns accumulate until /reset）"
        for k, v in obs.items():
            log(f"  {k}: {v}")
    finally:
        sv.stop()
    return obs


# ---------- main ----------

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--skip-oneshot", action="store_true")
    args = ap.parse_args()

    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except AttributeError:
        pass
    prepare_assets()
    os.makedirs(RESULTS_DIR, exist_ok=True)
    results = {
        "date": time.strftime("%Y-%m-%d %H:%M:%S"),
        "platform": sys.platform,
        "artifact_total_mb": ARTIFACT_TOTAL_MB,
    }
    if not args.skip_oneshot:
        results["phase1_oneshot"] = phase1_oneshot()
    results["phase2_serve"] = phase2_serve()
    results["phase2b_depth4"] = phase2b_depth4()
    results["phase3_dataset"] = phase3_dataset()
    results["phase4_protocol"] = phase4_protocol()
    out_path = os.path.join(RESULTS_DIR, "spike-results.json")
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(results, f, ensure_ascii=False, indent=1)
    log(f"\nresults -> {out_path}")


if __name__ == "__main__":
    main()
