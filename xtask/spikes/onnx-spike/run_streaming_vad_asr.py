#!/usr/bin/env python3
"""ONNX 流式 VAD+ASR spike — 完整端到端验证。

验证：
1. FSMN-VAD 离线 VAD（整段切分）
2. FSMN-VAD online 流式 VAD（chunk-by-chunk，cache 传递）
3. Paraformer ASR（VAD 切分后逐段识别）
4. 测量延迟、内存、识别效果
5. 测试 CUDA/DirectML GPU 加速
6. 说明 ort crate load-dynamic 特性
"""

import os
import sys
import time
import wave
import json
import psutil
import numpy as np
import onnxruntime as ort

sys.stderr.reconfigure(encoding="utf-8", errors="replace")
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

import warnings
warnings.filterwarnings("ignore", category=SyntaxWarning)

SPIKE_DIR = os.path.dirname(os.path.abspath(__file__))
MODELS_DIR = os.path.join(SPIKE_DIR, "models")
VAD_DIR = os.path.join(MODELS_DIR, "fsmn-vad-onnx-v2")
WAV_PATH = os.path.join(MODELS_DIR, "fsmn-vad-onnx", "asr_example.wav")
PARAFORMER_DIR = os.path.join(MODELS_DIR, "paraformer-zh-onnx")


def load_wav_16k_mono(path):
    with wave.open(path, "rb") as wf:
        assert wf.getframerate() == 16000
        assert wf.getnchannels() == 1
        raw = wf.readframes(wf.getnframes())
        return np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0


def get_mem_mb():
    return psutil.Process().memory_info().rss / 1024 / 1024


def test_offline_vad(wav_path):
    """测试离线 FSMN-VAD。"""
    print("\n" + "=" * 60)
    print("1. FSMN-VAD 离线 VAD（整段切分）")
    print("=" * 60)

    from funasr_onnx import Fsmn_vad

    mem_before = get_mem_mb()
    t0 = time.time()
    vad_model = Fsmn_vad(VAD_DIR, quantize=True)
    t_load = time.time() - t0
    mem_after = get_mem_mb()
    print(f"  模型加载: {t_load:.3f}s")
    print(f"  内存: {mem_before:.1f}MB -> {mem_after:.1f}MB (Δ{mem_after - mem_before:.1f}MB)")

    t0 = time.time()
    result = vad_model(wav_path)
    t_infer = time.time() - t0

    # result: [[[start_ms, end_ms], ...]]
    segments = []
    for batch in result:
        if isinstance(batch, list):
            for seg in batch:
                s_ms, e_ms = float(seg[0]), float(seg[1])
                segments.append((s_ms / 1000.0, e_ms / 1000.0))

    print(f"  推理耗时: {t_infer:.3f}s")
    print(f"  检测到 {len(segments)} 个语音段:")
    for i, (s, e) in enumerate(segments):
        print(f"    段 {i+1}: {s:.2f}s - {e:.2f}s (时长 {e-s:.2f}s)")

    print(f"  峰值内存: {get_mem_mb():.1f}MB")
    return segments


def test_streaming_vad(audio, sample_rate=16000):
    """测试流式 FSMN-VAD online。"""
    print("\n" + "=" * 60)
    print("2. FSMN-VAD Online 流式 VAD（chunk-by-chunk）")
    print("=" * 60)

    from funasr_onnx import Fsmn_vad_online

    mem_before = get_mem_mb()
    t0 = time.time()
    vad_model = Fsmn_vad_online(VAD_DIR, quantize=True)
    t_load = time.time() - t0
    mem_after = get_mem_mb()
    print(f"  模型加载: {t_load:.3f}s")
    print(f"  内存: {mem_before:.1f}MB -> {mem_after:.1f}MB (Δ{mem_after - mem_before:.1f}MB)")

    chunk_size = 1600  # 100ms
    audio_length = len(audio)
    param_dict = {"in_cache": []}

    all_segments = []
    t_infer_start = time.time()
    n_chunks = 0
    pending_start = None  # 待匹配的语音开始时间

    for offset in range(0, audio_length, chunk_size):
        end = min(offset + chunk_size, audio_length)
        is_final = (end >= audio_length - 1)
        chunk = audio[offset:end]

        result = vad_model(audio_in=chunk, param_dict=param_dict, is_final=is_final)
        if result:
            for batch in result:
                if isinstance(batch, list):
                    for seg in batch:
                        start_ms = float(seg[0])
                        end_ms = float(seg[1])
                        if start_ms >= 0 and end_ms >= 0:
                            # 完整段
                            s_sec = start_ms / 1000.0
                            e_sec = end_ms / 1000.0
                            all_segments.append((s_sec, e_sec))
                            print(f"  chunk {n_chunks}: VAD 段 {s_sec:.2f}s - {e_sec:.2f}s")
                        elif start_ms >= 0 and end_ms < 0:
                            # 语音开始，end=-1 表示未结束
                            s_sec = start_ms / 1000.0
                            pending_start = s_sec
                            print(f"  chunk {n_chunks}: VAD 语音开始 {s_sec:.2f}s (pending)")
        n_chunks += 1

    # 如果有 pending_start 没匹配到 end，用音频结尾补上
    if pending_start is not None:
        audio_dur = audio_length / sample_rate
        all_segments.append((pending_start, audio_dur))
        print(f"  [final] 补全段: {pending_start:.2f}s - {audio_dur:.2f}s")

    t_infer_total = time.time() - t_infer_start
    audio_dur = audio_length / sample_rate

    print(f"\n  音频时长: {audio_dur:.2f}s")
    print(f"  VAD 推理总耗时: {t_infer_total:.3f}s")
    print(f"  实时率 (RTF): {t_infer_total / audio_dur:.4f}x")
    print(f"  检测到 {len(all_segments)} 个语音段")
    for i, (s, e) in enumerate(all_segments):
        print(f"    段 {i+1}: {s:.2f}s - {e:.2f}s (时长 {e-s:.2f}s)")

    print(f"  峰值内存: {get_mem_mb():.1f}MB")
    return all_segments


