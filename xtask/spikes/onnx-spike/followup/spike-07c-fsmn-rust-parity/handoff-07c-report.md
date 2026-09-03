# Handoff 07C — FSMN-VAD Rust Parity Spike Report

> **生成时间**：2026-09-03
>
> **结论**：**GO** — FSMN-VAD ONNX 可在 Rust 中以有界、增量、可 reset 的方式用于实时端点检测
>
> **范围**：Spike only，不改生产实现。验证 FSMN-VAD ONNX 模型在 Rust 中的增量推理能力。

---

## 一、结论：GO

### GO 条件全部满足

| GO 条件 | 状态 | 数据 |
|---|---|---|
| Rust frame scores 与 Python oracle 在合理浮点误差内一致 | ✅ 通过 | max_diff < 0.006，0/599 帧超 0.01 容差 |
| segment start/end 与 oracle 差异不超过一个模型帧 | ✅ 通过 | 所有场景 segment 完全一致 |
| 支持真正增量 cache/reset | ✅ 通过 | 4 层 cache `[1,128,19,1]` 增量更新，reset 后 5x 一致 |
| 不形成 O(n²) 累积重算 | ✅ 通过 | 每 chunk 仅处理新增帧，不重算历史音频 |
| 单 chunk p95 < 输入 chunk 时间预算 | ✅ 通过 | p95=0.050ms << 10ms 预算 |
| 多 session/reset 后无状态串音 | ✅ 通过 | 3 trial × 3 scenario 交替，完全一致 |

### NO-GO 条件均不满足

| NO-GO 条件 | 状态 |
|---|---|
| 模型实际只能离线整段推理 | ❌ 不成立 — 模型支持流式增量推理 |
| 无法复现 cache | ❌ 不成立 — 4 层 cache 正确复现 |
| 必须累计重算 | ❌ 不成立 — 每 chunk 仅处理新增帧 |
| Rust/Python 无法达到可解释 parity | ❌ 不成立 — 达到 0.006 max_diff |
| 端点延迟不适合 hold-to-talk | ❌ 不成立 — p95=0.050ms |

---

## 二、ONNX Graph I/O 表

### 模型信息

| 属性 | 值 |
|---|---|
| 模型文件 | `fsmn-vad-onnx-v2/model_quant.onnx` (量化版) |
| ORT 版本 | 1.19.2 (动态加载) |
| Rust binding | `ort = "=2.0.0-rc.13"` (load-dynamic) |
| 特征提取 | `kaldi-native-fbank = "0.1"` |

### 输入

| 序号 | 名称 | dtype | shape | 说明 |
|---|---|---|---|---|
| 0 | `speech` | float32 | `[1, feats_length, 400]` | splice 后的 CMVN 归一化特征 |
| 1 | `in_cache0` | float32 | `[1, 128, 19, 1]` | FSMN 第 0 层 cache |
| 2 | `in_cache1` | float32 | `[1, 128, 19, 1]` | FSMN 第 1 层 cache |
| 3 | `in_cache2` | float32 | `[1, 128, 19, 1]` | FSMN 第 2 层 cache |
| 4 | `in_cache3` | float32 | `[1, 128, 19, 1]` | FSMN 第 3 层 cache |

### 输出

| 序号 | 名称 | dtype | shape | 说明 |
|---|---|---|---|---|
| 0 | `logits` | float32 | `[1, T, 248]` | 帧级 log-softmax（248 个 sil/sp/speech 标签） |
| 1 | `out_cache0` | float32 | `[1, 128, T', 1]` | 更新后的第 0 层 cache |
| 2 | `out_cache1` | float32 | `[1, 128, T', 1]` | 更新后的第 1 层 cache |
| 3 | `out_cache2` | float32 | `[1, 128, T', 1]` | 更新后的第 2 层 cache |
| 4 | `out_cache3` | float32 | `[1, 128, T', 1]` | 更新后的第 3 层 cache |

### 前处理 pipeline

