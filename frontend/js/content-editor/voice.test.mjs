/**
 * EditorVoiceController（编辑器连续听写状态机）测试（0.23.3）。
 *
 * 覆盖验收（phase 文档 §6.4）：
 * - confirmed segment 重复、乱序和恢复场景无重复、丢段或错序；
 * - 身份/epoch 过滤：迟到或异会话事件不进正文；
 * - 缺号拉 snapshot 补齐；无法补齐时跳号继续并提示；
 * - preview 不进正文（控制器不消费 preview 字段）；
 * - 结束保留本次追加范围（定位/整理入口）；错误码分类文案。
 *
 * 附带：source-engine 的 dictationGapPrefix 首段分隔纯函数。
 */

// tauri.js 顶层引用 window——先备好全局再 import
globalThis.window = globalThis;

const {test} = await import("node:test");
const assert = (await import("node:assert/strict")).default;
const {EditorVoiceController} = await import("./voice.js");
const {dictationGapPrefix} = await import("./engines/source-engine.js");
const {EditorAdapter} = await import("./editor-adapter.js");

/** 构造控制器 + fake api/adapter/listen */
function makeController({snapshot = null, startError = null} = {}) {
    const calls = {starts: [], stops: [], snapshots: []};
    const api = {
        async startEditorVoice(sessionRef, generation) {
            calls.starts.push({sessionRef, generation});
            if (startError) throw startError;
            return {epoch: 7};
        },
        async stopEditorVoice() {
            calls.stops.push(true);
        },
        async getEditorVoiceSnapshot(epoch, afterSeq) {
            calls.snapshots.push({epoch, afterSeq});
            return snapshot;
        },
    };
    const appends = [];
    const adapter = {
        appendDictation(text, opts = {}) {
            appends.push({text, ...opts});
        },
        locateText() {
            return true;
        },
    };
    const handlers = {};
    const listen = async (name, handler) => {
        handlers[name] = handler;
        return () => delete handlers[name];
    };
    const session = {isActive: true, sessionRef: "ed_voice", generation: 3};
    const events = {
        phases: [],
        appended: 0,
        ended: [],
        errors: [],
        gapLost: 0,
    };
    const controller = new EditorVoiceController(
        {api, adapter, getSession: () => session, listen},
        {
            onPhaseChanged: (p) => events.phases.push(p),
            onSegmentAppended: () => {
                events.appended += 1;
            },
            onEnded: (info) => events.ended.push(info),
            onError: (m) => events.errors.push(m),
            onGapLost: () => {
                events.gapLost += 1;
            },
            describeError: (code) => `#${code}`,
        },
    );
    return {controller, api, adapter, appends, handlers, events, calls, session};
}

const SEG = "blink://editor-voice-segment";
const STS = "blink://editor-voice-status";

/** 驱动 bind + start 并等微任务清空 */
async function start(controller) {
    await controller.bind();
    const p = controller.start();
    await new Promise((r) => setTimeout(r, 0));
    await p;
}

test("voice: start 冻结会话身份并进入 recording", async () => {
    const {controller, calls} = makeController();
    await start(controller);
    assert.deepEqual(calls.starts, [{sessionRef: "ed_voice", generation: 3}]);
    assert.equal(controller.phase, "recording");
    assert.equal(controller.epoch, 7);
    assert.equal(controller.sessionRef, "ed_voice");
    assert.equal(controller.generation, 3);
});

test("voice: confirmed 段按序追加，首段补段落分隔", async () => {
    const {controller, handlers, appends} = makeController();
    await start(controller);

    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, text: "第一句。"}});
    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 2, text: "第二句。"}});

    assert.deepEqual(appends, [
        {text: "第一句。", newParagraph: true},
        {text: "第二句。", newParagraph: false},
    ]);
    assert.equal(controller.joinedText, "第一句。第二句。");
});

test("voice: typed DraftSpan 按 span 身份追加并阻止跨 seq 重复", async () => {
    const {controller, handlers, appends} = makeController();
    await start(controller);
    const span = {
        spanId: 41,
        audioRange: {startSample: 0, endSample: 80_000},
        text: "稳定草稿。",
        revision: 1,
    };
    await handlers[SEG]({payload: {
        sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, span,
    }});
    await handlers[SEG]({payload: {
        sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 2, span: {...span, revision: 2},
    }});

    assert.deepEqual(appends.map((entry) => entry.text), ["稳定草稿。"]);
    assert.deepEqual(controller.draftSpans, [{seq: 1, ...span}]);
    assert.equal(controller.lastSeq, 2, "重复 span 仍消费事件 seq，避免后续持续误报缺号");
});

