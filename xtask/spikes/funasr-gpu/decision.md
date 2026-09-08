# 0.22.16.6 FunASR GPU 集成与采用门证据

状态：**最终 NO-GO，决定回滚至 CPU-only**（2026-09-09）。2026-09-08 的固定音频采用门证明 SenseVoice/Paraformer Vulkan 技术可运行且同 shape 热态很快，但随后真实 Blink 变长伪流式录音暴露 6～10 秒的新 shape pipeline 编译和串行 worker 阻塞；VAD 已触发却无法及时提交，产品体验劣于 CPU。实现提交 `ba20a72d` 仅作历史留档，不进入发布。

最终分发决定同步收口：CPU base 不单独 Release，继续随 Blink 一体构建和打包；Vulkan/CUDA runtime Release、`runtime-lock.json` 与用户侧额外 DLL 下载链路随代码回滚移除。

## 结论摘要

以下条目记录 2026-09-08 的中间技术验证；其中 runtime Release、按需下载和 GPU catalog 方案已被 2026-09-09 的最终 NO-GO 决策推翻。

- 已推翻的分发方案：Vulkan 增量包曾计划作为独立 `funasr-runtime-v*` Release 按需下载；最终决定 CPU base 随 Blink 一体构建/打包，GPU runtime 不发布。
- 实验期本地验证：debug Blink 曾通过 `BLINK_LOCAL_ENGINE_ARTIFACT_ROOT` 消费本地产物，用于走通 manifest、SHA-256 与 backend probe；该入口随 GPU/runtime Release 代码一并回滚。
- base runtime 已由锁定源码和 0001～0007 补丁链构建：`resources/bin/funasr-worker/` 含 3 个 worker、动态 ggml/llama/CPU DLL、MSVC runtime、manifest 和 license；manifest 文件闭合、SHA-256、EXE import check、CPU backend probe 均通过。
- `resources/stt/funasr-gguf/runtime-lock.json` 中 base/Vulkan/CUDA 三个 SHA-256 都是 `null`；按设计，应用 release 消费验证和 GPU 下载均拒绝继续。
- GPU artifact plan 已改为直接从 `runtime-lock.json` 读取 URL/SHA-256；当前静态校验通过，但实际下载仍因 lock 的 null SHA fail-closed。
- 本机硬件为 NVIDIA GeForce RTX 2070 SUPER、driver 610.62、compute capability 7.5、8 GiB；`vulkaninfo` 观测到 Vulkan 1.4.341。安装 Vulkan SDK 1.4.357.0 后，Vulkan runtime 已构建并通过 worker backend probe：动态加载 `ggml-vulkan.dll`，识别到 RTX 2070 SUPER，`requested_backend=actual_backend=vulkan`，graph `add_f32=success`。CUDA 构建仍因缺少 `nvcc` 阻断。
- 三个 CPU worker 的无模型 backend probe 均通过；SenseVoice 与 Paraformer 已使用本地受控模型和 ASCII 暂存音频完成真实 CPU/Vulkan 连续识别。音频暂存用于规避上游 Windows 窄路径限制，生产路径本身使用 UUID 文件名。
- 采用门阈值在运行前冻结于 [`adoption_thresholds.json`](../../funasr-worker/adoption_thresholds.json)：文本归一化编辑距离比例 ≤ 0.02、时间戳绝对误差 ≤ 20 ms、连续请求 ≥ 100、unexpected exit/stdout pollution/orphan = 0、GPU 至少 1.1× P50 加速或 CPU P95 ≤ 基线的 0.9×。本次没有因结果放宽阈值。

原始预检摘要：[`preflight-20260908.json`](runs/preflight-20260908.json)。本机真实 100 请求聚合结果：[`sensevoice-nvidia-vulkan-20260908.json`](runs/sensevoice-nvidia-vulkan-20260908.json)、[`paraformer-nvidia-vulkan-20260908.json`](runs/paraformer-nvidia-vulkan-20260908.json)。证据只包含硬件/制品/状态摘要和识别文本的长度与 SHA-256，不包含音频或转写正文。

## 验证入口与命令

采用门 runner：

```text
python xtask/funasr-worker/adoption_gate.py --self-test
python xtask/funasr-worker/adoption_gate.py --out xtask/spikes/funasr-gpu/runs/<timestamp>.json
```

有制品和受控音频后，必须显式提供 `--model-dir`、`--audio`，默认执行三个模型 × CPU/Vulkan/CUDA，并以同一批音频的 CPU 结果作为基线。runner 会执行：manifest 文件闭包/hash、worker backend probe、ready/health requested=actual、100 次串行请求、并发输入的串行响应、畸形 JSON/未知 type/错误协议版本/错误音频、stop/restart、空 backend 目录故障注入、stdout 污染检查、CPU P50/P95 与进程 CPU/RAM/VRAM 观测。

协议 v1 没有 cancel 消息；因此 worker 侧 cancel 标记为 untested，由 manager/provider Rust 测试覆盖。驱动禁用/不兼容只能在真实硬件上做，不通过删除系统驱动或伪造 DLL 模拟。

## 当前主机与制品观测

