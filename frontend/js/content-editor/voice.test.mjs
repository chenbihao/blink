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

test("voice: stop → stopping；ended 回落 idle", async () => {
    const {controller, handlers, calls} = makeController();
    await start(controller);

    await controller.stop();
    assert.equal(controller.phase, "stopping");
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
