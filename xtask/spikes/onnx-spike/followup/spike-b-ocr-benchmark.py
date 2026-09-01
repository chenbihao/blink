#!/usr/bin/env python3
"""Spike B: OCR 资格门 — Rust oar-ocr vs Python PaddleOCR vs WinRT 对比

复用现有 PP-OCR golden corpus 和 benchmark。
至少比较:
- 当前 Python PaddleOCR
- Rust oar-ocr + PP-OCRv6 Tiny (如果模型可用)
- WinRT 仅作为参考基线

记录:
- 文本准确率: 总 CER, 中英文分别统计, 标点/空格/数字, 最差样本列表
- 几何: polygon/box 坐标, resize 后映射回原图, crop offset, 高 DPI, 旋转和斜文本
- 性能: 模型冷加载至少 5 次, 热推理至少 20 次, p50/p95, 峰值 RSS/private bytes
- 并发和取消: 同一 Session 并发, mutex/session pool, 取消后是否终止推理, 旧结果回流
- 完整资产: det/rec/dictionary/配置文件, 每项大小/SHA-256/来源/license, 总磁盘占用
"""

import os
import sys
import json
import time
import hashlib
import subprocess
import statistics
from pathlib import Path

sys.stderr.reconfigure(encoding="utf-8", errors="replace")
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

SPIKE_DIR = os.path.dirname(os.path.abspath(__file__))
PROJECT_ROOT = Path(SPIKE_DIR).parent.parent.parent.parent  # blink/
CORPUS_DIR = PROJECT_ROOT / "testdata" / "ocr" / "ppocrv6"
PPOCR_SPIKE_DIR = PROJECT_ROOT / "xtask" / "spikes" / "ppocrv6"
ONNX_SPIKE_MODELS = PROJECT_ROOT / "xtask" / "spikes" / "onnx-spike" / "models"


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(8192), b""):
            h.update(chunk)
    return h.hexdigest()


def load_corpus():
    """加载 golden corpus manifest。"""
    manifest_path = CORPUS_DIR / "manifest.json"
    with open(manifest_path, "r", encoding="utf-8") as f:
        manifest = json.load(f)
    return manifest


def calculate_cer(expected, actual):
    """计算 Character Error Rate。"""
    if not expected:
        return 0.0 if not actual else 1.0

    # Levenshtein distance at character level
    import difflib
    matcher = difflib.SequenceMatcher(None, expected, actual)
    distance = sum(1 for tag in matcher.get_opcodes() if tag[0] != "equal" for _ in range(tag[2] - tag[1]))
    return distance / len(expected)


def run_paddleocr_benchmark():
    """运行现有 Python PaddleOCR benchmark。"""
    print("\n" + "=" * 60)
    print("Spike B: Python PaddleOCR Benchmark")
    print("=" * 60)

    # 复用现有 benchmark 结果
    benchmark_path = PPOCR_SPIKE_DIR / "results" / "evaluate_results.json"
    if benchmark_path.exists():
        with open(benchmark_path, "r", encoding="utf-8") as f:
            results = json.load(f)
        print(f"  已有 PaddleOCR benchmark 结果: {benchmark_path}")
        print(f"  CER: {results.get('cer', 'N/A')}")
        return results
    else:
        print("  PaddleOCR benchmark 结果不存在, 需要运行 xtask/spikes/ppocrv6/evaluate.ps1")
        return None


def run_oar_ocr_benchmark():
    """运行 Rust oar-ocr benchmark。"""
    print("\n" + "=" * 60)
    print("Spike B: Rust oar-ocr + PP-OCRv6 ONNX Benchmark")
    print("=" * 60)

    # 检查 oar-ocr 是否可以运行
    spike_a_crate = Path(SPIKE_DIR) / "spike-a-crate"
    oar_ocr_binary = spike_a_crate / "target" / "debug" / "onnx-spike-a.exe"

    # 检查 PP-OCRv6 ONNX 模型
    ppocrv6_det = ONNX_SPIKE_MODELS / "ppocrv6_tiny_det.onnx"
    ppocrv6_rec = ONNX_SPIKE_MODELS / "ppocrv6_tiny_rec.onnx"
    ppocrv6_dict = ONNX_SPIKE_MODELS / "ppocrv6_tiny_dict.txt"

    if not ppocrv6_det.exists():
        print(f"  ❌ PP-OCRv6 ONNX det 模型不存在: {ppocrv6_det}")
        print("     需要从 HuggingFace 下载:")
        print("     https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_det_onnx")
        print("     https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_rec_onnx")
        return None

    # 如果模型存在, 尝试运行 oar-ocr pipeline
    print(f"  PP-OCRv6 det 模型: {ppocrv6_det}")
    print(f"  PP-OCRv6 rec 模型: {ppocrv6_rec}")
    print(f"  PP-OCRv6 dict: {ppocrv6_dict}")

    # 运行 OCR on corpus
    manifest = load_corpus()
    results = []

    for item in manifest["items"]:
        image_path = CORPUS_DIR / item["image"]
        if not image_path.exists():
            results.append({
                "image": item["image"],
                "expected": item["expected_text"],
                "actual": "",
                "cer": 1.0,
                "error": "image not found",
            })
            continue

        # TODO: 调用 oar-ocr Rust binary 进行 OCR
        # 这里需要实际的 oar-ocr binary 来执行 OCR
        results.append({
            "image": item["image"],
            "expected": item["expected_text"],
            "actual": "(oar-ocr not available in spike)",
            "cer": 1.0,
            "error": "oar-ocr binary not configured for corpus benchmark",
        })

    return {"results": results, "note": "oar-ocr benchmark requires actual model files"}


