"""Compare waveform pauses with production EnergyVad cuts on unlisted private WAVs.

The input names may contain spoken text. Reports use only anonymous content hashes.
Run the Rust private_corpus_vad_parameter_sweep test first to refresh its JSON report.
"""

from __future__ import annotations

import argparse
import array
import hashlib
import json
import math
import sys
import wave
from html import escape
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


def pause_intervals(values: list[float], gate: float) -> list[dict[str, int]]:
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
            if index - start >= 20 and start > first and index <= last:
                pauses.append(
                    {"start_ms": start * 10, "end_ms": index * 10, "duration_ms": (index - start) * 10}
                )
            start = None
    return pauses


def find_run(runs: list[dict], threshold: float, silence_ms: int, sentence_ms: int) -> dict:
    return next(
        run
        for run in runs
        if run["silence_threshold"] == threshold
        and run["min_silence_ms"] == silence_ms
        and run["min_sentence_ms"] == sentence_ms
    )


def make_svg(items: list[dict]) -> str:
    width, panel_height = 1200, 200
    left, right = 75, 1160
    height = 55 + panel_height * len(items)
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">',
        '<rect width="100%" height="100%" fill="#fff"/>',
        '<style>text{font-family:Arial,"Microsoft YaHei",sans-serif;fill:#263238} .small{font-size:12px} .title{font-size:15px;font-weight:600}</style>',
        '<text x="75" y="25" class="title">新增长语音：波形候选停顿与生产 VAD 切点</text>',
        '<text x="75" y="44" class="small">绿色＝低能量停顿（≥200ms）；橙线＝默认参数；紫线＝更高灵敏度。停顿只是候选，不代表语义句界。</text>',
    ]
    for index, item in enumerate(items):
        top = 60 + index * panel_height
        chart_top, chart_bottom = top + 32, top + 150
        duration = item["duration_ms"] / 1000
        values = item["rms"]
        ymax = max(item["p98"] * 1.12, item["gate"] * 2, 0.002)

        def x_pos(seconds: float) -> float:
            return left + seconds / duration * (right - left)

        def y_pos(value: float) -> float:
            return chart_bottom - min(value, ymax) / ymax * (chart_bottom - chart_top)

        parts.append(f'<text x="{left}" y="{top + 18}" class="title">{escape(item["case_id"])} · {duration:.2f}s</text>')
        parts.append(f'<rect x="{left}" y="{chart_top}" width="{right-left}" height="{chart_bottom-chart_top}" fill="#f8fafc"/>')
        for pause in item["pauses"]:
            x1 = x_pos(pause["start_ms"] / 1000)
            x2 = x_pos(pause["end_ms"] / 1000)
            parts.append(f'<rect x="{x1:.1f}" y="{chart_top}" width="{x2-x1:.1f}" height="{chart_bottom-chart_top}" fill="#c8ebd4" opacity="0.8"/>')
        points = " ".join(f"{x_pos(i * 0.01):.1f},{y_pos(value):.1f}" for i, value in enumerate(values))
        parts.append(f'<polyline points="{points}" fill="none" stroke="#3173a6" stroke-width="1"/>')
        gate_y = y_pos(item["gate"])
        parts.append(f'<line x1="{left}" x2="{right}" y1="{gate_y:.1f}" y2="{gate_y:.1f}" stroke="#778899" stroke-dasharray="3,4"/>')
        for key, color, dash in (("default_events", "#e06b26", ""), ("sensitive_events", "#8856ba", ' stroke-dasharray="5,3"')):
            for event in item[key]:
                x = x_pos(event["time_ms"] / 1000)
                parts.append(f'<line x1="{x:.1f}" x2="{x:.1f}" y1="{chart_top}" y2="{chart_bottom}" stroke="{color}" stroke-width="2"{dash}/>')
        for second in range(0, math.ceil(duration) + 1, 5):
            x = x_pos(second)
            parts.append(f'<line x1="{x:.1f}" x2="{x:.1f}" y1="{chart_bottom}" y2="{chart_bottom+4}" stroke="#607080"/>')
            parts.append(f'<text x="{x:.1f}" y="{chart_bottom+18}" text-anchor="middle" class="small">{second}s</text>')
    parts.append("</svg>")
    return "\n".join(parts)


