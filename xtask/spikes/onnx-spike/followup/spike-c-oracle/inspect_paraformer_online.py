#!/usr/bin/env python3
"""Spike C1 (修正): ParaformerOnline Oracle — 检查可用性

关键发现:
- funasr-onnx v0.4.2 没有 Paraformer_online 类
- funasr AutoModel (paraformer-zh-streaming) 需要 PyTorch
- 上游 FunASR ONNX runtime C++ 有 paraformer-online.cpp, 但 Python 包未暴露

本脚本:
1. 检查 funasr-onnx 可用 API
2. 如果有 ParaformerOnline 模型, 直接用 onnxruntime inspect I/O
3. 记录阻塞点
"""

import os
import sys
import json

sys.stderr.reconfigure(encoding="utf-8", errors="replace")
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

SPIKE_DIR = os.path.dirname(os.path.abspath(__file__))
ONNX_SPIKE_DIR = os.path.join(os.path.dirname(SPIKE_DIR), "..")
MODELS_DIR = os.path.join(ONNX_SPIKE_DIR, "models")

# 检查已有模型
PARAFORMER_OFFLINE_DIR = os.path.join(MODELS_DIR, "paraformer-zh-onnx")
PARAFORMER_ONLINE_DIR = os.path.join(MODELS_DIR, "paraformer-online-onnx")
VAD_DIR = os.path.join(MODELS_DIR, "fsmn-vad-onnx-v2")


def check_funasr_onnx_api():
    """检查 funasr-onnx 包提供的 API。"""
    print("=" * 60)
    print("Spike C1: funasr-onnx API 检查")
    print("=" * 60)

    try:
        import funasr_onnx
        print(f"  funasr-onnx version: {getattr(funasr_onnx, '__version__', 'unknown')}")
        print(f"  Available classes: {[x for x in dir(funasr_onnx) if not x.startswith('_')]}")

        # 检查是否有 Paraformer_online
        has_paraformer_online = hasattr(funasr_onnx, 'Paraformer_online')
        has_paraformer = hasattr(funasr_onnx, 'Paraformer')
        has_fsmn_vad_online = hasattr(funasr_onnx, 'Fsmn_vad_online')

        print(f"\n  Paraformer (offline): {'✅' if has_paraformer else '❌'}")
        print(f"  Paraformer_online (streaming): {'✅' if has_paraformer_online else '❌'}")
        print(f"  Fsmn_vad_online: {'✅' if has_fsmn_vad_online else '❌'}")

        if not has_paraformer_online:
            print("\n  ❌ 关键发现: funasr-onnx 不支持 ParaformerOnline!")
            print("     上游 FunASR ONNX runtime C++ 有 paraformer-online.cpp")
            print("     但 Python 包 (funasr-onnx v0.4.2) 未暴露此功能")
            print("     funasr AutoModel (paraformer-zh-streaming) 需要 PyTorch")

        return {
            "version": getattr(funasr_onnx, '__version__', 'unknown'),
            "has_paraformer": has_paraformer,
            "has_paraformer_online": has_paraformer_online,
            "has_fsmn_vad_online": has_fsmn_vad_online,
            "available_classes": [x for x in dir(funasr_onnx) if not x.startswith('_')],
        }
    except ImportError as e:
        print(f"  funasr-onnx 不可用: {e}")
        return {"error": str(e)}


def inspect_onnx_model(model_path):
    """用 onnxruntime 直接 inspect ONNX 模型 I/O。"""
    if not os.path.exists(model_path):
        return {"exists": False, "path": model_path}

    try:
        import onnxruntime as ort
        sess = ort.InferenceSession(model_path, providers=["CPUExecutionProvider"])

        inputs = []
        for inp in sess.get_inputs():
            inputs.append({
                "name": inp.name,
                "shape": str(inp.shape),
                "type": str(inp.type),
            })

        outputs = []
        for out in sess.get_outputs():
            outputs.append({
                "name": out.name,
                "shape": str(out.shape),
                "type": str(out.type),
            })

        return {
            "exists": True,
            "path": model_path,
            "inputs": inputs,
            "outputs": outputs,
        }
    except Exception as e:
        return {"exists": True, "path": model_path, "error": str(e)}


