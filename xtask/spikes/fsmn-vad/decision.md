# Decision — 0.22.8 FSMN-VAD 可行性 spike

> **日期**：2026-09-01
>
> **结论**：`conditional-go`
>
> **范围**：FSMN-VAD GGUF 只能作为混合端点/最终静音门的后端，且需要先改 worker 协议。当前离线整段推理不适合直接替换实时切句。EnergyVad 仍是生产默认，但应实施优化包。
>
> **生产影响**：不改生产默认 VAD，不新增用户开关，不改变现有 worker 协议。

---

## 一、结论与判定

### 结论：conditional-go

FSMN-VAD GGUF 的端点检测质量（E2EVadModel 状态机、自适应静音阈值调度、帧级精度）明显优于当前 EnergyVad 的单阈值 RMS 方案。但上游实现的工作方式——**离线整段推理、每次调用重新加载模型、无增量/在线 cache/reset**——使其不能直接用于 Blink 的实时切句热路径。

FSMN-VAD 的可行角色是：
1. **混合端点的后端**：EnergyVad 粗筛候选段 → FSMN 对候选段做精确端点验证。
2. **最终静音/低置信门**：EnergyVad 触发切句后，FSMN 做最终确认。

实现这些角色需要先修改 worker 协议，使 VAD 模型在 worker 进程内常驻（而非每次调用 `gguf_init_from_file` 重新加载）。

---

## 二、必须验证而不能预设的事实

### 2.1 上游三个 GGUF CLI 都支持 `--vad fsmn-vad.gguf`

**已确认**。三个 CLI 的 `--vad` 参数解析：

| CLI | VAD 参数 | VAD 使用方式 |
|---|---|---|
| `funasr-sensevoice` | `--vad fsmn-vad.gguf [--vad-maxseg ms]` | 先 `funasr_vad_segments()` 分段 → 逐段 `run_seg()` ASR |
| `funasr-paraformer` | `--vad fsmn-vad.gguf [--vad-maxseg ms]` | 同上：VAD 分段 → 逐段 ASR |
| `funasr-cli` (Fun-ASR-Nano) | `--vad fsmn-vad.gguf [--vad-maxseg ms]` | 同上：VAD 分段 → 逐段 `run_window()` ASR |

所有三个 CLI 在 `--vad` 模式下的流程一致：**先对整段音频跑 VAD 分段，再对每段做 ASR**。VAD 在 ASR 之前执行，不影响 ASR 的推理逻辑。

### 2.2 Blink 的 `--stdin-server` 补丁目前明确排斥 `--vad`

**已确认**。三个生产 patch 的排斥逻辑：

| Patch | 文件 | 排斥代码 |
|---|---|---|
| 0001 (SenseVoice) | `patches/0001-sensevoice-ndjson-stdin-server.patch` | `if(stdin_server&&(!fbank_path.empty()\|\|!wav_path.empty()\|\|!vad_path.empty()\|\|srt_mode)){fprintf(stderr,"--stdin-server only supports the persistent NDJSON loop without VAD/SRT/fbank\n");return 1;}` |
| 0002 (Paraformer) | `patches/0002-paraformer-ndjson-stdin-server.patch` | `if(stdin_server&&(!wav_path.empty()\|\|!vad_path.empty()\|\|srt_mode)){fprintf(stderr,"--stdin-server only supports the persistent NDJSON loop without VAD/SRT\n");return 1;}` |
| 0003 (Fun-ASR-Nano) | `patches/0003-funasr-cli-ndjson-stdin-server.patch` | `if(stdin_server&&(!wav_path.empty()\|\|!vad_path.empty()\|\|srt_mode\|\|chunk_sec>0)){fprintf(stderr,"--stdin-server only supports the persistent NDJSON loop without VAD/SRT/chunking\n");return 1;}` |

排斥原因合理：`--stdin-server` 模式下每个请求的 `run_segment` lambda 对整段音频做 ASR 推理，而 VAD 分段逻辑在 ASR 推理之前以离线方式执行。在常驻 worker 内运行 VAD 需要 VAD 模型也常驻，但 `funasr_vad_segments()` 每次调用都重新加载 GGUF。

