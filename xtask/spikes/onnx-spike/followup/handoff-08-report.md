# Handoff 08 — ParaformerOnline 正式注册与跨 Runtime 切模报告

**Date**: 2026-09-05
**Version**: 0.22.9-handoff-08
**前置核对**: Handoff 07F 完成矩阵验证（4 组合 × 3 样本 × 3 轮 warm，0 errors）后，由用户明确指示进入本 handoff。07F 报告将"默认模型资格"写为 REGISTER_NOT_EVALUATED——本 handoff 不触碰默认模型（默认仍为 `gguf/sensevoice-small-q8`），仅注册模型候选并打通跨 runtime 切换事务，与 07F 的资格门语义一致。

---

## 一、结论

| 项目 | 结论 |
|---|---|
| **ParaformerOnline 注册** | ✅ 完成——`onnx/paraformer-online` 注册为 `funasr` 下第 4 个模型候选，绑定 `ParaformerOnnxWorker` |
| **跨 runtime 切换事务** | ✅ 完成——`EngineManager::switch_model` 状态机 + 失败回滚 |
| **真实双向切模 E2E** | ✅ **PASS**——GGUF→ONNX→GGUF 全链路（真实安装事务 + 真实 worker + 真流式 roundtrip） |
| **门禁** | ✅ fmt / full clippy / 3102 tests / release-check 全绿 |
| **既有三 GGUF 模型** | ✅ id、能力、选择、安装路径零改动（单测锁定） |
| **默认模型** | ✅ 未改变（用户存量与 fresh-install 均不受影响） |

---

## 二、交付物

### 2.1 模型绑定（稳定 model id + 编译期绑定表）

| 项 | 值 |
|---|---|
| model id | `onnx/paraformer-online`（真源 `domain::config::stt_config::PARAFORMER_ONLINE_MODEL_ID`） |
| implementation | `ParaformerOnnxWorker`（`implementation_registry` 编译期绑定，fail-closed） |
| 能力声明 | languages=["zh"]、pseudo_streaming=yes、**true_streaming=yes**（native partial）、timestamps=no |
| 部署空间 | `impl-paraformer_onnx_worker`（per-implementation，与 GGUF engine 级兼容真源互不可见） |
| 超时覆盖 | start 30s + model_load 120s（ORT 初始化 + encoder 166MB / decoder 72MB 加载） |
| revision | `onnx-{ort.version}`（派生自 STT asset lock） |

装配点：`src/app/local_engine/implementation_registry.rs`（carried_models + install_plan + binding）、`funasr/paraformer_online.rs`（新模块：模型 descriptor / launch 构造 / 部署状态投影 / provider descriptor）、`model_installer.rs::make_funasr_model_registry`（第 4 个 descriptor）。

### 2.2 连接解析（start 按 selected 解析 implementation；VoiceService 选 port）

- **start 冻结链路**（`manager/lifecycle/start.rs`）：
  1. 从配置 selected（无则退化 descriptor 模型契约）按绑定表预解析 implementation；
  2. 按实现冻结模型身份：GGUF 走 model_storage manifest 回读；**ParaformerOnline 走 per-implementation 部署空间 active manifest 回读**（fingerprint = ONNX DLL SHA-256，64-hex）；
  3. 冻结后按 manifest model_id 复核 implementation（漂移 fail-closed）；
  4. deployment identity 从 **resolved implementation 的部署空间**读取（闭合映射：GGUF/OCR → engine 级，ParaformerOnnxWorker → impl 级）。
- **LaunchContext 注入**：domain `LaunchContext` 新增只读 `implementation: Option<ImplementationId>`——`FunasrAdapter::prepare_launch` 据此分派：GGUF 模型走现有 GGUF 构造（NDJSON worker，返回现有 transport）；ParaformerOnline 走 `build_paraformer_online_launch_descriptor`（blink.exe 隐藏子命令 `paraformer-worker --deployment <impl slot>`，二进制协议 v2）。
- **health 验证**：新增 `verify_paraformer_worker_health`——stdio 管道（进程句柄即身份）+ `send_hello`/`wait_ready`（worker 在 ORT Session 与模型真实加载后才回 Ready），成功后 `ParaformerOnlineAdapter::with_process` 存入 `EngineEntry.streaming_port`；失败附带 worker stderr 日志尾。
- **get_connection 投影**（`manager/status.rs`）：连接携带冻结 implementation + 按实例互斥的通道——GGUF 实例 = `worker: Some(Arc<dyn SttTransport>)`、ONNX 实例 = `streaming: Some(Arc<ParaformerOnlineAdapter>)`。
- **VoiceService**（`app/voice.rs`）：`begin_recording` 按 `conn.implementation` 分派——ONNX → 直接使用 streaming port（`is_ready()` 检查断连，native partial 真流式）；GGUF → 现有 `create_engine` + `GgufStreamingAdapter` 路径零改动。前端无任何 implementation/runtime/path 提交面（command 签名不变）。

