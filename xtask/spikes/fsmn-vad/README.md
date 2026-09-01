# 0.22.8 FSMN-VAD 可行性 spike

> **范围**：隔离原型，只产出证据、数据和决策报告。不直接替换生产 `EnergyVad`，不改变生产默认配置，不新增用户开关。
>
> **日期**：2026-09-01
>
> **前置**：0.22.7 GGUF 核心（三模型常驻 worker）已完成。

---

## 一、环境

| 项 | 值 |
|---|---|
| OS | Windows 11 x64 |
| Python | CPython 3.12.8（`-I -S` 模式，不创建 venv） |
| FunASR runtime | `runtime-llamacpp-v0.2.6` portable x64 CPU |
| FunASR commit | `55b662ccf9ea77237ba9253b3bddd953d4184f84` |
| llama.cpp commit | `803b7fcae893e9caaee3921779628fef83ac0965`（FunASC CMakeLists FetchContent 锁定） |
| ASR 模型 | SenseVoiceSmall Q8 GGUF、Paraformer Q8 GGUF、Fun-ASR-Nano Q4K_M GGUF |
| VAD 模型 | FSMN-VAD GGUF (`FunAudioLLM/fsmn-vad-GGUF`) |
| Blink worker patches | `xtask/funasr-worker/patches/0001-0003`（生产）+ `0004`（spike） |
| 共享头文件 | `xtask/funasr-worker/blink_worker_protocol.h` |

---

## 二、资产来源

### 模型

| 模型 | 来源 | SHA-256 | 许可 |
|---|---|---|---|
| SenseVoiceSmall Q8 | `FunAudioLLM/SenseVoiceSmall-GGUF` @ `90c1c619` | `4ae45c94422de949b387e2e0fb10d7e14e4c42c69db30c3444ecc7d4b844b7c5` | Apache-2.0 |
| Paraformer Q8 | `FunAudioLLM/Paraformer-GGUF` @ `1a5063b3` | `42bf76ea1575a336aaca4c1b7c01a82b79113e6d04d0d6b799561bfcf07ee011` | Apache-2.0 |
| Fun-ASR-Nano encoder | `FunAudioLLM/Fun-ASR-Nano-GGUF` @ `46e84950` | `f92f91d01a24fbed6c863495b2ee8c6a6788144a02858b75743f0946668de8a2` | Apache-2.0 |
| Fun-ASR-Nano LLM | `FunAudioLLM/Fun-ASR-Nano-GGUF` @ `46e84950` | `cc5057552aa9dddedcda73ea8889854e8a257eb07d0a561b7234465c1e856f22` | Apache-2.0 |
| FSMN-VAD | `FunAudioLLM/fsmn-vad-GGUF` | 见 Hugging Face 仓库 | Apache-2.0 |

模型 URL 已固定到上游 commit SHA（非 `resolve/main` 浮动 ref）。完整供应链锁定见 `resources/stt/funasr-gguf/worker-lock.json`。

### 测试音频

全部测试音频由脚本本地生成，来源为：
- Windows SAPI 语音合成（`System.Speech.Synthesis.SpeechSynthesizer`）
- 数学生成正弦波、白噪声（固定种子的 `System.Random`）

**不包含任何私人录音。** 所有音频 16kHz mono PCM s16le WAV。

生成命令：
```powershell
.\xtask\spikes\fsmn-vad\generate_fixtures.ps1
```

输出目录：`xtask/spikes/fsmn-vad/fixtures/audio/`，含 `manifest.json`（文件名、大小、SHA-256）。

### 源码分析

FSMN-VAD 的源码分析基于 FunASR commit `55b662cc` 的以下文件：
- `runtime/llama.cpp/funasr-common/funasr_vad.h`（核心实现）
- `runtime/llama.cpp/funasr-vad/funasr-vad.cpp`（独立 CLI）
- `runtime/llama.cpp/sensevoice/funasr-sensevoice/funasr-sensevoice.cpp`（`--vad` 使用）
- `runtime/llama.cpp/paraformer/funasr-paraformer/funasr-paraformer.cpp`（`--vad` 使用）
- `runtime/llama.cpp/fun-asr-nano/funasr-cli/funasr-cli.cpp`（`--vad` 使用）

