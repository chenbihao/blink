# Handoff 07 — 0.22.9 Production Gate 前置条件评估报告

> **生成时间**：2026-09-03
>
> **状态**：BLOCKED — 无法执行真实生产资格测试
>
> **原则**：未测项目不得写成通过。不降低阈值换取 GO。不注册模型。不改变默认模型/VAD。

---

## 一、结论：BLOCKED

**当前无法执行 Handoff 07 所要求的真实、可复现的生产资格测试。**

原因不是测试框架未搭建，而是多个生产链路关键组件尚未实现或不存在，导致测试矩阵中的多数组合无法运行真实推理。

---

## 二、前置条件评估

### 2.1 测试矩阵可行性

| 组合 | VAD | ASR | 可执行？ | 阻塞原因 |
|---|---|---|---|---|
| 1 | EnergyVad | Nano GGUF | ⚠️ 需验证 | GGUF worker binary 可能存在但未确认安装状态 |
| 2 | FSMN-VAD | Nano GGUF | ❌ | FSMN-VAD ONNX 实现是骨架占位（`run_vad_inference` 返回 `VadEvent::None`），无真实推理 |
| 3 | EnergyVad | ParaformerOnline ONNX | ❌ | 生产 ParaformerOnline worker 二进制不存在 |
| 4 | FSMN-VAD | ParaformerOnline ONNX | ❌ | 两者均不可用 |
| 回归 | — | SenseVoice/Paraformer-zh GGUF | ⚠️ 需验证 | 同组合 1 |

### 2.2 关键缺失组件

#### 缺失 1：FSMN-VAD ONNX 推理实现

`src/infra/stt/fsmn_vad_onnx.rs` 中的 `run_vad_inference` 方法是占位实现：

```rust
fn run_vad_inference(
    _session: &ort::session::Session,
    _config: &FsmnVadConfig,
    _samples: &[f32],
) -> VadEvent {
    // TODO(0.22.9 后续 handoff): 实现 fbank 前处理 → FSMN encoder → E2EVadModel 状态机
    // 当前为占位实现，返回 None（安全默认值，不切句）
    VadEvent::None
}
```

**影响**：FSMN-VAD 永远返回 `VadEvent::None`，不产生任何端点检测事件。无法测量句界 precision/recall/F1，无法比较与 EnergyVad 的 CER 差异。组合 2 和 4 无法执行。

**Spike 证据**：Spike D 已用 Python 脚本验证了 FSMN-VAD ONNX 的推理可行性（F1=0.708 vs EnergyVad F1=0.717），但该结果来自 Python 脚本，不是 Blink 内的 Rust 实现。Spike 数据不能直接作为生产 gate 证据。

#### 缺失 2：ParaformerOnline 生产 Worker 二进制

当前代码状态：
- `stream_worker_proto.rs` — 二进制协议 v2 已实现（帧编解码、消息类型、FakeWorker）
- `streaming_stt_adapter.rs` — `ParaformerOnlineAdapter` 已实现，但依赖 `StreamWorkerClient`
- `stream_worker_lifecycle.rs` — 生命周期测试使用 `FakeWorker`（模拟），不 spawn 真实子进程
- `providers/paraformer_onnx.rs` — Provider 负责下载和 self-test，但不启动常驻 worker

**不存在的东西**：一个编译好的 ParaformerOnline ONNX worker 二进制文件（类似 GGUF 的 `funasr-paraformer-worker.exe`），能够：
1. 加载 ORT DLL + encoder/decoder ONNX 模型
2. 通过二进制协议 v2 与 host 通信
3. 接收 PCM 音频帧，产生 Partial/Final 结果

Spike E 的 `onnx-spike-e.exe` 是 spike 验证程序，使用 NDJSON 协议（非二进制 v2），不是生产 worker。

**影响**：组合 3 和 4 无法执行。无法测量 first-partial p50/p95、final-after-release p95、RTF。

#### 缺失 3：真实测试语料

§3.12 要求：
> 使用带参考文本和句界标注的真实语音。至少覆盖近讲、远场、背景噪声、短句、长句、句中停顿、多句。

当前可用音频：
- `xtask/spikes/onnx-spike/models/asr_example.wav` — 1 个 10 秒 SAPI 合成音频，无参考文本，无句界标注
- `xtask/spikes/fsmn-vad/fixtures/audio/` — 合成正弦波/白噪声，不含人声录音

