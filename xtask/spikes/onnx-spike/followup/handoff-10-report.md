# Handoff 10 — 0.22.9 最终集成与 Release Gate 报告

**Date**: 2026-09-05
**Version**: 0.22.9-handoff-10
**构建**: `cargo xtask release`（含 Handoff 09 前端改动 + Handoff 10 修复），版本 0.22.7（本周期版本号未变）

---

## 一、结论

| 项目 | 结论 |
|---|---|
| **六项工程门禁** | ✅ 全部通过（fmt / clippy -D warnings / cargo test 3104 / 前端 55 文件 / xtask paraformer-worker / xtask release-check） |
| **正式制品排除** | ✅ MSI(wxs)/NSIS(nsi) 清单零 ONNX DLL、零 .onnx、零 PaddleOCR/Python/uv；仅随包 3 个 GGUF worker |
| **真实双向切模 E2E（新构建复测）** | ✅ PASS（9 步全绿；`stt-switch-e2e`，target/switch-e2e/handoff10-report.json） |
| **语料 gate 冒烟（新构建）** | ✅ 4 组合 × 3 样本，0 errors；判定与 07F/07E-R 一致：Paraformer **REGISTER_NOT_EVALUATED** |
| **Lifecycle 实测（新构建，production adapter）** | ✅ 100/100 start/stop（含 50 cancel + 50 end）、10/10 kill/restart、**0 orphan / 0 死锁 / 0 旧 generation**、reset 可复现 |
| **实机 GUI** | ✅ G1 主链路、设置页状态真实性、UI 双向跨 runtime 切换、语音链路（静音）、资源快照 |
| **ParaformerOnline 注册门** | ⚠️ **仍未通过**：first_partial p50≈1.27s > 400ms、RTF 口径无效、CER 对 Nano 基线 runner 未强制判定（07F 数据 Δ=6.2pp）。**本 handoff 未降低阈值，也未撤销候选**（注册为候选系 Handoff 08 用户明确指示，见 handoff-08-report §前置核对） |
| **FSMN-VAD** | auto 仍解析 EnergyVad（单测锁定）；生产链路无 FSMN 接线；FSMN 采用门证据不完整（无切句 F1），07E-R 的 ADOPT 判定不作为启用依据 |
| **默认模型** | ✅ 未改变。fresh-install 默认决策维持"待 product 确认"；默认仍为 SenseVoice GGUF（单测 `default_selection_remains_gguf_sensevoice` 锁定；fresh profile 实机场景未执行，不另行声称） |
| **`<300MB` 目标** | ⚠️ 引擎常驻态整树超出（SenseVoice ≈ 360MB、ONNX ≈ 786MB、Nano ≈ 1430MB）；按 phase §3.12 停止默认放行，提交产品决策 |

**总体判定：工程 GO（可发布的工程质量），产品/注册门 NO-GO 项明确留档（ParaformerOnline 注册门、FSMN 采用门、fresh-install 默认、<300MB 冲突）。**

---

## 二、工程门禁结果

| 门禁 | 命令 | 结果 |
|---|---|---|
| 1 | `cargo fmt --all -- --check` | ✅ PASS |
| 2 | `cargo clippy --bin blink --all-targets -- -D warnings` | ✅ PASS（0 warning） |
| 3 | `cargo test --bin blink` | ✅ 3104 passed / 0 failed / 6 ignored |
| 4 | `node frontend/run-tests.mjs` | ✅ 55 个测试文件全部通过（修复后复跑仍全绿） |
| 5 | `cargo xtask paraformer-worker` | ✅ PASS（STT asset-lock 校验 + manifest 生成） |
| 6 | `cargo xtask release-check` | ✅ PASS（版本/嵌入资源/许可/GGUF 供应链/资源排除/ONNX lock/STT lock） |
| 7 | 正式安装包实机 gate | 见 §四 |

---

## 三、实机与资源报告（本机 Windows 11，USB 麦克风）

### 3.1 GUI 实机（target/release/blink.exe 新构建，真实用户 profile）

