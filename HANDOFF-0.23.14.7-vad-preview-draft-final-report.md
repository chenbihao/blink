# HANDOFF：0.23.14.7 VAD / Preview-Draft Review 收尾交付报告

> 基线：`d3f1575b`（工作区另有 `docs/phases/0.23-editor-voice-ai-workflow.md` 的既有改动，未回退）。
> 本报告只含匿名 case_id 与数值，不含音频、私有转写正文或私有文件名。

---

## 1. 根因结论

### 1.1 case_17 环境声尾段误切

**Review 提出的两个候选归因都不成立，实测推翻：**

1. **不是"只由 400ms `natural_pause` 新分支放行"那么简单。**
   全量回放的 `accepted_via` 仪表确认 21.60s 环境声候选由 `natural_pause` 放行，
   但**只拒绝这一条分支无法修复**：该候选会继续累计 `quiet` 到 1100ms 由
   `long_pause` 再次放行；即使两条 pause 分支都拦住，`finalize` 的尾段 drain
   仍会把这段音频送进模型并追加伪文本。**三个出口必须同时收口。**
2. **能量口径的"有声可信度"没有区分度。**
   上一轮采用的"强帧占比 ≥ 20%"实测（修复前全量报告）：伪文本候选
   `strong/voiced` = 32.1%，合法噪声候选（case_11，模型回 NoSpeech）21.9%，
   真句最弱档（case_15 远场轻声）38.5%——**区间重叠**，任何占比门槛都在误伤边缘
   （20% 门槛距 case_11 仅 1.9 个百分点）。该判据已废弃。

**生效判据：形态证据——连续强有声段。** 强有声 = 10ms 帧 RMS ≥ VAD 自适应
`on` 阈值（scale-free，阈值随底噪浮动）。真语音的音节是连续发声段；环境声即使
帧总量凑够（甚至越过 `on`），形态上只是稀疏脉冲。全量 33 个候选实测：

| 候选 | 连续强有声段 | 结论 |
|---|---|---|
| case_17 尾段 21.60s（产生伪文本） | **50ms** | 拒绝 |
| case_11 3.60s（噪声，模型 NoSpeech） | 130ms | 保留（无正文，无害） |
| case_17 19.29s（强脉冲，模型 NoSpeech） | 140ms | 保留（无正文，无害） |
| 真句最弱档 case_15 远场轻声 17.85s | 120ms | 保留 |
| 其余全部真句候选 | 290ms～4230ms | 保留 |

门槛 `CREDIBLE_VOICED_RUN_MIN_MS = 100`（拒绝侧 2.0×、保留侧 1.2×），落在三处：
`natural_pause` 分支、`long_pause` 分支、终态尾段 drain 的送模范围门。

**两条实现约束（都有实测/测试依据）：**

- **不在 finalize 用"当下阈值"重新测量范围。** VAD `on` 随底噪自适应，尾段静音会
  让底噪漂移，同一段音频不同时刻结论相反（case_04 远场低声实测：流式累计 290ms、
  finalize 时刻重测 90ms → 正文全丢）。生产一律复用流式累计的
  `uncommitted_strong_run_max`。
- **送模范围门只作用于 PreviewDraft。** Legacy（G1/G3 默认 profile）没有候选观察
  路径，该累计值恒为 0，套用后会拒掉**全部**终态文本——引擎级不变量测试
  （`valley_*` / `retreated_*`）正是以挂起方式暴露了该缺陷。

**跨块累计必须有块间拼接**（整块全强时当前段要延续），否则连续段永远被截断在单块
长度（实测卡在 200ms，真音节测不出来）——这是首次上线该证据时"门没生效"的直接原因。

### 1.2 P1 / P2

- P1：`stop_recording` 的 `EventTaskWait::TimedOut` 分支对 G1/G2/G3 共用，abort 后
  恢复的全文只交给 G2 专用函数 → MainWindow/ChatWindow 丢弃恢复文本。