def test_paraformer(wav_path, segments, audio):
    """测试 Paraformer ASR。"""
    print("\n" + "=" * 60)
    print("3. Paraformer ASR（VAD 切分后逐段识别）")
    print("=" * 60)

    from funasr_onnx import Paraformer

    mem_before = get_mem_mb()
    t0 = time.time()
    asr_model = Paraformer(PARAFORMER_DIR, quantize=True)
    t_load = time.time() - t0
    mem_after = get_mem_mb()
    print(f"  模型加载: {t_load:.3f}s")
    print(f"  内存: {mem_before:.1f}MB -> {mem_after:.1f}MB (Δ{mem_after - mem_before:.1f}MB)")

    results = []
    t_infer_start = time.time()

    for i, (s, e) in enumerate(segments):
        start_sample = int(s * 16000)
        end_sample = int(e * 16000)
        chunk = audio[start_sample:end_sample]

        t_seg_start = time.time()
        result = asr_model(wav_content=chunk)
        t_seg = time.time() - t_seg_start

        text = result[0]["preds"][0] if result and isinstance(result[0].get("preds"), tuple) else ""
        if not text and result:
            text = str(result[0].get("preds", ""))
        print(f"  段 {i+1} ({s:.2f}-{e:.2f}s, {t_seg*1000:.0f}ms): {text}")
        results.append(text)

    t_infer_total = time.time() - t_infer_start
    full_text = "".join(results)
    print(f"\n  ASR 推理总耗时: {t_infer_total:.3f}s")
    print(f"  完整文本: {full_text}")
    print(f"  峰值内存: {get_mem_mb():.1f}MB")

    # 也测试整段直接识别
    print("\n  [对比] 整段直接识别:")
    t0 = time.time()
    result = asr_model(wav_content=audio)
    t_inf = time.time() - t0
    text = result[0]["preds"][0] if result and isinstance(result[0].get("preds"), tuple) else ""
    print(f"  整段 ({t_inf*1000:.0f}ms): {text}")

    return full_text


def test_cuda_support():
    """测试 ONNX Runtime CUDA 支持。"""
    print("\n" + "=" * 60)
    print("4. ONNX Runtime GPU 支持测试")
    print("=" * 60)

    print(f"  onnxruntime 版本: {ort.__version__}")
    available_providers = ort.get_available_providers()
    print(f"  可用 Providers: {available_providers}")

    # CPU (当前)
    if "CPUExecutionProvider" in available_providers:
        print("  ✅ CPUExecutionProvider 可用（当前使用）")

    # CUDA
    if "CUDAExecutionProvider" in available_providers:
        print("  ✅ CUDAExecutionProvider 可用！")
    else:
        print("  ❌ CUDAExecutionProvider 不可用")
        print("     → 需要 onnxruntime-gpu (pip install onnxruntime-gpu)")
        print("     → 或 Rust ort crate + cuda feature + GPU 版 onnxruntime.dll")

    # DirectML (Windows GPU)
    if "DmlExecutionProvider" in available_providers:
        print("  ✅ DmlExecutionProvider 可用！")
    else:
        print("  ℹ️ DmlExecutionProvider 不可用")
        print("     → 需要 onnxruntime-directml (pip install onnxruntime-directml)")
        print("     → 或 Rust ort crate + directml feature")

    # 说明 ort crate 支持的 EP
    print("\n  ort crate (Rust) 支持的 Execution Providers:")
    print("    ✅ cuda        — NVIDIA CUDA (Maxwell 7xx+)")
    print("    ✅ tensorrt   — NVIDIA TensorRT")
    print("    ✅ directml   — Windows DirectX 12 GPU")
    print("    ✅ openvino   — Intel")
    print("    ✅ onednn     — Intel oneDNN")
    print("    ⚠️  rocm       — AMD ROCm")
    print("    ⚠️  coreml     — macOS/iOS")
    print("    ⚠️  nnapi      — Android Neural Networks API")

    # 实测 CUDA/DML 推理
    vad_path = os.path.join(VAD_DIR, "model_quant.onnx")
    for ep_name, ep_config in [
        ("CUDAExecutionProvider", ("CUDAExecutionProvider", {"device_id": 0})),
        ("DmlExecutionProvider", ("DmlExecutionProvider", {"device_id": 0})),
    ]:
        if ep_name in available_providers:
            try:
                sess = ort.InferenceSession(
                    vad_path,
                    providers=[ep_config, "CPUExecutionProvider"],
                )
                actual = sess.get_providers()
                if ep_name in actual:
                    print(f"\n  ✅ {ep_name} 推理成功启用！")
                else:
                    print(f"\n  ⚠️ {ep_name} 请求了但回退到 CPU")
            except Exception as e:
                print(f"\n  ❌ {ep_name} 推理失败: {e}")

    return available_providers


