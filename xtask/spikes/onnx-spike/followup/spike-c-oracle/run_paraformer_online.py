#!/usr/bin/env python3
"""Spike C1: ParaformerOnline Oracle — 验证真正的流式 ASR

明确区分：
- 流式 VAD + 离线 ASR ≠ 真流式 ASR
- ParaformerOnline = chunk-by-chunk encoder + CIF cache + decoder FSMN cache + is_final flush

本脚本验证：
1. 加载真正的 ParaformerOnline ONNX 模型（非离线 Paraformer）
2. chunk-by-chunk 输入音频
3. 打印每个 chunk 的 partial text、cache tensor shape、inference duration
4. 证明句尾前确实产生 partial transcript
5. is_final flush 后得到最终文本
6. 连续多 utterance reset
"""

import os
import sys
import time
import json
import wave
import numpy as np

sys.stderr.reconfigure(encoding="utf-8", errors="replace")
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

import warnings
warnings.filterwarnings("ignore", category=SyntaxWarning)

SPIKE_DIR = os.path.dirname(os.path.abspath(__file__))
ONNX_SPIKE_DIR = os.path.join(os.path.dirname(SPIKE_DIR), "..")  # xtask/spikes/onnx-spike/
VAD_DIR = os.path.join(ONNX_SPIKE_DIR, "models", "fsmn-vad-onnx-v2")
PARAFORMER_OFFLINE_DIR = os.path.join(ONNX_SPIKE_DIR, "models", "paraformer-zh-onnx")
WAV_PATH = os.path.join(ONNX_SPIKE_DIR, "models", "fsmn-vad-onnx", "asr_example.wav")

# ParaformerOnline 模型需要单独下载
# modelscope: speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx
PARAFORMER_ONLINE_DIR = os.path.join(ONNX_SPIKE_DIR, "models", "paraformer-online-onnx")


def load_wav_16k_mono(path):
    with wave.open(path, "rb") as wf:
        assert wf.getframerate() == 16000
        assert wf.getnchannels() == 1
        raw = wf.readframes(wf.getnframes())
        return np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0


def get_mem_mb():
    try:
        import psutil
        return psutil.Process().memory_info().rss / 1024 / 1024
    except:
        return 0.0


