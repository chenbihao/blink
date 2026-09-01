# FunASR GGUF 流式可行性调研报告

> **Spike 日期**：2026-09-01
> **范围**：三个 GGUF 模型（SenseVoice / Paraformer / Fun-ASR-Nano）+ FSMN-VAD 的流式推理能力调研
> **目标**：回答"换到 llama.cpp 后是否支持真流式？如何适配 FunASR 默认 VAD？是否有更优方案？"

---

## 一、核心结论（先读这段）

| 问题 | 结论 | 证据 |
|---|---|---|
| **GGUF/llama.cpp worker 当前支持流式吗？** | ❌ **不支持**。三个模型都是"整段输入 → 整段输出"的一次性推理 | 源码实测 + 上游 [DESIGN.md §10](https://github.com/modelscope/FunASR/blob/main/runtime/llama.cpp/DESIGN.md) roadmap 明确列 "streaming" 为 TODO |
| **FunASR 上游有流式实现吗？** | ✅ **有**，但只在 ONNX runtime 路径 | `FsmnVadOnline`、`ParaformerOnline`、`FunTpassInferBuffer`（2pass C API） |
| **三个模型的架构是否支持流式？** | 🟡 **模型架构本身可以流式**（除 SenseVoice CTC 需改造外），但 GGUF runtime 未实现 | 见 §三 架构分析 |
| **Fun-ASR-Nano 的 LLM 部分能流式输出吗？** | ✅ **能**。llama.cpp 天然支持 token-by-token streaming decode | `llama_decode` + `llama_sampler_sample` 循环已在源码中 |
| **FSMN-VAD 能做流式吗？** | ✅ **能**。ONNX 版已有 `FsmnVadOnline`（cache 机制），但 GGUF 版是整段推理 | ONNX `FsmnVad::Forward` 接受 `in_cache` / 返回更新后的 cache |
| **我们自写的 EnergyVad 效果有 FunASR 默认 VAD 好吗？** | ❌ **大概率没有**。FSMN-VAD 是神经网络模型，对噪声/远场/低 SNR 场景远优于能量阈值 | 见 §五 对比分析 |

### 一句话总结

> **GGUF/llama.cpp runtime 当前是离线一次性推理**，上游 ONNX runtime 有完整的流式实现（`FsmnVadOnline` + `ParaformerOnline` + 2pass WebSocket server），但 GGUF 路径尚未移植。如果要做真流式，有两条路径（见 §六），**推荐方案 B：混合架构——GGUF 离线推理 + ONNX 流式 VAD**。

---

## 二、源码级证据

### 2.1 GGUF worker 的推理模式（当前）

三个 GGUF CLI 的核心循环都是：

```
整段 WAV → compute_fbank(整段) → encoder(整段 T) → 一次性解码 → 完整文本
```

**关键源码证据**：

#### SenseVoice（`funasr-sensevoice.cpp` L312-346）
```cpp
auto run_seg = [&](const std::vector<float>& fb, int T) -> std::string {
    // 构建整段 [N=T+4, F=560] 输入
    ggml_tensor* x = ggml_new_tensor_2d(c, GGML_TYPE_F32, F, N);
    // 50+20 层 encoder 一次性前向
    for (int i=0; i<m.c.num_blocks-1; i++)
        h = sanm_layer(c, m, ..., h, N, true);
    // CTC head 一次性输出 [V, N]
    ggml_tensor* logits = lin(c, m.g("ctc.ctc_lo.weight"), ...);
    // greedy CTC decode 整段
    for (int n=0; n<N; n++) { argmax → collapse → drop blank }
};
```
→ **无 chunk 边界、无 cache 传递、无增量输出**。

#### Paraformer（`funasr-paraformer.cpp` L166-209）
```cpp
auto run_seg = [&](const std::vector<float>& wav) -> std::string {
    auto fb = compute_fbank(wav, T);      // 整段 fbank
    // encoder 整段前向
    for (int i=0; i<m.c.enc_blocks-1; i++)
        h = enc_layer(c, m, ..., h, T, true);
    // CIF predictor 整段积分
    for (int t=0; t<L; t++) { integrate += alpha; if (fire) ... }
    // decoder 整段一次解码
    for (int i=0; i<m.c.dec_att; i++)
        x = dec_layer(c, m, ..., x, mem, N, T);
};
```
→ **CIF 积分是整段顺序循环，但无 cache 传递机制，无法增量**。

#### Fun-ASR-Nano（`funasr-cli.cpp` L232-257）
```cpp
auto run_window = [&](const std::vector<float>& seg) -> std::string {
    auto fbank = compute_fbank(seg, T);
    auto adp = run_encoder(em, fbank, T, 560, D);  // 整段 encoder
    // KV 清空，一次性注入 prefix + audio embeds + suffix
    llama_memory_clear(llama_get_memory(ctx), true);
    decode_batch(ctx, pre.size(), ...);       // prefix
    decode_batch(ctx, n_aud, nullptr, adp.data(), D, ...); // audio
    decode_batch(ctx, suf.size(), ...);       // suffix
    // 然后逐 token 自回归解码 ← 这部分天然是流式的！
    for (int i=0; i<npred; i++) {
        tk = llama_sampler_sample(smpl, ctx, -1);
        seg_text.append(buf, k);  // ← 可以边生成边输出
        decode_batch(ctx, 1, &tk, ...);
    }
};
```
→ **LLM 解码部分天然支持 token-by-token 流式输出，但 encoder 是整段的、且每个 window 间 KV 被清空**。

### 2.2 上游 ONNX runtime 的流式实现（对照）

| 组件 | ONNX 流式类 | 关键机制 |
|---|---|---|
| VAD | `FsmnVadOnline` | `input_cache_`（音频残帧缓存）+ `lfr_splice_cache_`（LFR 拼接缓存）+ `in_cache_`（4×128×19 FSMN 隐藏状态 cache）→ 每次只处理新 chunk，更新 cache |
| ASR | `ParaformerOnline` | `feats_cache_`（chunk 边界重叠）+ `hidden_cache_`/`alphas_cache_`（CIF 跨 chunk 积分）+ `fsmn_init_cache_`（decoder FSMN cache）+ `start_idx_cache_`（位置编码连续）→ 增量推理 |
| 2pass | `FunTpassInferBuffer` | online 模型先出实时粗略文本 → offline 模型在句尾做最终精校 → 用精校结果替换粗略文本 |

**ONNX `FsmnVad::Forward` 的 cache 接口**（`fsmn-vad.cpp` L67-130）：
```cpp
void FsmnVad::Forward(
    const std::vector<std::vector<float>> &chunk_feats,  // 当前 chunk 的特征
    std::vector<std::vector<float>> *out_prob,            // 当前 chunk 的输出
    std::vector<std::vector<float>> *in_cache,            // FSMN 隐藏状态 cache（4层×128×19）
    bool is_final)                                         // 是否最后一个 chunk
{
    // ONNX 模型输入 = [chunk_feats, cache_0, cache_1, cache_2, cache_3]
    // ONNX 模型输出 = [out_prob, new_cache_0, new_cache_1, new_cache_2, new_cache_3]
    // 非 final 时：将新 cache 写回 in_cache 供下次调用
    if (!is_final) {
        for (int i=1; i<5; i++) {
            float* data = vad_ort_outputs[i].GetTensorMutableData<float>();
            memcpy((*in_cache)[i-1].data(), data, sizeof(float)*128*19);
        }
    }
}
```

**ONNX `ParaformerOnline::CifSearch`**（`paraformer-online.cpp` L270-345）：
```cpp
void ParaformerOnline::CifSearch(
    std::vector<std::vector<float>> hidden,
    std::vector<float> alphas,
    bool is_final,
    std::vector<std::vector<float>>& list_frame)
{
    // 将上一 chunk 残留的 hidden_cache_ 和 alphas_cache_ 前置
    // → 跨 chunk 连续积分
    // → 每 fire 一次产生一个 acoustic embedding
    // → 未 fire 的部分存回 cache
    if (hidden_cache_.size() > 0) {
        hidden.insert(hidden.begin(), hidden_cache__.begin(), hidden_cache_.end());
        alphas.insert(alphas.begin(), alphas_cache_.begin(), alphas_cache_.end());
    }
    // ... 积分-触发循环 ...
    // 保存残留状态
    alphas_cache_.emplace_back(intergrate);
    hidden_cache_.emplace_back(hidden_cache);
}
```

→ **ONNX 的流式是通过模型的 ONNX 图本身接受 cache 张量作为额外输入/输出实现的**。

### 2.3 上游 DESIGN.md 的 roadmap

> **§10 Limitations & roadmap**:
> - VAD. Long audio needs segmentation; today Fun-ASR-Nano uses fixed `--chunk` windows. A real FSMN-VAD front end would close the last ~1.3% CER gap...
> - Encoder/decoder quantization (Q8 via gguf-py quants), **streaming**, timestamps...

→ **"streaming" 在 GGUF runtime 的 roadmap 中，但尚未实现**。

---

## 三、三个模型的流式架构分析

### 3.1 SenseVoice（SAN-M encoder + CTC）

| 维度 | 评估 |
|---|---|
| encoder 是否可流式 | 🟡 **理论可以但需改造**。SAN-M 的 self-attention 对全时间帧做 QK^T，直接 chunk 会丢失上下文。但 FSMN 分支天然支持 cache（滑动窗口）。要实现流式需要：attention 部分 chunk-boundary 重叠 + FSMN cache |
| CTC 解码 | ✅ **天然可增量**。逐帧 argmax → collapse → 只要新帧的输出可用，就能增量输出 |
| GGUF runtime 实现状态 | ❌ 整段推理，无 cache 接口 |
| 流式改造难度 | **高**（需要修改 ggml 计算图支持 chunk + cache 传递） |

### 3.2 Paraformer（SAN-M encoder + CIF + SAN-M decoder）

| 维度 | 评估 |
|---|---|
| encoder | 同 SenseVoice，需要 chunk + cache 改造 |
| CIF predictor | ✅ **天然可流式**。CIF 的积分-触发机制本身是顺序的，只要传递 `hidden_cache_` 和 `alphas_cache_` 即可跨 chunk 连续 |
| decoder | 🟡 SAN-M decoder 的 FSMN self-attn 可 cache；cross-attn 的 k/v 来自 encoder，如果 encoder 增量更新则 decoder 也可增量 |
| GGUF runtime 实现状态 | ❌ 整段推理 |
| 上游 ONNX 参考实现 | ✅ `ParaformerOnline` 有完整实现（`chunk_size=[0,10,5]` 即 600ms chunk） |
| 流式改造难度 | **中**（CIF 和 decoder FSMN 都有 cache 机制参考，主要是 encoder 需要改） |

### 3.3 Fun-ASR-Nano（SAN-M encoder + adaptor + Qwen3-0.6B LLM）

| 维度 | 评估 |
|---|---|
| encoder | 同上，需 chunk + cache 改造 |
| adaptor | 🟡 adaptor 的 `linear1 → relu → linear2 → adp_layers` 是逐帧独立的，理论上可以增量 |
| LLM 解码 | ✅ **天然流式**。llama.cpp 的 `llama_decode` + `llama_sampler_sample` 循环就是 token-by-token 流式输出 |
| KV cache | ⚠️ **当前每 window 清空 KV**。但 llama.cpp 支持保留 KV（`llama_memory_clear` 是显式调用的），如果 encoder 能增量产出 audio embeds，LLM 可以持续解码 |
| GGUF runtime 实现状态 | ❌ 整段 encoder + 清空 KV + 整段 prefix/suffix 注入 |
| 流式改造难度 | **中等偏高**（encoder 需改 + 需要设计增量 audio embeds 注入策略） |

### 3.4 FSMN-VAD

| 维度 | 评估 |
|---|---|
| 模型架构 | ✅ **天然支持流式**。FSMN 是深度可分离 1D 卷积，有固定长度的 cache（lorder=20 → 每层缓存 19 帧） |
| ONNX 实现 | ✅ `FsmnVadOnline` 完整实现（`in_cache_` 4×128×19） |
| GGUF 实现 | ❌ 整段推理。`funasr_vad_segments()` 接受整段 wav → 整段 fbank → 整段 encoder → 整段状态机 |
| 状态机 | ✅ E2EVadModel 状态机本身是逐帧的（silence schedule、segment emit 逻辑），只是当前喂入的是整段 |
| 流式改造难度 | **低**。FSMN 的 cache 机制简单明确（每层 `128×19` 的状态张量），ggml 中实现 shift-accumulate + cache 传递相对直接 |

---

## 四、Blink 当前伪流式架构回顾

```
麦克风音频流
    ↓ 16k mono f32 chunks
EnergyVad::process_chunk()        ← RMS 能量 + 静默时长 + 双阈值滞回
    ↓ SentenceEnd 事件
    ├── 定稿路径: 裁剪句子音频 → HTTP → GGUF worker → 完整文本 (confirmed)
    └── 预览路径: 500ms 定时 → 裁剪未确认音频 → HTTP → GGUF worker → 预览文本 (preview)
```

**关键特征**：
1. **VAD 切句在 Rust 侧**（EnergyVad），不在 worker 侧
2. **worker 只做整段推理**——每次收到的是裁剪后的 WAV 快照
3. **预览 = 累积音频重新推理**——O(n²) 重复计算
4. **定稿 = 句尾后一次性推理**——延迟取决于句子长度

**当前能力标记**（`gguf.rs`）：
```rust
// 所有三个模型：
pseudo_streaming: CapabilityFlag::yes(),
true_streaming: CapabilityFlag::no("stt.capability.streaming.no_incremental_encoder"),
```

---

## 五、EnergyVad vs FSMN-VAD 效果对比

| 维度 | EnergyVad | FSMN-VAD |
|---|---|---|
| 原理 | RMS 能量阈值 + 静默时长 | 神经网络（FSMN）分类 speech/noise/silence |
| 干净语音 | ✅ 效果好 | ✅ 效果好 |
| 背景噪声 | ⚠️ 阈值需手动调参，噪声大时误判 | ✅ 模型学习了噪声模式，自动区分 |
| 远场/低 SNR | ❌ 能量接近噪声底，容易误判 | ✅ 模型可利用频谱特征，远场更鲁棒 |
| 音乐/环境声 | ❌ 容易误判为语音 | ✅ 可区分语音 vs 非语音 |
| 静默检测精度 | 🟡 粗粒度（固定时长阈值） | ✅ 精细（逐帧分类 + 动态 silence schedule） |
| 多说话人 | ❌ 无法处理 | 🟡 不支持多说话人，但切段更准 |
| 资源占用 | ✅ 极低（O(1) 计算） | ⚠️ 需要 ~0.5s 加载模型 + ~10ms/chunk 推理 |
| 延迟 | ✅ 0ms（纯能量计算） | ⚠️ ~10ms/chunk（FSMN 前向） |

**结论**：FSMN-VAD 在真实场景（有噪声、远场、环境声）的切句精度**显著优于** EnergyVad。Blink 目前的 EnergyVad 适合 MVP，但如果要提升用户体验（减少误切/漏切），FSMN-VAD 是更好的选择。

---

## 六、适配方案与建议

### 方案 A：GGUF worker 改造为真流式（上游路线）

**做什么**：给 GGUF worker 添加 chunk-level + cache 接口，移植 ONNX 的流式逻辑到 ggml。

**改造量**：

| 模型 | 改造点 | 工作量 | 可行性 |
|---|---|---|---|
| FSMN-VAD | 给 `funasr_vad.h` 添加 `FsmnVadOnline` 类（cache 4×128×19 + input_cache + lfr_splice_cache），修改 ggml 图接受 cache 输入/输出 | ~2-3 天 | ✅ 高（cache 机制简单，ONNX 有参考） |
| Paraformer | encoder chunk + cache（最难）、CIF 跨 chunk、decoder FSMN cache | ~1-2 周 | 🟡 中（需要修改 ggml 计算图） |
| SenseVoice | encoder chunk + cache、CTC 增量输出 | ~1-2 周 | 🟡 中 |
| Fun-ASR-Nano | encoder chunk + cache + LLM 持续 KV | ~2-3 周 | 🟡 中偏高 |
| NDJSON 协议 | 新增 `transcribe_chunk` 消息类型 + cache 管理 | ~2-3 天 | ✅ 高 |

**优点**：
- 保持单一 GGUF 技术栈，不需要引入 ONNX runtime 依赖
- 模型常驻，无重复加载
- 上游 roadmap 也计划做（但我们不能等）

**缺点**：
- 需要深度修改 ggml 计算图（非 trivial）
- 上游可能不会很快跟进，维护成本高
- 需要逐模型验证流式推理的数值正确性

### 方案 B：混合架构——GGUF 离线 + ONNX 流式 VAD（推荐）

**做什么**：
- **VAD 层**：引入 `FsmnVadOnline`（ONNX runtime），替换 EnergyVad
- **ASR 层**：保持 GGUF worker 整段推理不变
- **伪流式引擎**：保持 `PseudoStreamingSttEngine` 的预览/定稿双轨架构

```
麦克风音频流
    ↓ 16k mono f32 chunks
FsmnVadOnline (ONNX)           ← 神经网络 VAD，流式 cache
    ↓ speech/silence segments
    ├── 定稿路径: VAD 句尾 → 裁剪句子音频 → GGUF worker → confirmed text
    └── 预览路径: 500ms 定时 → 裁剪未确认音频 → GGUF worker → preview text
```

**优点**：
- ✅ **VAD 质量大幅提升**（神经网络 vs 能量阈值）
- ✅ **改造范围小**——只替换 VAD 组件，ASR 层不动
- ✅ **ONNX FsmnVadOnline 已有完整实现**——直接用上游 C++ 代码
- ✅ **GGUF worker 不需要改**——保持当前的 NDJSON 整段推理协议
- ✅ **可以渐进式**——先只加 VAD，ASR 流式留给未来

**缺点**：
- ⚠️ 引入 ONNX runtime 依赖（~10-20MB DLL）+ VAD ONNX 模型文件（~20MB）
- ⚠️ ASR 仍然是伪流式（预览 = 重新推理整段），不是真流式
- ⚠️ 需要管理两个 runtime（GGUF worker 进程 + ONNX runtime in-process）

**依赖增量**：`onnxruntime.dll`（~15MB）+ `fsmn-vad-onnx` 模型（~5MB Q8 / ~20MB FP32）

### 方案 C：全 ONNX 路线——放弃 GGUF，回到 ONNX

**做什么**：用 FunASR 的 ONNX runtime 替换 GGUF worker，使用 `ParaformerOnline` + `FsmnVadOnline` + 2pass。

**优点**：
- ✅ 上游有完整的流式实现（VAD + ASR + 2pass）
- ✅ 流式延迟 ~600ms（Paraformer streaming chunk_size=[0,10,5]）
- ✅ 精度更高（2pass: online 粗版 + offline 精校）

**缺点**：
- ❌ **放弃 GGUF 的所有优势**（量化、CPU SIMD 优化、单二进制、无 Python）
- ❌ ONNX runtime 比 ggml 在 CPU 上的 ASR 推理慢 ~2-3x
- ❌ 需要重新构建整个 STT 后端
- ❌ ONNX 模型文件更大（SenseVoice FP32 = 936MB vs GGUF Q8 = 254MB）
- ❌ **不符合 Blink 的技术选型决策**（AGENTS.md 明确选了 llama.cpp）

### 方案 D：GGUF VAD-only 流式 + GGUF ASR 伪流式（折中）

**做什么**：
1. 给 GGUF `funasr_vad.h` 添加 `FsmnVadOnline` 类（cache 4×128×19），在 Rust 侧通过 FFI 调用
2. 保持 GGUF ASR worker 的整段推理不变
3. VAD 流式 + ASR 伪流式

```
麦克风音频流
    ↓ 16k mono f32 chunks
FsmnVadOnline (GGUF/ggml)      ← 神经网络 VAD，流式 cache，in-process
    ↓ speech/silence segments
    ├── 定稿路径: VAD 句尾 → 裁剪句子音频 → GGUF ASR worker → confirmed text
    └── 预览路径: 500ms 定时 → 裁剪未确认音频 → GGUF ASR worker → preview text
```

**优点**：
- ✅ **不引入 ONNX 依赖**——保持纯 GGUF/ggml 技术栈
- ✅ **VAD 是神经网络级别**——效果远优于 EnergyVad
- ✅ **VAD 流式推理**——每次只处理新 chunk（~10ms），无累积重算
- ✅ **改造可控**——只改 `funasr_vad.h`（单文件 header-only），Rust 侧 FFI 接入
- ✅ **GGUF ASR worker 不动**——保持稳定的 NDJSON 协议

**缺点**：
- ⚠️ 需要给 `funasr_vad.h` 添加 cache 机制（参考 ONNX 的 `FsmnVadOnline`，工作量 ~2-3 天）
- ⚠️ ASR 仍然是伪流式（预览 = 重新推理），但 VAD 切句更准后，预览的句子边界会更精确
- ⚠️ 需要在 Rust 侧管理 VAD cache 状态（但 cache 结构简单：4×128×19 float + 音频残帧缓存）

---

## 七、推荐方案与实施路径

### 推荐：方案 D → 方案 A（渐进式）

**Phase 1（立即可做，2-3 天）**：方案 D
- 给 `funasr_vad.h` 添加 `FsmnVadOnline` 类
- Rust 侧 FFI 封装 `FsmnVadOnline` 替换 `EnergyVad`
- 保持 ASR 伪流式架构不变
- **收益**：VAD 切句精度大幅提升，消除 EnergyVad 的误切/漏切问题

**Phase 2（中期，1-2 周）**：方案 A 的 VAD 部分
- 将 GGUF VAD 流式推理集成到 worker 进程中
- NDJSON 协议新增 `vad_chunk` 消息类型
- Rust 侧将音频流直接发给 worker，worker 内部做 VAD + ASR

**Phase 3（长期，2-4 周）**：方案 A 的 ASR 部分
- 给 Paraformer GGUF worker 添加 chunk-level encoder + CIF cache
- 实现真流式 ASR（~600ms 延迟）
- 这部分等上游 roadmap 推进后可以对接，或者我们先行实现

### 不推荐：方案 C
- 放弃 GGUF 路线不符合 Blink 技术选型
- ONNX runtime 在 CPU 上的 ASR 性能不如 ggml

---

## 八、技术风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| GGUF VAD cache 改造的数值正确性 | VAD 误判 | 逐帧对比 ONNX `FsmnVadOnline` 输出 |
| ASR 伪流式的 O(n²) 预览问题 | 长句子预览延迟增加 | VAD 切句更准后单句长度可控（<30s） |
| ONNX 依赖膨胀（如果选方案 B） | 安装包增大 | 选方案 D 可避免此问题 |
| 上游 GGUF streaming 实现与我们的改造不兼容 | 维护成本 | Phase 1 的 VAD 改造是独立的 header，上游变更影响可控 |

---

## 九、附录

### A. 文件索引

| 文件 | 作用 |
|---|---|
| `runtime/llama.cpp/funasr-common/funasr_vad.h` | GGUF FSMN-VAD 整段推理（当前） |
| `runtime/llama.cpp/sensevoice/funasr-sensevoice/funasr-sensevoice.cpp` | GGUF SenseVoice worker |
| `runtime/llama.cpp/paraformer/funasr-paraformer/funasr-paraformer.cpp` | GGUF Paraformer worker |
| `runtime/llama.cpp/fun-asr-nano/funasr-cli/funasr-cli.cpp` | GGUF Fun-ASR-Nano worker |
| `runtime/llama.cpp/funasr-common/blink_worker_protocol.h` | NDJSON worker 协议 |
| `runtime/onnxruntime/src/fsmn-vad-online.cpp` | ONNX 流式 VAD 实现 |
| `runtime/onnxruntime/src/fsmn-vad.cpp` | ONNX VAD base（Forward with cache） |
| `runtime/onnxruntime/src/paraformer-online.cpp` | ONNX 流式 Paraformer |
| `runtime/onnxruntime/include/funasrruntime.h` | ONNX C API（含 `FunTpassInferBuffer`） |
| `runtime/websocket/bin/websocket-server-2pass.cpp` | WebSocket 2pass server |
| `src/domain/stt/pseudo_streaming.rs` | Blink 伪流式引擎 |
| `src/domain/stt/vad.rs` | Blink EnergyVad |
| `src/app/local_engine/funasr/gguf.rs` | Blink GGUF 模型配置 + 能力声明 |

### B. FSMN-VAD cache 结构

```
FsmnVadOnline cache:
  in_cache_: 4 × (128 × 19) float  = 4 × 2432 × 4 bytes = 38,944 bytes ≈ 38KB
  input_cache_: 音频残帧（通常 < 400 samples = 25ms @ 16kHz）= ~1.6KB
  lfr_splice_cache_: LFR 拼接缓存（通常 < 2 帧 × 80 × 4 bytes）= ~640 bytes
  reserve_waveforms_: 重叠波形缓存（通常 < 1 chunk = ~9600 samples × 4 = 38.4KB）
  → 总 cache < 100KB，极轻量
```

### C. ParaformerOnline chunk_size 参数

```
chunk_size = [0, 10, 5]  # [left, center, right] in 60ms units
  → center = 10 × 60ms = 600ms per chunk
  → total window = (0+10+5) × 60ms = 900ms (with overlap)
  → encoder_chunk_look_back = 4 (历史 4 chunks = 240ms context)
  → decoder_chunk_look_back = 1 (decoder 历史回看 1 chunk)
  → 典型延迟: ~600ms
```
