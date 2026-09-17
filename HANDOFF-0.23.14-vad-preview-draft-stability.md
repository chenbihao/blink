# 0.23.14 VAD / Preview-Draft 稳定性 Handoff

> 状态：**✅ 已实施（2026-09-17）**。P0-1～P0-4 与 P1（除 LCP 前缀晋升外）全部落地，全量 Rust 测试 3465 项通过、双构建零 warning、前端测试全绿；实现落点与验收结论见 `docs/phases/0.23-editor-voice-ai-workflow.md` §9.3/§9.5/§9.6。剩余真机项：真实 corpus 回放对照（§3.2/§七）、阈值 corpus 联合扫参、人工 G2 四场景验收。
>
> Phase 真源：`docs/phases/0.23-editor-voice-ai-workflow.md` §九。若实施中改变方案，先取得用户对文档回写的确认。

## 一、任务目标

收口 G2 / Editor 使用的 PreviewDraft 链路中五类用户可感知问题：

1. 短句后出现很长静音仍不产生可靠 Draft。
2. Preview 递增过程中回退、重复或丢前缀。
3. 时间冻结短语在识别成功前推进锚点，错误/空文本后无法恢复。
4. 过期 Preview 仍占用单 worker，推迟 Draft。
5. G2 已排队但尚未成功注入的 Draft 提前从浮窗消失。

不要把本任务简化成调低 VAD 阈值。EnergyVad 已能检测主要静音，核心问题位于候选升级、范围账本、执行调度和 UI 交接。

## 二、开工前必读

- `AGENTS.md`
- `docs/specs/spec-architecture.md`
- `docs/specs/spec-backend.md`
- 若改浮窗：`docs/specs/spec-frontend.md`
- `docs/phases/0.23-editor-voice-ai-workflow.md` §3.3～§3.5、§九
- 本文件

工作区可能包含用户并行改动；不得回滚无关文件。文档是 single source of truth，除状态更新外的方案变更仍需用户确认。

## 三、基线证据

### 3.1 用户调试日志

16.81 秒停顿梯度录音：

- `boundaries=3 / decisions=5 / commits=3 / final_chars=53`
- 三次 Draft 样本数：84960、80000、91040（16kHz 下约 5.31s、5.00s、5.69s），终态尾段约 0.81s。
- 28 次 worker 完成，累计约 9457ms，平均 337.7ms，最大 769.5ms。
- 一次过期 Preview 已运行 487.7ms 后才记录 `丢弃过期预览（句尾已发生）`。
- Preview 字符数：
  - `8 → 8 → 10 → 20 → 16 → 0`
  - `3 → 6 → 5 → 5 → 0`
  - `3 → 3 → 10 → 9 → 19 → 14 → 20 → 0`

不要在代码/日志中新增转写正文；使用字符数、AudioRange、revision、request id 和匿名 case id。

### 3.2 真实 corpus 回放

模型从 `%APPDATA%\blink\models\funasr\gguf-fun-asr-nano-q4km-9faa9616b982\active.json` 读取 `slot_id`，payload 内需存在：

- `funasr-encoder-f16.gguf`
- `qwen3-0.6b-q4km.gguf`

若 AppData 在沙箱内拒绝访问，按 `AGENTS.md` 申请只读核验，不能据此判断模型不存在。

生产默认 `silence_threshold=0.001` 回放结果：

| 样本 | 结果 |
|---|---|
| case_01 短词 | 整段/伪流式 100%，2 次调用 |
| case_08 长停顿续说 | 约 85%，7 次调用；切段改变识别上下文 |
| case_13 停顿梯度 | 约 98%，16.81s 音频 27 次调用 |
| case_16 多短停顿+久停顿 | 预期 2 段，生产 PreviewDraft 仅接受 1 个 Draft；离线 VAD 可见更多候选/短句拒绝 |
| case_09 全静音 | 0 边界，最终为空 |

匿名数值报告：`target/stt-vad-real-pseudo-replay.json`。`target/` 报告可再生；私有正文报告、文件名、WAV 只允许留在 gitignored corpus 目录，禁止提交。

### 3.3 自动测试基线

- `domain::stt::vad::tests`：41 项通过。
- `domain::stt::pseudo_streaming::tests`：89 项通过。

