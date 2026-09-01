#!/usr/bin/env python3
"""Copy downloaded model files to expected paths for Spike E."""
import os
import shutil

MODELS_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "models")

# Source: downloaded model files
src_dir = os.path.join(
    MODELS_DIR,
    "test_cache",
    "models",
    "iic--speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx",
    "snapshots",
    "master",
)

# Destination: paraformer-online-onnx directory
dst_dir = os.path.join(MODELS_DIR, "paraformer-online-onnx")
os.makedirs(dst_dir, exist_ok=True)

# Mapping: downloaded filename -> expected filename
file_mapping = {
    "model_quant.onnx": "encoder.onnx",
    "decoder_quant.onnx": "decoder.onnx",
    "am.mvn": "am.mvn",
    "tokens.json": "tokens.json",
    "config.yaml": "config.yaml",
}

print("=== Copying model files to paraformer-online-onnx/ ===")
for src_name, dst_name in file_mapping.items():
    src_path = os.path.join(src_dir, src_name)
    dst_path = os.path.join(dst_dir, dst_name)
    if os.path.exists(src_path):
        size = os.path.getsize(src_path)
        shutil.copy2(src_path, dst_path)
        print(f"  {src_name} ({size:,} bytes) -> {dst_name}")
    else:
        print(f"  WARNING: {src_name} not found at {src_path}")

# Verify
print("\n=== Verification ===")
for fname in ["encoder.onnx", "decoder.onnx", "am.mvn", "tokens.json"]:
    path = os.path.join(dst_dir, fname)
    if os.path.exists(path):
        print(f"  {fname}: {os.path.getsize(path):,} bytes OK")
    else:
        print(f"  {fname}: MISSING!")

# Download WAV if not present
wav_path = os.path.join(MODELS_DIR, "asr_example.wav")
if os.path.exists(wav_path):
    print(f"\n  asr_example.wav: {os.path.getsize(wav_path):,} bytes OK")
else:
    print("\n=== Downloading test WAV file ===")
    import urllib.request
    import ssl
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    
    urls = [
        "https://github.com/k2-fsa/sherpa-onnx/raw/refs/heads/master/python/testspeech/asr_example.wav",
        "https://huggingface.co/wenet/wetoday/resolve/main/test_wavs/asr_example.wav",
        "https://huggingface.co/TODO/resolve/main/asr_example.wav",
    ]
    for url in urls:
        try:
            print(f"  Trying: {url}")
            req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
            with urllib.request.urlopen(req, timeout=30, context=ctx) as resp:
                data = resp.read()
                if len(data) > 1000 and data[:4] == b"RIFF":
                    with open(wav_path, "wb") as f:
                        f.write(data)
                    print(f"  Downloaded: {len(data):,} bytes")
                    break
                else:
                    print(f"  Not a WAV file (size={len(data)}, header={data[:4]})")
        except Exception as e:
            print(f"  Failed: {e}")
    else:
        print("  Generating synthetic WAV for topology test...")
        import struct
        import math
        sample_rate = 16000
        duration = 10.0
        num_samples = int(sample_rate * duration)
        samples = []
        for i in range(num_samples):
            t = i / sample_rate
            val = 0.3 * math.sin(2 * math.pi * 440 * t) + 0.1 * math.sin(2 * math.pi * 880 * t)
            samples.append(max(-32768, min(32767, int(val * 32767))))
        with open(wav_path, "wb") as f:
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
        print(f"  Generated: {os.path.getsize(wav_path):,} bytes")

print("\n=== ORT DLL check ===")
ort_dll = os.path.join(os.path.dirname(os.path.abspath(__file__)), "runtimes", "onnxruntime-cpu", "onnxruntime.dll")
if os.path.exists(ort_dll):
    print(f"  onnxruntime.dll: {os.path.getsize(ort_dll):,} bytes OK")
else:
    print(f"  onnxruntime.dll: MISSING at {ort_dll}")

print("\n=== All assets ready! ===")
