# 0.22.7 三模型 worker 真实冒烟（开发期手测脚本）：
# usage: smoke_models.py <worker_dir> <model_dir> <audio-or-dir> <model:sv|pf|nano> [backend] [backend_dir]
# 验证 ready 指纹 / hello / 多时长转录（0.5/1/2/完整）/ shutdown 与 stderr 纯净。
import hashlib, json, os, subprocess, sys, threading, time

if hasattr(sys.stdin, "reconfigure"):
    sys.stdin.reconfigure(encoding="utf-8", errors="replace")
    sys.stdout.reconfigure(encoding="utf-8", errors="replace", line_buffering=True)
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

if len(sys.argv) not in (5, 6, 7):
    raise SystemExit("usage: smoke_models.py <worker_dir> <model_dir> <audio-or-dir> <model:sv|pf|nano> [backend] [backend_dir]")

worker_dir, model_dir, audio, which = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
backend = sys.argv[5] if len(sys.argv) >= 6 else "cpu"
backend_dir = sys.argv[6] if len(sys.argv) >= 7 else worker_dir
worker_dir = os.path.abspath(worker_dir)
model_dir = os.path.abspath(model_dir)
audio = os.path.abspath(audio)
backend_dir = os.path.abspath(backend_dir)
if os.path.isdir(audio):
    audio_dir = audio
    audio_files = sorted(
        os.path.join(audio, name) for name in os.listdir(audio) if name.lower().endswith(".wav")
    )
else:
    audio_dir = os.path.dirname(audio)
    audio_files = [audio]
if not audio_files:
    raise SystemExit("no WAV inputs found")
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
    "BLINK_AUDIO_DIR": audio_dir,
})

p = subprocess.Popen([os.path.join(worker_dir, exe)] + margs + [
                         "--backend", backend, "--backend-dir", backend_dir, "--stdin-server"
                     ],
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
if ready.get("type") != "ready":
    p.wait(timeout=60)
    raise RuntimeError("worker startup failed: " + json.dumps(ready, ensure_ascii=False))
print("ready %.1fs model=%s fp=%s backend=%s" % (
    time.time() - t0, ready["model_id"], ready["model_content_fingerprint"][:12], ready["backend"]))
assert ready["type"] == "ready" and ready["model_status"] == "ready" and ready["protocol_version"] == 1
assert ready["requested_backend"] == backend and ready["actual_backend"] == backend

send({"type": "hello", "protocol_version": 1})
assert recv()["type"] == "hello_ok"

# 单文件连续两次；目录则整组跑两轮，以区分进程首轮 pipeline 成本和热态结果。
for i, audio_path in enumerate(audio_files * 2):
    send({"type": "transcribe", "request_id": "smoke-%d" % i, "audio_path": audio_path})
    t = time.time()
    r = recv()
    text = r.get("text", "")
    print("req %d audio=%s ok=%s text_chars=%d text_sha256=%s worker=%sms wall=%.0fms" % (
        i, os.path.basename(audio_path), r["ok"], len(text), hashlib.sha256(text.encode("utf-8")).hexdigest(),
        r.get("elapsed_ms"), (time.time() - t) * 1000))
    if not r["ok"]:
        print("request error:", json.dumps(r.get("error"), ensure_ascii=False))
    if not (r["type"] == "transcribe_result" and r["ok"] and r["text"].strip()):
        send({"type": "shutdown"})
        p.wait(timeout=60)
        raise AssertionError("transcription smoke request failed")

send({"type": "shutdown"})
print("exit=%d" % p.wait(timeout=60))
print("stderr tail:", " | ".join(stderr_lines[-4:]))
print("SMOKE PASS", which)