测试全绿只证明现有不变量；目前没有覆盖本 handoff 的核心失败场景。

## 四、根因与代码锚点

### 4.1 长静音不保证 Draft

`src/domain/stt/pseudo_streaming/mod.rs::candidate_readiness`：

- owned range ≥ `draft_min_s`（默认 5s）可接受；或
- strong pause（默认 700ms）且 owned ≥2s、voiced ≥1.2s；
- hard window / uncommitted cap 另走兜底。

因此约 0.8～2 秒短句后，无论继续静音多久，都可能无法变成 Draft。`ShortPhraseEnd` 只驱动 Preview phrase，不是可靠切分事件。

### 4.2 时间冻结不是事务

`transcribe_chunk_preview_draft` 的 `TIME_FREEZE_MIN_PREFIX_MS=1200` 路径在创建 phrase request 时立即把 `phrase_anchor` 推到 `roll_start`。`spawn_phrase_recognition` 后续若返回错误、空文本或因 generation 变化被丢弃，没有回滚/合并机制。

普通候选作废路径也在结果返回前推进 `phrase_anchor=quiet_start`，存在同类风险。

### 4.3 Preview 文本与音频范围不是同一粒度

`preview_phrases: Vec<(u64, String)>` 只记录结束位置；`latest_preview` 是多条 phrase + tail 的组合，`latest_preview_range` 却只记录最近结果。`settle_preview_after_commit` 只能按 end sample 删除 phrase，并在 committed 推进时整条清空 tail。谷底回退、部分覆盖或跨边界 phrase 无法精确处理。

### 4.4 stale 检查太晚且执行队列没有真正优先级

`spawn_preview_recognition` / `spawn_phrase_recognition` 先等待 `worker_gate`、执行完整转写，再检查 preview generation。Coordinator 记录了 Draft/Preview 状态，但 transport 任务仍由多个 `tokio::spawn` 竞争 gate；逻辑上的 Draft 优先级没有落实为 worker 前的统一调度。

PreviewDraft 路径还固定使用配置刷新间隔，没有复用 Legacy 的 `preview_interval(..., last_elapsed)` 自适应冷却。

### 4.5 G2 flush 没有确认闭环

`G2_DICTATION_RETENTION=0`；Draft 被 `send_g2_flush` 投递后 ledger 立即视为 flushed，随后浮窗 confirmed 为空。`run_g2_flush_worker` 可能晚几十毫秒成功，也可能因前台漂移/Unicode 失败进入 deferred，但没有 ack 回写 VoiceService/overlay。

### 4.6 验收参数不完全等同生产

`src/app/local_engine/funasr/corpus_runner.rs::detect_segments` 使用 `EnergyVad::new()`，其默认灵敏度仍是 0.005；生产 `SttConfig` 默认已是 0.001。先统一参数真源，再相信 segment acceptance 结果。

## 五、建议实施顺序

### P0-1 长静音可靠终结

- 在 PreviewDraft candidate policy 增加独立 long-pause 条件，而不是修改 EnergyVad 的普通 300ms 端点。
- 初始候选建议从 1000～1200ms 静音开始扫参；可信 voiced 下限同时用短词和非语音 corpus 决定，不要先写死结论。
- 允许短词在长静音期间发起 Draft；咳嗽/键盘/呼吸通过 audio gate + 模型 NoSpeech 消费，不产生 span。
- 保留 200～500ms 句内停顿不误切，G1/G3 Legacy 行为不变。

必须先写失败测试：300/600/900/1500ms voiced × 300/700/1200/2000ms silence；另含全静音、单脉冲、键盘/咳嗽近似输入。

### P0-2 PreviewSpan 与事务式锚点

- 用带 `AudioRange`、text、revision、status 的 PreviewSpan 取代 `(end_sample, text)`。
- 分开 `committed_phrase_anchor` 与 `pending_phrase_range`；识别成功且非空、身份有效后才提交。
- error/empty：保留范围并受限重试，或合并入下一短语；不得静默推进。
- stale：丢弃 pending，但必须让音频重新回到可识别覆盖范围。
- settle 按范围删除，保留边界后的尾部；对跨界 span 明确定义重新识别策略，不做字符串硬裁剪。

### P0-3 真实优先调度