**影响**：无法可靠计算 CER 和句界 precision/recall/F1。语料数量不足以计算 p95。

§3.12 明确要求：
> 若真实语料数量不足以可靠计算 CER/p95，明确报告不足，不用正弦波或合成音频冒充质量 gate。

#### 缺失 4：实时节奏音频投喂机制

§3.12 要求：
> 输入按真实时间节奏投喂，禁止快速灌 WAV 冒充交互延迟。

当前无机制将 WAV 文件按 16kHz 实时节奏（每 10ms 投送 160 samples）喂给生产 STT 管线。Spike 脚本使用 Python 整段加载后分段调用，不是生产路径的实时投喂。

---

## 三、已有资产清单

### 3.1 模型文件（已下载）

| 模型 | 路径 | 大小 | 状态 |
|---|---|---|---|
| FSMN-VAD ONNX v2 | `models/fsmn-vad-onnx-v2/` | 506KB | ✅ 可用 |
| ParaformerOnline ONNX | `models/paraformer-online-onnx/` | ~226MB | ✅ 可用 |
| Paraformer-zh ONNX | `models/paraformer-zh-onnx/` | ~227MB | ✅ 可用 |
| asr_example.wav | `models/asr_example.wav` | 312KB | ⚠️ 合成音频，不满足语料要求 |

### 3.2 ORT Runtime（已下载）

| 文件 | 路径 | 大小 |
|---|---|---|
| onnxruntime.dll (CPU-only 1.19.2) | `followup/runtimes/onnxruntime-cpu/` | 10.7MB |
| onnxruntime_providers_shared.dll | 同上 | 22KB |

### 3.3 Spike 验证程序（已构建）

| 程序 | 路径 | 用途 |
|---|---|---|
| onnx-spike-e.exe | `followup/spike-e-worker/target/release/` | NDJSON 协议 spike worker |
| spike-c-rust | `followup/spike-c-rust/` | Rust kaldi-native-fbank 验证 |

### 3.4 代码实现状态

| 组件 | 文件 | 状态 |
|---|---|---|
| VadFrontend trait | `src/domain/stt/vad_port.rs` | ✅ 已实现（Handoff 06） |
| EnergyVadAdapter | `src/infra/stt/energy_vad_adapter.rs` | ✅ 已实现 |
| FsmnVadOnnx | `src/infra/stt/fsmn_vad_onnx.rs` | ⚠️ 骨架——`run_vad_inference` 是占位 |
| 二进制协议 v2 | `src/infra/local_engine/stream_worker_proto.rs` | ✅ 已实现+测试 |
| ParaformerOnlineAdapter | `src/infra/local_engine/streaming_stt_adapter.rs` | ✅ 已实现+测试（FakeWorker） |
| ParaformerOnnxProvider | `src/infra/local_engine/providers/paraformer_onnx.rs` | ✅ 已实现（下载+self-test） |
| paraformer-selftest CLI | `src/cli/paraformer_selftest.rs` | ✅ 已实现 |
| stt_config vad_kind | `src/domain/config/stt_config.rs` | ✅ 已实现（Handoff 06） |
| 生产 ParaformerOnline worker binary | — | ❌ 不存在 |

### 3.5 Spike 证据（不可作为生产 gate 数据）

| Spike | 结论 | 数据文件 |
|---|---|---|
| Spike C2 Python | RTF=0.41x, 首partial=2.575s | `results/spike_c2_paraformer_online.json` |
| Spike C2 Rust KNF | RTF=0.16x, 首partial=0.174s | `results/spike_c2_rust_knf.json` |
| Spike D VAD/ASR 矩阵 | FSMN F1=0.708 vs Energy F1=0.717 | `results/spike_d_vad_asr_matrix.json` |
| Spike E Topology | kill+wait+restart 通过 | `results/spike_e_topology_comparison.json` |

> **重要**：以上数据来自 spike 脚本，不是 Blink 生产路径。§3.12 要求"spike 中未测或仅用于 feasibility 的指标不得直接写成 release 验收事实。"

---

## 四、§3.12 注册门对照