- P2：`compose_typed_result` 的不变量修复只改局部 `spans`，未写回
  `preview_phrases`/`preview_tail` 真源；且在更新 reported cursor 后再递增
  `preview_revision` → 下一次 compose 重建同一冲突并重复发事件。

---

## 2. G1/G3 超时恢复：修复前后行为

| | 修复前 | 修复后 |
|---|---|---|
| G1 MainWindow（超时） | 引擎恢复的全文被丢弃（只走 G2 恢复函数） | 走既有 `deliver_final_text`，全文交付 |
| G3 ChatWindow（超时） | 同上，静默丢失 | 同上，全文交付 |
| G2 ForegroundApp（超时） | `G2TerminalGate` + 账本终态恢复 | 不变（BarrierQueued 归属，最多一次终态） |
| detach 竞态 | — | 未重新引入：始终 `abort + await` 退出后才从引擎侧恢复 |
| 重复交付 | `final_delivery` 闸门 | 不变；空文本**不消耗**闸门（`claim_final_delivery` 纯函数） |

新增 `event_timeout_recovery_matrix_covers_g1_g2_g3`：注入 20ms 短预算触发超时，
断言 G1/G3 恢复文本恰好交付一次、无静默丢失、迟到 Final 被闸门拒绝；G2 恢复尾段
进 worker 且迟到 Final 不二次上屏。

---

## 3. Preview revision 重复问题如何保证不再发生

1. 违规 span 的修复**持久化到真源**：从 `preview_phrases` 移除，或清空
   `preview_tail` / `preview_tail_range`（尾部按 **AudioRange 身份**判定是否幸存，
   不做字符串裁剪——AudioRange 仍是唯一归属真源）。
2. 删除"更新 reported cursor 后再递增 revision"的缺陷：修复内容随本次 envelope
   交付一次，revision 只随真实预览状态变化推进。
3. 回归测试：
   - `consecutive_compose_persists_repair_and_does_not_reemit`——第二次 compose
     返回空、revision 稳定、真源满足不变量；
   - `compose_repair_keeps_surviving_tail_in_truth_source`——违规短语被删、幸存
     尾部保留。
   - 底层状态断言：区间单调、非退化、互不重叠。

---

## 4. case_17 最终由哪个 `accepted_via` 放行、为什么

| 边界 | `accepted_via` | 连续强有声段 | 依据 |
|---|---|---|---|
| 6.12s | `draft_min` | 1150ms | owned ≥ 5s 强制提交（未走 pause 分支） |
| 16.08s | `draft_min` | 4230ms | 同上 |
| 19.29s | `natural_pause` | 140ms | quiet 460ms ≥ 400ms、voiced 1890ms ≥ 800ms、连续段 140ms ≥ 100ms → 放行；模型回 NoSpeech，无 span |
| 21.60s | **不采纳** | 50ms | `natural_sentence_voiced_not_credible`（随后 `long_pause_voiced_not_credible`） |

工程报告的拒绝证据行（`decisions`）：
`{"audioMs": 21600, "outcome": "waiting", "reason": "natural_silence",
"quietMs": 440, "strongMs": 340, "strongRunMs": 50, "voicedMs": 1060,
"waitReason": "natural_sentence_voiced_not_credible"}`。
`hard_window` / `uncommitted_cap` 均已排除：该候选 owned 仅 2310ms
（draft_min 需 5s、max_uncommitted 需 12s，录音总长 23170ms 都到不了）。

---

## 5. case_17 修复前后对照