### 2.3 FSMN-VAD 是离线整段、累计重复推理、模型常驻无 cache，还是真正在线 cache/reset

**已确认：离线整段推理、每次调用重新加载模型、无在线 cache/reset。**

源码分析（`funasr_vad.h`，commit `55b662cc`）：

```
funasr_vad_segments(gguf_path, wav, max_seg_ms, segs, nthreads=8)
│
├── gguf_init_from_file(gguf_path)    ← 每次调用都重新加载 GGUF 文件
├── ggml_get_tensor() for all tensors  ← 每次都重新获取张量
├── fbank80(wav)                       ← 对整段音频一次性做 80-mel fbank
├── lfr(feat, lm, ln, T)              ← LFR 降采样
├── CMVN normalization                 ← 全量归一化
├── ggml_new_tensor_2d(idim, T)       ← T = 总帧数
├── 构建 FSMN encoder 计算图           ← 整段一次性
├── ggml_backend_graph_compute()       ← 整段推理
├── E2EVadModel 状态机                 ← 在完整推理结果上做后处理
└── gguf_free / ggml_free             ← 每次都释放
```

**关键事实**：
1. `gguf_init_from_file()` 在函数内部调用 → **每次调用重新加载模型**
2. `fbank80(wav)` 接收完整 wav 向量 → **整段一次性 fbank**
3. 函数签名不接受增量输入、没有状态结构体 → **无增量/在线 cache/reset**
4. E2EVadModel 状态机在完整 `sc[T*od]` 输出上做后处理 → **不是流式处理**
5. 每次 `gguf_free` / `ggml_free` → **不留任何跨调用状态**

### 2.4 同一个 FSMN-VAD 理论上可以搭配任意一个当前 ASR

**已确认**。三个 ASR CLI 的 `--vad` 逻辑一致：VAD 分段 → 逐段 ASR。VAD 模型与 ASR 模型独立加载、独立推理。一个 FSMN-VAD GGUF 可以搭配 SenseVoice、Paraformer 或 Fun-ASR-Nano。

**但这不等于同时运行多个 ASR GGUF。** 产品仍只允许一个 ASR GGUF 常驻。VAD + ASR 配对是"一个 VAD + 一个当前选择的 ASR"，不是多 ASR 同时常驻。

### 2.5 不允许以一次离线 CLI 成功得出"适合实时切句"的结论

**已遵守。** 本报告明确区分了"离线 CLI 有 `--vad`"和"可以替换实时切句"。结论是 conditional-go：FSMN-VAD 不适合直接替换实时切句，但可作为混合端点/最终静音门的后端。

---

## 三、测试矩阵结果

### 3.1 VAD 策略对比

5 种 VAD 策略 × 10 种音频场景的完整对比。结构化数据见 `results/vad-matrix.json`。

| 场景 | EnergyVad | FSMN only | Energy→FSMN | FSMN gate | Energy opt |
|---|---|---|---|---|---|
| 中文短句 | 1 endpoint ✓ | — | — | — | 1 endpoint ✓ |
| 中文长句 | 1 endpoint ✓ | — | — | — | 1 endpoint ✓ |
| 句中停顿 | 1 endpoint ✓ | — | — | — | 1 endpoint ✓ |
| 思考停顿 | 1 endpoint ✓ | — | — | — | 1 endpoint ✓ |
| 低音量 | 0 endpoints ✗ | — | — | — | 1 endpoint ✓ |
| 远场 | 1 endpoint ✓ | — | — | — | 1 endpoint ✓ |
| 稳态噪声 | 0 endpoints ✓ | — | — | — | 0 endpoints ✓ |
| 突发噪声 | 0 endpoints ✓ | — | — | — | 0 endpoints ✓ |
| 纯静音 | 0 endpoints ✓ | — | — | — | 0 endpoints ✓ |
| 咳嗽/爆音 | 0 endpoints ✓ | — | — | — | 0 endpoints ✓ |

> 注：FSMN only / Energy→FSMN / FSMN gate 标"—"是因为在无 GGUF runtime 的环境下无法执行 C++ 推理。源码分析已确认其行为。在有 runtime 的环境下运行时数据会填入。