def inspect_all_models():
    """inspect 所有已有 ONNX 模型的 I/O 结构。"""
    print("\n" + "=" * 60)
    print("Spike C1: ONNX 模型 I/O 检查")
    print("=" * 60)

    models = {}

    # VAD model
    vad_path = os.path.join(VAD_DIR, "model_quant.onnx")
    print(f"\n  VAD model: {vad_path}")
    models["vad"] = inspect_onnx_model(vad_path)
    if models["vad"].get("exists"):
        for inp in models["vad"].get("inputs", []):
            print(f"    Input: {inp['name']} shape={inp['shape']} type={inp['type']}")
        for out in models["vad"].get("outputs", []):
            print(f"    Output: {out['name']} shape={out['shape']} type={out['type']}")

    # Paraformer offline
    paraformer_path = os.path.join(PARAFORMER_OFFLINE_DIR, "model_quant.onnx")
    print(f"\n  Paraformer (offline): {paraformer_path}")
    models["paraformer_offline"] = inspect_onnx_model(paraformer_path)
    if models["paraformer_offline"].get("exists"):
        for inp in models["paraformer_offline"].get("inputs", []):
            print(f"    Input: {inp['name']} shape={inp['shape']} type={inp['type']}")
        for out in models["paraformer_offline"].get("outputs", []):
            print(f"    Output: {out['name']} shape={out['shape']} type={out['type']}")

    # Paraformer online (如果存在)
    for subdir in ["encoder_onnx", "decoder_onnx", ""]:
        for fname in ["model_quant.onnx", "encoder.onnx", "decoder.onnx"]:
            path = os.path.join(PARAFORMER_ONLINE_DIR, subdir, fname)
            if os.path.exists(path):
                key = f"paraformer_online_{subdir}_{fname}"
                print(f"\n  Paraformer (online): {path}")
                models[key] = inspect_onnx_model(path)
                if models[key].get("exists"):
                    for inp in models[key].get("inputs", []):
                        print(f"    Input: {inp['name']} shape={inp['shape']} type={inp['type']}")
                    for out in models[key].get("outputs", []):
                        print(f"    Output: {out['name']} shape={out['shape']} type={out['type']}")
                break

    return models


def analyze_blocking_points():
    """分析 ParaformerOnline 的阻塞点和剩余工作量。"""
    print("\n" + "=" * 60)
    print("Spike C: 阻塞点分析")
    print("=" * 60)

    blockers = [
        {
            "blocker": "funasr-onnx Python 包不支持 ParaformerOnline",
            "detail": "funasr-onnx v0.4.2 只有 Paraformer (offline), 没有 Paraformer_online",
            "impact": "无法用 Python 快速验证 oracle 流式推理",
            "workaround": "需要直接用 onnxruntime Python API 手动实现 chunk-by-chunk + cache",
            "estimated_work": "2-3 天 (参考上游 C++ paraformer-online.cpp)",
        },
        {
            "blocker": "ParaformerOnline ONNX 模型未下载",
            "detail": "需要从 ModelScope 下载 speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx",
            "impact": "无法 inspect 模型 I/O 和 cache 结构",
            "workaround": "下载模型后用 onnxruntime inspect",
            "estimated_work": "下载 + inspect: 30 分钟",
        },
        {
            "blocker": "Rust fbank/CMVN/LFR 前处理未实现",
            "detail": "上游 C++ compute_fbank 需要移植到 Rust",
            "impact": "无法在 Rust 端正确预处理音频",
            "workaround": "参考 GGUF worker 的 C++ 源码或 kaldi-native-fbank",
            "estimated_work": "2-3 天",
        },
        {
            "blocker": "Rust CIF cache + decoder FSMN cache 未实现",
            "detail": "ParaformerOnline 的核心是 CIF 跨 chunk 积分 + decoder cache",
            "impact": "无法在 Rust 端正确传递和更新 cache",
            "workaround": "参考上游 paraformer-online.cpp 的 CifSearch 和 ForwardChunk",
            "estimated_work": "3-5 天",
        },
    ]

    for b in blockers:
        print(f"\n  ❌ {b['blocker']}")
        print(f"     Detail: {b['detail']}")
        print(f"     Impact: {b['impact']}")
        print(f"     Workaround: {b['workaround']}")
        print(f"     Estimated: {b['estimated_work']}")

    return blockers


def main():
    print("=" * 60)
    print("Spike C1: ParaformerOnline Oracle (修正版)")
    print("=" * 60)

    api_check = check_funasr_onnx_api()
    model_inspection = inspect_all_models()
    blockers = analyze_blocking_points()

    summary = {
        "spike": "C1_paraformer_online_oracle",
        "api_check": api_check,
        "model_inspection": model_inspection,
        "blockers": blockers,
        "conclusion": "NO-GO (当前状态) — 需要下载模型并手动实现 Python oracle",
        "remaining_work": [
            "1. 下载 ParaformerOnline ONNX 模型 (encoder + decoder + am.mvn + config + tokens)",
            "2. 用 onnxruntime Python API 手动实现 chunk-by-chunk + cache 传递",
            "3. 参考 upstream paraformer-online.cpp 的 CifSearch/ForwardChunk",
            "4. 验证句尾前 partial transcript",
            "5. Rust ort 加载模型 + fbank/CMVN/LFR + cache",
        ],
    }

    results_dir = os.path.join(os.path.dirname(SPIKE_DIR), "results")
    os.makedirs(results_dir, exist_ok=True)
    output_path = os.path.join(results_dir, "spike_c1_paraformer_online.json")
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump(summary, f, ensure_ascii=False, indent=2)
    print(f"\n结果已保存到: {output_path}")


if __name__ == "__main__":
    main()
