#!/usr/bin/env python3
"""Create deterministic WAV prefixes for the 0.22.7 pseudo-streaming smoke."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import wave


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("source", type=Path)
    parser.add_argument("output_dir", type=Path)
    parser.add_argument("--seconds", type=float, nargs="+", default=[0.5, 1.0, 2.0, 3.0, 4.0])
    args = parser.parse_args()

    args.output_dir.mkdir(parents=True, exist_ok=True)
    outputs: list[dict[str, object]] = []
    with wave.open(str(args.source), "rb") as source:
        params = source.getparams()
        frames = source.readframes(params.nframes)
        bytes_per_frame = params.nchannels * params.sampwidth
        duration = params.nframes / params.framerate

        for seconds in args.seconds:
            frame_count = min(params.nframes, round(seconds * params.framerate))
            output = args.output_dir / f"prefix-{seconds:g}s.wav"
            with wave.open(str(output), "wb") as target:
                target.setparams(params)
                target.writeframes(frames[: frame_count * bytes_per_frame])
            outputs.append(
                {
                    "path": str(output.resolve()),
                    "requested_seconds": seconds,
                    "actual_seconds": frame_count / params.framerate,
                }
            )

    outputs.append(
        {
            "path": str(args.source.resolve()),
            "requested_seconds": "full",
            "actual_seconds": duration,
        }
    )
    print(json.dumps(outputs, ensure_ascii=False))


if __name__ == "__main__":
    main()