### 2.3 事务状态机（`manager/switch.rs`，新模块）

```text
switch_model(engine, target):
  claim（全程串行；cancel 信号不中断——无半状态）
  1. 冻结旧 snapshot（launch snapshot = active；selected = 配置真源）
  2. 验证目标（模型目录 + 绑定表 + 资产已安装；GGUF=model_storage，ONNX=impl 部署）→ 失败：Target 错误，零状态变更
  3. 引擎未运行 → 只 commit selected（CommittedSelectedOnly，不自动启动）
     目标已是 active → 幂等（Completed）
  4. stop old（优雅：GGUF NDJSON shutdown / ONNX Quit；Job Object 兜底）
  5. commit selected target（SelectedModelStore 端口：DB + 缓存 + CONFIG_CHANGED）
  6. start target（start_internal：冻结 → Ready → active 提交）→ Completed
  7. 失败回滚：恢复旧 selected → 按冻结 snapshot 重启旧模型 → RolledBack{target_error}
  8. 恢复也失败 → selected=旧模型；active=None；RollbackFailed{target_error, rollback_error}（双失败落状态 + 上抛）
```

配置提交端口 `SelectedModelStore`（manager 不接触 DB/AppHandle）：生产实现 `selected_store.rs`（ConfigStore 持久化 + `update_cache` + 事件广播，与 `set_local_stt_selection` 保存语义一致），wiring 在 main.rs 注入；测试注入内存 fake。命令层 `set_local_stt_selection` 改为事务单一入口（签名不变，失败矩阵语义按 handoff 升级：目标失败回滚 selected 并如实报错，不再"配置已保存但静默失败"）。

### 2.4 模型安装/状态/删除（Paraformer provider + per-implementation deployment）

- **安装**（`manager/models.rs::install_paraformer_online_model`）：`InstallTransaction` + `ParaformerOnnxProvider`，写入 `impl-paraformer_onnx_worker` 空间（ORT DLL + encoder/decoder/CMVN/tokenizer 全量 SHA-256 校验 + 隔离 self-test）；幂等（部署 `model_generation_id` 与 asset lock 一致即 Done）；安装前自动停止运行中的 ONNX worker（文件锁保护）；engine 级（GGUF）环境安装/修复入口零改动。
- **状态**（list_models / get_model_status）：ONNX 模型状态真源 = 该引擎 impl 部署空间——契约不一致或 generation 与 asset lock 不一致 → Corrupted（不可选，需显式 repair）；无部署 → NotInstalled。
- **删除**：冲突检查同现有（selected / launch snapshot），执行 = `DeploymentStore::remove_impl_space`（仅限 impl 级空间，engine 级兼容真源拒绝整体删除；删除前先收尾事务 + 停 ONNX worker）。
- **ensure_installed**：selected 解析为 ParaformerOnnxWorker 时，就绪判定读 impl 空间部署（不要求 GGUF 环境）；未安装返回可行动错误——**不自动安装 ~250MB 资产**。

### 2.5 失败矩阵覆盖（单测，`manager/tests.rs` + `switch.rs`）

| 覆盖项 | 测试 | 结果 |
|---|---|---|
| 未安装目标 | `switch_target_not_installed_fails_before_any_mutation`（验证步 fail-closed，零状态变更） | ✅ |
| 引擎未运行 | `switch_when_engine_stopped_commits_selected_only` | ✅ |
| 幂等 | `switch_same_target_is_idempotent` | ✅ |
| start 失败 + 回滚失败（双错误；selected=旧、active=None） | `switch_start_failure_with_failed_rollback_reports_both_errors` | ✅ |
| selected store 未接线 | `switch_without_selected_store_fails_closed` | ✅ |
| 未绑定模型不静默换模 | `switch_unbound_model_fails_closed` | ✅ |
| ONNX 连接投影/互斥/清理 | `get_connection_projects_streaming_port_for_onnx_instance` | ✅ |
| 状态投影（含 generation 漂移→Corrupted） | `paraformer_online_model_status_reflects_impl_deployment` | ✅ |
| hash 损坏 | 沿用既有 fail-closed：下载 hash 失败 → 安装事务失败回滚（E2E 安装链路验证）；部署损坏 → Corrupted 不可选（上测） | ✅ |
| Ready 超时 / worker early exit | `verify_paraformer_worker_health`：poisoned→SpawnFailed / 超时→Timeout，统一 rollback（单测以 fake exe 驱动同路径；真实 worker 生命周期由 07F lifecycle harness 覆盖） | ✅（代码路径）/ 07F（真实） |
| cancel | 事务全程持有 claim，`cancel_operation` 信号不中断 stop/commit/start——要么完整成功要么完整回滚（设计保证 + claim 单测沿用） | ✅（设计） |
| 应用退出 | `shutdown_all_blocking`（Job Object 兜底）+ graceful 路径 ONNX Quit 分支 | ✅（沿用 + 分支） |
| crash recovery | lease/probe/事务 journal fail-closed 恢复沿用 0.22.9 既有机制；impl 空间逐空间独立恢复（probe_blocking 已覆盖 impl 空间枚举） | ✅（沿用） |

