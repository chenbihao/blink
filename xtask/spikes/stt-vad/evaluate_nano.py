"""Compare VAD cut points using the installed Fun-ASR-Nano worker.

Uses filenames only as approximate references. The report contains opaque IDs and
error rates, never filenames, references, or recognized text.
"""

import argparse
import array
import hashlib
import json
import os
import queue
import subprocess
import tempfile
import threading
import time
import tomllib
import unicodedata
import wave
from pathlib import Path

try:
    import psutil
except ImportError:
    psutil = None


ROOT = Path(__file__).resolve().parents[3]
CORPUS = ROOT / "testdata/stt/corpus/my-wavs"
SWEEP = ROOT / "target/stt-vad-parameter-sweep.json"
OUTPUT = ROOT / "target/stt-vad-nano-evaluation.json"
# 0.23.7 candidate matrix: labels match runs[].label in the sweep report.
# Params: (threshold, min_silence_ms, min_sentence_ms, soft_s, hard_s, uncommitted_s).
PROFILES = {
    "A_800_8_12_12": (0.005, 300, 800, 8, 12, 12),
    "B_1000_8_12_12": (0.005, 300, 1000, 8, 12, 12),
    "C_1000_10_12_12": (0.005, 300, 1000, 10, 12, 12),
    "D_1000_8_14_14": (0.005, 300, 1000, 8, 14, 14),
    "E_1000_10_14_14": (0.005, 300, 1000, 10, 14, 14),
    "F_1000_10_16_16": (0.005, 300, 1000, 10, 16, 16),
    "G_1000_6_10_10": (0.005, 300, 1000, 6, 10, 10),
    "H_1000_10_14_16": (0.005, 300, 1000, 10, 14, 16),
}


def normalize(text):
    return "".join(
        char.lower()
        for char in unicodedata.normalize("NFKC", text)
        if char.isalnum()
    )


def character_error_rate(reference, hypothesis):
    if not reference:
        return None
    previous = list(range(len(hypothesis) + 1))
    for row, expected in enumerate(reference, 1):
        current = [row]
        for col, actual in enumerate(hypothesis, 1):
            current.append(
                min(
                    current[-1] + 1,
                    previous[col] + 1,
                    previous[col - 1] + (expected != actual),
                )
            )
        previous = current
    return round(previous[-1] / len(reference), 4)


def load_audio(path):
    with wave.open(str(path), "rb") as source:
        fmt = (source.getframerate(), source.getnchannels(), source.getsampwidth())
        if fmt != (48000, 2, 2):
            raise ValueError(f"unsupported WAV format: {fmt}")
        stereo = array.array("h")
        stereo.frombytes(source.readframes(source.getnframes()))
    mono = array.array("h")
    # The corpus is 48 kHz stereo. Average each 3-frame block to obtain
    # a 16 kHz mono signal; all profiles receive identical converted audio.
    for offset in range(0, len(stereo) - 5, 6):
        mono.append(round(sum(stereo[offset : offset + 6]) / 6))
    return mono


def write_audio(path, samples):
    with wave.open(str(path), "wb") as output:
        output.setnchannels(1)
        output.setsampwidth(2)
        output.setframerate(16000)
        output.writeframes(samples.tobytes())


def trim_trailing_silence(samples, off_threshold):
    """Match the production 150 ms tail buffer and absolute-sample gate."""
    threshold = max(off_threshold, 0.0005) * 32768
    for index in range(len(samples) - 1, -1, -1):
        if abs(samples[index]) > threshold:
            return samples[: min(len(samples), index + 1 + 2400)]
    return samples[:0]


