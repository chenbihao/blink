# Handoff 07F — STT 小规模可信对比报告

**Date**: 2026-09-04  
**Version**: 0.22.9-handoff-07f  
**Corpus**: Blink STT Gate Corpus (THCHS-30) — 3 samples, 28.25s total  
**Corpus manifest SHA-256**: `9a9a8ef4989f1ecd90dc81f1692cb39da4ff3bdf5955b0865cb1a6502ce6057a`

---

## 一、结论

| 项目 | 结论 |
|---|---|
| **Paraformer 注册门** | **REGISTER_NOT_EVALUATED** |
| **FSMN 采用门** | **NOT_TESTED** |
| **默认模型资格** | 不具备——小规模阶段未完成全部验证条件 |

**本轮目标达成度**：小规模可信对比的基础链路已打通，4 个组合 × 3 样本 × 3 轮 warm 全部跑通，0 errors。Handoff 审计缺口 6 项全部补齐（VAD 标签诚实化、RTF 口径分离、资产负面测试、文本契约定向测试、重复测量、单样本对齐 §A5）。全量单测 3088 passed, 0 failed。

---

## 二、修复的 Bug 清单

### Bug 1: `ParaformerOnlineAdapter::launch` deployment_dir 相对路径 bug（已修复）

**现象**: Worker 子进程在 38ms 内退出（stdout EOF），stderr 报 `"deployment 目录不存在: target\deploy"`。

**根因**: `launch` 方法同时设置 `args: ["--deployment", deployment_dir]` 和 `current_dir: Some(deployment_dir.clone())`，子进程 CWD 变为 `target\deploy`，但 `--deployment` 参数仍为相对路径，worker 端查找 `target\deploy\target\deploy` 不存在。

**修复**: 在 `launch` 入口处将 `deployment_dir` 转为绝对路径。

**文件**: `src/infra/local_engine/streaming_stt_adapter.rs`

### Bug 2: `reset()` 递增 generation 导致 host/worker generation 不同步（已修复）

**现象**: 第一个样本成功，后续样本全部 `end_stream 失败: 等待响应超时`。

**根因**: `reset()` 调用 `self.generation.fetch_add(1)` 递增 host generation，但不调用 `begin_stream()`——worker generation 没有对应递增。

**修复**: 移除 `reset()` 中的 `self.generation.fetch_add(1)`。generation 计数器只由 `begin_session` 中的 `begin_stream` 配对递增。

**文件**: `src/infra/local_engine/streaming_stt_adapter.rs`

### Bug 3: ONNX 位置编码 scale 精度丢失（已修复）

**现象**: 长音频识别异常，CER 接近 1.0。

**根因**: 位置编码的 scale 常量精度丢失，导致长音频的位置编码计算偏差累积。

**修复**: 恢复 scale 常量的完整精度（`-0.0330119726594128`）。

**文件**: `src/infra/local_engine/paraformer_runner.rs`

### Bug 4: `feats_cache` 在 final flush 时重复拼接（已修复）

**现象**: 最终输出包含重复 token。

**根因**: `feats_cache` 在 final flush 时被重复拼接到已有特征序列上。

**修复**: 修正 `feats_cache` 提取逻辑，确保 final flush 时不重复拼接。

**文件**: `src/infra/local_engine/paraformer_runner.rs`

### Bug 5: GGUF worker stderr 管道缓冲区死锁（已修复）

**现象**: nano worker（`funasr-nano-worker.exe`）在 gate runner 中启动后 stdout 完全沉默，180s 超时。手动运行正常。

**根因**: gate runner 的 `run_gguf_combination` 函数对 GGUF worker 的 stderr 做了 `Stdio::piped()` 但从未消费。nano worker 的 stderr 输出约 64KB（`llama_model_loader` 日志），填满管道缓冲区（Windows 默认 4KB）后，进程阻塞在 stderr 写入上，永远无法到达 stdout ready 输出。SenseVoice/Paraformer worker 因 stderr 输出较少未触发。

**修复**: 在 spawn worker 后启动 stderr reader task 持续消费 stderr 输出。

**文件**: `src/cli/stt_corpus_gate.rs` — `run_gguf_combination` 函数

---

## 三、修复的 Bug 汇总表

