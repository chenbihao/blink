# 0.22.7.2 worker 协议冒烟（开发期手测脚本）：
# 用真实 SenseVoice Q8 模型驱动 funasr-sensevoice-worker.exe 的 --stdin-server，
# 验证 ready 指纹 / hello / transcribe / shutdown 全链路与 stdout 纯净性。
import json, os, subprocess, sys, time

worker = sys.argv[1]
model = sys.argv[2]
audio = sys.argv[3]
audio_dir = os.path.dirname(os.path.abspath(audio))
payload_dir = os.path.dirname(os.path.abspath(model))

env = dict(os.environ)
env.update({
    "BLINK_ENGINE_ID": "funasr",
    "BLINK_INSTANCE_ID": "inst-smoke",
    "BLINK_ENGINE_TOKEN": "smoke-token-0123456789abcdef",
    "BLINK_MODEL_ID": "gguf/sensevoice-small-q8",
    "BLINK_MODEL_REVISION": "v0.2.6",
    "BLINK_MODEL_PAYLOAD_DIR": payload_dir,
    "BLINK_AUDIO_DIR": audio_dir,
})

p = subprocess.Popen(
    [worker, "-m", model, "--stdin-server"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    env=env, cwd=os.path.dirname(worker),
)

def send(o):
    p.stdin.write((json.dumps(o) + "\n").encode("utf-8"))
    p.stdin.flush()

def recv():
    line = p.stdout.readline().decode("utf-8")
    if not line:
        raise RuntimeError("stdout EOF: " + p.stderr.read(4000).decode("utf-8", "replace"))
    return json.loads(line)

t0 = time.time()
ready = recv()
print("ready in %.1fs: model=%s fp=%s backend=%s token_fp=%s" % (
    time.time() - t0, ready["model_id"], ready["model_content_fingerprint"][:16],
    ready["backend"], ready["token_fingerprint"]))
assert ready["type"] == "ready" and ready["model_status"] == "ready"
assert ready["protocol_version"] == 1
assert ready["engine_id"] == "funasr" and ready["instance_id"] == "inst-smoke"

send({"type": "hello", "protocol_version": 1})
hello = recv()
assert hello["type"] == "hello_ok", hello
print("hello ok")

for i in range(3):
    send({"type": "transcribe", "request_id": "smoke-%d" % i, "audio_path": audio})
    t = time.time()
    r = recv()
    dt = (time.time() - t) * 1000
    assert r["type"] == "transcribe_result" and r["request_id"] == "smoke-%d" % i
    print("req %d ok=%s text=%r worker_elapsed=%sms wall=%.0fms" % (
        i, r["ok"], r.get("text", "")[:60], r.get("elapsed_ms"), dt))
    assert r["ok"] and r["text"].strip()

# 错误路径：越界音频路径
send({"type": "transcribe", "request_id": "bad-1", "audio_path": "C:/Windows/win.ini"})
r = recv()
assert r["type"] == "error" and r["error"]["code"] == "audio_path_rejected", r
print("path rejection ok:", r["error"]["code"])

# 错误路径：畸形 JSON / 未知 type / 版本不匹配
p.stdin.write(b"this is not json\n"); p.stdin.flush()
r = recv(); assert r["type"] == "error" and r["error"]["code"] == "bad_json", r
send({"type": "bogus", "request_id": "x"})
r = recv(); assert r["type"] == "error" and r["error"]["code"] == "unknown_type", r
send({"type": "hello", "protocol_version": 99})
r = recv(); assert r["type"] == "error" and r["error"]["code"] == "unsupported_protocol_version", r
print("error paths ok")

send({"type": "shutdown"})
rc = p.wait(timeout=15)
err_tail = p.stderr.read().decode("utf-8", "replace")[-500:]
print("exit=%d" % rc)
print("--- stderr tail ---")
print(err_tail)
assert rc == 0
print("SMOKE PASS")