- **G1 主链路**：Alt+Space 唤起（日志 show+focus <50ms 量级）→ 输入 "notepad" → 结果 + 图标懒加载 → Enter 启动 Notepad3 + 启动器隐藏（reason=launch）→ 复测通过。启动器 WebView2 在截屏捕获中呈暗色，为捕获伪影（搜索/启动链路日志全部正常）。
- **G2 外部应用注入**：Notepad3 前台 → 长按 Alt+Space 1.8s → `语音录音开始 target=ForegroundApp`、伪流式 VAD+GGUF 通道就绪 → 松开 → Final text_len=0 → "识别结果为空,跳过交付"（诚实空结果），无崩溃。**真人语音内容注入未测（无发声源）**。
- **G3 对话语音**：与 G2 同一 VoiceService/录音链路（begin_recording 已实测）；对话窗口独立 UI 路径本 handoff 未单独走查。
- **设置页状态真实性**：
  - FunASR 卡片唯一模型写入口；4 候选（3 GGUF + 1 ONNX）业务特征行正确（多语种/真伪流式/worker 类型/中文质量）。
  - selected ≠ installed 不说谎：Nano（未安装）选中时显示"配置模型"徽章。
  - 运行中卡片显示 active implementation 只读诊断："GGUF worker（常驻）" / "ONNX worker（独立进程）"。
  - 切换后 SenseVoice 行"配置模型 + 实际加载 + 当前"徽章分明；语音页只读展示跟随（FunASR · SenseVoice Small）。
- **跨 runtime 切换（UI，引擎运行中）**：GGUF→ONNX（Ready 2.1s）→ ONNX→GGUF（ONNX 优雅退出 115ms → GGUF Ready 2.4s），事务日志完整，无孤儿。
- **主动停止**：`优雅停止 stdio worker：shutdown + EOF → NormalExit → lease 已删除 → deliberate stop，不进入错误态`。
- **Alt+A OCR**：**跳过**——本机 Alt+A 被微信占用，用户协助验证（未执行，不标记完成）。

### 3.2 资源快照（WS/Private MB，PowerShell 实测）

| 状态 | 主进程 | worker | 备注 |
|---|---|---|---|
| 未加载（引擎未启动） | 96.2 / 39.3 | — | 无 worker 进程 |
| SenseVoice GGUF Ready | 110.1 / 43.7 | 250.3 / 245.5 | 与 07F 一致 |
| ParaformerOnline ONNX Ready | 115.7 / 49.9 | **536.9 / 670.0** | blink.exe paraformer-worker 子命令 |
| 停止后 | 115.7 / 49.7 | **无进程** | 重型资源确定释放 ✅ |
| Nano GGUF（gate 内） | ~14 | 1143→1318 | 07F/新构建语料 gate 快照 |
| Paraformer-zh（gate 内） | ~14 | 233→235 | 同上 |
| Lifecycle 主进程 | 13.4→14.0 | — | 10 轮 kill/restart 无增长 |

**识别中**：以语料 gate worker 快照为准（07F + 本轮）；GUI 实时识别中快照未测（无发声源）。
**OCR 冷/热**：未测（见上）。

### 3.3 CLI 实机（production 路径 harness）

- `stt-switch-e2e`：verdict=PASS（新构建复测，9 步）。
- `stt-corpus-gate`（--max-samples 3 --repeats 1 --skip-lifecycle --skip-fsmn）：4 组合 0 错误；CER/RTF 与 07F 同量级；ONNX RTF_infer ≈ 0.015-0.02（口径分离后纯推理远低于 0.8 阈值，但注册门口径仍按 wall-clock 执行，未放宽）。
- `stt-gate-harness --mode lifecycle`（production ParaformerOnlineAdapter + binary v2 worker，deployment=target/deploy）：100/100 + 10/10，0 orphan / 0 deadlock / 0 stale-gen，主进程内存稳定。

---

## 四、安装包实机 gate

- 制品：`Blink_0.22.7_x64_en-US.msi`、`Blink_0.22.7_x64-setup.exe`（含 Handoff 10 修复的重建版）。
- payload 排除扫描（wxs/nsi 清单）：onnxruntime/.onnx/paddleocr/python3/uv 命中 0；GGUF worker 3 个随包 ✅。
- **原地安装升级（用户实机执行）**：在既有 0.21.17 安装位置（D:\DevTools\Blink）以 NSIS 制品交互式安装升级至 0.22.7 构建并启动运行正常（安装后 DisplayVersion=0.22.7、InstallLocation 不变、用户 profile 数据沿用）。此即"启动 + 安装器升级"场景的真实证据。
- **全新安装 / 卸载场景**：按用户指示跳过，未执行、不标记完成。
- **注册表说明**：0.22.9 phase 产品代码不做任何注册表操作；唯一写入方是 NSIS 安装器的标准卸载注册（HKCU\...\Uninstall\Blink）。handoff 计划中"注册表备份/还原"仅是测试卫生措施——若向独立目录安装第二份制品，同名 key 会被劫持、卸载测试副本会删掉用户既有安装的卸载注册；用户改为原地安装后该措施不再需要（备份文件已清理）。

