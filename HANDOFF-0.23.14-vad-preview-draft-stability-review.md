下面是一份可独立指派给 Agent 的完整 handoff。

# HANDOFF：0.23.14 VAD / Preview / Draft 稳定性 Review 后续修复与验证

## 一、背景

基线提交：`59c3d759`  
仓库：`D:\Projects\Coding\blink`

本轮 Review 结论：

- 3 个阻断问题（P1）
- 2 个次要边界问题（P2）
- 1 个补充优化：VAD 谷内切点策略

现有定向测试虽然通过，但没有覆盖以下并发顺序和边界组合。目标不是推翻 0.23.14 的设计，而是补齐 Preview 范围契约、G2 终态可靠交付，以及更稳健的低能量谷切分。

## 二、开始前必读

- `AGENTS.md`
- `docs/specs/spec-backend.md`
- `docs/specs/spec-architecture.md`
- `docs/specs/spec-phase.md`
- `docs/phases/0.23-local-stt-vad-optimization.md`

如需修改 specs 中的长期决策，先向用户确认；单纯回写 0.23.14 的实施状态和验证结果可以按 phase 规范处理。

---

## 三、问题清单

| 优先级 | 问题 | 主要风险 |
|---|---|---|
| P1 | 复合 Preview 的 `audioRange` 不完整 | confirmed 与 preview 重复、草稿漂移、调试时间轴失真 |
| P1 | Final/cancel 是否发送终态冲刷依赖异步 `deferred` 标志 | 已定稿文本可能未上屏便随 worker 退出而丢失 |
| P1 | Error 路径没有可靠的终态补交 | 前台漂移或 Unicode 注入失败时确定性丢字 |
| P2 | 过早谷底可能让时间冻结永久卡住 | 长语音 Preview 前缀滚出窗口、增量预览失效 |
| P2 | `long_pause == strong_pause` 会遮蔽强停顿规则 | 300ms 有声即可绕过原有 2s/1.2s 保护 |
| 补充 | 当前部分切点位于刚跌破阈值的位置 | 尾音被切紧、短语断词、Preview 稳定性下降 |

---

# 四、修复要求

## P1-1：修正复合 Preview 的范围契约

涉及位置：

- `src/domain/stt/pseudo_streaming/mod.rs`
  - `PreviewSpan`
  - `preview_phrases`
  - `preview_tail`
  - `latest_preview_range`
  - `settle_preview_after_commit`
  - `compose_typed_result`
- `src/app/voice.rs`
  - Draft 与缓存 Preview 的 overlap 处理

### 当前问题

`latest_preview` 是多个冻结短语与当前尾部的组合文本：

```text
PreviewSpan A + PreviewSpan B + preview_tail
```

但对外 `audioRange` 通常只对应最后一个短语或尾部。settle 后 `latest_preview_range` 还可能被清为 `None`，进而退化成：

```text
(committed_sample_end, committed_sample_end)
```

例如：

```text
Preview 文本：A + B + C
实际范围：
A = [0, 1s)
B = [1s, 2s)
C = [2s, 3s)

对外却只报告 C 的 [2s, 3s)
```

当 Draft `[0, 1s)` 到达时，`voice.rs` 判断它与 `[2s, 3s)` 不重叠，旧 Preview 不会及时退场，可能显示成：

```text
confirmed: A
preview:   A + B + C
```

### 修复要求

必须建立真实、单一的 Preview 范围真源，不允许“组合文本配最后一个局部范围”。

可选实现方向：

1. 推荐：类型化 Preview 事件携带可独立 settle 的 span 列表，包括每段文本及音频范围。
2. 或者维护组合 Preview 的完整范围包络，同时确保 Draft 提交后的 Preview 快照能够在同一个逻辑更新中原子刷新。
3. 如果仍保留单字符串 Preview，不能依靠下一音频块再修正，否则会产生短暂重复或清空闪烁。

必须满足：

- 非空 Preview 不得使用零长度退化范围。
- Preview 文本包含哪些 span，对外范围就必须覆盖哪些 span。
- Draft 覆盖前缀时，未覆盖后缀继续显示。
- 不能通过字符串长度、LCP/LCS 猜测音频归属。
- Draft 与 Preview 的 UI 投影不能出现一帧以上的重复前缀或后缀闪烁。

---

## P1-2：Final/cancel 必须无条件排入终态 barrier

涉及位置：

- `src/app/voice.rs`
  - `run_g2_flush_worker`
  - `run_g2_ack_task`
  - `send_g2_flush`
  - `deliver_g2_remaining`
  - `cancel_recording`