```
PCM (16kHz mono f32) 
  → scale ×32768 (int16 range)
  → kaldi-native-fbank OnlineFeature (80-mel, 25ms frame, 10ms shift, hamming, preemph=0.97)
  → splice (5 frames → 400-dim)
  → CMVN ((x + means) * vars, 400-dim)
  → ONNX inference (speech + 4 cache inputs → logits + 4 cache outputs)
  → softmax → speech_prob = 1 - P(silence)
  → 3-frame majority smoothing
  → endpoint state machine (lookback 200ms, lookahead 100ms, max_silence 800ms)
```

### 关键参数

| 参数 | 值 | 来源 |
|---|---|---|
| `n_mels` | 80 | config.yaml |
| `frame_length_ms` | 25 | config.yaml |
| `frame_shift_ms` | 10 | config.yaml |
| `splice_len` (lfr_m) | 5 | config.yaml |
| `cache_layers` | 4 | config.yaml (fsmn_layers) |
| `cache_dim` | 128 | config.yaml (proj_dim) |
| `cache_lorder` | 19 | ONNX graph (config.yaml 标 20 但模型用 19) |
| `input_dim` | 400 | 5 × 80 |
| `max_end_silence_ms` | 800 | FunASR E2EVadModel 默认 |
| `lookback_start_ms` | 200 | FunASR E2EVadModel 默认 |
| `lookahead_end_ms` | 100 | FunASR E2EVadModel 默认 |
| `snip_edges` | true | kaldi-native-fbank 默认（与 FunASR 生产一致） |

---

## 三、Python/Rust Parity 数据

### 3.1 场景总览

| 场景 | 描述 | 音频时长 | 帧数 | Chunk 数 | Segment 匹配 | Max Score Diff |
|---|---|---|---|---|---|---|
| continuous_chunk | 连续 chunk | 3.5s | 208 | 35 | ✅ 一致 | 0.005372 |
| short_phrase | 短句 | 0.7s | 40 | 7 | ✅ 一致 | 0.002149 |
| mid_pause | 句中停顿 | 2.7s | 160 | 27 | ✅ 一致 | 0.005953 |
| long_silence | 长静音 | 3.0s | 178 | 30 | ✅ 一致 | 0.000575 |
| pure_noise | 纯噪声 | 3.0s | 178 | 30 | ✅ 一致 | 0.001886 |
| real_audio | 真实音频 | 10.1s | 599 | 101 | ✅ 一致 | 0.003130 |

### 3.2 Frame Score Parity 详情

所有场景的帧级 score 对比：

- **帧数完全一致**：6/6 场景 Python 和 Rust 产生相同帧数
- **最大帧差异**：0.005953（mid_pause 场景 frame 130）— 远低于 0.01 容差
- **超过 0.01 容差的帧数**：0/1363（全部帧）
- **平均差异范围**：0.0002 ~ 0.0016

### 3.3 Per-Chunk Parity

所有 35+7+27+30+30+101 = 230 个 inference chunk 的 input_shape、output_shape、frame count 全部一致。匹配 chunk 的 frame score max_diff 均 < 0.006。

### 3.4 剩余浮点差异来源

0.001~0.006 的微小差异来自：
1. **Fbank 实现**：Python 手动 FFT (numpy) vs Rust kaldi-native-fbank C++ FFT — 不同的 FFT 实现（Cooley-Tukey vs KissFFT）产生 O(1e-4) 级差异
2. **Mel filterbank**：Python Slaney 手动实现 vs kaldi-native-fbank 内置实现 — 归一化精度差异
3. **Softmax 计算**：numpy vs Rust 手动实现 — 浮点累加顺序差异

这些差异不影响端点检测的二值决策（speech_prob > 0.5），因为：
- 差异量级 < 0.006
- 决策阈值 0.5
- 即使 score 差 0.006，只有在 score ≈ 0.5 ± 0.003 的帧才会改变决策
- 3-frame 平滑进一步降低了单帧翻转的影响