test("voice: stopping 阶段仍接收 final Draft，避免尾段丢失", async () => {
    const {controller, handlers, appends} = makeController();
    await start(controller);
    controller.phase = "stopping";
    await handlers[SEG]({payload: {
        sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1,
        span: {
            spanId: 9,
            audioRange: {startSample: 0, endSample: 40_000},
            text: "尾段。",
            revision: 1,
        },
    }});
    assert.deepEqual(appends.map((entry) => entry.text), ["尾段。"]);
});

test("voice: 重复 seq 与异身份/异 epoch 事件全部忽略（§6.4 无重复）", async () => {
    const {controller, handlers, appends} = makeController();
    await start(controller);

    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, text: "A"}});
    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, text: "A"}});
    handlers[SEG]({payload: {sessionRef: "ed_other", generation: 3, epoch: 7, seq: 2, text: "X"}});
    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 9, epoch: 7, seq: 2, text: "X"}});
    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 99, seq: 2, text: "X"}});

    assert.deepEqual(appends, [{text: "A", newParagraph: true}]);
});

test("voice: 缺号拉 snapshot 补齐后再消费当前段（§6.4 无丢段）", async () => {
    const {controller, handlers, appends, calls, events} = makeController({
        snapshot: {
            epoch: 7,
            truncated: 0,
            segments: [
                {seq: 2, text: "补二"},
                {seq: 3, text: "补三"},
            ],
        },
    });
    await start(controller);

    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, text: "一"}});
    // 事件乱序：seq=4 到达而 2/3 缺失 → 拉快照补齐
    await handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 4, text: "四"}});
    await new Promise((r) => setTimeout(r, 0));

    assert.equal(calls.snapshots.length, 1);
    assert.deepEqual(calls.snapshots[0], {epoch: 7, afterSeq: 1});
    assert.deepEqual(appends.map((a) => a.text), ["一", "补二", "补三", "四"]);
    assert.deepEqual(appends.map((a) => a.newParagraph), [true, false, false, false]);
    assert.equal(controller.lastSeq, 4);
    assert.equal(events.gapLost, 0);
});

test("voice: 快照无法补齐缺号 → 跳号继续并提示（不丢当前段）", async () => {
    const {controller, handlers, appends, events} = makeController({snapshot: null});
    await start(controller);

    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, text: "一"}});
    await handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 5, text: "五"}});

    assert.deepEqual(appends.map((a) => a.text), ["一", "五"]);
    assert.equal(controller.lastSeq, 5);
    assert.equal(events.gapLost, 1);
});

test("voice: preview 只走浮窗，控制器不写正文", async () => {
    const {controller, handlers, appends} = makeController();
    await start(controller);

    handlers[STS]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, phase: "recording", preview: "预览中"}});
    assert.deepEqual(appends, [], "preview 不得进入正文");
    assert.equal(controller.phase, "recording");
});

test("voice: paused/recording 状态投影；ended 保留范围并回调", async () => {
    const {controller, handlers, events} = makeController();
    await start(controller);

    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, text: "一"}});
    handlers[STS]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, phase: "paused"}});
    assert.equal(controller.phase, "paused");

    handlers[STS]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, phase: "recording"}});
    assert.equal(controller.phase, "recording");

    handlers[STS]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, phase: "ended"}});
    assert.equal(controller.phase, "idle");
    assert.deepEqual(events.ended, [{count: 1}]);
    assert.equal(controller.joinedText, "一", "结束后本次追加文本保留供定位");
});

test("voice: stop command 返回后即使 ended 事件丢失也回落 idle", async () => {
    const {controller, handlers, calls} = makeController();
    await start(controller);

    await controller.stop();
    assert.equal(controller.phase, "idle");
    assert.equal(calls.stops.length, 1);

    handlers[STS]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, phase: "ended"}});
    assert.equal(controller.phase, "idle");
    assert.deepEqual(controller.segments, [], "无段时清空运行状态");
});