| 项目 | 观测 |
|---|---|
| OS / Python | Windows 11 build 10.0.26200 / Python 3.14.6 |
| GPU | NVIDIA GeForce RTX 2070 SUPER |
| NVIDIA driver | 610.62 |
| CUDA capability | 7.5 |
| VRAM | 8192 MiB |
| Vulkan loader/API | `vulkaninfo`: instance 1.4.341；GPU API 1.4.341 |
| worker artifact | pass：base manifest 26 files；CPU probe/import check pass；zip 5,470,568 bytes，SHA-256 `ca4871859bb4a2693050c6442679b15b0363b60c4160bfb7924898b589b33121` |
| Vulkan artifact | pass：增量 manifest 3 files；manifest 记录 `glslc shaderc v2026.3`；Vulkan probe/import check/hardware test pass；zip 16,178,497 bytes，SHA-256 `dc97c84569f6aee2ae4f2b866e80467cf4c8288b26540bdaee9cf81f8c898744` |
| runtime lock | blocked：3 个 SHA-256 为 `null` |
| GPU artifact install plans | static pass：从 runtime lock 读取 SHA；消费仍 blocked：lock SHA 为 `null` |
| 受控音频 / 模型 | pass（SenseVoice、Paraformer）：使用仓库测试语料的本地 ASCII 暂存副本；模型只在本机读取，未复制或上传用户音频 |

## 组合采用矩阵

`blocked/untested` 是证据状态，不等于 pass。CPU 的“自动化稳定性”只表示 Rust 纯逻辑/事务测试通过，不能替代真实 worker 识别。

| 模型/组件 | Backend | 正确性 | 稳定性 | 性能收益 | requested=actual | 结论 |
|---|---|---|---|---|---|---|
| SenseVoice | CPU | pass：受控音频 100 次输出一致 | pass：100/100；串行并发与协议负向路径通过 | baseline：P50 3081.7 ms、P95 3239.0 ms | pass：ready/health requested=actual=cpu | 本机基线通过 |
| SenseVoice | Vulkan | pass：与 CPU 编辑距离比例 0 | pass：100/100；0 stdout pollution | pass：热态 P50 73.5 ms、P95 77.2 ms，约 41.9×；首次请求约 6.0 s | pass：ready/health requested=actual=vulkan | NVIDIA 本地门通过；最终 catalog 仍待跨厂/release 门 |
| SenseVoice | CUDA | blocked/untested：本机缺 nvcc，未产出 worker | blocked/untested | blocked/untested | blocked/untested | no-go，不进入 catalog |
| Paraformer | CPU | pass：受控音频 100 次输出一致 | pass：100/100；协议负向路径、stop/restart、异常退出通过 | baseline：P50 2266.7 ms、P95 2767.5 ms | pass：ready/health requested=actual=cpu | 本机基线通过 |
| Paraformer | Vulkan | pass：与 CPU 编辑距离比例 0；5 条语料矩阵文本 hash 全部相同 | pass：100/100；0 stdout pollution；协议负向路径、stop/restart、异常退出通过 | pass：热态 P50 125.5 ms、P95 129.3 ms，约 18.06×；首次请求 8.52 s，且不同输入 shape 可能再次触发约 10 s 管线编译 | pass：ready/health requested=actual=vulkan | NVIDIA 本地门通过，进入 Paraformer model-scoped catalog；正式发布仍待 release SHA |
| Paraformer | CUDA | blocked/untested：未完成 CUDA 模型采用门 | blocked/untested | blocked/untested | blocked/untested | no-go，不进入 catalog |
| Nano encoder | CPU | blocked/untested：缺受控模型/音频 | 部分 pass：worker probe；100 请求 blocked | blocked：无同批实测 | pass（probe）；ready/health blocked | no-go，保持 CPU-only |
| Nano encoder | Vulkan | backend graph pass；模型输出 blocked：缺受控模型/音频 | probe pass；100 请求 blocked | blocked：无同批实测 | pass（probe）；ready/health blocked | no-go，保持 CPU-only |
| Nano encoder | CUDA | blocked/untested：组件未声明 GPU profile，且未 probe | blocked/untested | blocked/untested | blocked/untested | no-go，保持 CPU-only |
| Nano Qwen | CPU | blocked/untested：缺受控模型/音频 | 部分 pass：worker probe；100 请求 blocked | blocked：无同批实测 | pass（probe）；ready/health blocked | no-go，保持 CPU-only |
| Nano Qwen | Vulkan | backend graph pass；模型输出 blocked：缺受控模型/音频 | probe pass；100 请求 blocked | blocked：无同批实测 | pass（probe）；ready/health blocked | no-go，保持 CPU-only |
| Nano Qwen | CUDA | blocked/untested：组件未声明 GPU profile，且未 probe | blocked/untested | blocked/untested | blocked/untested | no-go，保持 CPU-only |

中间实现曾把 Paraformer 的 `auto` 候选扩展为 Vulkan → CPU；最终 NO-GO 后该 catalog 变更撤销，三个模型均恢复 CPU-only。

## 自动化门禁结果