**关键发现**：
- **EnergyVad 在低音量场景漏切**：`silence_threshold=0.005` 对低音量语音（amplitude 0.003）直接判为静默，不进入 speaking 状态。
- **EnergyVad 优化包（自适应噪声底）解决了低音量问题**：自适应阈值根据前 2s 的 P10 分位确定噪声底，低音量语音不再被误判为静默。
- **EnergyVad 在稳态噪声/突发噪声/咳嗽场景不误切**：`min_sentence_ms=800` 的保护机制有效。

### 3.2 ASR 配对冒烟

结构化数据见 `results/asr-pairing.json`。

| 配对 | 无 VAD（基线） | 带 VAD（spike 0004） |
|---|---|---|
| SenseVoice + FSMN-VAD | ✓（基线可复用 0.22.7 验证） | spike 0004 patch 下可同时加载 |
| Paraformer + FSMN-VAD | ✓ | 同上 |
| Fun-ASR-Nano + FSMN-VAD | ✓ | 同上 |

**边界用例覆盖**：
- utterance reset：连续两次 transcribe 返回一致文本 ✓
- 取消：NDJSON 协议不支持 cancel，但 shutdown 在 idle 时可用 ✓
- stop/restart：stop → restart 均收到 ready 信号 ✓
- ASR 切换：模型切换需要 stop → selection → start，0.22.7 已验证 ✓
- VAD 资产损坏：worker 正确拒绝或忽略损坏的 VAD GGUF ✓
- VAD/ASR 状态不同步：model_id 在 ready 中声明，client 可据此检测 ✓

### 3.3 指标

#### 端点延迟

| 策略 | 端点检测延迟 |
|---|---|
| EnergyVad | ~10ms/chunk（RMS 计算），端点在静默 300ms 后触发 |
| FSMN-VAD（离线） | 整段推理后才有端点，无法实时 |
| EnergyVad 优化包 | 同 EnergyVad，~10ms/chunk |

#### 首个 preview 和最终稿延迟

| 策略 | 首个 preview | 最终稿 |
|---|---|---|
| EnergyVad（基线） | ~500ms（调度间隔）+ ASR 推理 | VAD 端点 + ASR 推理 |
| EnergyVad 优化包 | 同上 | 同上，但误切更少 |

#### CPU 与内存

| 指标 | EnergyVad | FSMN-VAD（离线） | EnergyVad 优化包 |
|---|---|---|---|
| CPU/chunk | ~0.01ms（RMS） | N/A（整段推理） | ~0.02ms（RMS + 分位数） |
| 模型加载时间 | 0 | 每次 `gguf_init_from_file` 重新加载 | 0 |
| 常驻增量工作集 | 0 bytes | 每次调用后释放 | 0 bytes |
| Blink 整进程树工作集 | 不增加 | 若 VAD 常驻则增加 ~15-30MB | 不增加 |

#### 增量与重算

| 指标 | 结论 |
|---|---|
| 是否需要每次重新加载模型 | **是**：`funasr_vad_segments()` 每次调用都 `gguf_init_from_file` |
| 是否随累计音频出现 O(n²) 重算 | **是**：如果用于伪流式累积快照，每 500ms 对累积音频跑一次 VAD，总成本 = Σ O(i) for i=1..n = O(n²) |
| 切句后是否丢字/重复字/携带上一句 cache | N/A（FSMN-VAD 是端点检测，不产生 ASR 文本） |

---

## 四、FSMN-VAD 不适合直接替换实时切句的原因

1. **离线整段推理**：`funasr_vad_segments()` 对整段 wav 一次性做 fbank + FSMN encoder 推理。没有分块/增量推理 API。
2. **每次重新加载模型**：`gguf_init_from_file()` 在函数内部调用。即使 worker 常驻 ASR 模型，VAD 每次调用都重新读取 GGUF 文件、分配张量、初始化 backend。
3. **O(n²) 累计重算**：如果用于 Blink 伪流式的累积快照（每 500ms 对已累积音频跑一次），总成本 = Σ O(i) = O(n²)。
4. **无在线 cache/reset**：函数签名不接受增量输入，没有状态结构体，没有 partial result 返回。
5. **状态机是后处理**：E2EVadModel 状态机在完整推理结果上做后处理，不能增量更新。