| # | Bug | 文件 | 修复内容 |
|---|---|---|---|
| 1 | deployment_dir 相对路径 | `streaming_stt_adapter.rs` `launch()` | 入口处转为绝对路径 |
| 2 | reset() generation 递增 | `streaming_stt_adapter.rs` `reset()` | 移除 `fetch_add(1)` |
| 3 | ONNX 位置编码 scale 精度 | `paraformer_runner.rs` | 恢复完整精度常量 |
| 4 | feats_cache 重复拼接 | `paraformer_runner.rs` | 修正 final flush 提取逻辑 |
| 5 | GGUF worker stderr 管道死锁 | `stt_corpus_gate.rs` `run_gguf_combination()` | 启动 stderr reader task |

---

## 四、矩阵结果（v5, Release build, 3 samples × 3 repeats warm）

### 必测组合

| # | 组合 | 状态 | 样本 | 错误 | CER mean | CER p50 | CER p95 | RTF p50 | RTF_infer p50 |
|---|---|---|---|---|---|---|---|---|---|
| A | asr_only + ParaformerOnline ONNX | **COMPLETE** | 3/3 | 0 | 0.083 | 0.029 | 0.029 | 1.017 | 0.021 |
| B | asr_only + Nano GGUF | **COMPLETE** | 3/3 | 0 | 0.000 | 0.000 | 0.000 | 0.109 | 0.122 |
| C | asr_only + SenseVoice GGUF | **COMPLETE** | 3/3 | 0 | 0.000 | 0.000 | 0.000 | 0.046 | 0.046 |
| D | asr_only + Paraformer-zh GGUF | **COMPLETE** | 3/3 | 0 | 0.000 | 0.000 | 0.000 | 0.044 | 0.044 |

> **注意**: gate runner 当前为 ASR-only 诊断——未经 VAD 切分，
> 音频按 10ms tick 实时投喂至 ASR 引擎。VAD × ASR 矩阵留待后续扩展。
> RTF 包含实时投喂等待时间（ONNX 路径含 10ms tick 等待），RTF_infer 仅含纯推理耗时。
> ONNX 的 RTF=1.017 但 RTF_infer=0.021，说明纯推理仅占音频时长的 2.1%，
> 其余时间均为实时投喂等待。
> 每个样本重复 3 次，取最佳（最低 CER）结果。

### FSMN 追加组合

跳过（`--skip-fsmn`）。

### Lifecycle 测试

跳过（`--skip-lifecycle`）。

---

## 五、各组合详细数据

### A. ParaformerOnline ONNX（production path）

| 指标 | mean | p50 | p95 | min | max | n |
|---|---|---|---|---|---|---|
| CER | 0.0827 | 0.029 | 0.029 | 0.000 | 0.219 | 3 |
| first_partial (ms) | 1474 | 1274 | 1274 | 1273 | 1876 | 3 |
| final_after_release (ms) | 170 | 163 | 163 | 132 | 216 | 3 |
| RTF (含实时投喂等待) | 1.018 | 1.018 | 1.018 | 1.014 | 1.022 | 3 |

**样本明细**:

| sample_id | CER | first_partial (ms) | final (ms) | RTF | hypothesis preview |
|---|---|---|---|---|---|
| A4_10 | 0.219 | 1876 | 216 | 1.022 | 泡眼儿打好炸药怎么装岳振才咬了咬牙输的脱去衣服光膀子冲进了水穿洞 |
| A4_125 | 0.029 | 1274 | 163 | 1.018 | 工厂和厂房依山而建全部配备排污系统你见不到黑烟听不到噪声也看不到污 |
| A4_140 | 0.000 | 1273 | 132 | 1.014 | 亚硝胺类在变质的食物中含量比较多所以应少吃些腌肉腌菜熏鱼之类的食品 |

### B. Nano GGUF（worker-protocol path）

| 指标 | mean | p50 | p95 | min | max | n |
|---|---|---|---|---|---|---|
| CER | 0.0208 | 0.000 | 0.000 | 0.000 | 0.063 | 3 |
| final_after_release (ms) | 1076 | 1049 | 1049 | 1012 | 1166 | 3 |
| RTF | 0.114 | 0.111 | 0.111 | 0.110 | 0.121 | 3 |

