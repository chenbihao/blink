# Handoff：0.23.13 VAD 底噪自举死锁修复 + 预览增量两处引擎改动

> 交接人：ZCode 会话（2026-09-17）。接手人：flash。
> 任务性质：**收尾**——代码已全部写完，剩 1 个测试失败需修复 + 完整验证（自动化 + 手动实测）。

---

## 一、背景（3 分钟读懂）

用户的实时语音（G2 前台注入 / 编辑器连续听写）出现两个问题，已定位到**引擎侧**（不是 0.23.13 渐进上屏重构引入）：

1. **灰色预览层从不显示** → 根因：麦克风（USB Condenser，高增益）语音 RMS 低于 VAD off 阈值。`compute_thresholds` 里 `base = max(noise_floor, silence_threshold×0.5)`，配置值 0.005 是自适应的**下限锁**，把阈值钉死在麦克风电平之上。
2. **调低 silence_threshold 到 0.001 + 调高麦克风增益后，预览能显示但不增量累积**（灰色只剩最近 3 秒碎片）→ 根因：底噪上升护栏 `p25 < off` **自指**（off 由底噪算出）→ 环境底噪高于初始 off 时底噪永远升不上去（**自举死锁**）→ VAD 永远判"正在说话" → 停顿永远检测不到 → 短语账本（`preview_phrases`）永远不冻结 → 预览只有滚动尾窗、切句只剩 12s 硬窗口。

用户日志实证：整个会话 `preview_revision` 在涨但 `phrases=0`；唯一草稿 `samples=192000` = 正好 12.0s 硬窗口。

## 二、已完成的三项改动（代码全在，勿重写）

### 1. `src/domain/stt/vad.rs` — 底噪护栏改稳定性判定（核心修复）

- `update_noise_floor`：上升条件从 `p25 < off`（自指死锁）改为：
  - `p75 <= p25 × NOISE_FLOOR_STABLE_RATIO (2.5)`（窗口分位挤拢 = 平稳噪声态）
  - **且** `p25 <= NOISE_FLOOR_ABS_MAX (0.006)`（绝对吸收上限：稳态但响于 0.006 的是语音不是底噪——纯稳定性判定会把持续元音/响纯音也吸进去，vad.rs 现有谷底测试因此炸过，这个上限是补丁）
- 慢升快降速率、THRESHOLD_MIN/MAX 钳制、首帧保守种子全部保留。
- 新增 2 个单测：`noise_floor_converges_to_steady_ambient_above_initial_threshold`（死锁场景收敛）、`noise_floor_frozen_during_varying_speech_window`（语音窗口不抬底噪）。

### 2. `src/domain/stt/pseudo_streaming/mod.rs` — 时间触发短语冻结兜底

- `transcribe_chunk_preview_draft` 里，滚动预览窗口计算前插入：前缀 `[phrase_anchor, total-3s]` 积满 **1.2s**（`TIME_FREEZE_MIN_PREFIX_MS`）且无 pending 定稿/无候选作废路径产出时，对该前缀走 `RequestAudioGate`（`PHRASE_MIN_VOICED_MS=500` 有声门控）→ Valid 则作为 `phrase_snapshot` 送 `spawn_phrase_recognition` 并推进锚点；NoSpeech 纯静音直接跳锚点；TooShort 等待。
- 效果：**停顿检测失灵时灰色预览仍按短语增量累积**，不再只剩 3 秒滚动窗碎片。
- 另加短语回填观测日志（debug 级）：`短语定稿入账 ledger_len=N chars=N` / `短语定稿识别返回空文本` / `短语定稿识别失败`（此前 transport 错误被静默吞掉）。

### 3. `silence_threshold` 退役（配置 + UI）

- `src/domain/config/stt_config.rs`：默认 0.005 → **0.001**，字段保留（旧配置反序列化兼容），文档改为"兼容字段"。
- 前端：`frontend/settings.html` 移除"静默阈值"滑杆块；`frontend/js/settings/tabs/voice.js` 移除对应 control 项；`frontend/js/settings/tabs/voice-vad.js` 的 `VAD_DEFAULTS.silence_threshold` → 0.001（与 Rust 对齐）；`frontend/js/i18n/zh.js` / `en.js` 删除对应 label/hint 两键。
- 注意：`stt_config.rs:1833` 的旧配置兼容测试断言是 fixture 显式值 0.005，**保持 0.005 不要改**。

