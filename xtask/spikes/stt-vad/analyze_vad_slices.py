"""Slicing review for unlisted private WAVs: waveform pauses vs production VAD boundaries.

Inputs are the private full-transcribe baseline (corpus dir, gitignored) written by
the gated Rust test `private_corpus_unlisted_full_transcribe`. Outputs stay in the
same private corpus dir: per-file pause list, VAD cut mapping and recognition diff.
"""

from __future__ import annotations

import argparse
import array
import hashlib
import json
import math
import sys
import wave
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]


def rms_10ms(path: Path) -> list[float]:
    with wave.open(str(path), "rb") as audio:
        if audio.getsampwidth() != 2 or audio.getcomptype() != "NONE":
            raise ValueError("pause inspection requires uncompressed PCM16 WAV")
        channels = audio.getnchannels()
        frames_per_bin = max(1, round(audio.getframerate() / 100))
        values = []
        while raw := audio.readframes(frames_per_bin):
            samples = array.array("h")
            samples.frombytes(raw)
            if sys.byteorder != "little":
                samples.byteswap()
            frames = len(samples) // channels
            if not frames:
                continue
            power = 0.0
            for offset in range(0, frames * channels, channels):
                mono = sum(samples[offset : offset + channels]) / (channels * 32768.0)
                power += mono * mono
            values.append(math.sqrt(power / frames))
    return values


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int((len(ordered) - 1) * fraction))]


def smooth_50ms(values: list[float]) -> list[float]:
    prefix = [0.0]
    for value in values:
        prefix.append(prefix[-1] + value * value)
    smoothed = []
    for index in range(len(values)):
        start = max(0, index - 2)
        end = min(len(values), index + 3)
        smoothed.append(math.sqrt((prefix[end] - prefix[start]) / (end - start)))
    return smoothed


def pause_intervals(values: list[float], gate: float, min_ms: int) -> list[dict[str, int]]:
    active = [index for index, value in enumerate(values) if value >= gate]
    if len(active) < 2:
        return []
    first, last = active[0], active[-1]
    pauses = []
    start = None
    for index in range(first, last + 2):
        quiet = index <= last and values[index] < gate
        if quiet and start is None:
            start = index
        elif not quiet and start is not None:
            if index - start >= min_ms // 10 and start > first and index <= last:
                pauses.append(
                    {"start_ms": start * 10, "end_ms": index * 10, "duration_ms": (index - start) * 10}
                )
            start = None
    return pauses


def normalize(text: str) -> str:
    import unicodedata

    cleaned = []
    for char in text:
        code = ord(char)
        if char.isspace() or unicodedata.category(char).startswith("P"):
            continue
        if 0xFF01 <= code <= 0xFF5F or 0x3000 <= code <= 0x303F:
            continue
        cleaned.append(char)
    return "".join(cleaned).lower()


def diff_positions(left: str, right: str) -> tuple[int, int]:
    """Rough char-level diff count via edit distance (returns distance, max_len)."""
    previous = list(range(len(right) + 1))
    for i, left_char in enumerate(left, 1):
        current = [i] + [0] * len(right)
        for j, right_char in enumerate(right, 1):
            current[j] = min(previous[j] + 1, current[j - 1] + 1, previous[j - 1] + (left_char != right_char))
        previous = current
    return previous[len(right)], max(len(left), len(right))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path, default=ROOT / "testdata/stt/corpus/my-wavs")
    parser.add_argument("--min-pause-ms", type=int, default=150)
    args = parser.parse_args()

    baseline = json.loads((args.corpus / "full-transcribe-baseline.json").read_text(encoding="utf-8"))
    by_hash = {
        "new_" + hashlib.sha256((args.corpus / case["filename"]).read_bytes()).hexdigest()[:8]: case
        for case in baseline["cases"]
    }

    report = []
    for path in sorted(args.corpus.iterdir()):
        if not path.is_file() or path.suffix.lower() != ".wav":
            continue
        case_id = "new_" + hashlib.sha256(path.read_bytes()).hexdigest()[:8]
        case = by_hash.get(case_id)
        if case is None:
            continue
        rms = rms_10ms(path)
        smoothed = smooth_50ms(rms)
        p20, p90 = percentile(smoothed, 0.2), percentile(smoothed, 0.9)
        gate = min(0.02, max(0.001, p20 * 2.5, p90 * 0.18))
        pauses = pause_intervals(smoothed, gate, args.min_pause_ms)

        vad_events = [event["time_ms"] for event in case["offline_vad"]["boundaries"]]
        # 每个切点归类：命中 ≥min_silence 的波形停顿（正常切）或落在连续语音内（疑误切）
        cut_analysis = []
        for event in case["offline_vad"]["boundaries"]:
            time_ms = event["time_ms"]
            inside_pause = next(
                (
                    pause
                    for pause in pauses
                    if pause["start_ms"] - 300 <= time_ms <= pause["end_ms"] + 100
                ),
                None,
            )
            cut_analysis.append(
                {
                    "time_ms": time_ms,
                    "reason": event["reason"],
                    "matched_pause_ms": inside_pause["duration_ms"] if inside_pause else None,
                    "verdict": "cut_at_pause" if inside_pause else "cut_inside_speech",
                }
            )
        # 每个波形停顿归类：是否产生切点（含 350ms 内接近匹配）
        pause_analysis = []
        for pause in pauses:
            matched = any(
                pause["start_ms"] - 300 <= cut["time_ms"] <= pause["end_ms"] + 100
                for cut in cut_analysis
            )
            pause_analysis.append(
                {
                    **pause,
                    "cut": matched,
                }
            )

        whole_norm = normalize(case["whole_file"]["text"])
        stream_norm = normalize(case["streaming"]["final_text"])
        distance, length = diff_positions(stream_norm, whole_norm)

        report.append(
            {
                "filename": case["filename"],
                "case_id": case_id,
                "duration_ms": case["duration_ms"],
                "gate": round(gate, 6),
                "waveform_pauses": pause_analysis,
                "vad_cuts": cut_analysis,
                "stream_boundaries": case["streaming"]["boundaries"],
                "rejected_short_sentences": case["offline_vad"]["rejected_short_sentences"],
                "whole_text": case["whole_file"]["text"],
                "stream_text": case["streaming"]["final_text"],
                "whole_chars": len(whole_norm),
                "stream_chars": len(stream_norm),
                "stream_vs_whole_distance": distance,
                "stream_vs_whole_similarity_pct": round((length - distance) * 100 / length) if length else 100,
            }
        )

    output = args.corpus / "vad-slice-analysis.json"
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"Analyzed {len(report)} wavs -> {output}")
    for item in report:
        cuts = item["vad_cuts"]
        false_cuts = [cut for cut in cuts if cut["verdict"] == "cut_inside_speech"]
        uncut_pauses = [p for p in item["waveform_pauses"] if not p["cut"]]
        print(
            f"{item['case_id']} dur={item['duration_ms']}ms cuts={len(cuts)} "
            f"false_cuts={len(false_cuts)} pauses={len(item['waveform_pauses'])} "
            f"uncut_pauses={[(p['duration_ms']) for p in uncut_pauses]} "
            f"sim={item['stream_vs_whole_similarity_pct']}%"
        )


if __name__ == "__main__":
    main()
