# ONNX Follow-up Spike

> Spike 范围: Blink 0.22.8–0.22.10 ONNX 迁移补充验证  
> 仓库位置: `xtask/spikes/onnx-spike/followup/`

## 快速命令

```powershell
# 编译 Spike A (ORT 负面加载实验)
cd xtask/spikes/onnx-spike/followup/spike-a-crate
cargo build --release

# 运行 Spike A (需要 CPU-only ORT DLL)
cd target/release
./onnx-spike-a.exe
# 结果输出到 ../results/spike_a_result.json

# 编译 Spike B (OCR 资格门)
cd xtask/spikes/onnx-spike/followup/spike-b-ocr-qual
cargo build --release
cd target/release
./onnx-spike-b.exe
# 结果输出到 ../results/spike_b_ocr_qualification.json

# 编译 Spike E（最小 Hybrid 可行性）
cd xtask/spikes/onnx-spike/followup/spike-e-worker
cargo build --release
cd target/release
./onnx-spike-e.exe
# 结果输出到 ../results/spike_e_topology_comparison.json

# 运行 Spike C1 (ParaformerOnline API 检查)
../../.tmp-venv/Scripts/python.exe ../spike-c-oracle/inspect_paraformer_online.py
# 结果输出到 ../results/spike_c1_paraformer_online.json

# 运行 Spike C2 (Python onnxruntime Oracle — 流式 ASR 验证)
../../.tmp-venv/Scripts/python.exe ../spike-c-oracle/spike_c2_paraformer_online.py
# 结果输出到 ../results/spike_c2_paraformer_online.json

# 运行 Spike D (VAD+ASR 对比矩阵)
cd spike-d-vad-asr-matrix
../../.tmp-venv/Scripts/python.exe spike_d_vad_asr_matrix.py
# 结果输出到 ../results/spike_d_vad_asr_matrix.json

# 编译 Spike C Rust (kaldi-native-fbank 实现)
cd spike-c-rust
cargo build --release

# 运行 Spike C Rust (需要 ORT DLL)
$env:ORT_DYLIB_PATH = "d:/Projects/Coding/blink/xtask/spikes/onnx-spike/followup/runtimes/onnxruntime-cpu/onnxruntime.dll"
.\target\release\onnx-spike-c.exe
# 结果输出到 results/spike_c2_rust_knf.json

# 生成 cargo tree
cd spike-a-crate
cargo tree > ../results/cargo_tree.txt
cargo tree -e features > ../results/cargo_tree_features.txt
```

## 目录结构

```
followup/
├── HANDOFF.md                        # 交接文档 (含完整状态和剩余工作)
├── README.md                          # 本文件
├── decision.md                        # 完整决策报告
├── results/                           # 实验结果
│   ├── spike_a_result.json            # Spike A: ORT 负面加载 (真实执行)
│   ├── spike_b_ocr_qualification.json # Spike B: OCR 资格门 (22 项 corpus, GO)
│   ├── spike_c1_paraformer_online.json # Spike C1: ParaformerOnline API 检查
│   ├── spike_c2_paraformer_online.json # Spike C2: Python oracle (GO, 2030 行)
│   ├── spike_e_topology_comparison.json # Spike E: 最小 Hybrid 可行性 (真实执行)
│   ├── cargo_tree.txt                 # 依赖树
│   └── cargo_tree_features.txt        # Feature 树
├── runtimes/
│   ├── onnxruntime-cpu/               # CPU-only ORT 1.19.2
│   │   ├── onnxruntime.dll            # 11,234,848 bytes
│   │   └── onnxruntime_providers_shared.dll
│   ├── onnxruntime-win-x64-1.19.2/    # 原始下载
│   ├── corrupted_model.onnx           # 损坏模型 (测试用)
│   ├── zero_byte.dll                  # 0 字节 DLL (测试用)
│   └── random_bytes.dll              # 随机字节 DLL (测试用)
├── spike-a-crate/                     # Rust: ORT 负面加载实验
│   ├── Cargo.toml
│   ├── src/main.rs
│   └── target/release/onnx-spike-a.exe
├── spike-b-ocr-qual/                  # Rust: OCR 资格门
│   ├── Cargo.toml
│   ├── src/main.rs
│   └── target/release/onnx-spike-b.exe
├── spike-b-ocr-benchmark.py          # Python: OCR baseline benchmark
├── spike-c-oracle/                    # Python: ParaformerOnline oracle
│   ├── inspect_paraformer_online.py   # C1: API 检查
│   ├── run_paraformer_online.py       # (废弃, 被 C2 取代)
│   └── spike_c2_paraformer_online.py  # C2: 完整 Python oracle (928 行)
├── spike-c-rust/                      # Rust: ParaformerOnline (kaldi-native-fbank)
│   ├── Cargo.toml                     # 依赖: ort, kaldi-native-fbank, hound
│   ├── src/main.rs                    # ~600 行, GO
│   └── target/release/onnx-spike-c.exe
├── spike-d-vad-asr-matrix/            # Python: VAD+ASR 对比矩阵
│   ├── spike_d_vad_asr_matrix.py      # 主入口
│   └── spike_d_models.py              # EnergyVad/FSMN-VAD/ParaformerOnline/Offline 实现
└── spike-e-worker/                    # Rust: 最小 Hybrid 可行性
    ├── Cargo.toml
    ├── src/main.rs
    └── target/release/onnx-spike-e.exe
```