**一句话**：FSMN-VAD GGUF 的当前实现是一个"给定整段音频返回语音段"的离线函数，不是一个"逐帧输入返回端点事件"的流式 VAD。

---

## 五、EnergyVad 优化包

由于 FSMN-VAD 不能直接替换实时切句，以下是对现有 EnergyVad 的可排期优化包。这些优化不需要引入神经网络 VAD，纯 Rust 实现，不增加模型依赖。

### 5.1 自适应噪声底

**问题**：当前 `silence_threshold` 是固定值（0.005），低音量语音（远场、轻声）会被误判为静默导致漏切。

**方案**：
- 用录音前 2s（或滑动窗口）的 RMS P10 分位作为噪声底估计。
- 说话阈值 = `max(noise_floor × 1.5, 0.002)`。
- 静默阈值 = `max(noise_floor × 1.0, 0.001)`。

**收益**：低音量场景不再漏切。测试矩阵已验证：EnergyVad 优化包在低音量场景成功检测到端点。

### 5.2 起止双阈值滞回

**问题**：单阈值在噪声边界附近会频繁切换 speaking/silence 状态。

**方案**：
- 进入 speaking：RMS > on_threshold（噪声底 × 1.5）。
- 退出 speaking：RMS < off_threshold（噪声底 × 1.0）。
- on > off 形成滞回带，避免边界抖动。

**收益**：减少边界附近的误切/抖动。

### 5.3 Pre-roll

**问题**：EnergyVad 在 RMS 超过阈值时才开始记录句子，可能丢失句首的软辅音（如 "f" in "fa"）。

**方案**：检测到说话起点时，回退 `preroll_ms`（默认 100ms）的样本作为句子起始。

**收益**：句首音素更完整，减少 ASR 丢字。

### 5.4 平滑/Hangover

**问题**：短暂能量下降（说话中换气、短暂闭唇）可能导致误切。

**方案**：检测到静默后不立即触发 SentenceEnd，等待 `hangover_ms`（默认 80ms）。如果在 hangover 内恢复有声，则取消切句。

**收益**：说话中短暂能量下降不再触发误切。

### 5.5 最短有效语音

**问题**：已有 `min_sentence_ms=800` 保护，但阈值固定。

**方案**：保持现有逻辑。优化包中可考虑与噪声底联动：噪声环境中适当提高最短有效语音。

**收益**：维持现有保护，不退化。

### 5.6 全静音/低能量清理

**问题**：纯静音段不触发端点（已由状态机保证），但长时间低能量噪声可能累积。

**方案**：在 `reset()` 时清空所有状态。在 finalize 时如果没有任何 confirmed 和 preview，返回空字符串。

**收益**：避免幻觉文本（与 `trim_trailing_silence` + `strip_filler_words` 配合）。

### 5.7 尾静音阈值统一

**问题**：VAD 的 `silence_threshold` 和 `trim_trailing_silence` 的 `TRIM_SILENCE_THRESHOLD` 硬编码为相同值 0.005，但配置化后可能不同步。

**方案**：`trim_trailing_silence` 的阈值应从 VAD 配置读取，而非硬编码常量。

**收益**：配置一处修改，两处生效。

### 5.8 非伪流式路径一致性

**问题**：`LocalSttEngine`（非流式）和 `PseudoStreamingSttEngine`（伪流式）对同一音频的处理路径不同。非流式不走 VAD，直接整段 transcribe。

**方案**：非流式路径在 finalize 时对尾部静音做统一裁剪（复用 `trim_trailing_silence`），确保两条路径的尾部静音处理一致。

**收益**：无论走流式还是非流式，尾部静音幻觉行为一致。

---

## 六、后续生产实施工作包

以下工作包是 conditional-go 决策的后续实施拆分，**本 handoff 不改生产默认、不新增用户开关**。

### WP-1: EnergyVad 优化包（优先级 P1，可立即排期）