| 门禁项 | 要求 | 当前状态 |
|---|---|---|
| >1.2s 有效语音句尾前产生非空 partial | 真实 worker 推理 | ❌ 无法执行 |
| first-partial p50 ≤ 400ms | 真实时间节奏 | ❌ 无法执行 |
| first-partial p95 < 800ms | 同上 | ❌ 无法执行 |
| final-after-release p95 ≤ 800ms | 真实 worker | ❌ 无法执行 |
| RTF p95 < 0.8 | 真实 worker | ❌ 无法执行 |
| 中文 CER 相对 Nano 恶化 ≤1pp | 真实语料+参考文本 | ❌ 无语料 |
| 100x start/stop/cancel 零 orphan/死锁/泄漏 | 真实 worker | ❌ 无 worker binary |
| 10x kill/restart 零 orphan/死锁/泄漏 | 真实 worker | ❌ 无 worker binary |

## 五、§3.12 FSMN-VAD 采用门对照

| 门禁项 | 要求 | 当前状态 |
|---|---|---|
| 下游 CER 相对 EnergyVad 恶化 ≤0.5pp | 真实 FSMN-VAD 推理 | ❌ `run_vad_inference` 返回 None |
| 句界 F1 不低于 EnergyVad 超过 0.02 | 真实 FSMN-VAD 推理 | ❌ 同上 |

---

## 六、下一步建议

### 6.1 解除阻塞需要的最小工作集

1. **实现 FSMN-VAD ONNX 推理**：将 Spike C2 Rust 的 fbank/LFR/CMVN、FSMN encoder 推理和 E2EVadModel 状态机移植到 `fsmn_vad_onnx.rs` 的 `run_vad_inference` 方法中。Spike C2 Rust 源码位于 `followup/spike-c-rust/src/main.rs`（~600 行），可直接参考。

2. **构建生产 ParaformerOnline worker binary**：将 Spike E worker 扩展为使用二进制协议 v2 的生产 worker，或新建一个 worker 项目。需要：
   - 加载 ORT DLL + encoder/decoder 模型
   - 实现二进制协议 v2 的 worker 端（帧解析、消息处理）
   - 实现连续 PCM 音频的 fbank → LFR → CMVN → encoder → CIF → decoder 推理
   - 产生 Partial/Final 消息

3. **准备真实测试语料**：录制或获取带参考文本和句界标注的中文语音语料，覆盖：
   - 近讲、远场、背景噪声
   - 短句、长句、句中停顿、多句
   - 至少 20+ 条样本以支持 p95 计算

4. **实现实时节奏音频投喂工具**：构建一个测试工具，将 WAV 文件按 16kHz 实时节奏（160 samples/10ms）投喂给生产 STT 管线。

### 6.2 可先行执行的项目

以下项目不依赖缺失组件，可先行执行：

| 项目 | 依赖 | 可行性 |
|---|---|---|
| GGUF 三模型回归冒烟 | 需 GGUF worker 已安装 | ⚠️ 需确认安装状态 |
| 100x start/stop/cancel（GGUF） | 同上 | ⚠️ 同上 |
| 10x kill/restart（GGUF） | 同上 | ⚠️ 同上 |
| EnergyVad 单测（已有） | 无 | ✅ 已通过 |
| 二进制协议 v2 单测（已有） | FakeWorker | ✅ 已通过 |
| ParaformerOnlineAdapter 单测（已有） | FakeWorker | ✅ 已通过 |

---

## 七、禁止事项确认

- ✅ 未降低阈值换取 GO
- ✅ 未注册模型
- ✅ 未改变默认模型/VAD
- ✅ 未改生产 UI 或文档
- ✅ 未测项目未写成通过
- ✅ 未用合成音频冒充质量 gate

---

## 八、关联文件

| 文件 | 说明 |
|---|---|
| `docs/phases/0.22-local-model-runtime-ppocrv6.md` §3.12 | 生产门禁规范 |
| `xtask/spikes/onnx-spike/followup/decision.md` | Spike C/D/E 完整决策报告 |
| `xtask/spikes/fsmn-vad/decision.md` | FSMN-VAD 可行性 spike |
| `src/infra/stt/fsmn_vad_onnx.rs` | FSMN-VAD 骨架实现 |
| `src/infra/local_engine/stream_worker_proto.rs` | 二进制协议 v2 |
| `src/infra/local_engine/streaming_stt_adapter.rs` | ParaformerOnlineAdapter |
| `src/infra/local_engine/providers/paraformer_onnx.rs` | Provider（下载+self-test） |
| `followup/spike-c-rust/src/main.rs` | Rust ParaformerOnline 推理参考实现 |
| `followup/spike-e-worker/src/main.rs` | Spike E worker 参考 |