## 模型文件

模型文件在 `xtask/spikes/onnx-spike/models/` 下:

```
models/
├── fsmn-vad-onnx-v2/                  # FSMN-VAD 流式 VAD
│   ├── model_quant.onnx               # 506,744 bytes
│   ├── am.mvn                         # 8,042 bytes
│   ├── config.yaml                    # 1,271 bytes
│   └── README.md
├── paraformer-zh-onnx/                # Paraformer 离线 ASR
│   ├── model_quant.onnx               # 238,380,216 bytes (~227MB)
│   ├── am.mvn                         # 11,211 bytes
│   ├── config.yaml                    # 2,632 bytes
│   ├── tokens.json                    # 102,083 bytes
│   └── README.md
├── paraformer-online-onnx/           # ParaformerOnline 流式 ASR (C2 已验证 GO)
│   ├── encoder.onnx                   # encoder
│   ├── decoder.onnx                   # decoder
│   ├── am.mvn                         # CMVN
│   ├── config.yaml                    # 模型配置
│   └── tokens.json                    # 词表
├── ppocrv6-onnx/                      # PP-OCRv6 Tiny OCR
│   ├── pp-ocrv6_tiny_det.onnx         # 1,780,590 bytes
│   ├── pp-ocrv6_tiny_rec.onnx         # 4,462,639 bytes
│   └── ppocrv6_tiny_dict.txt          # 27,156 bytes
├── sherpa-paraformer-online/          # Sherpa 备选模型 + test_wavs
│   ├── encoder.int8.onnx
│   ├── decoder.int8.onnx
│   └── test_wavs/                     # 测试音频
└── asr_example.wav                    # 测试音频 (C2 oracle 使用)
```

## 关键发现摘要

### Spike A: ORT 负面加载 (✅ 完成)

- `ort::init_from(path)` 立即通过 `libloading::Library::new` 加载 DLL
- CPU-only ORT 1.19.2 + ort crate 2.0.0-rc.13 完全兼容
- 9 个负面加载场景全部执行，无 panic 或 access violation
- 损坏模型返回 `Protobuf parsing failed`，不 crash
- `pending_restart` 必须: Windows 锁定已加载 DLL

### Spike B: OCR 资格门 (✅ 完成, GO)

- 22 项 golden corpus 全部执行, 0 项 error
- 中英文 CER = **0.000** (生产质量)
- BBox valid ratio = **1.0** (几何完全正确)
- 冷加载 p50 = 36.9ms, 热推理 p50 = 368.6ms (大图)
- 峰值内存 44.7MB, 资产总磁盘 16.0MB
- ⚠️ 日语 CER = 0.80, 垂直文本 CER = 0.85 (PP-OCRv6 Tiny 不支持)