- 所有 worker 请求统一进入一条调度队列：Terminal/Draft > Phrase > Preview。
- gate 获得后、调用 transport 前复核 generation/request identity；stale 直接退出且不占模型时间。
- pending Preview 保持 latest-only；Draft 到来时删除尚未开始的 Preview/Phrase（事务式 anchor 能恢复覆盖）。
- Preview 间隔至少为配置值与 `2 × last_inference_elapsed` 的较大值，并设置合理上限；Draft 在途时停止制造普通 Preview。

### P0-4 G2 注入确认

- `G2FlushJob` 增加 job/seq 身份和 ack channel；worker 返回 delivered/deferred/failed。
- ledger 不把 queued 当 delivered；浮窗保留 queued/injecting Draft，成功后才移除。
- deferred 文本仍由同一 worker 保序；终态补交必须结合 ack 水位计算，不能仅凭已入队文本裁剪 Final。
- 保持录音中 Unicode-only、防真实 Ctrl+V 打断 hold 状态机的既有铁则。

### P1 稳定前缀与成本控制

- 不把任意 1.2 秒时间前缀直接冻结成可靠文字。
- 优先低能量谷底；无谷底连续语音采用重叠窗口，多次 hypothesis 的稳定公共前缀晋升。
- 尾部允许改写；稳定前缀单调增长。保留 300～500ms 重叠上下文，使用音频范围和 LCP/LCS 对齐，禁止仅按字符串相似度决定可靠 Draft。
- 增加指标：候选/接受原因、quiet/voiced/owned ms、queue/wall/inference ms、stale-before-worker 数、Preview 回缩字符数、注入 ack 延迟。不得记录正文。

## 六、必须补的测试

1. `short_utterance_long_pause_finalizes_before_resume`
2. `short_word_long_pause_survives_noise_gate`
3. `phrase_error_does_not_advance_committed_anchor`
4. `empty_phrase_merges_or_retries_without_prefix_loss`
5. `stale_preview_is_dropped_before_transport_call`
6. `draft_runs_before_queued_phrase_and_preview`
7. `retreated_boundary_preserves_uncommitted_preview_suffix`
8. `preview_span_settle_removes_only_covered_ranges`
9. `g2_overlay_keeps_queued_draft_until_injection_ack`
10. `g2_deferred_flush_is_not_counted_as_delivered`

同时复跑现有 41 项 VAD、89 项 pseudo_streaming，防止为了体验修复破坏范围/水位不变量。

## 七、验收命令与人工验收

```powershell
cargo test --bin blink
cargo check --bin blink --no-default-features
Set-Location frontend
node --test js/voice-overlay/layout.test.mjs js/content-editor/voice.test.mjs
```

私有 corpus 回放按 `AGENTS.md` 读取当前 slot。`pseudo_streaming_real_worker_replay` 需要 `BLINK_STT_REAL_PSEUDO=1`；正文对照入口 `private_corpus_unlisted_full_transcribe` 需要 `BLINK_STT_FULL_TEXT=1`（`all` 会复验已收录 case）。优先使用临时 corpus 副本，避免覆盖现有私有 baseline；结束后恢复备份并确认 `git status --short` 无源码/文档外改动。

人工 G2 验收：

- 短词后保持 1～2 秒静音，文字在静音期间定稿并上屏。
- 说多个 300～800ms 停顿的短语，稳定前缀只增长，尾部可修正但不整段消失。
- 切走前台后制造 deferred，再回到目标并松键；浮窗待交付状态、最终文本与目标应用内容一致且不重复。
- 连续无停顿说话 >12 秒；Draft 不被 Preview 队列阻塞，worker 占用与调用数低于 0.23.13 基线。

## 八、完成定义

- Phase §9.5 全部验收通过。
- 真实 case_01/08/09/13/16 与 >12 秒连续说话样本完成匿名数值对照。
- 16.81 秒样本调用数和累计推理占用显著下降，最终相似度不低于基线。
- 日志能证明 stale Preview 在 worker 前被淘汰、G2 Draft 在 ack 后才退出浮窗。
- 全量测试、双构建与前端测试通过；自审 diff，无无关改动。
- 如需回写实现决策或调整阈值，先向用户确认后更新 phase/spec。