### 当前问题

`g2_deferred` 由独立 ack task 异步更新。Final/cancel 判断时，可能出现：

1. Draft 已经推进 ledger 的 `queued/flushed_text`。
2. 渐进 job 已发送，但 worker 尚未处理。
3. Final 文本恰好等于 `flushed_text`，所以 `remaining` 为空。
4. ack task 尚未把 `g2_deferred` 置为 `true`。
5. Final/cancel 认为无需发送终态 job。
6. worker 随后发现前台漂移，把渐进文本放入 `deferred`。
7. sender 被释放，worker 带着未上屏文本退出。

这是实际丢字，不只是浮窗状态不准。

### 修复要求

Final 和 cancel 都必须无条件向单消费者 worker 排入一个终态 barrier，即使：

- `remaining` 为空；
- 当前 `deferred` 标志为 false；
- 所有 Draft 都已经记为 queued；
- 当前看起来没有新增文本。

终态 barrier 的职责：

- 排在此前所有渐进 job 之后；
- 合并 worker 内的 deferred 文本；
- 允许完整注入路径和剪贴板降级；
- 成功后统一 ack；
- 作为“此前所有交付工作已完成”的可靠顺序边界。

`g2_deferred` 可以保留用于诊断，但不能再参与正确性决策。

同时检查：

- `send_g2_flush` 应返回发送结果。
- channel 不存在或已关闭时，不能继续把文本视为 worker 已接管。
- ledger 的 queued/flushed 水位必须与“成功进入 worker 队列”保持一致，失败时需要回滚、重排或显式终止并保留可恢复文本。
- `final_delivery` 不能在终态 job 实际没有进入队列时永久关闭重试权。

---

## P1-3：Error 路径必须有最终补交

涉及位置：

- `src/app/voice.rs`
  - `SttEvent::Error`
  - `stop_recording`
  - G2 worker 生命周期清理

### 当前问题

Error 路径把 pending Draft 作为 `unicode_only=true` 的渐进 job 发送，然后事件循环退出。

如果：

- 前台已经离开目标窗口；或
- 严格 Unicode 注入失败；

worker 会把文本放入 deferred。之后没有非 Unicode 终态 job 负责重试，最终 sender 被释放，worker 退出并丢弃 deferred。

### 修复要求

Error 不得成为“只发渐进 job 就退出”的终态。

需要确保以下任一机制成立：

1. Error 到达后立即安排终态 barrier；或
2. Error 仅标记会话终止，松键/stop 时统一排入终态 barrier；或
3. worker 生命周期由明确的 shutdown job 收口，shutdown 前强制冲刷 deferred。

必须保证：

- Error 之后已定稿 Draft 不丢失。
- 不会因为录音仍处于 hold 状态而提前注入真实 Ctrl+V、破坏输入状态机。
- 松键后的完整注入路径一定会运行。
- 终态注入失败不能只记日志并静默丢失；至少应保留可见文本并发出明确错误。

---

## P2-1：修复过早谷底导致的时间冻结失活

涉及位置：

- `src/domain/stt/pseudo_streaming/mod.rs`
  - `low_energy_valley_start_in_range`
  - 时间冻结调度逻辑
  - `RequestAudioGate::evaluate`

### 当前问题

当前逻辑总是选范围内最后一个连续低能量谷的起点，再检查该切点之前是否具有至少 500ms 有声。

例如：

```text
100ms 语音
+ 120ms 微停顿
+ 4s 连续语音
```

选出的谷底起点只有 100ms。audio gate 返回 `TooShort`，锚点不推进。

如果后续没有新的低能量谷，每次扫描仍会选择同一个 100ms 切点，时间冻结永久无法触发，旧 Preview 前缀最终滚出窗口。

### 修复要求

谷底选择必须同时满足：

- 切点之前具有足够可信有声；
- 切点大于当前 anchor；
- 切点不会落到已提交或已预留范围之前；
- 不合格的早期谷底不能阻止继续寻找后面的谷底；
- 没有合格谷底时，最终应回退到安全的 `roll_start`，而不是永久等待。

建议流程：

1. 枚举候选低能量谷。
2. 从新到旧寻找“切点前已满足有声门槛”的候选。
3. 都不合格时使用滚动窗安全切点。
4. `TooShort` 必须有进展策略，不能重复选择同一无效切点。

---

## P2-2：修正 long/strong pause 相等时的规则遮蔽

涉及位置：