### 4. 测试改动

- 新增 `tests.rs::time_freeze_finalizes_prefix_without_pause_candidates`（**已通过**）：4.5s 连续无停顿语音 → 断言短语入账 + 锚点推进 + 组合预览含冻结文本。**教训：测试结尾不要调 `engine.finalize().await`**——终态识别会等一个没人应答的 transport oneshot，挂死（已踩坑）。
- 改写 `preview_draft_long_session_does_not_overload_after_draft_commits`：从 4 段×6s 改为 8 段×(3.2s 语音+0.8s 停顿)=32s，试图避开时间冻结线。**当前仍失败，见下**。

## 三、当前验证状态（截至交接）

| 项 | 状态 |
|---|---|
| `cargo check --bin blink`（默认特性） | ✅ 通过 |
| `cargo check --bin blink --no-default-features` | ✅ 通过 |
| `cargo test --bin blink` | ❌ **3446 过 / 1 失败** |
| 前端 `node --test js/settings/tabs/voice-vad-windows.test.mjs` | ✅ 通过 |
| 手动实测（G2 / 编辑器 / VAD 调试页） | ⛔ 未做（需要真人说话，见第五节） |

**唯一失败**：`domain::stt::pseudo_streaming::tests::preview_draft_long_session_does_not_overload_after_draft_commits`
- 现象：`tests.rs:107` 的 `wait_until` 2 秒超时（"条件未在超时内成立"）——某轮喂完语音+停顿并应答预期次数的 transport 调用后，`pcm_committed_end` 没推进。
- 推断：时间冻结在某一轮额外发起了一次短语识别调用，**吃掉了脚本化应答**，真正的句定稿调用没被应答 → committed 不推进。（我按"前缀峰值 1.0s < 1.2s 冻结线"估算了 3.2s 语音段应不触发，但实测仍触发了——说明我对锚点/边界回退位置（`low_energy_valley_offset` 会把边界往前挪）的估算有偏差，**不要信我的估算，以实际调用序列为准**。）

**修复建议（按优先级）**：
1. **测试侧重构（推荐，最稳）**：把"每轮 `wait_for_calls_or_fail(index+1)` 应答一次"改成"循环：`call_target += 1` → 等待 → 用下一个 sender 应答 `"第N段。"` → 检查 `pcm_committed_end` 是否推进，推进才进入下一轮"。channel 备 24~32 个；短语调用收到 "第N段。" 只进预览账本，不影响最终断言（最终文本只由 Draft 拼接）。这样无论时间冻结每轮发起 0~2 次调用都稳。
2. 或行为侧收紧：时间冻结额外加 `inner.boundary_candidate.is_none()` 条件（停顿候选存在期间不抢跑，交给候选升级/作废路径处理）——需要评估是否会重新引入"停顿检测失灵时不冻结"（即死锁场景下 candidate 可能长期存在？死锁场景 VAD 永不判安静、候选根本不会形成，所以该条件不影响兜底效果，但请验证）。
3. 或调大 `TIME_FREEZE_MIN_PREFIX_MS`——不推荐，会削弱兜底时效。

修完必须：`cargo test --bin blink` 全绿 + `cargo check --bin blink --no-default-features` 通过（AGENTS §二双构建验收）。

## 四、自动验证清单（代码层）

```bash
cargo test --bin blink                    # 3447 个全过
cargo check --bin blink --no-default-features
cd frontend && node --test js/settings/tabs/voice-vad-windows.test.mjs
node --test js/voice-overlay/layout.test.mjs js/content-editor/voice.test.mjs   # overlay 相关不回归
```

## 五、手动实测清单（核心验收，需要真人对麦克风说话）

环境：`cargo tauri dev`；设置页把日志调到 **debug**（默认 error 看不到关键行）。测试麦克风：用户现用 "麦克风 (USB Condenser Microphone)"，高增益、底噪约 0.003。