---

## 四、Cache/Reset 结论

### 4.1 增量 Cache

FSMN-VAD ONNX 模型使用 4 层 cache 实现真正的增量推理：

- **Cache 结构**：`[1, 128, 19, 1]` per layer — 4 层 × 128 维 × 19 帧
- **Cache 更新**：每次 inference 的 `out_cache{0-3}` 直接作为下次 `in_cache{0-3}` 输入
- **增量性**：每 chunk 仅处理新增帧（由 `input_cache` 和 `fbank_offset` 保证），不重算历史音频
- **无 O(n²)**：每 chunk 的 inference 时间与音频总长无关（恒定 ~0.05ms）

### 4.2 Reset 测试

```
Reset Test: 5x reset + reprocess scenario 1
  5x reset consistent: true
```

5 次完整 reset + reprocess 产生完全一致的 segment 结果。reset 清空：
- 4 层 FSMN cache → zeros
- input_cache → empty
- fbank 累积波形 → empty
- fbank_offset → 0
- 状态机 → initial

### 4.3 Multi-Session 测试

```
Multi-session test: alternating scenarios
  Multi-session consistent: true
```

3 trial × 3 scenario 交替执行（reset → process → finalize），所有 trial 结果完全一致。无状态串音。

### 4.4 Final Flush

`finalize()` 正确处理末尾未闭合的 speech segment：
- 如果 `in_speech == true`，以 `total_samples / SR` 作为 end time
- 闭合当前 segment

---

## 五、延迟和内存

### 5.1 单 Chunk 延迟

| 指标 | Python (numpy) | Rust (ORT + kaldi-native-fbank) |
|---|---|---|
| p50 | 0.004ms | 0.000ms |
| p95 | 0.094ms | 0.050ms |
| 时间预算 | 10ms (10ms audio chunk) | 10ms |
| Within budget | ✅ YES | ✅ YES |

Rust p95 = 0.050ms，远低于 10ms 时间预算。占用不到预算的 0.5%。

### 5.2 内存

| 指标 | 值 |
|---|---|
| 模型加载时间 | 0.031s |
| 模型加载内存增量 | 15.3MB |
| 峰值工作集 | 24.6MB |
| 进程基线（加载前） | 6.1MB |

FSMN-VAD ONNX 模型常驻增量约 15-18MB，远低于 300MB 常驻内存预算。加上 ORT runtime 和 fbank 缓冲，总增量约 18-19MB。

### 5.3 对比 EnergyVad

| 指标 | EnergyVad | FSMN-VAD ONNX (Rust) |
|---|---|---|
| 单 chunk CPU | ~0.01ms (RMS) | ~0.05ms (fbank + ONNX) |
| 常驻增量工作集 | 0 bytes | ~15-18MB |
| 模型加载时间 | 0 | ~31ms |
| 端点检测质量 | 单阈值 RMS | 神经网络帧级 score |
| 增量/可 reset | ✅ | ✅ |
| O(n²) 重算 | 无 | 无 |

---

## 六、算法状态合同

### 6.1 状态机定义

```
States: IDLE → IN_SPEECH → (silence ≥ 800ms) → IDLE

IDLE:
  speech_prob > 0.5 → IN_SPEECH
    current_start = max(0, frame_time - 200ms)  // lookback

IN_SPEECH:
  speech_prob ≤ 0.5 → silence_frames++
    silence_frames × 10ms ≥ 800ms → 
      segment = (current_start, frame_time + 100ms)  // lookahead
      → IDLE

Finalize:
  if IN_SPEECH:
    segment = (current_start, total_samples / SR)
```

### 6.2 参数

| 参数 | 值 | 说明 |
|---|---|---|
| `speech_prob_threshold` | 0.5 | softmax 后 P(speech) > 0.5 判为语音 |
| `smoothing_window` | 3 frames | 3-frame majority 平滑 |
| `max_end_silence_ms` | 800 | 连续静音 800ms 触发端点 |
| `lookback_start_ms` | 200 | 语音起点回退 200ms |
| `lookahead_end_ms` | 100 | 端点前看 100ms |
| `frame_in_ms` | 10 | 模型帧步长 |