class NanoWorker:
    def __init__(self, audio_dir):
        appdata = Path(os.environ["APPDATA"]) / "blink"
        model_root = (
            appdata / "models/funasr/gguf-fun-asr-nano-q4km-9faa9616b982"
        )
        active = json.loads((model_root / "active.json").read_text(encoding="utf-8"))
        payload = model_root / "slots" / active["slot_id"] / "payload"
        runtime = appdata / "runtimes/engines/funasr"
        deployment = json.loads((runtime / "deployment.json").read_text(encoding="utf-8"))
        worker = runtime / deployment["slot"] / "funasr-nano-worker.exe"
        environment = os.environ.copy()
        environment.update(
            BLINK_ENGINE_ID="funasr",
            BLINK_INSTANCE_ID="vad-evaluation",
            BLINK_ENGINE_TOKEN="vad-evaluation-token",
            BLINK_MODEL_ID="gguf/fun-asr-nano-q4km",
            BLINK_MODEL_REVISION="gguf-v0.2.6",
            BLINK_MODEL_PAYLOAD_DIR=str(payload),
            BLINK_AUDIO_DIR=str(audio_dir),
            BLINK_WORKER_THREADS="4",
        )
        self.process = subprocess.Popen(
            [
                str(worker),
                "--enc",
                str(payload / "funasr-encoder-f16.gguf"),
                "-m",
                str(payload / "qwen3-0.6b-q4km.gguf"),
                "--stdin-server",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            cwd=worker.parent,
            env=environment,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
        self.messages = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()
        ready = self._receive(60)
        if ready.get("type") != "ready":
            raise RuntimeError("Nano worker did not become ready")
        self.sequence = 0

    def _read(self):
        for line in self.process.stdout:
            self.messages.put(line)
        self.messages.put(None)

    def _receive(self, timeout):
        line = self.messages.get(timeout=timeout)
        if line is None:
            raise RuntimeError(f"Nano worker exited: {self.process.poll()}")
        return json.loads(line.decode("utf-8"))

    def transcribe(self, audio_path):
        self.sequence += 1
        request_id = f"evaluation-{self.sequence}"
        request = {
            "type": "transcribe",
            "request_id": request_id,
            "audio_path": str(audio_path),
        }
        self.process.stdin.write((json.dumps(request) + "\n").encode("utf-8"))
        self.process.stdin.flush()
        started = time.monotonic()
        response = self._receive(120)
        if response.get("request_id") != request_id or response.get("ok") is not True:
            error = response.get("error") or {}
            raise RuntimeError(f"Nano transcription failed: {error.get('code', 'unknown')}")
        return response.get("text") or "", round(time.monotonic() - started, 3)

    def peak_rss_mb(self):
        """Peak working set of the worker process in MB (0 when psutil is absent)."""
        if psutil is not None:
            try:
                process = psutil.Process(self.process.pid)
                return process.memory_info().peak_wset / (1024 * 1024)
            except psutil.Error:
                return 0.0
        return 0.0

    def close(self):
        if self.process.poll() is None:
            try:
                self.process.stdin.write(b'{"type":"shutdown"}\n')
                self.process.stdin.flush()
                self.process.wait(timeout=5)
            except (OSError, subprocess.TimeoutExpired):
                self.process.kill()
                self.process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--labeled", action="store_true", help="evaluate manifest cases")
    args = parser.parse_args()
    output = (
        ROOT / "target/stt-vad-nano-labeled-evaluation.json"
        if args.labeled
        else OUTPUT
    )
    manifest = tomllib.loads((CORPUS / "manifest.toml").read_text(encoding="utf-8"))
    labeled = {case["filename"]: case for case in manifest["cases"]}
    sweep = json.loads(SWEEP.read_text(encoding="utf-8"))
    runs_by_label = {run["label"]: run for run in sweep["runs"]}
    by_profile = {}
    for label, params in PROFILES.items():
        run = runs_by_label.get(label.split("_")[0])
        if run is None:
            raise SystemExit(f"sweep report is missing profile {label}")
        actual = (
            run["silence_threshold"],
            run["min_silence_ms"],
            run["min_sentence_ms"],
            run["soft_window_s"],
            run["hard_window_s"],
            run["max_uncommitted_s"],
        )
        if actual != params:
            raise SystemExit(f"sweep parameters drifted for {label}: {actual}")
        by_profile[label] = {case["case_id"]: case for case in run["cases"]}
    selected_profiles = list(PROFILES)
    report = {
        "profiles": {key: PROFILES[key] for key in selected_profiles},
        "cases": [],
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="stt-vad-nano-", dir=output.parent) as folder:
        audio_dir = Path(folder)
        worker = NanoWorker(audio_dir)
        try:
            for path in sorted(CORPUS.glob("*.wav")):
                listed = labeled.get(path.name)
                if bool(listed) != args.labeled:
                    continue
                case_id = (
                    listed["case_id"]
                    if listed
                    else "new_" + hashlib.sha256(path.read_bytes()).hexdigest()[:8]
                )
                samples = load_audio(path)
                reference = normalize(listed["expected_text"] if listed else path.stem)
                results = {}
                baseline_label = next(iter(PROFILES))
                default_case = by_profile[baseline_label][case_id]
                variants = {"whole": ([], [default_case["terminal_off_threshold"]])}
                for label in selected_profiles:
                    case = by_profile[label][case_id]
                    events = case["events"]
                    variants[label] = (
                        [event["time_ms"] for event in events],
                        [event["off_threshold"] for event in events]
                        + [case["terminal_off_threshold"]],
                    )
                whole_text = None
                for label, (cuts, thresholds) in variants.items():
                    boundaries = [0] + [min(len(samples), ms * 16) for ms in cuts] + [len(samples)]
                    texts = []
                    elapsed = 0.0
                    durations = []
                    for index, (start, end) in enumerate(zip(boundaries, boundaries[1:])):
                        if end - start < 1600:
                            continue
                        trimmed = trim_trailing_silence(samples[start:end], thresholds[index])
                        if not trimmed:
                            continue
                        segment = audio_dir / f"segment-{label}-{index}.wav"
                        write_audio(segment, trimmed)
                        try:
                            text, seconds = worker.transcribe(segment)
                        finally:
                            segment.unlink(missing_ok=True)
                        texts.append(text)
                        elapsed += seconds
                        durations.append(round(len(trimmed) / 16000, 2))
                    if label == "whole":
                        whole_text = normalize("".join(texts))
                    combined = normalize("".join(texts))
                    results[label] = {
                        "cuts_ms": cuts,
                        "segment_durations_s": durations,
                        "cer": character_error_rate(reference, combined),
                        "cer_vs_whole": character_error_rate(whole_text, combined),
                        "transcript_chars": len(combined),
                        "inference_seconds": round(elapsed, 2),
                    }
                report["cases"].append(
                    {
                        "case_id": case_id,
                        "reference_kind": "manifest" if listed else "filename_proxy",
                        "duration_seconds": round(len(samples) / 16000, 2),
                        "reference_chars": len(reference),
                        "results": results,
                    }
                )
                output.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
                print(f"evaluated {case_id}", flush=True)
        finally:
            # 在 worker 退出前采样峰值 working set（进程退出后 psutil 无法读取）
            report["worker_peak_working_set_mb"] = round(worker.peak_rss_mb(), 1)
            worker.close()
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"report: {output}")


if __name__ == "__main__":
    main()