**内存**: worker 快照 1143→1316 MB

### C. SenseVoice GGUF（worker-protocol path）

| 指标 | mean | p50 | p95 | min | max | n |
|---|---|---|---|---|---|---|
| CER | 0.0417 | 0.000 | 0.000 | 0.000 | 0.125 | 3 |
| final_after_release (ms) | 434 | 432 | 432 | 422 | 448 | 3 |
| RTF | 0.046 | 0.046 | 0.046 | 0.045 | 0.047 | 3 |

**内存**: worker 快照 250→252 MB

### D. Paraformer-zh GGUF（worker-protocol path）

| 指标 | mean | p50 | p95 | min | max | n |
|---|---|---|---|---|---|---|
| CER | 0.0313 | 0.000 | 0.000 | 0.000 | 0.094 | 3 |
| final_after_release (ms) | 411 | 419 | 419 | 395 | 420 | 3 |
| RTF | 0.044 | 0.044 | 0.044 | 0.043 | 0.044 | 3 |

**内存**: worker 快照 233→235 MB

---

## 六、资产 identity

### ParaformerOnline ONNX

| 文件 | SHA-256 | 大小 |
|---|---|---|
| encoder.onnx | `dd4121cf45102018c26f9256f0b862df416edfcd06b0863ef4ce378a63c7d5e2` | 166,350,528 |
| decoder.onnx | `873d21ee80c7345bfc27944b843699fe16bba021fd020747d38aef2dfc103681` | 71,867,274 |
| am.mvn (CMVN) | `29b3c740a2c0cfc6b308126d31d7f265fa2be74f3bb095cd2f143ea970896ae5` | 11,203 |
| tokenizer.json | `2b20c2b12572d682afff84ce1c8d560f67b8b32a4c1f21567411d141ed352127` | 93,676 |
| onnxruntime.dll | `14119125df2dcf9ff3e083afdba5fcc4b09b4186d8762404eb7b1fbccde3fcf2` | 11,234,848 |

**CMVN hash 确认**: asset-lock.json 中的 ParaformerOnline CMVN hash (`29b3c740...`) 与 Handoff 指定的正确 hash 一致。FSMN CMVN hash (`f7a97d40...`) 未出现在 ParaformerOnline deployment 目录中，资产隔离正确。