| 项 | 修复前 | 修复后 |
|---|---|---|
| raw VAD 边界（离线诊断） | 4：6390 / 16230 / 19440 / 21760 ms | 4（未变，VAD 行为未改动） |
| 生产采纳边界 | 4：6120 / 16080 / 19290 / **21600** | **3**：6120 / 16080 / 19290 |
| 生产 Draft span | 3：0–6120(11 字) / 6120–16080(17 字) / **19290–21600(2 字伪文本)** | **2**：0–6120(11 字) / 6120–16080(17 字) |
| 提交水位推进 | 6120 / 16080 / 19290 / 21600 | 6120 / 16080 / 19290（尾段按 NoSpeech 消费，不送模） |
| Final 字符数 | 30（= 11 + 17 + 2 伪文本） | **28**（= 11 + 17，期望文本 29 字，差句末句号） |
| 尾段模型输出 | 2 字符伪文本进入终态 | 尾段不再进入模型 |
| 整条 vs 伪流式相似度 | 未留存（私有基线按运行覆盖） | **100%**（流式 28 字 vs 整条 29 字） |
| Preview 回退字符 | 0 | 3（噪声候选作废时清退跨界 tail；该文本未提交） |

**未使用的"让测试变绿"手段**：未改 `expected_segments`；未抬高全局能量阈值或
`min_sentence_ms`；未依赖"模型最终没加字"而继续允许环境声产生 Draft；未只统计
raw VAD 边界；未按文件名/case ID 写特判。

---

## 6. 全 17 例 corpus 不匹配清单（生产 Draft 数 vs manifest 段数）

修复后不匹配（9 例）：case_01 `0/1`、case_02 `0/1`、case_05 `0/1`、case_07 `4/1`、
case_13 `4/5`、case_14 `2/5`、case_15 `5/4`、case_16 `3/2`、case_17 `2/3`。
修复前为 8 例（case_06 在本轮最终跑次中回到 `1/1` 一致；case_17 因去掉噪声 Draft
新增一条）。

口径说明：这些是"**有正文的 Draft 数**"与"人工分段期望"的差异，raw VAD 边界数已
单列诊断列、未混算。case_01/02/05 为短词——正文由**终态交付**（Draft 数 0，字符数
2/2/10 正确）；case_17 的采纳边界数 3 与期望 3 一致，但 19.29s 由模型回 NoSpeech
（无 span），内容段 2+3 仍合并（离线 VAD 在 6.39–16.23s 之间同样没有边界），故
"有正文的 Draft"为 2。除 case_03/04/15 的幻觉删减外，文本与修复前逐字一致。

---

## 7. case_12 / case_13 / case_17 专项

**case_12（通过条件全满足）**：4 个生产 Draft，全部在 Final 前提交
（commit 2680/7660/10370/13320ms）；范围 0–2.68s、2.68–7.66s、7.66–10.37s、
10.37–13.32s **连续、无重叠、无缺口**；Preview span 单调不重叠；Final 41 字
保留尾句；`preview_retreat_chars` = 0。**未为了 case_17 把首句重新合并到第二句。**

**case_13**：采纳边界 4（`draft_min` 1 + `natural_pause` 2 + `strong_pause` 1），
生产 Draft 3 → 4（更接近期望 5），最终文本**字符数不变 55**、逐字一致；范围
0–5.34 / 5.34–8.05 / 8.05–12.79 / 12.79–16.15s 连续。差异来源是送模调用数下降使
最后一个 Draft 在 finalize 前完成，提交归属由终态回到流式 span，非文本增删。

**case_17**：见 §4/§5。达标判定：正文结束后的纯环境声不再产生被生产采纳的可靠
Draft（21.60s 候选拒绝、终态尾段不送模）✓；最终文本不再追加环境声伪文本
（28 字，无 2 字伪文本）✓；生产采纳边界 3 = 期望 3 ✓；raw VAD 仍产生噪声候选，
拒绝证据为显式的 `natural_sentence_voiced_not_credible`（连续段 50ms）✓。

---

## 8. 全量测试、检查与真实 worker 回放

| 项 | 结果 |
|---|---|
| `cargo test --bin blink` | **3505 passed / 0 failed / 8 ignored** |
| `cargo test --bin blink pseudo_streaming` | 142 passed / 0 failed |
| `cargo check --bin blink --no-default-features` | 通过，零 warning |
| `git diff --check` | 通过 |
| 真实 Nano 全量 corpus 回放（17 例，`BLINK_STT_REAL_PSEUDO=1`） | **实际执行，未跳过**，1 passed，235s |
| G2 同构回放（`BLINK_STT_G2_PROJECTION=1`） | 实际执行，1 passed |
| 整条 vs 伪流式文本基线（`BLINK_STT_FULL_TEXT=all`） | 实际执行，1 passed |