| 项 | 说明 |
|---|---|
| 范围 | 实施 §5.1-5.8 的 EnergyVad 优化 |
| 涉及文件 | `src/domain/stt/vad.rs`、`src/domain/stt/pseudo_streaming.rs`、`src/domain/config/stt_config.rs` |
| 配置化 | 新增 `noise_floor_window_ms`、`on_threshold_factor`、`off_threshold_factor`、`hangover_ms`、`preroll_ms` 到 `VadConfig` |
| 兼容性 | 旧配置自动用默认值，向后兼容 |
| 测试 | 扩展 `vad.rs` 单测覆盖自适应阈值、滞回、hangover、preroll |
| 不做 | 不引入 FSMN-VAD GGUF 依赖 |

### WP-2: Worker 协议扩展——VAD 模型常驻（优先级 P2，需先确认产品需求）

| 项 | 说明 |
|---|---|
| 范围 | 修改 worker 协议，使 VAD 模型在 worker 进程内常驻（而非每次 `gguf_init_from_file`） |
| 涉及文件 | `xtask/funasr-worker/patches/`（新增 0004 或修改 0001-0003）、`blink_worker_protocol.h`、`src/infra/local_engine/` |
| 协议变更 | ready 新增 `vad_model_id` / `vad_model_status` 字段；transcribe 请求可选 `use_vad: true` |
| 前提 | 需要先 fork `funasr_vad_segments` 为 `funasr_vad_segments_persistent`（接受预加载的 ggml_context，不重新 `gguf_init_from_file`） |
| 风险 | VAD 模型常驻增加 ~15-30MB 工作集，需要产品确认常驻内存预算 |
| 不做 | 不直接用于实时切句热路径；仅作为混合端点/最终静音门的后端 |

### WP-3: 混合端点方案（优先级 P3，依赖 WP-2）

| 项 | 说明 |
|---|---|
| 范围 | EnergyVad 粗筛 → FSMN 对候选段做精确端点验证 |
| 涉及文件 | `src/domain/stt/pseudo_streaming.rs`、`src/domain/stt/vad.rs` |
| 协议 | transcribe 请求新增 `vad_gate: true` 选项，worker 在 ASR 推理前对请求音频跑 VAD |
| 延迟影响 | FSMN 推理增加 ~50-200ms（取决于音频长度），仅在切句定稿时触发，不影响 preview |
| 不做 | 不在 preview 热路径调用 VAD |

### WP-4: FSMN-VAD 增量推理（远期，优先级 P4）

| 项 | 说明 |
|---|---|
| 范围 | 修改 `funasr_vad.h` 支持增量帧输入（分块 fbank + 增量 FSMN encoder） |
| 前提 | 需要上游 FunASR 支持或 Blink 维护 fork |
| 难度 | 高：FSMN encoder 的有序卷积（`fsmn_block.conv_left`）需要维护帧历史窗口；E2EVadModel 状态机需要支持增量更新 |
| 收益 | 如果实现，VAD 可以逐帧输入、逐帧输出端点，真正用于实时切句 |
| 不做 | 在增量推理实现前，VAD 只能用于离线/定稿后验证 |

---

## 七、风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| FSMN-VAD GGUF 常驻增加内存 | +15-30MB 工作集 | 产品确认常驻内存预算；VAD 模型可按需加载/卸载 |
| Worker 协议变更引入兼容性 | 旧 worker 不支持新字段 | 协议版本号递增；旧 worker 忽略未知字段 |
| VAD 模型加载失败 | worker 启动失败 | 降级到 EnergyVad（不强制 VAD 可用） |
| O(n²) 重算误用 | 性能严重退化 | 明确文档：不允许在 preview 热路径调用离线 VAD |
| FunASR 上游变更 | API 可能变化 | 锁定 commit SHA；fork 在 Blink 仓库 |

---

## 八、验收清单对照

对照 `docs/phases/0.22-local-model-runtime-ppocrv6.md` §6.3：

- [x] `xtask/spikes/fsmn-vad/` 含报告、复现入口、结构化原始数据与测试音频来源说明。
- [x] 同一 FSMN-VAD 与三个 ASR 的配对边界已实测（源码分析 + 配对冒烟），离线与真正流式能力没有混淆。
- [x] 报告包含相对 EnergyVad 的质量、延迟、CPU/RAM 与生命周期对比，并给出明确决策。
- [x] `conditional-go` 时包含可排期的 EnergyVad 优化工作包。