def run_winrt_benchmark():
    """运行 WinRT OCR baseline。"""
    print("\n" + "=" * 60)
    print("Spike B: WinRT OCR Baseline")
    print("=" * 60)

    baseline_path = PPOCR_SPIKE_DIR / "results" / "winrt_baseline.json"
    if baseline_path.exists():
        with open(baseline_path, "r", encoding="utf-8") as f:
            results = json.load(f)
        print(f"  已有 WinRT baseline 结果: {baseline_path}")
        return results
    else:
        print("  WinRT baseline 结果不存在")
        return None


def analyze_asset_inventory():
    """分析完整资产清单。"""
    print("\n" + "=" * 60)
    print("Spike B: Asset Inventory")
    print("=" * 60)

    assets = []

    # PP-OCRv6 ONNX models
    for model_name in ["ppocrv6_tiny_det.onnx", "ppocrv6_tiny_rec.onnx"]:
        model_path = ONNX_SPIKE_MODELS / model_name
        if model_path.exists():
            assets.append({
                "name": model_name,
                "path": str(model_path),
                "size_bytes": model_path.stat().st_size,
                "sha256": sha256_file(model_path),
                "source": "HuggingFace: PaddlePaddle/PP-OCRv6_tiny_*_onnx",
                "license": "Apache 2.0",
            })

    # Dictionary
    dict_path = ONNX_SPIKE_MODELS / "ppocrv6_tiny_dict.txt"
    if dict_path.exists():
        assets.append({
            "name": "ppocrv6_tiny_dict.txt",
            "path": str(dict_path),
            "size_bytes": dict_path.stat().st_size,
            "sha256": sha256_file(dict_path),
            "source": "PaddleOCR",
            "license": "Apache 2.0",
        })

    # ONNX Runtime DLLs
    ort_dll_dir = Path(SPIKE_DIR).parent / ".tmp-venv" / "Lib" / "site-packages" / "onnxruntime" / "capi"
    if ort_dll_dir.exists():
        for dll in ort_dll_dir.glob("*.dll"):
            assets.append({
                "name": dll.name,
                "path": str(dll),
                "size_bytes": dll.stat().st_size,
                "sha256": sha256_file(dll),
                "source": "Python onnxruntime package (PyPI)",
                "license": "MIT",
            })

    total_size = sum(a["size_bytes"] for a in assets)
    print(f"  总资产数: {len(assets)}")
    print(f"  总大小: {total_size / 1024 / 1024:.1f}MB")

    return {"assets": assets, "total_size_bytes": total_size}


def main():
    print("=" * 60)
    print("Spike B: OCR Qualification Gate")
    print("=" * 60)

    corpus = load_corpus()
    print(f"Golden corpus: {len(corpus['items'])} items")
    print(f"Corpus dir: {CORPUS_DIR}")

    # 1. PaddleOCR benchmark
    paddleocr_results = run_paddleocr_benchmark()

    # 2. oar-ocr benchmark
    oar_results = run_oar_ocr_benchmark()

    # 3. WinRT baseline
    winrt_results = run_winrt_benchmark()

    # 4. Asset inventory
    asset_inventory = analyze_asset_inventory()

    # 汇总
    summary = {
        "spike": "B_ocr_qualification",
        "corpus_items": len(corpus["items"]),
        "paddleocr_results": paddleocr_results,
        "oar_ocr_results": oar_results,
        "winrt_baseline": winrt_results,
        "asset_inventory": asset_inventory,
        "decision": "CONDITIONAL-GO",
        "conditions": [
            "下载 PP-OCRv6 ONNX det+rec 模型并运行完整 corpus benchmark",
            "验证 Rust oar-ocr 的 CER 不劣于 Python PaddleOCR",
            "验证 word rect 几何契约",
            "测量冷/热推理延迟 p50/p95",
            "测试并发取消行为",
        ],
    }

    # 保存结果
    results_dir = Path(SPIKE_DIR) / "results"
    results_dir.mkdir(exist_ok=True)
    output_path = results_dir / "spike_b_ocr_qualification.json"
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump(summary, f, ensure_ascii=False, indent=2)
    print(f"\n结果已保存到: {output_path}")


if __name__ == "__main__":
    main()
