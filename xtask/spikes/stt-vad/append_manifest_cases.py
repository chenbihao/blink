"""Append manifest cases for the six unlisted long wavs using whole-file best texts.

Reads full-transcribe-baseline.json (written by the gated Rust replay) for exact
filenames and transcripts; appends case_12..case_17 to manifest.toml.
Run once, manually; the corpus dir is private/gitignored.
"""

from __future__ import annotations

import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
CORPUS = ROOT / "testdata/stt/corpus/my-wavs"

# filename -> (case_id, scene, tags, expected_segments)
PLAN = {
    "明确的多句话": ("case_12", "多句不同长度停顿", ["multi_sentence", "varying_pauses", "missed_cut_probe"], 4),
    "同一段内容分别停顿": ("case_13", "停顿时长梯度 200/350/500/800ms", ["pause_gradient", "strong_pause"], 5),
    "一段连续说话超过": ("case_14", "超过12秒连续说话两遍", ["continuous_speech", "over_12s", "forced_cut"], 5),
    "正常音量，同一段话": ("case_15", "同一段话三种音量/距离版本", ["volume_variants", "soft_voice", "far_mic"], 4),
    "测试一下 这里停顿": ("case_16", "多个短停顿加一个久停顿", ["short_pauses", "long_pause", "no_false_cut"], 2),
    "风扇或键盘声": ("case_17", "风扇键盘噪声长句加纯环境音尾段", ["fan_noise", "keyboard_noise", "environment_tail", "false_cut_probe"], 3),
}


def main() -> None:
    baseline = json.loads((CORPUS / "full-transcribe-baseline.json").read_text(encoding="utf-8"))
    manifest_path = CORPUS / "manifest.toml"
    manifest = manifest_path.read_text(encoding="utf-8")
    existing_ids = {line.split('"')[1] for line in manifest.splitlines() if line.startswith("case_id")}
    blocks = []
    for case in baseline["cases"]:
        filename = case["filename"]
        text = case["whole_file"]["text"].strip()
        if not text:
            raise SystemExit(f"whole text empty for {filename}; refusing to add empty expectation")
        for prefix, (case_id, scene, tags, segments) in PLAN.items():
            if filename.startswith(prefix):
                break
        else:
            raise SystemExit(f"no plan entry for {filename}")
        if case_id in existing_ids:
            raise SystemExit(f"{case_id} already present; refusing to duplicate")
        tag_line = ", ".join(f'"{tag}"' for tag in tags)
        blocks.append(
            f"[[cases]]\n"
            f'case_id = "{case_id}"\n'
            f'filename = "{filename}"\n'
            f'scene = "{scene}"\n'
            f"tags = [{tag_line}]\n"
            f'expected_text = "{text}"\n'
            f"expected_empty = false\n"
            f"expected_segments = {segments}\n"
        )
        print(f"{case_id} <- {filename[:30]}... segments={segments} chars={len(text)}")
    if len(blocks) != len(PLAN):
        raise SystemExit(f"matched {len(blocks)} of {len(PLAN)} planned cases")
    manifest_path.write_text(manifest.rstrip("\n") + "\n\n" + "\n".join(blocks), encoding="utf-8")
    print(f"appended {len(blocks)} cases to {manifest_path}")


if __name__ == "__main__":
    main()