| 检查 | 结果 | 证据 |
|---|---|---|
| `cargo fmt --all -- --check` | pass | 本次运行通过 |
| `cargo clippy --bin blink --all-targets -- -D warnings` | pass | 0 warning；修复了一处前置 Vulkan loader 代码的 clippy 写法，见 `src/infra/platform/gpu.rs` |
| `cargo test --bin blink` | pass | 3009 passed / 0 failed / 6 ignored |
| `node frontend/run-tests.mjs` | pass | 60/60 测试文件 |
| `cargo test --manifest-path xtask/Cargo.toml` | pass | 13 passed / 0 failed |
| `python adoption_gate.py --self-test` | pass | 阈值/编辑距离/manifest 闭包负向测试通过 |
| base worker probe（3 个 worker） | pass | 无模型 backend probe；requested=actual=cpu、graph add_f32 success、stdout 协议行纯净 |
| Vulkan worker probe | pass | RTX 2070 SUPER；动态加载 `ggml-vulkan.dll`；requested=actual=vulkan、graph add_f32 success |
| SenseVoice CPU/Vulkan 真实推理 | pass（本机局部采用门） | 同一受控音频各 100 请求；文本距离 0；Vulkan 热态 P50 73.5 ms，对 CPU 约 41.9× |
| Paraformer CPU/Vulkan 真实推理 | pass（本机局部采用门） | 同一受控音频各 100 请求；文本距离 0；Vulkan 热态 P50 125.5 ms，对 CPU 约 18.06×；冷态 shape 编译成本已记录 |
| `smoke_models.py`（最终重建产物） | pass | Paraformer CPU/Vulkan 各 2 请求；文本 hash 一致；Vulkan 首次 8004.4 ms、热态 130.4 ms |
| `cargo xtask release-check` | blocked/fail-closed | 三个 runtime asset SHA 仍为 `null`；命令按设计非零退出 |
| 应用 release 不现场编译 llama.cpp | 实验实现 pass，最终撤销 | 最终决定 CPU worker 继续随 Blink 一体构建/打包，不保留独立 runtime Release |
| runtime 下载/SHA/安装/self-test/干净机启动 | blocked | runtime lock 三个 SHA 为 `null`，且 GPU install plan hash 为空，不可安全下载消费 |

## 本轮新增构建/阻塞证据

1. clean pin 构建应用 `0001`～`0007`；0004 修复动态 backend 所需 shared library，0005 修复 opaque `ggml_cgraph` 的 public API 访问，0006 修复 VAD 对动态 CPU backend 的直接符号依赖，0007 对齐实际 CPU DLL 名称 `ggml-cpu-x64.dll`。
2. base CMake/Ninja 构建 344/344 targets；CPU probe 输出 `requested_backend=cpu`、`actual_backend=cpu`、`device_name=CPU`、graph `add_f32=success`；stderr 的 ggml load 诊断不污染 stdout 协议。
3. 首次 Vulkan 构建暴露 Windows 路径过长：原 build tree 令 shader object/PDB 路径达到约 268 字符并触发 MSVC C1041。xtask 将 CMake build tree 收短到 `target/fw-build/<flavor>-<fingerprint>` 后，628 targets 全部完成。
4. Vulkan 增量 artifact 已产出并通过文件闭合、SHA-256、EXE import、真实硬件 backend probe；CUDA 仍因缺少 `nvcc` 未产出。这些结果只证明 backend 可加载并执行测试图，不替代模型推理采用门。

## 未完成且不能伪造为通过的证据

1. NVIDIA/AMD/Intel 各一台真实 Windows Vulkan 设备的 worker probe、图计算和输出等价。
2. CUDA 声明架构/最低驱动、cuBLAS/cudart 文件闭包、包体增量与错误驱动提示。
3. SenseVoice CUDA 的同批文本、时间戳、冷启动、热推理 P50/P95、CPU/RAM/VRAM；SenseVoice/Paraformer Vulkan 的 VRAM 采样当前不可用。
4. Nano encoder、Nano Qwen 的权重、graph、I/O、实际设备闭合；Nano 仍保留 CPU-only 声明。
5. 协议 v1 cancel、安装取消、rollback、休眠恢复与真实驱动初始化失败；Paraformer 的 100 请求、错误音频、stop/restart 和异常退出已通过。
6. 三厂 Vulkan 矩阵；本机只完成 NVIDIA 上 SenseVoice/Paraformer 真实模型推理，AMD/Intel 仍未验证。

## 重新立项条件与回滚

回滚以提交 `ba20a72d` 的父提交为产品基线：恢复 CPU-only worker/catalog/config/UI，移除 GPU runtime workflow、lock、按需下载和本地 artifact 旁路；0.22.15 EnergyVad、伪流式句子事务与文本防回退不受影响。CPU worker、manifest 与许可继续随 Blink 一体构建和打包。

未来重新立项必须先用真实变长音频复验 first-preview、sentence-end-to-commit 和 release-to-final 尾延迟，不能再以固定 shape 的热态 P50 作为采用依据；还必须同时解决 Vulkan backend 包体、多厂设备矩阵和用户侧分发成本。