---

## 三、数据隐私说明

- 所有测试音频为机器生成，不含人声录音。
- Spike 脚本不访问网络、不上传数据。
- 结果 JSON 中不记录任何用户隐私信息。
- 工作进程的诊断日志走 stderr，不包含音频内容。

---

## 四、复现命令

### 1. 生成测试音频

```powershell
.\xtask\spikes\fsmn-vad\generate_fixtures.ps1
```

### 2. VAD 策略对比矩阵

```powershell
python .\xtask\spikes\fsmn-vad\run_vad_matrix.py
```

如需用真实 FSMN-VAD GGUF：
```powershell
$env:FSMN_VAD_GGUF = "path/to/fsmn-vad.gguf"
$env:FUNASR_VAD_CLI = "path/to/funasr-vad.exe"
python .\xtask\spikes\fsmn-vad\run_vad_matrix.py
```

### 3. ASR 配对冒烟

```powershell
$env:FUNASR_WORKER_EXE = "path/to/funasr-sensevoice-worker.exe"
$env:SENSEVOICE_GGUF = "path/to/sensevoice-small-q8.gguf"
$env:FSMN_VAD_GGUF = "path/to/fsmn-vad.gguf"
python .\xtask\spikes\fsmn-vad\run_asr_pairing.py
```

### 4. 构建 spike worker（可选）

如需用 spike 0004 patch 构建"支持 --vad 的 stdin-server"worker：

```powershell
# 参考 xtask/spikes/funasr-runtime/quick_spike.ps1 的构建流程
# 应用 0001 + 0004（spike 变体）而非 0001（生产）
# 或直接用上游 funasr-vad CLI 做对比
```

---

## 五、产物清单

| 文件 | 说明 |
|---|---|
| `README.md` | 本文件 |
| `decision.md` | go / conditional-go / no-go 决策报告 |
| `generate_fixtures.ps1` | 测试音频生成脚本 |
| `run_vad_matrix.py` | VAD 策略对比矩阵（10 场景 × 5 策略） |
| `run_asr_pairing.py` | ASR 配对冒烟测试（3 ASR × VAD + 边界用例） |
| `patches/0004-sensevoice-vad-stdin-server-spike.patch` | Spike 专用 worker patch（允许 --vad + --stdin-server） |
| `results/fsmn-vad-source-analysis.json` | FSMN-VAD 源码分析（离线/在线/cache 确认） |
| `results/vad-matrix.json` | VAD 策略对比结构化数据 |
| `results/asr-pairing.json` | ASR 配对冒烟结构化数据 |
| `fixtures/audio/` | 测试音频（.wav）+ manifest.json |

---

## 六、关联文档

- [0.22 phase §3.8](../../docs/phases/0.22-local-model-runtime-ppocrv6.md) — 两项 spike 的决策边界
- [0.22 phase §5.2](../../docs/phases/0.22-local-model-runtime-ppocrv6.md) — 0.22.8 规划
- [0.22 phase §6.3](../../docs/phases/0.22-local-model-runtime-ppocrv6.md) — 0.22.8 验收清单
- [`src/domain/stt/vad.rs`](../../src/domain/stt/vad.rs) — 生产 EnergyVad 实现
- [`src/domain/stt/pseudo_streaming.rs`](../../src/domain/stt/pseudo_streaming.rs) — 伪流式引擎
- [`xtask/funasr-worker/patches/`](../../xtask/funasr-worker/patches/) — 生产 worker patch
- [`xtask/spikes/funasr-runtime/`](../funasr-runtime/) — 0.22.7 GGUF 常驻 worker 验证