**测试语**（用户原始用例）：`测试一下这里停顿，然后再次停顿，然后还是停顿，然后我再给出久一点的。你看看这样会不会断呢？`（在每个"停顿"处自然停 0.3~0.8s）

| # | 测什么 | 预期效果（达标线） |
|---|---|---|
| 1 | **G2 实时听写**（按住语音键对前台应用说测试语） | 灰色预览**逐短语累积**："测试一下。"→"测试一下。这里停顿。"→"…然后再次停顿。"……不再出现"只剩最新一段"的碎片式跳变 |
| 2 | 同上，看日志 | `短语定稿入账 ledger_len=1/2/3...` 递增；出现 **自然切句**的 `定稿识别完成 seg=N samples=非192000`（不再是清一色 12.00s 硬窗口/`切分·硬窗口`） |
| 3 | **VAD 调试页**（设置→语音→VAD 调试，回放一段带停顿的录音） | 能量曲线在句中停顿处**下穿 off 阈值线**（修复前 off 是平直低位线永远穿不过）；时间线预览 entries 是累积文本，audio range 不再清一色 3.0s 滚动窗 |
| 4 | **长连续无停顿说话**（>5s 不换气） | 时间冻结兜底触发：日志出现 `短语定稿入账`，灰色仍增量（这是兜底项的验收） |
| 5 | **设置页 UI** | VAD 切句参数卡片**没有**"静默阈值"滑杆（只剩静默时长/最短句长/窗口三项）；点"恢复默认"后配置 `silence_threshold=0.001` |
| 6 | **回归：渐进上屏** | G2 第 3 段草稿定稿时最老一段出现在目标应用；松键/ESC/出错时已定稿内容不丢 |
| 7 | **回归：编辑器连续听写** | 浮窗显示 confirmed 窗口（最新 1 段）+ preview 双层；段落最终落正文 |
| 8 | **回归：G1（主窗口）/G3（chat）** | Legacy 路径行为不变 |
| 9 | **观察项（非阻塞）** | 连续说话时 `GGUF worker 转录完成` 频率——时间冻结会增加调用量，若占空比过高（听感卡顿/预览延迟明显）把 `TIME_FREEZE_MIN_PREFIX_MS` 从 1200 调到 1500-2000 再看 |

**如果 1/2 不达标的调参旋钮**：
- 停顿仍检测不到（曲线穿不过 off）：`NOISE_FLOOR_STABLE_RATIO` 2.5 → 3.0~3.5（更容易判定"平稳"）；或确认麦克风底噪是否 >0.006（超出 `NOISE_FLOOR_ABS_MAX` 吸收上限——上限要不要放宽需权衡持续语音误吸收）。
- 短语音被误吸进底噪（预览又消失）：`NOISE_FLOOR_STABLE_RATIO` 降到 2.0，或 `NOISE_FLOOR_ABS_MAX` 降到 0.004。
- 短语冻结太碎/太频：`TIME_FREEZE_MIN_PREFIX_MS` 调大。

## 六、注意事项

- **版本编号**：代码注释统一用 0.23.13；仓库 Cargo.toml 还是 0.23.10，phase 文档 0.23.13 行已由用户起头——收尾时对齐编号。
- **文档回写**：0.23 phase 文档（`docs/phases/0.23-editor-voice-ai-workflow.md`）描述旧行为的部分需要更新（预览增量机制、silence_threshold 退役、稳定性护栏）——**按 AGENTS 约定需用户确认后再写**，本 handoff 文件用完可删。
- 工作区有用户并行改动（chat/resource 等无关文件），**不要动它们**；本任务相关文件仅：`src/domain/stt/vad.rs`、`src/domain/stt/pseudo_streaming/mod.rs`、`src/domain/stt/pseudo_streaming/tests.rs`、`src/domain/config/stt_config.rs`、`frontend/settings.html`、`frontend/js/settings/tabs/voice.js`、`frontend/js/settings/tabs/voice-vad.js`、`frontend/js/i18n/zh.js`、`frontend/js/i18n/en.js`。
- 用户已手改本地配置 `silence_threshold=0.001`（与默认一致，无需回滚）。