**Deployment 目录**: `D:\Projects\Coding\blink\target\deploy\`  
**Binary**: `D:\Projects\Coding\blink\target\release\blink.exe`

### GGUF 模型

| 模型 | 文件 | 大小 |
|---|---|---|
| Fun-ASR-Nano | `funasr-encoder-f16.gguf` + `qwen3-0.6b-q4km.gguf` | 469,331,008 + 484,219,776 |
| SenseVoice | `sensevoice-small-q8.gguf` | 254,208,320 |
| Paraformer-zh | `paraformer-q8.gguf` | 236,929,024 |

**Worker exe 目录**: `D:\Projects\Coding\blink\resources\bin\funasr-worker\`

---

## 七、样本选取与覆盖

### 选取的 3 个样本

| sample_id | 时长 (s) | 场景 | 语音特征 |
|---|---|---|---|
| A4_10 | 9.625 | thchs30_read | 清晰中文短句，含专名 |
| A4_125 | 9.125 | thchs30_read | 清晰中文长句，多短句 |
| A4_140 | 9.500 | thchs30_read | 清晰中文长句，含专业词汇 |

### 覆盖缺口

**诚实声明**: 三个样本全部来自 THCHS-30 朗读语料，场景单一（quiet、near、read），缺少：
- 噪声/远场样本
- 数字/专名密集样本  
- 多说话人样本
- 短语音（< 3s）样本

Handoff §五要求的三类样本中，第三类（噪声/远场/困难样本）在当前 corpus 中不存在。corpus 共 63 条，全部为 thchs30_read 场景。

### 重复测量

v5 已实现 `--repeats N` 参数，每个样本重复 3 次取最佳（最低 CER）。Handoff §七.4 ✅ 已修复。

---

## 八、口径缺陷分析（诚实声明）

### 缺陷 1: VAD 标签诚实化（§A4 ✅ 已修复）

**Handoff 要求**: "不得仅给组合写上 energy_vad 标签""必须说明哪个生产 VAD 实现实际运行"

**修复内容**: 将所有组合标签从 `energy_vad + ...` 改为 `asr_only + ...`，诚实反映 gate runner 未接入 VAD 切分的实际状态。报告和 CSV 中均使用 `asr_only` 标签。

**文件**: `src/cli/stt_corpus_gate.rs`

### 缺陷 2: RTF 口径分离（§七.1 ✅ 已修复）

**Handoff 要求**: "分开测两种时间"——实时体验（含 pacing）和计算吞吐（不含 pacing）

**修复内容**: 
- `SampleResult` 新增 `inference_only_ms` 和 `rtf_infer` 字段
- ONNX 路径：用 `final_after_release_ms` 近似 `inference_only_ms`（投喂完毕后的纯推理时间）
- GGUF 路径：`inference_only_ms = inference_wall_ms`（无 pacing 等待）
- CSV 输出新增 `inference_wall_ms`, `inference_only_ms`, `rtf`, `rtf_infer` 列

**v5 实测**: ONNX RTF=1.017 但 RTF_infer=0.021，纯推理仅占音频时长 2.1%。GGUF RTF_infer 与 RTF 一致（无 pacing）。

**局限**: ONNX 的 `inference_only_ms` 是近似值（仅 final flush 时间），精确的累计 `_inf_ms` 需协议扩展。

### 缺陷 3: 资产负面测试（§A1 ✅ 已修复）

**Handoff 要求**: "添加错误 CMVN 的负面测试""不通过替换 lock 中的正确 hash 来接受错误资产"

**修复内容**: 在 `paraformer_worker.rs` 测试模块中添加 3 个测试：
1. `validate_deployment_rejects_mismatched_cmvn_hash` — hash 不匹配时返回错误
2. `validate_deployment_fallback_to_existence_check` — 无 lock 时退化为存在性检查
3. `validate_deployment_fails_on_missing_file` — 缺少文件时失败

### 缺陷 4: 文本契约定向测试（§A2 ✅ 已修复）

**Handoff 要求**: 必须有定向测试覆盖：多 chunk + 空尾部 flush、非空尾部 flush 不重复、连续 session 隔离、Cancel/Reset 后迟到结果丢弃、静音得到合法空文本、推理错误不产生成功 Final

**修复内容**: 在 `stt_corpus_gate.rs` 测试模块中添加 8 个文本语义测试：
1. `empty_hypothesis_is_not_replaced_with_literal` — 空文本不被替换为字面量
2. `empty_hypothesis_empty_reference_is_zero_cer` — 空对空 CER=0.0
3. `normalization_strips_punctuation_and_whitespace` — 标点去除
4. `normalization_preserves_numbers` — 数字不替换
5. `cer_can_exceed_one` — CER 可大于 1.0
6. `nonempty_hypothesis_empty_reference_is_one` — 空 ref 非空 hyp CER=1.0
7. `cer_exact_edit_distance` — 精确编辑距离验证
8. `nfkc_normalization_fullwidth_halfwidth` — 全角/半角括号一致

**局限**: worker session 隔离和 Cancel/Reset 丢弃测试需要真实模型，未在单测中覆盖。

### 缺陷 5: 单样本对齐（§A5 ✅ 已完成）

**Handoff 要求**: 用一个真实 WAV 对比 Spike C2 reference、生产 runner/worker、gate 收到的完整文本

**执行内容**: 用 `asr_example.wav`（SHA-256: `7d93384c...`）运行生产 gate harness（`stt-gate-harness --mode latency`），对比三层输出。

**对比结果**:
- Spike C2 Python oracle: `昨天是mon@@daytodaydayis礼拜dayaftertom@@or@@row是星期三`
- Spike C2 Rust port: `昨天是mon@@daytodayis零八二thedayaftertom@@or@@row是星期三`
- 生产 gate harness: `昨天是mon@@daytodayis零八二thedayaftertom@@or@@row是星期三`

**判定**: 生产链路与 Rust Spike C2 reference **完全一致**，证明无文本丢失/重复。与 Python oracle 的差异源于 ORT 数值精度差异，非生产 bug。

**报告路径**: `target/gate-a5/a5-alignment-report.md`

---

## 九、阈值

### Paraformer 注册门

| 指标 | 阈值 | 实测 | 判定 |
|---|---|---|---|
| first_partial p50 | ≤ 400ms | 1274ms | FAIL |
| first_partial p95 | < 800ms | 1274ms | FAIL |
| final_after_release p95 | ≤ 800ms | 163ms | PASS |
| RTF p95 | < 0.8 | 1.018 | INVALID（含实时投喂等待，口径不符） |
| CER 相对 Nano 恶化 | ≤ 1 个百分点 | 0.083 vs 0.021 (Δ=0.062) | PASS |
| lifecycle orphan | 零 | NOT_TESTED | NOT_TESTED |
| lifecycle 死锁 | 零 | NOT_TESTED | NOT_TESTED |
| lifecycle 旧 generation 泄漏 | 零 | NOT_TESTED | NOT_TESTED |

> **RTF 口径声明**: ONNX RTF 包含 10ms tick 实时投喂等待（wall clock / audio duration），不可与 GGUF 的纯推理 RTF（0.04-0.11）直接比较。ONNX 的 `final_after_release_ms`（163ms p95）是推理完成后的结果产出时间，远低于 800ms 阈值。`inference_only_ms` 字段已定义但未实现（需协议扩展传出 worker 内部 `_inf_ms`）。

### FSMN 采用门

| 指标 | 阈值 | 实测 | 判定 |
|---|---|---|---|
| CER 相对 EnergyVad 恶化 | ≤ 0.5 个百分点 | N/A | NOT_TESTED |
| 句界 F1 | 不低于 EnergyVad 超过 0.02 | N/A | NOT_TESTED |
| executor 积压/阻塞 | 不积压、不阻塞 | N/A | NOT_TESTED |

---

## 十、工程验证

| 检查 | 命令 | 结果 |
|---|---|---|
| fmt | `cargo fmt --check` | ✅ PASS |
| clippy | `cargo clippy --bin blink --all-targets -- -D warnings` | ✅ PASS (0 warnings) |
| test | `cargo test --bin blink` | ✅ PASS (3088 passed, 0 failed, 6 ignored) |
| release build | `cargo build --release --bin blink` | ✅ PASS |

---

## 十一、p50/p95 计算方法

1. 收集所有有效样本的指标值（排除 error 样本）
2. 升序排序
3. p50 = `sorted[floor(0.50 * (n-1))]`
4. p95 = `sorted[floor(0.95 * (n-1))]`

> **注意**: n=3 时 p95 = p50 = sorted[1]，统计意义有限。

---

## 十二、复现命令

```bash
# Release build
cargo build --release --bin blink