- `src/domain/config/stt_config.rs`
- `src/domain/stt/pseudo_streaming/coordinator.rs`
- `src/domain/stt/pseudo_streaming/mod.rs`
- `frontend/js/settings/tabs/voice-recognition.js`

### 当前问题

当前 sanitize 只保证：

```text
long_pause_ms >= strong_pause_ms
```

允许两者相等。

但 `candidate_readiness` 先判断 long pause。两者相等时，long 分支会用约 300ms 有声门槛接受候选，强停顿原来的：

```text
owned >= 2s
voiced >= 1.2s
```

将永远不可达。

### 修复要求

选择并统一一种明确语义：

- 推荐：强制 `long_pause_ms > strong_pause_ms`，前后端使用相同最小间隔，例如一个滑块步长；或
- 调整判断顺序，让相等时先执行强停顿规则，long pause 只能在更晚时刻放宽条件。

必须同步：

- 后端配置 sanitize；
- `RecognitionSettings::sanitize`；
- 前端 normalize；
- 设置页滑块联动；
- 单元测试和配置 round-trip。

---

# 五、补充优化：统一“谷内切点”策略

## 当前行为

目前三类切点语义不一致：

1. 普通 VAD 句尾：取第一次跌破 `off_threshold` 的位置，即低能量段起点。
2. 时间冻结谷底：同样取低能量段起点。
3. 硬窗口回退：取最长低能量段的结束位置。
4. 送模前 `trim_trailing_silence` 会在最后一个有声采样后最多保留 150ms 尾部。

问题在于：如果上层已经在低能量段起点截断，后面的 `trim_trailing_silence` 无法补回已被切掉的尾音，150ms 尾部保护失效。轻声尾音、擦音和自然衰减可能被切得过紧。

## 目标策略

不要使用单个最低采样点；它容易被数字零点、爆音或随机噪声影响。

建议实现共享的“稳健谷内切点”：

1. 继续按 10ms frame 计算 RMS。
2. 找到连续低能量区间。
3. 对 RMS 做约 30ms 的短窗口平滑。
4. 在谷内选择平滑最低区域或最低平台中点。
5. 切点至少比谷起点晚约 30–50ms。
6. 第一段携带的低能量尾巴限制在约 100–150ms 内。
7. 下一段的前导静音继续由 `RequestAudioGate` 裁剪。
8. 谷底前有效语音不足门槛时，跳过该谷或回退安全切点。

示意：

```text
语音 ──自然下降── [低能量谷] ──重新上升── 语音
                  ↑ 当前部分路径切在这里
                        ↑ 建议切在平滑谷底/最低平台附近
```

## 与英文幻觉的关系

英文语气词幻觉主要来自较长静音或稳态噪声进入模型，而不是几十毫秒的自然衰减尾部。

现有 `trim_trailing_silence` 本来就允许最多 150ms 尾部缓冲。因此本次调整应遵守：

- 可以保留短衰减尾音；
- 不允许完整 300ms、800ms 或更长静音送模；
- 纯静音仍不发起模型请求；
- `/sil` 和中文尾部英文 filler 后处理保持有效；
- 不通过无限加长尾部解决断词。

最终参数应以真实 corpus 回放结果确定，不要只凭合成音频固定。

---

# 六、必须新增的自动化测试

## Preview 范围与 settle

至少覆盖：

1. `PreviewSpan A + B + tail` 组合后，对外范围覆盖全部可见文本。
2. Draft 只覆盖 A 时：
  - A 退场；
  - B 与 tail 保留；
  - 不出现 `confirmed=A, preview=A+B+tail`。
3. settle 后仍有非空 Preview 时，`audioRange` 不得退化为零长度。
4. 跨界 span 整条删除后，剩余音频重新进入尾部识别。
5. Draft 与 Preview 状态更新不依赖额外音频块才能完成一致投影。

## G2 终态与并发顺序

建议给 worker/注入层增加可控测试替身，确定性覆盖：

1. 渐进 job 已 queued，但 worker 尚未处理；Final 与 flushed 文本完全一致。
2. worker 处理渐进 job 时前台漂移，产生 deferred。
3. Final 的 remaining 为空，仍有空文本终态 barrier 排入并冲刷 deferred。
4. cancel 发生在渐进 job 处理之前，仍能完成终态补交。
5. Error 后渐进 Unicode 注入失败，松键时完整注入成功。
6. channel 关闭或 send 失败时，ledger 不得错误推进为已由 worker 接管。
7. 重复 Final/stop/cancel 不重复上屏。
8. ack 只能清退已实际成功注入的 seq。
9. 终态 barrier 成功后 worker 不得以非空 deferred 退出。

