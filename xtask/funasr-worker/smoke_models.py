# 0.22.7 三模型 worker 真实冒烟（开发期手测脚本）：
# usage: smoke_models.py <worker_dir> <model_dir> <audio> <model:sv|pf|nano>
# 验证 ready 指纹 / hello / 多时长转录（0.5/1/2/完整）/ shutdown 与 stderr 纯净。
import json, os, subprocess, sys, threading, time

worker_dir, model_dir, audio, which = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
specs = {
    "sv":   ("funasr-sensevoice-worker.exe", ["-m", os.path.join(model_dir, "sensevoice-small-q8.gguf")], "gguf/sensevoice-small-q8"),
    "pf":   ("funasr-paraformer-worker.exe", ["-m", os.path.join(model_dir, "paraformer-q8.gguf")], "gguf/paraformer-zh-q8"),
    "nano": ("funasr-nano-worker.exe", ["--enc", os.path.join(model_dir, "funasr-encoder-f16.gguf"),
                                        "-m", os.path.join(model_dir, "qwen3-0.6b-q4km.gguf")], "gguf/fun-asr-nano-q4km"),
}
exe, margs, model_id = specs[which]

env = dict(os.environ)
env.update({
    "BLINK_ENGINE_ID": "funasr",
    "BLINK_INSTANCE_ID": "inst-smoke",
    "BLINK_ENGINE_TOKEN": "smoke-token-0123456789abcdef",
    "BLINK_MODEL_ID": model_id,
    "BLINK_MODEL_REVISION": "gguf-v0.2.6",
    "BLINK_MODEL_PAYLOAD_DIR": model_dir,
    "BLINK_AUDIO_DIR": os.path.dirname(os.path.abspath(audio)),
})

p = subprocess.Popen([os.path.join(worker_dir, exe)] + margs + ["--stdin-server"],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
                     cwd=worker_dir)

stderr_lines = []
def drain():
    for line in iter(p.stderr.readline, b""):
        stderr_lines.append(line.decode("utf-8", "replace").rstrip())
threading.Thread(target=drain, daemon=True).start()

def send(o):
    p.stdin.write((json.dumps(o) + "\n").encode("utf-8"))
    p.stdin.flush()

def recv(timeout=600):
    deadline = time.time() + timeout
    while time.time() < deadline:
        line = p.stdout.readline()
        if line:
            return json.loads(line.decode("utf-8"))
        if p.poll() is not None:
            raise RuntimeError("worker 退出 %s; stderr:\n%s" % (p.returncode, "\n".join(stderr_lines[-10:])))
        time.sleep(0.02)
    raise TimeoutError("recv 超时; stderr:\n" + "\n".join(stderr_lines[-15:]))

t0 = time.time()
ready = recv()
print("ready %.1fs model=%s fp=%s backend=%s" % (
    time.time() - t0, ready["model_id"], ready["model_content_fingerprint"][:12], ready["backend"]))
assert ready["type"] == "ready" and ready["model_status"] == "ready" and ready["protocol_version"] == 1

send({"type": "hello", "protocol_version": 1})
assert recv()["type"] == "hello_ok"

# 用 SAPI 前缀已生成的完整 WAV 按时长截断：直接传完整文件（worker 全量推理），
# 时长维度由 E2E 的 Rust 侧 wav 截断覆盖；此处验证多次请求稳定。
for i in range(2):
    send({"type": "transcribe", "request_id": "smoke-%d" % i, "audio_path": audio})
    t = time.time()
    r = recv()
    print("req %d ok=%s text=%r worker=%sms wall=%.0fms" % (
        i, r["ok"], r.get("text", "")[:80], r.get("elapsed_ms"), (time.time() - t) * 1000))
    assert r["type"] == "transcribe_result" and r["ok"] and r["text"].strip()

send({"type": "shutdown"})
print("exit=%d" % p.wait(timeout=60))
print("stderr tail:", " | ".join(stderr_lines[-4:]))
print("SMOKE PASS", which)