test("voice: start 结构化错误 → 文案 + 回 idle（voice_busy）", async () => {
    const {controller, events} = makeController({
        startError: {code: "voice_busy", message: "已有语音输入在进行", retryable: false},
    });
    await start(controller);
    assert.equal(controller.phase, "idle");
    assert.deepEqual(events.errors, ["#voice_busy"]);
});

test("voice: resyncIfActive 在录音中拉快照补段，idle 不动", async () => {
    const {controller, calls} = makeController({
        snapshot: {epoch: 7, truncated: 0, segments: [{seq: 1, text: "补"}]},
    });
    await controller.resyncIfActive();
    assert.equal(calls.snapshots.length, 0);

    await start(controller);
    await controller.resyncIfActive();
    assert.equal(calls.snapshots.length, 1);
    assert.equal(controller.segments.length, 1);
});

test("voice: 会话结束联动 → 清运行状态并兜底 stop", async () => {
    const {controller, calls} = makeController();
    await start(controller);
    await controller.handleSessionEnded();
    await new Promise((r) => setTimeout(r, 0));
    assert.equal(controller.phase, "idle");
    assert.equal(controller.sessionRef, null);
    assert.equal(calls.stops.length, 1, "正常路径未停时兜底释放麦克风");
});

test("voice: 已 ended 的段缓存也会在 EditorSession 结束时清空", async () => {
    const {controller, handlers} = makeController();
    await start(controller);
    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, text: "旧段"}});
    handlers[STS]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, phase: "ended"}});
    assert.equal(controller.phase, "idle");
    assert.equal(controller.joinedText, "旧段");

    controller.handleSessionEnded();
    assert.equal(controller.joinedText, "");
    assert.equal(controller.epoch, 0);
});

test("voice: STT error code 走本地化映射，不直接展示后端 message", async () => {
    const {controller, handlers, events} = makeController();
    await start(controller);
    handlers[STS]({payload: {
        sessionRef: "ed_voice", generation: 3, epoch: 7,
        phase: "error", code: "stt_failed", message: "raw backend detail",
    }});
    assert.deepEqual(events.errors, ["#stt_failed"]);
});

test("voice: idle 时 toggle 无会话给出 stale 文案", async () => {
    const {controller, events, session} = makeController();
    session.isActive = false;
    await start(controller);
    assert.equal(controller.phase, "idle");
    assert.deepEqual(events.errors, ["#stale_session"]);
});

// ── 首段分隔纯函数（Source 视图）─────────────────────────────────

test("dictationGapPrefix: 空文本/已有空行不补，单换行补一个换行，其余补空行", () => {
    assert.equal(dictationGapPrefix(""), "");
    assert.equal(dictationGapPrefix("正文"), "\n\n");
    assert.equal(dictationGapPrefix("正文\n"), "\n");
    assert.equal(dictationGapPrefix("正文\n\n"), "");
});

// ── Adapter 委托 ──────────────────────────────────────────────────

test("adapter: appendDictation 按 newParagraph 分派引擎方法", async () => {
    const {EditorAdapter: _E} = {EditorAdapter};
    const calls = [];
    const fakeEngine = {
        appendText(text) {
            calls.push(["appendText", text]);
        },
        appendParagraph(text) {
            calls.push(["appendParagraph", text]);
        },
    };
    const host = {
        sourceEl: {hidden: true},
        mdContainerEl: {hidden: true},
        mdToolbarEl: null,
    };
    const adapter = new EditorAdapter(host, {}, {
        source: () => fakeEngine,
        markdown: () => fakeEngine,
    });
    adapter.loadInitial({body: "已有内容", markdownPolicy: "disabled"});
    // loadInitial 按 disabled 策略进 Source 引擎

    adapter.appendDictation("第一段", {newParagraph: true});
    adapter.appendDictation("第二段", {newParagraph: false});
    adapter.appendDictation("", {newParagraph: true}); // 空文本忽略

    assert.deepEqual(calls, [
        ["appendParagraph", "第一段"],
        ["appendText", "第二段"],
    ]);
});

// ── 0.23.6 §5.7：本轮听写真实追加范围 ────────────────────────────────