### Spike C1: ParaformerOnline API 检查 (✅ 完成)

- `funasr-onnx` v0.4.2 **没有** `Paraformer_online` 类
- 可用类: `Paraformer`(离线), `Fsmn_vad`, `Fsmn_vad_online`, `SenseVoiceSmall` 等
- 结论: 需要直接用 onnxruntime Python API 手动实现

### Spike C2: Python onnxruntime Oracle (✅ 完成, GO)

- 完整流水线已实现: fbank → LFR → CMVN → pos_emb → encoder → CIF → decoder → greedy
- **RTF = 0.4074x** (远低于 1.0 实时阈值)
- **首个 partial 延迟 = 2.575s** (句尾前产生)
- 20x reset 全部一致, Cancel + reset 通过
- Encoder I/O: 2 inputs → 3 outputs (enc, enc_len, alphas)
- Decoder I/O: 20 inputs (含 16 层 FSMN cache) → 19 outputs
- Python oracle 是 Rust 实现的完整参考 (928 行代码)

### Spike C2 Rust: kaldi-native-fbank 实现 (✅ 完成, GO)

- 使用 `kaldi-native-fbank` crate v0.1.0 替换手动 DFT fbank
- **RTF = 0.1609x** (比 Python oracle 快 2.5x, 比手动 DFT Rust 快 1.7x)
- **首个 partial 延迟 = 0.174s** (比 Python oracle 快 14.8x)
- 最终文本: `昨天是mon@@daytodayis零八二thedayaftertom@@or@@row是星期三`
- 20x reset 全部一致, Cancel + reset 通过
- 帧数恒定 (每 chunk 20 frames, 无累积)
- **结论: Rust 真流式 STT 技术可行性已验证 GO**

### Spike E: 最小 Hybrid 可行性 (✅ 完成, HYBRID_FEASIBILITY_GO)

- 最小化 hybrid topology 可行性验证 — 12 项检查全通过
- Worker 进程加载真实 ORT DLL + ParaformerOnline encoder (166MB) + decoder (72MB)
- NDJSON over stdin/stdout 流式推理: 17 chunk, 首个 partial="昨" (chunk 1)
- 最终文本: `昨天是mon@@daytodayis零八二thedayaftertom@@or@@row是星期三`
- 优雅退出: Quit → worker 立即退出
- Kill + wait + restart: worker #2 kill (exit_code=1), worker #3 重启成功
- 无残留进程: tasklist 确认无 orphan
- Topology 定案: **HYBRID** — OCR in-process；ParaformerOnline 独立 ONNX worker；Nano 保持现有 GGUF worker
- 本 Spike 不比较 in-process/worker 性能；延迟、内存、CPU、RTF、p50/p95 与生产 payload 格式进入实现期 gate

### Spike D: GGUF/ONNX/VAD 对比矩阵 (✅ 完成)

- 16 项 corpus (11 合成 + 5 真实 WAV), 覆盖安静近讲/远场/风扇空调/键盘/音乐/短词/长句/句中停顿/多句/纯噪声/纯静默
- 6 组合测试: EnergyVad/FSMN-VAD × ParaformerOnline/ParaformerOffline (+ dup B 一致性)
- VAD 结果: EnergyVad F1=0.717 (P=0.719, R=0.812, FA=9, FR=3); FSMN-VAD F1=0.708 (P=0.812, R=0.656, FA=3, FR=8)
- ASR 结果: ParaformerOnline RTF=0.16-0.22, 首 partial=0.166s; Paraformer offline RTF=0.064
- 关键发现: FSMN-VAD 在真实音频上 0 FA/0 FR (优于 EnergyVad), 但合成短句漏切 (800ms 静默阈值太高)
- 结论: FSMN-VAD 条件推荐 (需调低 max_end_silence_time); 真流式值得 (首 partial<1s)
- 代码: `spike-d-vad-asr-matrix/spike_d_vad_asr_matrix.py` + `spike-d-vad-asr-matrix/spike_d_models.py`
