# ONNX Runtime Spike — 语音 + VAD 流式验证

> **Spike 日期**：2026-09-01
> **目标**：验证 ONNX runtime 是否能像 llama.cpp 一样轻量（单 DLL + 单模型文件），跑通语音 + VAD 流式效果

## 环境准备

```bash
# 1. 创建 venv
python -m venv .venv
.venv\Scripts\activate

# 2. 安装依赖（仅需 onnxruntime + funasr-onnx，不需要 PaddlePaddle）
pip install onnxruntime funasr-onnx

# 3. 下载模型
# FSMN-VAD ONNX（509KB quant）：
#   https://huggingface.co/funasr/fsmn-vad-onnx
# Paraformer-zh ONNX（238MB quant）：
#   https://huggingface.co/funasr/Paraformer-large
# SenseVoice ONNX（~230MB quant）：
#   https://huggingface.co/DennisHuang648/SenseVoiceSmall-onnx

# 4. 跑 spike
python run_onnx_vad_asr.py
```

## 验证项

1. ONNX runtime 是否单 DLL 依赖（不需要安装框架）
2. ONNX 模型文件大小 vs GGUF 对比
3. FSMN-VAD 流式推理（chunk + cache）
4. Paraformer/SenseVoice 流式推理效果
5. 资源占用（内存、CPU）