### 2.6 真实双向切模 E2E

**命令**：`blink.exe stt-switch-e2e`（新隐藏 CLI 入口，`src/cli/stt_switch_e2e.rs`；真实 blink.exe 生产路径，worker 即 blink.exe 自身子命令）。

**结果**：`verdict: PASS`（报告 `target/switch-e2e/report.json`，全量日志 `target/switch-e2e/e2e-full.log`）。第二轮（资产已就位）关键时序：

| 步骤 | 结果 | 证据 |
|---|---|---|
| GGUF 环境安装（bundled worker） | ✅ | probe: active 部署有效 + self_test 通过 → Ready |
| start GGUF | ✅ Ready | NDJSON ready 握手 + 身份校验，`implementation=funasr_gguf_worker` 冻结 |
| 安装 ParaformerOnline（首轮） | ✅ | ORT 64MB + 4 模型文件下载、逐文件 hash 校验、隔离 self-test、`安装事务完成`（部署 `dep-000001a06f7042c7`，impl 空间） |
| 切换 GGUF→ONNX | ✅ Completed | `stop`（deliberate NormalExit，137ms 优雅退出）→ selected 提交 → ONNX start → `paraformer worker: hello + 等待 Ready` → Model Ready（impl 空间部署 `dep-000001a06f7042c7`）→ "目标模型 Ready，active 已提交" |
| ONNX 真流式 roundtrip | ✅ | streaming port（二进制协议 v2）：begin_session → 1s 静音推送 → finish_session → Final（静音输入 Final 文本长度 0，合法） |
| 切换 ONNX→GGUF | ✅ Completed | `paraformer worker 已在优雅窗口内退出`（Quit 115ms）→ GGUF Ready → transport check_ready 通过 |
| 收尾 stop | ✅ | desired=Stopped、active_implementation=None |

---

## 三、E2E 过程中发现并修复的问题

### 问题 1: STT asset-lock 下载 URL 不可达（HTTP 401）

**现象**: 安装事务下载 encoder.onnx 失败——asset-lock 指向的 HF 仓库 `modelscope/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-onnx` 匿名访问 401（仓库不存在/被门禁）。

**修复**: 07F 的模型实际来自 ModelScope `iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx`。核对 ModelScope API 公开的逐文件 SHA-256 与 asset-lock **完全一致**（model_quant.onnx=encoder、decoder_quant.onnx=decoder、am.mvn、tokens.json=tokenizer），URL 改为 ModelScope 实际路径。SHA-256 逐文件强校验不变（不可变性合同）。**文件**: `resources/stt/paraformer-onnx/asset-lock.json`。

### 问题 2: ModelScope LFS 大文件对默认 UA 返回 403

**现象**: URL 修正后 11KB 的 am.mvn 可下载，166MB 的 encoder.onnx（LFS 重定向）403。curl/python 全 UA 均通过，reqwest 默认 UA 403，显式 UA 通过（逐 UA 探针定位）。

**修复**: `ParaformerOnnxProvider::download_and_verify` 的 client 显式设置产品 UA `Blink/0.22 (stt-asset-download)`。**文件**: `src/infra/local_engine/providers/paraformer_onnx.rs`。

### 问题 3: 隔离 self-test 与真实模型契约不符（安装期 self-test 必失败）

**现象**: 全部文件下载+hash 通过后，`paraformer-selftest` 失败——通用维度解析（动态维→64）把 encoder 输入撑到 2,293,760 元素超过 1M 元素守卫；且只喂 1 个输入（真实 encoder 需要 speech+speech_lengths，decoder 需要 enc + enc_len + acoustic_embeds + acoustic_embeds_len + 16 层 cache）。

**根因**: 该 selftest 首次在真实 ParaformerOnline 模型上执行（07F 走 spike runner，未经安装事务的 self-test 路径）。