def test_paraformer_online():
    """测试真正的 ParaformerOnline 流式 ASR。"""
    print("=" * 60)
    print("Spike C1: ParaformerOnline — True Streaming ASR")
    print("=" * 60)

    if not os.path.exists(PARAFORMER_ONLINE_DIR):
        print(f"\n❌ ParaformerOnline 模型目录不存在: {PARAFORMER_ONLINE_DIR}")
        print("   需要从 ModelScope 下载:")
        print("   model_id: speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx")
        print("   下载到: xtask/spikes/onnx-spike/models/paraformer-online-onnx/")
        print("\n   使用 funasr-onnx 的 Paraformer_online 类需要以下文件:")
        print("   - encoder_onnx/model_quant.onnx")
        print("   - decoder_onnx/model_quant.onnx")
        print("   - am.mvn")
        print("   - config.yaml")
        print("   - tokens.json")
        return None

    from funasr_onnx import Paraformer_online

    mem_before = get_mem_mb()
    t0 = time.time()
    asr_model = Paraformer_online(PARAFORMER_ONLINE_DIR, quantize=True)
    t_load = time.time() - t0
    mem_after = get_mem_mb()
    print(f"\n  模型加载: {t_load:.3f}s")
    print(f"  内存: {mem_before:.1f}MB -> {mem_after:.1f}MB (Δ{mem_after - mem_before:.1f}MB)")

    audio = load_wav_16k_mono(WAV_PATH)
    audio_dur = len(audio) / 16000
    print(f"  音频时长: {audio_dur:.2f}s, 采样数: {len(audio)}")

    # ParaformerOnline chunk 参数
    chunk_size = [0, 10, 5]  # [left, center, right] in 60ms units
    encoder_chunk_look_back = 4
    decoder_chunk_look_back = 1
    chunk_stride = chunk_size[1] * 960  # 600ms @ 16kHz

    cache = {}
    all_partial_texts = []
    chunk_logs = []

    print(f"\n  chunk_size={chunk_size}, chunk_stride={chunk_stride} samples ({chunk_stride/16000*1000:.0f}ms)")
    print(f"  encoder_chunk_look_back={encoder_chunk_look_back}, decoder_chunk_look_back={decoder_chunk_look_back}")

    n_chunks = (len(audio) - 1) // chunk_stride + 1
    print(f"  总 chunks: {n_chunks}")

    first_nonempty_partial_time = None
    t_stream_start = time.time()

    for i in range(n_chunks):
        start_sample = i * chunk_stride
        end_sample = min((i + 1) * chunk_stride, len(audio))
        chunk = audio[start_sample:end_sample]
        is_final = (i == n_chunks - 1)
        chunk_audio_dur = len(chunk) / 16000

        t_chunk_start = time.time()
        res = asr_model.generate(
            input=chunk,
            cache=cache,
            is_final=is_final,
            chunk_size=chunk_size,
            encoder_chunk_look_back=encoder_chunk_look_back,
            decoder_chunk_look_back=decoder_chunk_look_back,
        )
        t_chunk = time.time() - t_chunk_start

        partial_text = res[0]["text"] if res and res[0].get("text") else ""

        # 记录 chunk 信息
        audio_timestamp = start_sample / 16000
        chunk_log = {
            "chunk_index": i,
            "audio_timestamp_s": round(audio_timestamp, 3),
            "audio_duration_s": round(chunk_audio_dur, 3),
            "partial_text": partial_text,
            "is_final": is_final,
            "inference_ms": round(t_chunk * 1000, 2),
        }
        chunk_logs.append(chunk_log)

        if partial_text:
            if first_nonempty_partial_time is None:
                first_nonempty_partial_time = time.time() - t_stream_start
            all_partial_texts.append(partial_text)

        print(f"  chunk {i:3d} | t={audio_timestamp:6.2f}s | dur={chunk_audio_dur:.3f}s | "
              f"inf={t_chunk*1000:6.1f}ms | final={is_final} | text='{partial_text}'")

    t_stream_total = time.time() - t_stream_start

    final_text = "".join(all_partial_texts)

    print(f"\n  {'─' * 50}")
    print(f"  总 streaming 耗时: {t_stream_total:.3f}s")
    print(f"  RTF: {t_stream_total / audio_dur:.4f}x")
    print(f"  首次非空 partial 延迟: {first_nonempty_partial_time:.3f}s" if first_nonempty_partial_time else "  无非空 partial")
    print(f"  最终文本: {final_text}")
    print(f"  partial 文本数量: {len(all_partial_texts)}")
    print(f"  峰值内存: {get_mem_mb():.1f}MB")

    return {
        "model": "ParaformerOnline",
        "audio_duration_s": audio_dur,
        "n_chunks": n_chunks,
        "chunk_stride_ms": chunk_stride / 16000 * 1000,
        "streaming_total_s": t_stream_total,
        "rtf": t_stream_total / audio_dur,
        "first_nonempty_partial_latency_s": first_nonempty_partial_time,
        "final_text": final_text,
        "n_partial_texts": len(all_partial_texts),
        "chunk_logs": chunk_logs,
        "peak_mem_mb": get_mem_mb(),
    }