---

## 五、本 handoff 修复清单（明确小问题）

| # | 问题 | 修复 | 文件 |
|---|---|---|---|
| 1 | i18n key 泄漏：OCR 卡片显示原始 key（`local_engine.config.onnx_assets` / `ort_runtime` 等 10 个），根因 `t(key, fallback)` 误用——t() 第二参是插值 params 不是 fallback（0.22.8 遗留） | 补齐 zh/en 10 个 key（t() 实现 idempotent，无需改调用点） | `frontend/js/i18n/zh.js`、`en.js` |
| 2 | i18n key 泄漏：语音页跳转按钮显示 `voice.local.model.goto_engines`（settings.html data-i18n key 不存在） | 补齐 zh/en key | `frontend/js/i18n/zh.js`、`en.js` |
| 3 | 运行中切换模型无用户确认——违反 0.22.7 契约（"服务运行中切换模型应经用户确认"）；git 历史核实确认框在早前重构中丢失 | "使用"按钮在 active implementation 存在时先 confirmDialog（未运行仍直接提交）；新增 i18n key `local_engine.model.action.switch_confirm_desc` | `frontend/js/settings/tabs/engines/local-engine-models.js`、`zh.js`、`en.js` |

**自审**：修复后 `node frontend/run-tests.mjs` 55 文件全绿；未触碰 Rust 代码；未改变任何状态机/事务语义（确认框仅加在 UI 入口，controller `selectModel` 未改）。

---

## 六、P0/P1/P2 剩余问题

| 级别 | 问题 | 状态/建议 |
|---|---|---|
| **P1** | ParaformerOnline 注册门未通过（first_partial p50≈1.27s > 400ms；RTF 注册口径无效；CER 对 Nano 基线 Δ≈6.2pp 未被 runner 强制判定） | 需产品决策：(a) 完成协议扩展 `_inf_ms` + 重新校准 CIF 首窗后再跑全量 gate，或 (b) 接受现状并把候选降级/下架。阈值不得降低。 |
| **P1** | `<300MB` 目标与引擎常驻冲突（见 §3.2） | 按 phase §3.12 提交产品决策（按需启动/空闲回收/目标修订）。默认模型维持 SenseVoice，未放行。 |
| **P2** | FSMN 采用门证据不完整（无切句 F1 标注；07E-R ADOPT 判定只比 CER） | 保持 FSMN 不启用（现状即安全侧）；如需启用，补句界标注语料后重跑。 |
| **P2** | 07F 报告 §九把 CER 门（Δ=0.062>1pp）标为 PASS，与阈值矛盾 | 以本报告为准：CER 门 **未通过/未强制**；建议在 07F 报告追加勘误（未改动，尊重历史报告）。 |
| **P2** | Alt+A OCR 实机未测（微信占用热键）；G3 对话窗口 UI 未单独走查 | 用户日常使用中验证；不阻塞 0.22.9 工程收口。 |
| **P2** | 卡片"模型/服务：未知"初态（本会话首次打开未推送状态时） | 可读性小瑕疵，不误导（未知≠声称）；留 0.22.10 打磨。 |

---

## 七、交付物

| 交付物 | 路径 |
|---|---|
| E2E 复测报告 | `target/switch-e2e/handoff10-report.json` |
| 语料 gate 冒烟 | `target/gate-handoff10/gate_report.json` / `.md` |
| Lifecycle 实测 | `target/gate-handoff10/lifecycle.json` |
| profile 配置备份 | `target/gate-handoff10/stt_config_backup.json`、`ocr_config_backup.json`（已原样恢复） |
| 资源快照脚本 | `target/gate-handoff10/snapshot.ps1`、`sendkeys.ps1`、`holdkeys.ps1`、`sendtext.ps1` |
| 本报告 | `xtask/spikes/onnx-spike/followup/handoff-10-report.md` |

## 八、禁止项遵守情况

- ✅ 未降低任何注册门/采用门阈值；未把未测项标记完成
- ✅ 未改变 fresh-install 默认模型（验证为 SenseVoice）；用户存量 selected 已原样恢复（Nano）
- ✅ 未启用 FSMN auto（resolve_vad_kind 零改动）；未删除 EnergyVad
- ✅ 未实现 offline ONNX / 2pass；未动 spec/product 跨版本决策（phase 仅据实更新 checkbox/状态事实）
- ✅ 未自动终止未知进程；测试全部使用自启动进程与受管 worker