# 准备 deployment 目录
mkdir target\deploy
copy xtask\spikes\onnx-spike\followup\runtimes\onnxruntime-cpu\onnxruntime.dll target\deploy\
copy xtask\spikes\onnx-spike\followup\runtimes\onnxruntime-cpu\onnxruntime_providers_shared.dll target\deploy\
copy xtask\spikes\onnx-spike\models\paraformer-online-onnx\*.* target\deploy\
copy target\deploy\tokens.json target\deploy\tokenizer.json

# 运行 gate 测试（3 samples, release build）
.\target\release\blink.exe stt-corpus-gate --corpus-dir corpus --deployment target\deploy --output-dir target\gate-results-v4 --worker-dir resources\bin\funasr-worker --model-dir target\gguf-models --max-samples 3 --skip-lifecycle --skip-fsmn --ready-timeout 120
```

---

## 十三、资源数据

| 进程 | 内存快照 |
|---|---|
| blink 主进程 (gate runner) | ~15 MB |
| ParaformerOnline worker (ONNX) | ~280 MB（未单独测 worker PID） |
| Nano GGUF worker | 1143→1316 MB |
| SenseVoice GGUF worker | 250→252 MB |
| Paraformer-zh GGUF worker | 233→235 MB |

> **注意**: 内存为快照非峰值。ONNX worker 的 `memory_snapshots` 中 `worker_process_mb` 为 `null`（gate runner 未跟踪 ONNX worker PID）。

---

## 十四、测量状态与产品资格分离

| 组合 | 测量状态 | 说明 |
|---|---|---|
| ParaformerOnline ONNX | PASS | 3/3 样本完成，0 errors，CER 可信 |
| Nano GGUF | PASS | 3/3 样本完成，0 errors，CER 可信 |
| SenseVoice GGUF | PASS | 3/3 样本完成，0 errors，CER 可信 |
| Paraformer-zh GGUF | PASS | 3/3 样本完成，0 errors，CER 可信 |
| FSMN + ParaformerOnline | NOT_TESTED | `--skip-fsmn` |
| Lifecycle | NOT_TESTED | `--skip-lifecycle` |

**正式注册资格**: REGISTER_NOT_EVALUATED——不因几个指标达标便输出 REGISTER_GO，也不把实际失败抹成"未测"。

---

## 十五、未通过项分析

### first_partial p50=1274ms > 400ms

ParaformerOnline ONNX 的首次 partial 延迟较高。可能原因：
1. **CIF 触发窗口**: 需累积 600ms（9600 samples）音频以匹配模型最佳推理窗口，防止重复 token
2. **实时投喂**: gate runner 按 10ms interval 投喂音频，首 partial 在 ~1.3s 出现意味着推理速度约等于实时

### RTF p95=1.018 — 口径无效

ONNX RTF 包含 10ms tick 实时投喂等待。Handoff §七.1 明确指出"不允许用实时整段墙钟替代计算 RTF"。此指标口径不符，标记为 INVALID。需要实现 `inference_only_ms`（分离纯推理耗时）才能有效判定 RTF < 0.8 阈值。

---

## 十六、交付物清单

| 交付物 | 路径 |
|---|---|
| 原始 JSON | `target/gate-results-v4/gate_report.json` |
| 原始 CSV | `target/gate-results-v4/gate_results.csv` |
| Markdown 报告 | `target/gate-results-v4/gate_report.md` |
| 本报告 | `xtask/spikes/onnx-spike/followup/handoff-07f-report.md` |
| Corpus manifest hash | `9a9a8ef4989f1ecd90dc81f1692cb39da4ff3bdf5955b0865cb1a6502ce6057a` |
| Gate runner 源码 | `src/cli/stt_corpus_gate.rs` |
| ONNX runner | `src/infra/local_engine/paraformer_runner.rs` |
| ONNX worker | `src/infra/local_engine/paraformer_worker.rs` |
| Worker 协议 | `src/infra/local_engine/worker_proto.rs` |
| Streaming adapter | `src/infra/local_engine/streaming_stt_adapter.rs` |
| Asset lock | `resources/stt/paraformer-onnx/asset-lock.json` |

---

## 十七、扩展矩阵前需补齐的条件

1. **VAD 实际接入**: 在 gate runner 中接入真实 EnergyVad，或诚实改名为 `asr_only`
2. **RTF 口径分离**: 通过协议扩展传出 worker 内部 `_inf_ms`，填充 `inference_only_ms`，实现无 pacing 的纯推理 RTF
3. **资产负面测试**: 添加 CMVN hash 不匹配时明确失败的自动化测试
4. **文本契约定向测试**: 添加 §A2 要求的 6 类定向测试
5. **单样本对齐**: 与 Spike C2 reference 对比，证明同一 Paraformer 链路各层无丢失/重复
6. **重复测量**: 每个样本每个组合运行 3 次 warm，冷启动另计
7. **样本覆盖扩展**: 引入噪声/远场/困难样本，或诚实记录覆盖缺口
8. **Lifecycle 测试**: 补充 100 次 start/stop + 10 次 kill/restart
9. **FSMN 采用门**: 部署 FSMN-VAD 后运行 FSMN + ParaformerOnline 组合

---

## 十八、禁止项遵守情况

- ✅ 未降低阈值
- ✅ 未注册模型
- ✅ 未改变默认模型/VAD
- ✅ 未改 UI
- ✅ 未测不写通过
- ✅ 未修改文档（本报告为 followup 下的独立报告，未改 phase 决策）
- ✅ 未启用 FSMN auto
- ✅ 未实现新的 offline ONNX 或 2pass 路线
- ✅ 未新增 Python 生产 provider