### 6.3 适用场景

此状态机合同适用于：
- Hold-to-talk 语音输入
- 实时录音端点检测
- 伪流式 STT 的切句热路径

**不适用**：
- 离线整段音频的 VAD 分段（应使用 E2EVadModel 后处理）
- 多说话人分离

---

## 七、建议

### 7.1 立即可做：填充生产实现

`src/infra/stt/fsmn_vad_onnx.rs` 中的 `run_vad_inference` 当前是占位实现。Spike 07C 已验证的完整 pipeline 可直接移植：

1. **fbank 前处理**：使用 `kaldi-native-fbank` OnlineFeature，配置 `snip_edges=true, preemph=0.97, hamming, 80-mel`
2. **splice + CMVN**：5-frame splice → 400-dim → CMVN (am.mvn)
3. **ONNX inference**：`speech + 4×in_cache → logits + 4×out_cache`
4. **softmax + smoothing**：`1 - P(silence)`, 3-frame majority
5. **状态机**：lookback 200ms / lookahead 100ms / max_silence 800ms

### 7.2 与 decision.md 的关系

`xtask/spikes/fsmn-vad/decision.md` 的 `conditional-go` 结论基于 **GGUF 离线实现**。Spike 07C 验证的是 **ONNX 在线增量实现**，两者不矛盾：

- GGUF 离线实现：不支持增量 cache/reset → conditional-go
- ONNX 在线增量实现：支持增量 cache/reset、p95=0.05ms、parity 验证 → **GO**

### 7.3 对 EnergyVad 的影响

Spike 07C 证明了 FSMN-VAD ONNX 可用于实时切句热路径。建议：

1. **短期**：实施 decision.md §5 的 EnergyVad 优化包（自适应噪声底、双阈值滞回等）
2. **中期**：将 FSMN-VAD ONNX 作为生产 VAD 前端实现，替换 EnergyVad 的切句热路径
3. **长期**：考虑 FSMN-VAD + EnergyVad 混合方案（EnergyVad 粗筛 → FSMN 精确端点验证）

### 7.4 不做

- 不在本 spike 修改生产实现
- 不改变 worker 协议
- 不注册模型
- 不改变默认 VAD 策略

---

## 八、复现入口

### 文件

| 文件 | 说明 |
|---|---|
| `spike-07c-fsmn-rust-parity/spike_07c_oracle.py` | Python oracle（ONNX runtime + 手动 kaldi fbank） |
| `spike-07c-fsmn-rust-parity/src/main.rs` | Rust runner（ort + kaldi-native-fbank） |
| `spike-07c-fsmn-rust-parity/Cargo.toml` | Rust 依赖锁定 |
| `spike-07c-fsmn-rust-parity/compare_parity.py` | Parity 对比脚本 |
| `results/spike_07c_oracle.json` | Python oracle 输出 |
| `results/spike_07c_rust.json` | Rust runner 输出 |

### 运行

```bash
# Python oracle
python xtask/spikes/onnx-spike/followup/spike-07c-fsmn-rust-parity/spike_07c_oracle.py

# Rust runner
cd xtask/spikes/onnx-spike/followup/spike-07c-fsmn-rust-parity
cargo build --release
.\target\release\onnx-spike-07c.exe

# Parity comparison
python xtask/spikes/onnx-spike/followup/spike-07c-fsmn-rust-parity/compare_parity.py
```

### 依赖

- Python: `onnxruntime`, `numpy`
- Rust: `ort = "=2.0.0-rc.13"`, `kaldi-native-fbank = "0.1"`, `hound = "3"`
- 模型: `fsmn-vad-onnx-v2/model_quant.onnx` + `am.mvn`
- ORT runtime: `onnxruntime.dll` v1.19.2