def test_load_dynamic_feature():
    """说明 ort crate 的 load-dynamic 特性。"""
    print("\n" + "=" * 60)
    print("5. onnxruntime.dll 分发方式说明")
    print("=" * 60)

    # 找到 onnxruntime.dll
    capi_dir = os.path.join(os.path.dirname(ort.__file__), "capi")
    dll_path = os.path.join(capi_dir, "onnxruntime.dll")
    if os.path.exists(dll_path):
        dll_size = os.path.getsize(dll_path) / 1024 / 1024
        print(f"  onnxruntime.dll 路径: {dll_path}")
        print(f"  onnxruntime.dll 大小: {dll_size:.1f}MB")

    # 列出 capi 目录中的所有 DLL
    if os.path.exists(capi_dir):
        dlls = [f for f in os.listdir(capi_dir) if f.endswith(".dll")]
        if dlls:
            print(f"\n  capi 目录中的 DLL:")
            for f in dlls:
                fpath = os.path.join(capi_dir, f)
                fsize = os.path.getsize(fpath) / 1024 / 1024
                print(f"    {f}: {fsize:.1f}MB")

    print("\n  ── ort crate (Rust) 三种获取 DLL 的方式 ──")
    print()
    print("  1. download 策略（默认）:")
    print("     → 编译时自动下载预编译 DLL 到 target/ 旁")
    print("     → 不进 exe，但发布时需要手动捆绑 DLL")
    print()
    print("  2. system 策略:")
    print("     → 链接 ORT_LIB_LOCATION 指定的系统 DLL")
    print("     → 适合开发者自带 DLL 的场景")
    print()
    print("  3. load-dynamic feature（★ 推荐 Blink 使用）:")
    print("     → 无编译期链接依赖")
    print("     → 运行时通过 LoadLibrary/dlopen 加载")
    print("     → 路径由 ORT_DYLIB_PATH 环境变量控制")
    print("     → 完全可以运行时下载 DLL，放到指定路径")
    print("     → onnxruntime.dll 不进 exe，不膨胀安装包")
    print()
    print("  ── Blink 可行方案 ──")
    print("  → Cargo.toml: ort = { version = \"2\", features = [\"load-dynamic\"] }")
    print("  → 运行时: 设置 ORT_DYLIB_PATH 指向已下载的 onnxruntime.dll")
    print("  → onnxruntime.dll 作为 ManagedBinary artifact 运行时下载")
    print("  → 安装包不膨胀: exe 保持十几 MB，DLL 按需下载 17.3MB")
    print("  → GPU 版: 下载 onnxruntime-gpu DLL (更大) 替换 CPU 版")


def main():
    print("=" * 60)
    print("ONNX 流式 VAD+ASR Spike 完整测试")
    print("=" * 60)

    audio = load_wav_16k_mono(WAV_PATH)
    audio_dur = len(audio) / 16000
    print(f"音频: {WAV_PATH}")
    print(f"  时长: {audio_dur:.2f}s, 采样数: {len(audio)}")
    print(f"  初始内存: {get_mem_mb():.1f}MB")

    # 1. 离线 VAD（正确切分）
    offline_segments = test_offline_vad(WAV_PATH)

    # 2. 流式 VAD
    streaming_segments = test_streaming_vad(audio)

    # 3. Paraformer ASR（用离线 VAD 的切分结果）
    paraformer_text = test_paraformer(WAV_PATH, offline_segments, audio)

    # 4. CUDA 支持
    providers = test_cuda_support()

    # 5. load-dynamic 说明
    test_load_dynamic_feature()

    # 总结
    print("\n" + "=" * 60)
    print("总结")
    print("=" * 60)
    print(f"  音频时长: {audio_dur:.2f}s")
    print(f"  离线 VAD: {len(offline_segments)} 段, {offline_segments}")
    print(f"  流式 VAD: {len(streaming_segments)} 段, {streaming_segments}")
    if paraformer_text:
        print(f"  Paraformer 识别: {paraformer_text}")
    print(f"  可用 Providers: {providers}")
    print(f"  最终内存: {get_mem_mb():.1f}MB")


if __name__ == "__main__":
    main()
