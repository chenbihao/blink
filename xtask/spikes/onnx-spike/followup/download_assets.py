#!/usr/bin/env python3
"""Download ParaformerOnline model assets and test WAV for Spike E."""
import os
import sys

MODELS_DIR = os.path.join(os.path.dirname(__file__), "..", "models")
os.makedirs(MODELS_DIR, exist_ok=True)

# 1. Download ParaformerOnline model from ModelScope
print("=== Downloading ParaformerOnline model from ModelScope ===")
try:
    from modelscope import snapshot_download
    model_dir = snapshot_download(
        "speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx",
        cache_dir=MODELS_DIR,
    )
    print(f"Model downloaded to: {model_dir}")
    
    # Check expected files
    for subdir, fname in [
        ("encoder_onnx", "model_quant.onnx"),
        ("decoder_onnx", "model_quant.onnx"),
    ]:
        path = os.path.join(model_dir, subdir, fname)
        if os.path.exists(path):
            size = os.path.getsize(path)
            print(f"  {subdir}/{fname}: {size} bytes")
        else:
            print(f"  WARNING: {subdir}/{fname} not found!")
    
    # Check am.mvn, tokens, config
    for fname in ["am.mvn", "config.yaml", "tokens.json", "token.txt"]:
        path = os.path.join(model_dir, fname)
        if os.path.exists(path):
            size = os.path.getsize(path)
            print(f"  {fname}: {size} bytes")
        else:
            # Check in subdirs
            for sub in ["encoder_onnx", "decoder_onnx", "am"]:
                path2 = os.path.join(model_dir, sub, fname)
                if os.path.exists(path2):
                    size = os.path.getsize(path2)
                    print(f"  {sub}/{fname}: {size} bytes")
                    break
            else:
                print(f"  WARNING: {fname} not found!")
except Exception as e:
    print(f"ERROR downloading model: {e}")
    sys.exit(1)

# 2. Download test WAV if not present
wav_path = os.path.join(MODELS_DIR, "asr_example.wav")
if os.path.exists(wav_path):
    print(f"\nWAV already exists: {os.path.getsize(wav_path)} bytes")
else:
    print("\n=== Downloading test WAV file ===")
    import urllib.request
    import ssl
    # Try multiple URLs
    urls = [
        "https://github.com/k2-fsa/sherpa-onnx/raw/refs/heads/master/python/testspeech/asr_example.wav",
        "https://huggingface.co/wenet/wetoday/resolve/main/asr_example.wav",
    ]
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    for url in urls:
        try:
            print(f"  Trying: {url}")
            req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
            with urllib.request.urlopen(req, timeout=30, context=ctx) as resp:
                data = resp.read()
                with open(wav_path, "wb") as f:
                    f.write(data)
                print(f"  Downloaded: {len(data)} bytes")
                break
        except Exception as e:
            print(f"  Failed: {e}")
    else:
        print("  WARNING: Could not download WAV file from any source")
        print("  Will generate a synthetic WAV for testing")
        # Generate synthetic WAV
        import struct
        import math
        sample_rate = 16000
        duration = 10.0  # 10 seconds
        num_samples = int(sample_rate * duration)
        # Create a simple sine wave
        samples = []
        for i in range(num_samples):
            t = i / sample_rate
            # Mix of tones
            val = 0.3 * math.sin(2 * math.pi * 440 * t) + 0.1 * math.sin(2 * math.pi * 880 * t)
            samples.append(int(val * 32767))
        with open(wav_path, "wb") as f:
            # WAV header
            nchannels = 1
            bits_per_sample = 16
            byte_rate = sample_rate * nchannels * bits_per_sample // 8
            block_align = nchannels * bits_per_sample // 8
            data_size = num_samples * 2
            f.write(b"RIFF")
            f.write(struct.pack("<I", 36 + data_size))
            f.write(b"WAVE")
            f.write(b"fmt ")
            f.write(struct.pack("<IHHIIHH", 16, 1, nchannels, sample_rate, byte_rate, block_align, bits_per_sample))
            f.write(b"data")
            f.write(struct.pack("<I", data_size))
            for s in samples:
                f.write(struct.pack("<h", s))
        print(f"  Generated synthetic WAV: {os.path.getsize(wav_path)} bytes")

print("\n=== Asset preparation complete ===")
print(f"Models dir: {MODELS_DIR}")