真实 worker 回放聚合（修复前 → 修复后）：

| 指标 | 修复前 | 修复后 |
|---|---|---|
| 模型调用数 | 283 | 277（−2.1%） |
| 累计推理时间 | 100854ms | 92030ms（−8.7%） |
| Final 总字符 | 444 | 427（减少的都是静音/环境声尾段幻觉） |
| 无正文调用 | 64 | 55 |
| raw VAD 边界总数 | 32 | 32（未变） |
| 生产采纳总数 | 33 | 32 |
| Preview 回退字符 | 6 | 5 |
| accepted_via 直方图 | draft_min 10 / natural_pause 20 / strong_pause 1 / hard_window 2 | draft_min 10 / **natural_pause 19** / strong_pause 1 / hard_window 2 |

逐例相似度（整条基线为参照）：内容类 14 例均 ≥ 85%，其中 11 例 100%；
**case_17 100%**、case_12 100%、case_13 92%、case_15 86%、case_16 100%、
case_03 100%（修复前流式 16 字 vs 整条 7 字）。三个空期望用例（全程静音 /
仅咳嗽呼吸）流式 0 字、整条被模型幻觉出 4 字，其 0% 属正确行为。

新增/改写测试：G1/G2/G3 超时矩阵、`natural_pause`/`long_pause` 可信度门、
送模范围门语义、profile 契约（Legacy 不受门影响）、送模范围支持门槛值断言。

---

## 9. 剩余非阻断观察项

1. Preview 可变层仍允许短暂回退；交付跑次全局为 5，独立 Review 复跑为 6（修复前
   基线 6），分布会随模型输出时序浮动：交付跑次 case_17 为 3，Review 复跑为 0，
   case_12 两次均为 0。回退来自候选作废时清退跨界 tail 且不会进入可靠正文，
   **未用字符串裁剪制造视觉单调。**
2. 环境声尾段的 **Preview** 仍可能短暂显示幻觉文本（终态已不可能提交）。本轮把门
   收在采纳层与送模层，未介入预览层——预览窗口对远场轻声更敏感，贸然加门会延迟或
   抑制正常渐进上屏，建议后续专项评估"预览层噪声门"。
3. 连续强有声段判据不是"语音检测器"：它区分"连续发声形态 vs 稀疏脉冲"。若出现
   **稳态且响度稳定高于 `on`** 的环境声（如恒定转速风扇底噪），该判据不会拒绝；
   届时需引入谱平坦度/谐波性等频谱证据（当前无 FFT 依赖，热路径成本需另行评估）。
4. 模型输出存在小范围运行间差异（同一配置下 case_13/case_06 的 Draft span 计数在
   相邻跑次间浮动，文本字符数一致），A/B 对比需预留该噪声余量。
5. 人工 G2 场景验收、`long_pause_ms` 与可信有声下限的联合扫参仍待产品侧确认。

---

## 10. 文档状态

- `docs/phases/0.23-editor-voice-ai-workflow.md` §9.8 → **✅ 已完成**（含 case_17
  根因定案、判据表、修复前后对照、全量回归与观察项）。
- 阶段表 `0.23.14.7` 行 → **✅**（依赖 0.23.14.6）。
- 顶层 0.23.14 相关表述（文档状态行、阶段清单、验证条目、§九 标题）→ **✅ 已完成**。
- 源码中本批次注释的 `0.23.15` 已全部统一为 `0.23.14.7`（`src/` 内已无残留）。

**说明**：文档改动按 `AGENTS.md` §6 通常需要用户确认；本次为 HANDOFF 明确授权的
§9.8 / 0.23.14.7 / 0.23.14 状态与结论回写，建议以 `git diff` 复核后入库。
