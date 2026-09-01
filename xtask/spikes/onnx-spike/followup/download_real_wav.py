#!/usr/bin/env python3
"""Download real test WAV files for Spike E Round 2."""
import os
import urllib.request
import ssl

MODELS_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "models")
wav_path = os.path.join(MODELS_DIR, "asr_example.wav")

# Try multiple sources for real speech WAV
urls = [
    # Sherpa-onnx test wavs (HuggingFace)
    "https://huggingface.co/csukuangfj/sherpa-onnx-streaming-paraformer-bilingual-zh-en/resolve/main/test_wavs/0.wav",
    "https://huggingface.co/alphacephei/sherpa-onnx-streaming-paraformer-bilingual-zh-en/resolve/main/test_wavs/0.wav",
    # FunASR test wav
    "https://huggingface.co/funasr/Paraformer-online/resolve/main/asr_example.wav",
    # ModelScope
    "https://www.modelscope.cn/api/v1/models/iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx/repo?Revision=master&FilePath=asr_example.wav",
]

ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE

for url in urls:
    try:
        print(f"Trying: {url}")
        req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
        with urllib.request.urlopen(req, timeout=30, context=ctx) as resp:
            data = resp.read()
            # Check it's a valid WAV (RIFF header)
            if len(data) > 1000 and data[:4] == b"RIFF":
                with open(wav_path, "wb") as f:
                    f.write(data)
                print(f"SUCCESS: Downloaded {len(data):,} bytes")
                # Verify WAV format
                import wave
                import io
                wf = wave.open(io.BytesIO(data), "rb")
                print(f"  Sample rate: {wf.getframerate()}, channels: {wf.getnchannels()}, frames: {wf.getnframes()}")
                print(f"  Duration: {wf.getnframes() / wf.getframerate():.2f}s")
                break
            else:
                print(f"  Not a WAV file (size={len(data)}, header={data[:4]!r})")
    except Exception as e:
        print(f"  Failed: {e}")

else:
    print("Could not download real WAV from any source. Using synthetic WAV.")
