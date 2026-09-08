"""0.22.16.6 FunASR GPU adoption gate.

This runner deliberately treats missing runtime/model/audio/hardware as blocked.
It never changes a requested backend to CPU and never turns a failed explicit
backend probe into a pass.  It is intentionally stdlib-only so it can run on a
clean Windows validation host.

Exit codes:
  0 = every requested combination passed the adoption gate
  1 = a requested combination failed a gate
  2 = validation was blocked or untested because an input was unavailable
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid
import wave
from pathlib import Path
from statistics import median
from typing import Any


if hasattr(sys.stdin, "reconfigure"):
    sys.stdin.reconfigure(encoding="utf-8", errors="replace")
    sys.stdout.reconfigure(encoding="utf-8", errors="replace", line_buffering=True)
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")


ROOT = Path(__file__).resolve().parents[2]
DEFAULT_WORKER_DIR = ROOT / "resources" / "bin" / "funasr-worker"
DEFAULT_THRESHOLD_FILE = Path(__file__).with_name("adoption_thresholds.json")
DEFAULT_EVIDENCE_DIR = ROOT / "xtask" / "spikes" / "funasr-gpu" / "runs"
MODELS: dict[str, dict[str, Any]] = {
    "sensevoice": {
        "exe": "funasr-sensevoice-worker.exe",
        "args": lambda directory: ["-m", str(directory / "sensevoice-small-q8.gguf")],
        "model_id": "gguf/sensevoice-small-q8",
    },
    "paraformer": {
        "exe": "funasr-paraformer-worker.exe",
        "args": lambda directory: ["-m", str(directory / "paraformer-q8.gguf")],
        "model_id": "gguf/paraformer-zh-q8",
    },
    "nano_encoder": {
        "exe": "funasr-nano-worker.exe",
        "args": lambda directory: [
            "--enc",
            str(directory / "funasr-encoder-f16.gguf"),
            "-m",
            str(directory / "qwen3-0.6b-q4km.gguf"),
        ],
        "model_id": "gguf/fun-asr-nano-q4km",
    },
}
BACKENDS = ("cpu", "vulkan", "cuda")
BLOCKED = "blocked"
PASS = "pass"
FAIL = "fail"


def utc_now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def json_load(path: Path) -> Any:
    with path.open("r", encoding="utf-8") as handle:
        return json.load(handle)


def run_capture(command: list[str], cwd: Path | None = None, timeout: int = 30) -> dict[str, Any]:
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
            check=False,
        )
        return {
            "command": command,
            "returncode": completed.returncode,
            "stdout": completed.stdout.strip(),
            "stderr": completed.stderr.strip(),
        }
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"command": command, "error": str(error)}


def percentile(values: list[float], percent: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    position = (len(ordered) - 1) * percent / 100
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)


def normalized_text(value: str) -> str:
    return re.sub(r"\s+", " ", value.strip()).casefold()


def edit_distance(left: str, right: str) -> int:
    previous = list(range(len(right) + 1))
    for left_index, left_char in enumerate(left, 1):
        current = [left_index]
        for right_index, right_char in enumerate(right, 1):
            current.append(
                min(
                    current[-1] + 1,
                    previous[right_index] + 1,
                    previous[right_index - 1] + (left_char != right_char),
                )
            )
        previous = current
    return previous[-1]


def text_distance_ratio(left: str, right: str) -> float:
    left = normalized_text(left)
    right = normalized_text(right)
    denominator = max(len(left), len(right), 1)
    return edit_distance(left, right) / denominator


def redact_text(value: str) -> dict[str, Any]:
    normalized = normalized_text(value)
    return {"chars": len(normalized), "sha256": hashlib.sha256(normalized.encode()).hexdigest()}


def process_count(patterns: tuple[str, ...]) -> int | None:
    """Return a best-effort count without treating query failure as zero."""
    if os.name != "nt":
        return None
    result = run_capture(["powershell", "-NoProfile", "-Command", "Get-Process | Select-Object -ExpandProperty Path"])
    if "stdout" not in result:
        return None
    return sum(
        1
        for line in result["stdout"].splitlines()
        if any(line.casefold().endswith(pattern.casefold()) for pattern in patterns)
    )


def inventory() -> dict[str, Any]:
    result: dict[str, Any] = {
        "captured_at": utc_now(),
        "platform": {
            "system": platform.system(),
            "release": platform.release(),
            "version": platform.version(),
            "machine": platform.machine(),
            "processor": platform.processor(),
            "python": platform.python_version(),
        },
        "tools": {},
        "gpu": {},
    }
    if shutil.which("nvidia-smi"):
        result["gpu"]["nvidia_smi"] = run_capture(
            [
                "nvidia-smi",
                "--query-gpu=name,driver_version,memory.total,compute_cap",
                "--format=csv,noheader,nounits",
            ]
        )
    else:
        result["gpu"]["nvidia_smi"] = "not-installed"
    vulkaninfo = shutil.which("vulkaninfo")
    result["gpu"]["vulkan"] = (
        run_capture([vulkaninfo, "--summary"], timeout=30) if vulkaninfo else "vulkaninfo-not-installed"
    )
    for name in ("cargo", "node", "python", "nvidia-smi", "vulkaninfo"):
        result["tools"][name] = shutil.which(name)
    try:
        import psutil  # type: ignore

        result["host_memory_bytes"] = psutil.virtual_memory().total
        result["cpu_count"] = psutil.cpu_count(logical=True)
    except ImportError:
        result["host_memory_bytes"] = None
        result["cpu_count"] = os.cpu_count()
    return result


def validate_manifest(directory: Path) -> dict[str, Any]:
    manifest_path = directory / "manifest.json"
    if not manifest_path.is_file():
        return {"status": BLOCKED, "reason": f"missing {manifest_path}"}
    try:
        manifest = json_load(manifest_path)
        entries = manifest["files"]
        declared: set[str] = set()
        failures: list[str] = []
        for entry in entries:
            relative = entry["path"]
            path = Path(relative)
            if path.is_absolute() or ".." in path.parts:
                failures.append(f"unsafe path: {relative}")
                continue
            if relative in declared:
                failures.append(f"duplicate path: {relative}")
            declared.add(relative)
            actual = directory / path
            if not actual.is_file():
                failures.append(f"missing file: {relative}")
                continue
            if actual.stat().st_size != entry["size_bytes"]:
                failures.append(f"size mismatch: {relative}")
            if sha256_file(actual) != entry["sha256"]:
                failures.append(f"sha256 mismatch: {relative}")
        actual_files = {
            str(path.relative_to(directory)).replace("\\", "/")
            for path in directory.rglob("*")
            if path.is_file() and path.name != "manifest.json"
        }
        if actual_files != declared:
            failures.append("manifest file closure mismatch")
        return {
            "status": PASS if not failures else FAIL,
            "artifact_id": manifest.get("artifact", {}).get("id", manifest.get("artifact_id")),
            "artifact_version": manifest.get("artifact", {}).get("version", manifest.get("artifact_version")),
            "files": len(declared),
            "failures": failures,
        }
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        return {"status": FAIL, "reason": f"invalid manifest: {error}"}


def validate_runtime_lock() -> dict[str, Any]:
    path = ROOT / "resources" / "stt" / "funasr-gguf" / "runtime-lock.json"
    if not path.is_file():
        return {"status": FAIL, "reason": f"missing {path}"}
    lock = json_load(path)
    failures: list[str] = []
    for artifact_id, entry in lock.get("artifacts", {}).items():
        url = entry.get("url", "")
        digest = entry.get("sha256")
        if "/download/" not in url or "latest" in url:
            failures.append(f"{artifact_id}: URL is not immutable release download")
        if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-fA-F]{64}", digest):
            failures.append(f"{artifact_id}: SHA-256 is missing or invalid")
    return {"status": PASS if not failures else BLOCKED, "failures": failures, "path": str(path)}


def validate_gpu_artifact_hash_sources() -> dict[str, Any]:
    path = ROOT / "src" / "app" / "local_engine" / "funasr" / "descriptor.rs"
    if not path.is_file():
        return {"status": BLOCKED, "reason": f"missing {path}"}
    source = path.read_text(encoding="utf-8")
    missing = []
    for label in ("vulkan", "cuda"):
        match = re.search(
            rf"artifact_id:\s*{label}_artifact,.*?archive_sha256:\s*String::new\(\)",
            source,
            re.DOTALL,
        )
        if match:
            missing.append(label)
    return {
        "status": BLOCKED if missing else PASS,
        "missing_immutable_sha256": missing,
        "path": str(path),
    }


class WorkerSession:
    def __init__(self, command: list[str], env: dict[str, str], cwd: Path, timeout: int):
        self.command = command
        self.env = env
        self.cwd = cwd
        self.timeout = timeout
        self.process = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.messages: queue.Queue[tuple[str, Any]] = queue.Queue()
        self.stderr_lines: list[str] = []
        self._stdout_thread = threading.Thread(target=self._read_stdout, daemon=True)
        self._stderr_thread = threading.Thread(target=self._read_stderr, daemon=True)
        self._stdout_thread.start()
        self._stderr_thread.start()

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        for raw in self.process.stdout:
            line = raw.decode("utf-8", "replace").rstrip("\r\n")
            if not line.strip():
                self.messages.put(("stdout_violation", line))
                continue
            try:
                self.messages.put(("json", json.loads(line)))
            except json.JSONDecodeError:
                self.messages.put(("stdout_violation", line[:200]))
        self.messages.put(("eof", None))

    def _read_stderr(self) -> None:
        assert self.process.stderr is not None
        for raw in self.process.stderr:
            self.stderr_lines.append(raw.decode("utf-8", "replace").rstrip("\r\n"))

    def send_raw(self, value: str) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(value.encode("utf-8") + b"\n")
        self.process.stdin.flush()

    def send(self, value: dict[str, Any]) -> None:
        self.send_raw(json.dumps(value, ensure_ascii=False, separators=(",", ":")))

    def recv(self, timeout: float | None = None) -> tuple[str, Any]:
        return self.messages.get(timeout=timeout or self.timeout)

    def stop(self, graceful: bool = True) -> int | None:
        if self.process.poll() is None and graceful:
            try:
                self.send({"type": "shutdown"})
            except (BrokenPipeError, OSError):
                pass
        try:
            return self.process.wait(timeout=self.timeout)
        except subprocess.TimeoutExpired:
            self.process.kill()
            return self.process.wait(timeout=10)


def make_env(model_id: str, model_dir: Path, audio: Path) -> dict[str, str]:
    environment = dict(os.environ)
    environment.update(
        {
            "BLINK_ENGINE_ID": "funasr",
            "BLINK_INSTANCE_ID": f"adoption-{uuid.uuid4().hex[:12]}",
            "BLINK_ENGINE_TOKEN": "adoption-gate-token",
            "BLINK_MODEL_ID": model_id,
            "BLINK_MODEL_REVISION": "adoption-gate-controlled-batch",
            "BLINK_MODEL_PAYLOAD_DIR": str(model_dir.resolve()),
            "BLINK_AUDIO_DIR": str(audio.parent.resolve()),
        }
    )
    return environment


def recv_json(session: WorkerSession, expected_type: str) -> dict[str, Any]:
    kind, value = session.recv()
    if kind != "json":
        raise RuntimeError(f"expected {expected_type}, got {kind}: {value}")
    if value.get("type") != expected_type:
        raise RuntimeError(f"expected {expected_type}, got {value.get('type')}: {value}")
    return value


def probe(worker: Path, backend: str, backend_dir: Path, timeout: int) -> dict[str, Any]:
    completed = run_capture(
        [str(worker), "--backend", backend, "--backend-dir", str(backend_dir), "--blink-backend-probe"],
        cwd=worker.parent,
        timeout=timeout,
    )
    stdout = completed.get("stdout", "")
    lines = [line for line in stdout.splitlines() if line.strip()]
    output: dict[str, Any] = {
        "backend": backend,
        "returncode": completed.get("returncode"),
        "stderr_tail": completed.get("stderr", "")[-1000:],
        "stdout_line_count": len(lines),
    }
    if len(lines) == 1:
        try:
            value = json.loads(lines[0])
            output["response"] = value
            output["requested_actual_match"] = (
                value.get("requested_backend") == backend and value.get("actual_backend") == backend
            )
            output["status"] = PASS if completed.get("returncode") == 0 and value.get("ok") and output["requested_actual_match"] else FAIL
            return output
        except json.JSONDecodeError:
            pass
    output["status"] = FAIL if completed.get("returncode") not in (None, 0) else BLOCKED
    output["reason"] = "probe did not produce one JSON response line"
    return output


def sample_process(pid: int) -> dict[str, float | int | None]:
    try:
        import psutil  # type: ignore

        process = psutil.Process(pid)
        memory = process.memory_info()
        cpu = process.cpu_percent(interval=None)
        vram_mb = None
        if shutil.which("nvidia-smi"):
            telemetry = run_capture(
                [
                    "nvidia-smi",
                    "--query-compute-apps=pid,used_memory",
                    "--format=csv,noheader,nounits",
                ],
                timeout=10,
            )
            for line in telemetry.get("stdout", "").splitlines():
                fields = [field.strip() for field in line.split(",")]
                if len(fields) >= 2 and fields[0] == str(pid):
                    try:
                        vram_mb = float(fields[1])
                    except ValueError:
                        pass
        return {"cpu_percent": cpu, "rss_bytes": memory.rss, "vms_bytes": memory.vms, "vram_mb": vram_mb}
    except (ImportError, OSError):
        return {"cpu_percent": None, "rss_bytes": None, "vms_bytes": None}


def run_session(
    worker: Path,
    worker_dir: Path,
    backend_dir: Path,
    model_key: str,
    model_dir: Path,
    audio: Path,
    backend: str,
    requests: int,
    timeout: int,
) -> dict[str, Any]:
    spec = MODELS[model_key]
    # The upstream Windows audio loader accepts narrow paths. Corpus fixtures
    # may have localized names, while production already uses UUID WAV names;
    # stage an ASCII filename so the adoption runner exercises inference rather
    # than failing before decoding.
    audio_staging = tempfile.TemporaryDirectory(prefix="blink-adoption-audio-")
    staged_audio = Path(audio_staging.name) / "sample.wav"
    shutil.copy2(audio, staged_audio)
    audio = staged_audio
    command = [
        str(worker),
        "--backend",
        backend,
        "--backend-dir",
        str(backend_dir),
        *spec["args"](model_dir),
        "--stdin-server",
    ]
    session = WorkerSession(command, make_env(spec["model_id"], model_dir, audio), worker_dir, timeout)
    resource_samples: list[dict[str, Any]] = []
    stdout_violations = 0
    unexpected_messages: list[str] = []
    results: list[dict[str, Any]] = []
    output_texts: list[str] = []
    wall_ms: list[float] = []
    started = time.perf_counter()
    try:
        ready = recv_json(session, "ready")
        session.send({"type": "hello", "protocol_version": 1})
        hello = recv_json(session, "hello_ok")
        session.send({"type": "health", "request_id": "adoption-health"})
        health = recv_json(session, "health")
        requested_actual = all(
            item.get("requested_backend") == backend and item.get("actual_backend") == backend
            for item in (ready, hello, health)
        )
        for index in range(requests):
            request_id = f"adoption-{index:04d}"
            t0 = time.perf_counter()
            session.send({"type": "transcribe", "request_id": request_id, "audio_path": str(audio)})
            while True:
                kind, value = session.recv()
                if kind == "stdout_violation":
                    stdout_violations += 1
                    continue
                if kind != "json":
                    raise RuntimeError(f"request {request_id} ended with {kind}")
                if value.get("type") == "transcribe_result" and value.get("request_id") == request_id:
                    break
                unexpected_messages.append(str(value.get("type")))
            wall_ms.append((time.perf_counter() - t0) * 1000)
            results.append(
                {
                    "request_id": request_id,
                    "ok": bool(value.get("ok")),
                    "text": redact_text(value.get("text", "")),
                    "worker_elapsed_ms": value.get("elapsed_ms"),
                }
            )
            output_texts.append(value.get("text", ""))
            resource_samples.append(sample_process(session.process.pid))
        session.send({"type": "transcribe", "request_id": "concurrent-a", "audio_path": str(audio)})
        session.send({"type": "transcribe", "request_id": "concurrent-b", "audio_path": str(audio)})
        concurrent_ids = []
        for _ in range(2):
            kind, value = session.recv()
            if kind == "json" and value.get("type") == "transcribe_result":
                concurrent_ids.append(value.get("request_id"))
            else:
                unexpected_messages.append(f"concurrent:{kind}")
        session.send({"type": "transcribe", "request_id": "adoption-bad-audio", "audio_path": "C:/Windows/win.ini"})
        bad_audio = recv_json(session, "error")
        session.send_raw("this is not json")
        bad_json = recv_json(session, "error")
        session.send({"type": "bogus", "request_id": "adoption-unknown"})
        unknown_type = recv_json(session, "error")
        session.send({"type": "hello", "protocol_version": 99})
        bad_version = recv_json(session, "error")
        status = PASS if (
            requested_actual
            and requests >= 1
            and len(results) == requests
            and all(item["ok"] for item in results)
            and not stdout_violations
            and not unexpected_messages
            and concurrent_ids == ["concurrent-a", "concurrent-b"]
            and bad_audio.get("error", {}).get("code") == "audio_path_rejected"
            and bad_json.get("error", {}).get("code") == "bad_json"
            and unknown_type.get("error", {}).get("code") == "unknown_type"
            and bad_version.get("error", {}).get("code") == "unsupported_protocol_version"
        ) else FAIL
        return {
            "status": status,
            "backend": backend,
            "requested_actual": requested_actual,
            "ready": {
                "model_id": ready.get("model_id"),
                "device_name": ready.get("device_name"),
                "device_id": ready.get("device_id"),
                "buffer_type": ready.get("buffer_type"),
                "requested_backend": ready.get("requested_backend"),
                "actual_backend": ready.get("actual_backend"),
            },
            "health": {
                "requested_backend": health.get("requested_backend"),
                "actual_backend": health.get("actual_backend"),
            },
            "request_count": requests,
            "successful_results": sum(1 for item in results if item["ok"]),
            "latency_ms": {
                "p50": percentile(wall_ms, 50),
                "p95": percentile(wall_ms, 95),
                "min": min(wall_ms) if wall_ms else None,
                "max": max(wall_ms) if wall_ms else None,
            },
            "resource": resource_summary(resource_samples),
            "concurrent": {
                "response_ids": concurrent_ids,
                "serial_semantics": concurrent_ids == ["concurrent-a", "concurrent-b"],
            },
            "protocol_error_paths": {
                "bad_audio": bad_audio.get("error", {}).get("code"),
                "bad_json": bad_json.get("error", {}).get("code"),
                "unknown_type": unknown_type.get("error", {}).get("code"),
                "unsupported_protocol_version": bad_version.get("error", {}).get("code"),
            },
            "stdout_violations": stdout_violations,
            "unexpected_messages": unexpected_messages,
            "redacted_results": results,
            "output_texts": output_texts,
            "duration_ms": (time.perf_counter() - started) * 1000,
        }
    except (RuntimeError, queue.Empty, BrokenPipeError, OSError, subprocess.SubprocessError) as error:
        return {
            "status": FAIL,
            "backend": backend,
            "error": str(error),
            "stderr_tail": "\n".join(session.stderr_lines[-20:]),
            "stdout_violations": stdout_violations,
        }
    finally:
        session.stop(graceful=True)
        audio_staging.cleanup()


def resource_summary(samples: list[dict[str, Any]]) -> dict[str, Any]:
    if not samples:
        return {"samples": 0, "cpu_percent_p95": None, "rss_bytes_p95": None, "vram_mb_p95": None}
    cpu = [float(item["cpu_percent"]) for item in samples if item.get("cpu_percent") is not None]
    rss = [float(item["rss_bytes"]) for item in samples if item.get("rss_bytes") is not None]
    vram = [float(item["vram_mb"]) for item in samples if item.get("vram_mb") is not None]
    return {
        "samples": len(samples),
        "cpu_percent_p95": percentile(cpu, 95),
        "rss_bytes_p95": percentile(rss, 95),
        "vram_mb_p95": percentile(vram, 95),
        "vram_note": "VRAM is sampled through nvidia-smi when the worker appears as a compute process; Vulkan/AMD/Intel may remain unavailable.",
    }


def crash_session(
    worker: Path,
    worker_dir: Path,
    backend_dir: Path,
    model_key: str,
    model_dir: Path,
    audio: Path,
    backend: str,
    timeout: int,
) -> dict[str, Any]:
    spec = MODELS[model_key]
    command = [
        str(worker),
        "--backend",
        backend,
        "--backend-dir",
        str(backend_dir),
        *spec["args"](model_dir),
        "--stdin-server",
    ]
    session = WorkerSession(command, make_env(spec["model_id"], model_dir, audio), worker_dir, timeout)
    try:
        kind, value = session.recv()
        if kind != "json" or value.get("type") != "ready":
            return {"status": FAIL, "reason": f"crash test did not reach ready: {kind}"}
        pid = session.process.pid
        session.process.kill()
        exit_code = session.process.wait(timeout=10)
        return {"status": PASS if exit_code != 0 else FAIL, "pid": pid, "exit_code": exit_code}
    except (OSError, queue.Empty, subprocess.SubprocessError) as error:
        return {"status": FAIL, "reason": str(error)}
    finally:
        session.stop(graceful=False)


def compare_outputs(cpu: dict[str, Any], gpu: dict[str, Any], threshold: float) -> dict[str, Any]:
    # Raw texts stay memory-only for the comparison and are removed before the
    # JSON evidence is written; persisted evidence contains only redacted digests.
    if "output_texts" not in cpu or "output_texts" not in gpu:
        return {"status": BLOCKED, "reason": "privacy-preserving run omitted comparable text sidecar"}
    distances = [
        text_distance_ratio(left, right)
        for left, right in zip(cpu["output_texts"], gpu["output_texts"], strict=True)
    ]
    return {
        "status": PASS if distances and max(distances) <= threshold else FAIL,
        "max_text_distance_ratio": max(distances) if distances else None,
        "p95_text_distance_ratio": percentile(distances, 95),
    }


def model_files_present(model_key: str, model_dir: Path) -> list[str]:
    spec = MODELS[model_key]
    args = spec["args"](model_dir)
    return [
        args[index + 1]
        for index, value in enumerate(args)
        if value in ("-m", "--enc")
        and index + 1 < len(args)
        and not Path(args[index + 1]).is_file()
    ]


def run_self_test() -> int:
    assert text_distance_ratio("Hello  world", "hello world") == 0
    assert text_distance_ratio("abcd", "abce") == 0.25
    assert percentile([1, 2, 3, 4], 50) == 2.5
    with tempfile.TemporaryDirectory(prefix="blink-adoption-test-") as directory:
        root = Path(directory)
        (root / "manifest.json").write_text(
            json.dumps({"schema": 2, "artifact": {"id": "test"}, "files": [{"path": "worker.exe", "size_bytes": 1, "sha256": hashlib.sha256(b"x").hexdigest()}]}),
            encoding="utf-8",
        )
        (root / "worker.exe").write_bytes(b"x")
        assert validate_manifest(root)["status"] == PASS
        (root / "extra.dll").write_bytes(b"x")
        assert validate_manifest(root)["status"] == FAIL
    print("adoption_gate self-test: pass")
    return 0


def preflight(args: argparse.Namespace, thresholds: dict[str, Any]) -> dict[str, Any]:
    worker_dir = args.worker_dir.resolve()
    backend_dir = (args.backend_dir or args.worker_dir).resolve()
    models = list(MODELS) if args.model == "all" else [args.model]
    result: dict[str, Any] = {
        "status": PASS,
        "worker_dir": str(worker_dir),
        "backend_dir": str(backend_dir),
        "manifest": validate_manifest(worker_dir),
        "runtime_lock": validate_runtime_lock(),
        "gpu_artifact_hash_sources": validate_gpu_artifact_hash_sources(),
        "thresholds": thresholds,
    }
    blockers: list[str] = []
    execution_blockers: list[str] = []
    if result["manifest"]["status"] != PASS:
        reason = "runtime manifest is not available and verified"
        blockers.append(reason)
        execution_blockers.append(reason)
    if result["runtime_lock"]["status"] != PASS:
        blockers.append("runtime-lock.json does not contain all immutable SHA-256 values")
    if result["gpu_artifact_hash_sources"]["status"] != PASS:
        blockers.append("GPU artifact plans do not contain immutable archive SHA-256 values")
    if not backend_dir.is_dir():
        reason = f"missing backend directory: {backend_dir}"
        blockers.append(reason)
        execution_blockers.append(reason)
    for model_key in models:
        worker = worker_dir / MODELS[model_key]["exe"]
        if not worker.is_file():
            reason = f"missing worker: {worker}"
            blockers.append(reason)
            execution_blockers.append(reason)
    if args.model_dir is None:
        reason = "--model-dir was not provided"
        blockers.append(reason)
        execution_blockers.append(reason)
    elif not args.model_dir.is_dir():
        reason = f"missing model directory: {args.model_dir}"
        blockers.append(reason)
        execution_blockers.append(reason)
    if args.audio is None:
        reason = "--audio was not provided"
        blockers.append(reason)
        execution_blockers.append(reason)
    elif not args.audio.is_file():
        reason = f"missing controlled audio: {args.audio}"
        blockers.append(reason)
        execution_blockers.append(reason)
    minimum_requests = thresholds["stability"]["minimum_sequential_requests"]
    if args.requests < minimum_requests:
        reason = f"--requests must be at least the frozen lifecycle minimum ({minimum_requests})"
        blockers.append(reason)
        execution_blockers.append(reason)
    result["blockers"] = blockers
    result["execution_blockers"] = execution_blockers
    result["evidence_collection_ready"] = not execution_blockers
    result["status"] = BLOCKED if blockers else PASS
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker-dir", type=Path, default=DEFAULT_WORKER_DIR)
    parser.add_argument("--backend-dir", type=Path, help="directory containing optional GPU backend DLLs")
    parser.add_argument("--model-dir", type=Path)
    parser.add_argument("--audio", type=Path)
    parser.add_argument("--model", choices=["all", *MODELS], default="all")
    parser.add_argument("--backend", choices=["all", *BACKENDS], default="all")
    parser.add_argument("--requests", type=int, default=100)
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--thresholds", type=Path, default=DEFAULT_THRESHOLD_FILE)
    parser.add_argument("--out", type=Path)
    parser.add_argument(
        "--collect-blocked-evidence",
        action="store_true",
        help="run the local matrix when only release-publication preflight checks are blocked; the report and exit status remain blocked",
    )
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return run_self_test()
    # Workers run with the artifact directory as their cwd. Resolve user inputs
    # before spawning so model/audio paths keep pointing at the repository files.
    args.worker_dir = args.worker_dir.resolve()
    if args.backend_dir is not None:
        args.backend_dir = args.backend_dir.resolve()
    if args.model_dir is not None:
        args.model_dir = args.model_dir.resolve()
    if args.audio is not None:
        args.audio = args.audio.resolve()
    thresholds = json_load(args.thresholds)
    report: dict[str, Any] = {
        "schema": 1,
        "runner": "xtask/funasr-worker/adoption_gate.py",
        "started_at": utc_now(),
        "inventory": inventory(),
        "preflight": preflight(args, thresholds),
        "matrix": [],
        "release_consumer": {
            "runtime_lock": validate_runtime_lock(),
            "gpu_artifact_hash_sources": validate_gpu_artifact_hash_sources(),
            "application_release_static_guard": PASS,
        },
    }
    release_workflow = (ROOT / ".github" / "workflows" / "release.yml").read_text(encoding="utf-8")
    if re.search(r"cargo\s+(?:@args\s+)?funasr-worker", release_workflow, re.IGNORECASE):
        report["release_consumer"]["application_release_static_guard"] = FAIL
        report["release_consumer"]["reason"] = "application release workflow references worker build"
    if report["preflight"]["status"] == PASS or (
        args.collect_blocked_evidence and report["preflight"]["evidence_collection_ready"]
    ):
        backend_dir = (args.backend_dir or args.worker_dir).resolve()
        models = list(MODELS) if args.model == "all" else [args.model]
        backends = list(BACKENDS) if args.backend == "all" else [args.backend]
        for model_key in models:
            missing = model_files_present(model_key, args.model_dir)
            worker = args.worker_dir / MODELS[model_key]["exe"]
            baseline: dict[str, Any] | None = None
            if missing:
                for backend in backends:
                    report["matrix"].append(
                        {"model": model_key, "backend": backend, "status": BLOCKED, "reason": f"missing model files: {missing}"}
                    )
                continue

            # CPU is always run as the comparison baseline, even when the
            # caller requested only a GPU backend. The same audio batch and
            # request count therefore define correctness and performance.
            cpu_probe = probe(worker, "cpu", args.worker_dir, args.timeout)
            cpu_entry: dict[str, Any] = {"model": model_key, "backend": "cpu", "probe": cpu_probe}
            if cpu_probe["status"] == PASS:
                cpu_entry["run"] = run_session(worker, args.worker_dir, args.worker_dir, model_key, args.model_dir, args.audio, "cpu", args.requests, args.timeout)
                baseline = cpu_entry["run"]
                cpu_entry["correctness"] = PASS if cpu_entry["run"]["status"] == PASS else cpu_entry["run"]["status"]
                cpu_entry["status"] = cpu_entry["run"]["status"]
                # The CPU backend is dynamic too. The base artifact directory
                # intentionally contains ggml-cpu but no Vulkan/CUDA backend,
                # which is the real "GPU files absent" deployment shape.
                cpu_empty_probe = probe(worker, "cpu", args.worker_dir, args.timeout)
                cpu_entry["fault_injection"] = {
                    "no_gpu_backend_files": cpu_empty_probe,
                    "gpu_probe_failure": "untested (requires a real backend artifact with a disabled/incompatible driver)",
                }
                restart = run_session(worker, args.worker_dir, args.worker_dir, model_key, args.model_dir, args.audio, "cpu", 1, args.timeout)
                crash = crash_session(worker, args.worker_dir, args.worker_dir, model_key, args.model_dir, args.audio, "cpu", args.timeout)
                cpu_entry["lifecycle"] = {
                    "stop_restart": restart["status"],
                    "abnormal_exit": crash["status"],
                    "cancel": "untested (protocol v1 has no cancel message; manager tests cover cancellation)",
                    "install_cancel_rollback_repair_upgrade": "covered by Rust manager/provider tests; not a worker protocol operation",
                }
                if cpu_empty_probe["status"] != PASS or restart["status"] != PASS or crash["status"] != PASS:
                    cpu_entry["status"] = FAIL
            else:
                cpu_entry.update({"status": cpu_probe["status"], "reason": "CPU baseline probe failed"})
            # Keep the measured CPU baseline in the evidence even when the
            # requested matrix contains only a GPU backend.
            report["matrix"].append(cpu_entry)

            for backend in backends:
                if backend == "cpu":
                    continue
                entry = {"model": model_key, "backend": backend}
                entry["probe"] = probe(worker, backend, backend_dir, args.timeout)
                if entry["probe"]["status"] == PASS:
                    entry["run"] = run_session(worker, args.worker_dir, backend_dir, model_key, args.model_dir, args.audio, backend, args.requests, args.timeout)
                    if baseline is None or "output_texts" not in baseline or "output_texts" not in entry["run"]:
                        entry["correctness"] = {"status": BLOCKED, "reason": "CPU baseline unavailable"}
                    else:
                        entry["correctness"] = compare_outputs(
                            baseline,
                            entry["run"],
                            thresholds["correctness"]["text_normalized_edit_distance_ratio_max"],
                        )
                    cpu_p50 = baseline.get("latency_ms", {}).get("p50") if baseline else None
                    gpu_p50 = entry["run"].get("latency_ms", {}).get("p50")
                    cpu_use = baseline.get("resource", {}).get("cpu_percent_p95") if baseline else None
                    gpu_use = entry["run"].get("resource", {}).get("cpu_percent_p95")
                    speedup = cpu_p50 / gpu_p50 if cpu_p50 and gpu_p50 else None
                    cpu_reduction = gpu_use / cpu_use if cpu_use and gpu_use else None
                    entry["performance"] = {
                        "speedup_ratio": speedup,
                        "gpu_cpu_to_cpu_cpu_ratio": cpu_reduction,
                        "vram_mb_p95": entry["run"].get("resource", {}).get("vram_mb_p95"),
                        "status": (
                            PASS
                            if speedup is not None and speedup >= thresholds["performance"]["minimum_gpu_speedup_ratio"]
                            or cpu_reduction is not None and cpu_reduction <= thresholds["performance"]["maximum_gpu_cpu_regression_ratio"]
                            else BLOCKED
                        ),
                    }
                    if entry["run"]["status"] != PASS or entry["correctness"]["status"] != PASS or entry["performance"]["status"] != PASS:
                        entry["status"] = FAIL if FAIL in (entry["run"]["status"], entry["correctness"]["status"]) else BLOCKED
                    else:
                        entry["status"] = PASS
                    with tempfile.TemporaryDirectory(prefix="blink-adoption-empty-backend-") as empty:
                        empty_probe = probe(worker, backend, Path(empty), args.timeout)
                    entry["fault_injection"] = {
                        "empty_backend_dir": empty_probe,
                        "explicit_failure_no_cpu_fallback": empty_probe["status"] != PASS,
                        "driver_initialization_failure": "untested (requires a real incompatible/disabled driver)",
                    }
                    restart = run_session(worker, args.worker_dir, backend_dir, model_key, args.model_dir, args.audio, backend, 1, args.timeout)
                    crash = crash_session(worker, args.worker_dir, backend_dir, model_key, args.model_dir, args.audio, backend, args.timeout)
                    entry["lifecycle"] = {
                        "stop_restart": restart["status"],
                        "abnormal_exit": crash["status"],
                        "cancel": "untested (protocol v1 has no cancel message; manager tests cover cancellation)",
                        "install_cancel_rollback_repair_upgrade": "covered by Rust manager/provider tests; not a worker protocol operation",
                    }
                else:
                    entry["status"] = entry["probe"]["status"]
                report["matrix"].append(entry)
            # Never serialize raw recognition text into evidence.
            for entry in report["matrix"]:
                if entry.get("model") == model_key:
                    entry.get("run", {}).pop("output_texts", None)
    report["finished_at"] = utc_now()
    if args.out is None:
        DEFAULT_EVIDENCE_DIR.mkdir(parents=True, exist_ok=True)
        args.out = DEFAULT_EVIDENCE_DIR / f"adoption-{time.strftime('%Y%m%d-%H%M%S', time.gmtime())}.json"
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"status": report["preflight"]["status"], "out": str(args.out), "blockers": report["preflight"].get("blockers", [])}, ensure_ascii=False))
    statuses = [entry["status"] for entry in report["matrix"]]
    if report["preflight"]["status"] == BLOCKED or BLOCKED in statuses:
        return 2
    if report["release_consumer"]["application_release_static_guard"] == FAIL or FAIL in statuses:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