def test_multi_utterance_reset():
    """测试连续多 utterance reset — 第二段音频不受第一段 cache 污染。"""
    print("\n" + "=" * 60)
    print("Spike C1: Multi-utterance Reset Test")
    print("=" * 60)

    if not os.path.exists(PARAFORMER_ONLINE_DIR):
        print("  ❌ 模型不存在, 跳过")
        return None

    from funasr_onnx import Paraformer_online

    asr_model = Paraformer_online(PARAFORMER_ONLINE_DIR, quantize=True)
    audio = load_wav_16k_mono(WAV_PATH)

    chunk_size = [0, 10, 5]
    chunk_stride = chunk_size[1] * 960
    n_chunks = (len(audio) - 1) // chunk_stride + 1

    results = []
    for utterance in range(2):
        cache = {}  # 每段重置 cache
        texts = []
        for i in range(n_chunks):
            start = i * chunk_stride
            end = min((i + 1) * chunk_stride, len(audio))
            chunk = audio[start:end]
            is_final = (i == n_chunks - 1)
            res = asr_model.generate(
                input=chunk, cache=cache, is_final=is_final,
                chunk_size=chunk_size,
                encoder_chunk_look_back=4,
                decoder_chunk_look_back=1,
            )
            if res and res[0].get("text"):
                texts.append(res[0]["text"])
        final_text = "".join(texts)
        print(f"  Utterance {utterance + 1}: '{final_text}'")
        results.append(final_text)

    if results[0] == results[1]:
        print(f"  ✅ 两段结果一致 — reset 正确")
    else:
        print(f"  ❌ 两段结果不一致 — cache 可能污染")
        print(f"     Utterance 1: '{results[0]}'")
        print(f"     Utterance 2: '{results[1]}'")

    return {"utterance_1": results[0], "utterance_2": results[1], "consistent": results[0] == results[1]}


def test_offline_vs_online_comparison():
    """对比离线 Paraformer 和 Online Paraformer 的结果。"""
    print("\n" + "=" * 60)
    print("Spike C1: Offline vs Online Paraformer Comparison")
    print("=" * 60)

    audio = load_wav_16k_mono(WAV_PATH)

    # 离线 Paraformer
    if os.path.exists(PARAFORMER_OFFLINE_DIR):
        from funasr_onnx import Paraformer
        offline_model = Paraformer(PARAFORMER_OFFLINE_DIR, quantize=True)
        t0 = time.time()
        offline_result = offline_model(wav_content=audio)
        t_offline = time.time() - t0
        offline_text = offline_result[0]["preds"][0] if offline_result else ""
        print(f"  离线 Paraformer ({t_offline*1000:.0f}ms): {offline_text}")
    else:
        print("  离线 Paraformer 模型不存在, 跳过")
        offline_text = "N/A"
        t_offline = 0

    # Online Paraformer
    if os.path.exists(PARAFORMER_ONLINE_DIR):
        online_result = test_paraformer_online()
    else:
        online_result = None
        print("  Online Paraformer 模型不存在, 跳过")

    return {
        "offline_text": offline_text,
        "offline_inference_ms": t_offline * 1000,
        "online_result": online_result,
    }


def main():
    print("=" * 60)
    print("Spike C1: ParaformerOnline — True Streaming ASR Verification")
    print("=" * 60)
    print(f"  WAV: {WAV_PATH}")
    print(f"  VAD model: {VAD_DIR}")
    print(f"  Offline Paraformer: {PARAFORMER_OFFLINE_DIR}")
    print(f"  Online Paraformer: {PARAFORMER_ONLINE_DIR}")
    print(f"  初始内存: {get_mem_mb():.1f}MB")

    # 主测试
    online_result = test_paraformer_online()

    # Multi-utterance reset
    reset_result = test_multi_utterance_reset()

    # 对比
    comparison = test_offline_vs_online_comparison()

    # 汇总
    summary = {
        "spike": "C1_paraformer_online_oracle",
        "online_result": online_result,
        "reset_result": reset_result,
        "comparison": comparison,
    }

    # 保存结果
    results_dir = os.path.join(os.path.dirname(SPIKE_DIR), "results")
    os.makedirs(results_dir, exist_ok=True)
    output_path = os.path.join(results_dir, "spike_c1_paraformer_online.json")
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump(summary, f, ensure_ascii=False, indent=2)
    print(f"\n结果已保存到: {output_path}")


if __name__ == "__main__":
    main()