## 长静音与强停顿

覆盖：

1. `strong=1200ms, long=1200ms` 的明确预期。
2. 前后端 sanitize 结果完全一致。
3. 300ms 有声不能在相等门槛下意外绕过强停顿保护。
4. 默认 `strong=700ms, long=1100ms` 行为不回归。
5. 0.8–2s 短句 + 长静音仍能在复语前形成 Draft。

## 谷内切点与时间冻结

至少覆盖：

1. `100ms speech + 120ms valley + 4s speech` 不得永久卡住时间冻结。
2. `1s speech + 150ms valley + continuous speech` 应在谷内切分，而不是谷起点或滚动窗硬切。
3. 多个谷底时跳过 voiced 不足的早期谷，选择后续合格谷。
4. 无合格谷底时最终回退 `roll_start`。
5. 切点前后音频范围连续、不重叠、不丢样本。
6. 送模尾部低能量长度不超过约 150ms。
7. 纯静音输入不调用模型。
8. 低能量尾音不会被直接截在下降沿。

---

# 七、真实 corpus 验证

从：

```text
testdata/stt/corpus/my-wavs
```

至少重跑此前关注的：

- case 01
- case 08
- case 13
- case 16
- silence/no-speech 用例
- 一条短句后长静音用例
- 一条句内早期微停顿后连续长语音用例
- 一条中文尾音自然衰减明显的用例

使用 `%APPDATA%\blink\models\funasr\active.json` 的当前 `slot_id`，从对应 slot payload 读取真实模型。沙箱无法访问 AppData 时，申请只读权限，不得直接判定模型不存在。

需要记录：

- Draft 数量及音频范围；
- Preview 每次文本、范围和回退字符数；
- 是否出现 confirmed/preview 重复；
- worker 调用次数；
- stale-before-worker 数量；
- 每次模型耗时；
- 最终 CER/人工文本对比；
- 是否出现 `Yeah/Okay/...` 等英文幻觉；
- 每个送模 WAV 的尾部低能量时长；
- G2 queued、deferred、ack、terminal barrier 的顺序。

---

# 八、手工 G2 验证矩阵

必须真实验证：

1. 正常前台输入：
  - Draft 渐进上屏；
  - 成功 ack 后浮窗对应段退场；
  - Final 不重复注入。

2. Draft 入队后立即切换前台并松键：
  - remaining 即使为空，也会执行终态 barrier；
  - 文本最终完整进入原目标；
  - worker 不带 deferred 退出。

3. Draft 入队后立即 cancel：
  - 已定稿文本不丢；
  - 未定稿 Preview 不注入；
  - 不重复上屏。

4. 模拟 Unicode 注入失败：
  - 录音中保持 pending 可见；
  - 松键后通过完整注入路径补交；
  - ack 后再清退。

5. 模拟 STT Error：
  - 已定稿 Draft 最终补交；
  - 不因真实 Ctrl+V keydown 提前破坏 hold 状态；
  - 松键后资源正确回收。

6. 连续多段 Draft：
  - 顺序稳定；
  - 不丢段；
  - 不重复；
  - 不跨会话 ack。

---

# 九、回归命令

至少执行：

```text
cargo test --bin blink domain::stt::pseudo_streaming
cargo test --bin blink domain::stt::dictation
node --test frontend/js/settings/tabs/voice-recognition.test.mjs
cargo check --bin blink --no-default-features
cargo test --bin blink
git diff --check
```

如新增 G2 worker/scheduler 单测，必须单独列出测试名称和结果。

---

# 十、完成标准

只有同时满足以下条件才可视为完成：

- 3 个 P1 均有确定性自动化测试。
- Final/cancel/Error 都通过终态 barrier 收口。
- worker 不会以非空 deferred 正常退出。
- ledger queued/acked 水位与真实交付一致。
- 复合 Preview 的文本与音频范围一致。
- Draft settle 后无重复前缀、无退化范围、无明显闪烁。
- 过早谷底不能永久阻塞时间冻结。
- long/strong 相等语义明确且前后端一致。
- 谷内切点不再贴着刚跌破阈值的位置。
- 送模尾部静音继续受 100–150ms 上限保护。
- silence 用例不触发模型或英文幻觉。
- 真实 corpus 与手工 G2 验证完成，并附日志证据。
- 对本次改动进行 diff 自审，确认未破坏 G1/G3、Editor、Legacy profile。