/** 构造带 run-range 记录的 fake adapter 控制器 */
function makeRunController() {
    const calls = {begins: 0, freezes: 0};
    let anchor = null;
    let appended = "";
    const adapter = {
        appendDictation(text, opts = {}) {
            appended += text;
        },
        beginDictationRun() {
            calls.begins += 1;
            anchor = appended.length;
        },
        freezeDictationRun() {
            calls.freezes += 1;
            if (anchor == null || appended.length <= anchor) return null;
            return {
                handle: {kind: "source", start: anchor, end: appended.length},
                text: appended.slice(anchor),
                blockSafe: true,
            };
        },
    };
    const base = makeController();
    base.controller._adapter = adapter;
    return {...base, adapter, calls, get appended() {
        return appended;
    }};
}

test("voice: start 建立听写锚点，结束冻结本轮真实范围（非全文猜测）", async () => {
    const run = makeRunController();
    await start(run.controller);
    assert.equal(run.calls.begins, 1, "start 时记录锚点");

    const {handlers} = run;
    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, text: "第一句。"}});
    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 2, text: "第二句。"}});
    handlers[STS]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, phase: "ended"}});

    assert.equal(run.controller.phase, "idle");
    assert.equal(run.calls.freezes, 1, "结束时冻结一次范围");
    assert.deepEqual(run.controller.runHandle, {
        handle: {kind: "source", start: 0, end: 8},
        text: "第一句。第二句。",
        blockSafe: true,
    }, "handle 覆盖本轮全部追加文本");
});

test("voice: 无段结束/清空/失效时 runHandle 不残留", async () => {
    const run = makeRunController();
    await start(run.controller);

    // 无段结束 → _clearRun
    const {handlers} = run;
    handlers[STS]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, phase: "ended"}});
    assert.equal(run.controller.runHandle, null);
    assert.equal(run.calls.freezes, 0, "无段不冻结");

    // 有段结束后：dismiss 与视图切换失效都清缓存
    await start(run.controller);
    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, text: "一"}});
    handlers[STS]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, phase: "ended"}});
    assert.ok(run.controller.runHandle);

    run.controller.clearRunResult();
    assert.deepEqual(run.controller.segments, []);
    assert.equal(run.controller.runHandle, null);

    run.controller.segments = ["残留"];
    run.controller.runHandle = {stale: true};
    run.controller.invalidateRun();
    assert.deepEqual(run.controller.segments, []);
    assert.equal(run.controller.runHandle, null);
});

// ── 0.23.6 §5.7：听写快照缺口显式报告 ────────────────────────────────

test("voice: 快照首段不连续（区间已淘汰）→ onGapLost 一次并从最早可用段继续", async () => {
    const {controller, handlers, appends, events, calls} = makeController({
        snapshot: {
            epoch: 7,
            truncated: 5,
            segments: [
                {seq: 10, text: "十"},
                {seq: 11, text: "十一"},
            ],
        },
    });
    await start(controller);

    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, text: "一"}});
    // seq 2-9 已被快照淘汰：缺口必须显式报告
    await handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 12, text: "十二"}});
    await new Promise((r) => setTimeout(r, 0));

    assert.equal(events.gapLost, 1, "真实缺口显式报告一次");
    assert.deepEqual(appends.map((a) => a.text), ["一", "十", "十一", "十二"], "从最早可用段继续消费");
    assert.equal(controller.lastSeq, 12);
    assert.deepEqual(calls.snapshots[0], {epoch: 7, afterSeq: 1});
});

test("voice: 快照首段紧接 lastSeq 时即使 truncated>0 也不误报", async () => {
    const {controller, handlers, events, calls} = makeController({
        snapshot: {
            epoch: 7,
            truncated: 3,
            segments: [{seq: 3, text: "三"}],
        },
    });
    await start(controller);

    // 已处理 1-2，重新聚焦补齐 3：淘汰区间早已越过，不得因历史 truncated 误报
    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 1, text: "一"}});
    handlers[SEG]({payload: {sessionRef: "ed_voice", generation: 3, epoch: 7, seq: 2, text: "二"}});
    await controller.resyncIfActive();

    assert.equal(events.gapLost, 0, "已越过淘汰区间不误报");
    assert.equal(controller.segments.length, 3);
    assert.equal(calls.snapshots.length, 1);
});