**修复**: 按 ParaformerOnline 真实输入契约重写最小推理段——encoder：speech [1,64,560] + speech_lengths [1]（batch 维）；decoder：enc [1,64,512] + enc_len [1] + acoustic_embeds [1,8,512] + acoustic_embeds_len [1] + in_cache_0..15 [1,512,10]。实测 encoder 3 输出、decoder 18 输出（logits+sample_ids+16 out_cache），契约完整验证。**文件**: `src/cli/paraformer_selftest.rs`。

> 上述 3 项均为本 handoff 新接线的生产路径问题，不涉及既有 GGUF 链路。

---

## 四、工程验证

| 检查 | 命令 | 结果 |
|---|---|---|
| fmt | `cargo fmt --check` | ✅ PASS |
| clippy | `cargo clippy --bin blink --all-targets -- -D warnings` | ✅ PASS（0 warnings） |
| test | `cargo test --bin blink` | ✅ **3102 passed, 0 failed**（含 8 个新切换事务/失败矩阵测试） |
| release-check | `cargo xtask release-check` | ✅ PASS（STT asset-lock 校验通过） |
| release build | `cargo build --release --bin blink` | ✅ PASS |
| 真实双向切模 E2E | `blink.exe stt-switch-e2e` | ✅ **PASS** |

---

## 五、禁止项遵守情况

- ✅ 未迁移默认模型（default 配置真源断言锁定：`default_selection_remains_gguf_sensevoice`）
- ✅ 未删除 GGUF（三条 GGUF 路径 id/能力/安装/选择零改动；绑定表测试锁定 5 = 3 GGUF + 1 ONNX + 1 OCR）
- ✅ 未实现 offline ONNX / 2pass
- ✅ 未改 UI（前端零改动；`set_local_stt_selection` command 签名不变）
- ✅ 未修改 phase/spec/product 文档（本报告为 followup 独立报告）
- ✅ 前端不能提交 implementation/runtime/path（LaunchContext.implementation 为 manager 注入的只读字段；无任何 command 接受该类输入）
- ✅ FSMN 未启用（07F 判定 NOT_TESTED → `auto` 继续解析 EnergyVad，`vad_kind`/`resolve_vad_kind` 零改动）

---

## 六、交付物清单

| 交付物 | 路径 |
|---|---|
| 模型绑定/装配 | `src/app/local_engine/funasr/paraformer_online.rs`（新）、`implementation_registry.rs`、`model_installer.rs`、`domain/config/stt_config.rs` |
| 连接解析 | `manager/lifecycle/start.rs`、`manager/health.rs`、`manager/status.rs`、`funasr/mod.rs`（adapter 分派）、`domain/local_engine/adapter.rs`（LaunchContext.implementation）、`app/voice.rs` |
| 事务状态机 | `manager/switch.rs`（新）、`selected_store.rs`（新）、`commands/stt.rs`、`main.rs`（wiring） |
| 安装/状态/删除 | `manager/models.rs`、`manager/deployment.rs`、`infra/local_engine/deployment.rs`（remove_impl_space） |
| stop/exit/优雅退出 | `manager/lifecycle/stop.rs`、`manager/lifecycle/start.rs`（exit monitor） |
| E2E 工具 | `src/cli/stt_switch_e2e.rs`（新）、`src/cli/mod.rs`（隐藏入口分派） |
| 资产/自测修复 | `resources/stt/paraformer-onnx/asset-lock.json`、`src/cli/paraformer_selftest.rs`、`providers/paraformer_onnx.rs`（UA） |
| E2E 证据 | `target/switch-e2e/report.json`（verdict=PASS）、`target/switch-e2e/e2e-full.log` |
| 本报告 | `xtask/spikes/onnx-spike/followup/handoff-08-report.md` |

## 七、复现命令

```bash
# 门禁
cargo fmt --check
cargo clippy --bin blink --all-targets -- -D warnings
cargo test --bin blink
cargo xtask release-check
cargo build --release --bin blink

# 真实双向切模 E2E（真实安装/切换/真流式 roundtrip；重复运行幂等）
./target/release/blink.exe stt-switch-e2e --output target/switch-e2e/report.json
```

## 八、边界与后续

- ONNX 安装态在本机 %APPDATA% 已就位（约 250MB，`impl-paraformer_onnx_worker` 空间）；删除入口（模型卡片删除按钮 → `remove_impl_space`）可清理。
- streaming roundtrip 使用静音输入（Final 文本长度 0 为合法）；识别质量不属本 handoff 验收范围（07F 已用 THCHS-30 语料给出 CER 数据）。
- lifecycle（100 次 start/stop + kill/restart）由 07F spike E harness 覆盖的协议链路与本 handoff 的 ONNX start/stop 路径一致（同一 `ManagedProcess` + 协议 v2）；如需产品级 lifecycle 数字，建议后续在注册后的模型上重跑 `stt-gate-harness --mode lifecycle`。