def make_png(items: list[dict], path: Path) -> None:
    """Write an easy-to-preview raster copy when Pillow is available."""
    try:
        from PIL import Image, ImageDraw, ImageFont
    except ImportError:
        return
    width, panel_height = 1200, 200
    left, right = 75, 1160
    image = Image.new("RGB", (width, 55 + panel_height * len(items)), "white")
    draw = ImageDraw.Draw(image)
    try:
        font = ImageFont.truetype("arial.ttf", 14)
        title_font = ImageFont.truetype("arial.ttf", 18)
    except OSError:
        font = title_font = ImageFont.load_default()
    draw.text((left, 10), "Waveform pauses vs production VAD cuts", fill="#263238", font=title_font)
    draw.text((left, 35), "Green: quiet >=200ms    Orange: default VAD    Purple: sensitive VAD", fill="#263238", font=font)
    for index, item in enumerate(items):
        top = 60 + index * panel_height
        chart_top, chart_bottom = top + 32, top + 150
        duration = item["duration_ms"] / 1000
        ymax = max(item["p98"] * 1.12, item["gate"] * 2, 0.002)

        def x_pos(seconds: float) -> int:
            return round(left + seconds / duration * (right - left))

        def y_pos(value: float) -> int:
            return round(chart_bottom - min(value, ymax) / ymax * (chart_bottom - chart_top))

        draw.text((left, top + 4), f"{item['case_id']}  ({duration:.2f}s)", fill="#263238", font=font)
        draw.rectangle((left, chart_top, right, chart_bottom), fill="#f8fafc")
        for pause in item["pauses"]:
            draw.rectangle((x_pos(pause["start_ms"] / 1000), chart_top, x_pos(pause["end_ms"] / 1000), chart_bottom), fill="#c8ebd4")
        points = [(x_pos(i * 0.01), y_pos(value)) for i, value in enumerate(item["rms"])]
        draw.line(points, fill="#3173a6", width=1)
        gate_y = y_pos(item["gate"])
        for x in range(left, right, 8):
            draw.line((x, gate_y, min(x + 4, right), gate_y), fill="#778899", width=1)
        for key, color in (("default_events", "#e06b26"), ("sensitive_events", "#8856ba")):
            for event in item[key]:
                x = x_pos(event["time_ms"] / 1000)
                draw.line((x, chart_top, x, chart_bottom), fill=color, width=2)
        for second in range(0, math.ceil(duration) + 1, 5):
            x = x_pos(second)
            draw.line((x, chart_bottom, x, chart_bottom + 4), fill="#607080")
            draw.text((x - 8, chart_bottom + 7), f"{second}s", fill="#607080", font=font)
    image.save(path)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path, default=ROOT / "testdata/stt/corpus/my-wavs")
    parser.add_argument("--report", type=Path, default=ROOT / "target/stt-vad-parameter-sweep.json")
    parser.add_argument("--output-dir", type=Path, default=ROOT / "target")
    args = parser.parse_args()
    report = json.loads(args.report.read_text(encoding="utf-8"))
    runs = report["runs"]
    default = find_run(runs, 0.005, 300, 800)
    sensitive = find_run(runs, 0.002, 300, 800)
    default_cases = {case["case_id"]: case for case in default["cases"]}
    sensitive_cases = {case["case_id"]: case for case in sensitive["cases"]}
    items = []
    for path in args.corpus.iterdir():
        if not path.is_file() or path.suffix.lower() != ".wav":
            continue
        case_id = "new_" + hashlib.sha256(path.read_bytes()).hexdigest()[:8]
        if case_id not in default_cases:
            continue
        rms = rms_10ms(path)
        smoothed = smooth_50ms(rms)
        p20, p90 = percentile(smoothed, 0.2), percentile(smoothed, 0.9)
        gate = min(0.02, max(0.001, p20 * 2.5, p90 * 0.18))
        items.append(
            {
                "case_id": case_id,
                "duration_ms": default_cases[case_id]["duration_ms"],
                "rms": rms,
                "p20": p20,
                "p90": p90,
                "p98": percentile(rms, 0.98),
                "gate": gate,
                "pauses": pause_intervals(smoothed, gate),
                "default_events": default_cases[case_id]["events"],
                "sensitive_events": sensitive_cases[case_id]["events"],
            }
        )
    items.sort(key=lambda item: item["duration_ms"])
    args.output_dir.mkdir(parents=True, exist_ok=True)
    summary = [{key: value for key, value in item.items() if key not in ("rms", "p98")} for item in items]
    (args.output_dir / "stt-vad-new-wavs-pauses.json").write_text(
        json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    (args.output_dir / "stt-vad-new-wavs-overview.svg").write_text(make_svg(items), encoding="utf-8")
    make_png(items, args.output_dir / "stt-vad-new-wavs-overview.png")
    print(f"Analyzed {len(items)} anonymous new WAVs; wrote pause summary and waveform overview.")


if __name__ == "__main__":
    main()
