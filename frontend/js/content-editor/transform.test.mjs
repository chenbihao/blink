/**
 * EditorTransformController（编辑器 AI 整理状态机）测试（0.23.4）。
 *
 * 覆盖验收（phase 文档 §6.5）：
 * - 启动冻结会话身份 + scope + range handle + revision；
 * - AI 未配置/范围为空 → 不发请求；
 * - ai_already_active 结构化错误透出活跃窗口；
 * - 完成事件身份 + requestId 过滤；候选未经确认不触碰正文；
 * - 请求后正文变化 → stale（仅复制或重新整理，apply 拒绝）；
 * - 确认应用走 Adapter 单事务替换（replaceRange 一次调用）；
 * - Engine 复核失败（replaceRange false）→ 候选作废不抛错；
 * - 失败事件（截断/取消）回落 idle；视图切换 cancel 清理运行与候选。
 */

globalThis.window = globalThis;

const {test} = await import("node:test");
const assert = (await import("node:assert/strict")).default;
const {
    EditorTransformController,
    STILL_WORKING_AFTER_SECONDS,
    elapsedSeconds,
    runningLabelKey,
} = await import("./transform.js");
const {t: realT} = await import("../i18n/index.js");
const {SourceEngine} = await import("./engines/source-engine.js");

const COMPLETED = "blink://editor-transform-completed";
const FAILED = "blink://editor-transform-failed";

/** 候选卡 DOM 桩（0.23.7 等待态断言用：只需 textContent / disabled / querySelector）。 */
function makeCardStub() {
    const node = () => ({
        textContent: "",
        title: "",
        disabled: false,
        innerHTML: "",
        addEventListener() {},
        removeEventListener() {},
        classList: {add() {}, remove() {}, toggle() {}},
        setAttribute() {},
        getAttribute() {
            return null;
        },
    });
    const runningLabel = node();
    const runningElapsed = node();
    const body = node();
    body.querySelector = (sel) => {
        if (sel === ".editor-diff-running-label") return runningLabel;
        if (sel === ".editor-diff-running-elapsed") return runningElapsed;
        return null;
    };
    return {
        card: node(),
        title: node(),
        staleStrip: node(),
        body,
        applyBtn: node(),
        copyBtn: node(),
        discardBtn: node(),
        closeBtn: node(),
        _runningLabel: runningLabel,
        _runningElapsed: runningElapsed,
    };
}

/** 构造控制器 + fake api/adapter/listen */
function makeController({
    frozen = undefined,
    noFreeze = false,
    replaceResult = true,
    startError = null,
    revision = 5,
    el = {},
    now = undefined,
    timers = null,
} = {}) {
    const calls = {starts: [], cancels: [], replaces: [], copies: []};
    const api = {
        async startEditorTransform(request) {
            calls.starts.push(request);
            if (startError) throw startError;
            return {requestId: 101};
        },
        async cancelEditorTransform(requestId) {
            calls.cancels.push(requestId);
        },
    };
    const defaultFrozen = {handle: {kind: "source", start: 0, end: 4, text: "第一段。", blockSafe: true}, text: "第一段。", blockSafe: true};
    const adapter = {
        revision,
        freezeRange(scope, text) {
            if (noFreeze) return null;
            if (scope === "selection") return frozen ?? defaultFrozen;
            if (!text) return null;
            return {handle: {kind: "source", start: 0, end: text.length, text, blockSafe: true}, text, blockSafe: true};
        },
        replaceRange(handle, newText) {
            calls.replaces.push({handle, newText});
            return replaceResult;
        },
    };
    const handlers = {};
    const listen = async (name, handler) => {
        handlers[name] = handler;
        return () => delete handlers[name];
    };
    const session = {isActive: true, sessionRef: "ed_t", generation: 2};
    const events = {phases: [], status: [], errors: [], applied: []};
    const deps = {
        api,
        adapter,
        getSession: () => session,
        listen,
        copyToClipboard: async (text) => calls.copies.push(text),
        el,
    };
    if (now) deps.now = now;
    if (timers) {
        deps.setTimer = (fn, ms) => timers.start(fn, ms);
        deps.clearTimer = (id) => timers.clear(id);
    }
    const controller = new EditorTransformController(
        deps,
        {
            onPhaseChanged: (p) => events.phases.push(p),
            onStatus: (m) => events.status.push(m),
            onError: (m) => events.errors.push(m),
            onApplied: (s) => events.applied.push(s),
        },
    );
    return {controller, api, adapter, handlers, events, calls, session};
}

async function bind(controller) {
    await controller.bind();
}

test("transform: selection start 冻结身份与范围三元组", async () => {
    const {controller, calls} = makeController({revision: 9});
    await bind(controller);
    await controller.start("selection");

    assert.equal(calls.starts.length, 1);
    assert.deepEqual(calls.starts[0], {
        sessionRef: "ed_t",
        generation: 2,
        scope: "selection",
        text: "第一段。",
        revision: 9,
        rangeHandle: JSON.stringify({kind: "source", start: 0, end: 4, text: "第一段。", blockSafe: true}),
    });
    assert.equal(controller.phase, "running");
});

test("transform: 完成事件构建候选；未经确认不触碰正文", async () => {
    const {controller, handlers, calls} = makeController();
    await bind(controller);
    await controller.start("selection");

    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 101, scope: "selection",
        revisedText: "第一段（整理）。", revision: 5,
        rangeHandle: JSON.stringify({kind: "source", start: 0, end: 4, text: "第一段。", blockSafe: true}),
    }});

    assert.equal(controller.phase, "candidate");
    assert.equal(controller.candidate.revisedText, "第一段（整理）。");
    assert.equal(controller.candidate.stale, false, "revision 未变非 stale");
    assert.equal(calls.replaces.length, 0, "未经确认不修改正文");
});

test("transform: 完成事件 requestId/身份不匹配被忽略", async () => {
    const {controller, handlers} = makeController();
    await bind(controller);
    await controller.start("selection");

    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 999, scope: "selection",
        revisedText: "旧请求", revision: 5,
    }});
    assert.equal(controller.phase, "running", "异 requestId 不落候选");

    handlers[COMPLETED]({payload: {
        sessionRef: "ed_other", generation: 2, requestId: 101, scope: "selection",
        revisedText: "异会话", revision: 5,
    }});
    assert.equal(controller.phase, "running", "异会话不落候选");
});

test("transform: 请求期间正文变化 → 候选 stale，apply 拒绝", async () => {
    const {controller, handlers, adapter} = makeController();
    await bind(controller);
    await controller.start("selection");

    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 101, scope: "selection",
        revisedText: "整理稿", revision: 5,
    }});
    assert.equal(controller.candidate.stale, false);

    adapter.revision = 6; // 用户在整理期间编辑
    controller.notifyContentChanged();
    assert.equal(controller.candidate.stale, true);

    const applied = await controller.apply();
    assert.equal(applied, false, "stale 候选不可应用");
});

test("transform: 确认应用走单事务替换并清候选", async () => {
    const {controller, handlers, calls} = makeController({replaceResult: true});
    await bind(controller);
    await controller.start("selection");
    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 101, scope: "selection",
        revisedText: "整理稿", revision: 5,
    }});

    const applied = await controller.apply();
    assert.equal(applied, true);
    assert.equal(calls.replaces.length, 1);
    assert.equal(calls.replaces[0].newText, "整理稿");
    assert.equal(calls.replaces[0].handle.blockSafe, true);
    assert.equal(controller.candidate, null);
    assert.equal(controller.phase, "idle");
});

test("transform: Engine 复核失败（replaceRange false）→ 候选作废不抛错", async () => {
    const {controller, handlers, events} = makeController({replaceResult: false});
    await bind(controller);
    await controller.start("selection");
    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 101, scope: "selection",
        revisedText: "整理稿", revision: 5,
    }});

    const applied = await controller.apply();
    assert.equal(applied, false);
    assert.equal(controller.candidate, null, "复核失败候选作废");
    assert.ok(events.errors.length > 0);
    assert.equal(events.applied.length, 0, "失败不触发 onApplied");
});

test("transform: 应用成功回调 onApplied(scope)——听写 chips 清理接线（§6.2）", async () => {
    const {controller, handlers, events} = makeController({replaceResult: true});
    await bind(controller);
    const handle = {
        handle: {kind: "source", start: 0, end: 5, text: "本轮听写", blockSafe: true},
        text: "本轮听写",
        blockSafe: true,
    };
    await controller.start("dictation", {handle});
    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 101, scope: "dictation",
        revisedText: "整理稿", revision: 5,
    }});

    const applied = await controller.apply();
    assert.equal(applied, true);
    assert.deepEqual(events.applied, ["dictation"], "携带 scope 供接线方清理对应 chips");
});

test("transform: 候选卡内联展示应用禁用原因（跨块仅复制不再只靠 hover tooltip，§6.1）", async () => {
    const card = makeCardStub();
    // 桩的 classList.toggle 不记录状态：包一层跟踪 hidden，供可见性断言
    let staleVisible = true;
    card.staleStrip.classList.toggle = (cls, force) => {
        if (cls === "hidden") staleVisible = !(force ?? !staleVisible);
    };
    const {controller, handlers, adapter} = makeController({
        el: card,
        frozen: {handle: {kind: "markdown", from: 2, to: 6, text: "跨段范围", blockSafe: false}, text: "跨段范围", blockSafe: false},
    });
    await bind(controller);
    await controller.start("selection");
    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 101, scope: "selection",
        revisedText: "整理稿", revision: 5,
    }});

    assert.equal(card.applyBtn.disabled, true, "跨块候选应用按钮禁用");
    assert.equal(staleVisible, true, "禁用原因条内联可见");
    assert.equal(card.staleStrip.textContent, realT("editor.transform.copyOnlyHint"));

    // stale 优先于 copyOnly 展示
    adapter.revision = 6;
    controller.notifyContentChanged();
    assert.equal(staleVisible, true);
    assert.equal(card.staleStrip.textContent, realT("editor.transform.stale"));
});

test("transform: 正常候选不显示禁用原因条", async () => {
    const card = makeCardStub();
    let staleVisible = true;
    card.staleStrip.classList.toggle = (cls, force) => {
        if (cls === "hidden") staleVisible = !(force ?? !staleVisible);
    };
    const {controller, handlers} = makeController({el: card});
    await bind(controller);
    await controller.start("selection");
    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 101, scope: "selection",
        revisedText: "整理稿", revision: 5,
    }});

    assert.equal(card.applyBtn.disabled, false);
    assert.equal(staleVisible, false, "blockSafe 且非 stale 时不显示提示条");
});

test("transform: 失败事件回落 idle；cancelled 不重复报错", async () => {
    const {controller, handlers, events} = makeController();
    await bind(controller);
    await controller.start("selection");

    handlers[FAILED]({payload: {sessionRef: "ed_t", generation: 2, requestId: 101, code: "length"}});
    assert.equal(controller.phase, "idle");
    assert.ok(events.errors.some((m) => m.includes("截断")), "截断错误有提示");

    // 迟到失败（已 idle）忽略
    const before = events.errors.length;
    handlers[FAILED]({payload: {sessionRef: "ed_t", generation: 2, requestId: 101, code: "timeout"}});
    assert.equal(events.errors.length, before);
});

test("transform: cancel 运行中请求调后端；candidate 态仅本地清理", async () => {
    const {controller, handlers, calls} = makeController();
    await bind(controller);
    await controller.start("selection");

    // 运行中取消：调后端 + 清运行
    await controller.cancel({silent: true});
    assert.deepEqual(calls.cancels, [101]);
    assert.equal(controller.phase, "idle");
    assert.equal(controller.pendingRun, null);

    // candidate 态"取消"（视图切换场景）：无服务端请求可取消，仅丢候选
    await controller.start("selection");
    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 101, scope: "selection",
        revisedText: "整理稿", revision: 5,
    }});
    assert.equal(controller.phase, "candidate");
    await controller.cancel({silent: true});
    assert.deepEqual(calls.cancels, [101], "candidate 无请求，不重复调后端");
    assert.equal(controller.candidate, null);
    assert.equal(controller.phase, "idle");
});

test("transform: ai_already_active 错误携带活跃窗口文案", async () => {
    const startError = {
        code: "ai_already_active",
        message: "AI 正在其他窗口处理",
        detail: {kind: "ai_already_active", activeWindow: "chat"},
    };
    const {controller, events} = makeController({startError});
    await bind(controller);
    await controller.start("selection");

    assert.equal(controller.phase, "idle");
    assert.ok(events.errors[0].includes("AI 对话"), `错误应含活跃窗口: ${events.errors[0]}`);
});

test("transform: 范围为空不发请求；dictation 用拼接文本定位", async () => {
    const {controller, calls} = makeController({noFreeze: true});
    await bind(controller);
    await controller.start("selection", {});
    assert.equal(calls.starts.length, 0, "空选区不发请求");
    assert.equal(controller.phase, "idle");

    // dictation：adapter.freezeRange 收到拼接文本
    let captured;
    const c2 = makeController();
    c2.controller._adapter.freezeRange = (scope, text) => {
        captured = {scope, text};
        return {handle: {kind: "source", start: 0, end: 3, text, blockSafe: true}, text, blockSafe: true};
    };
    await c2.controller.start("dictation", {text: "听写内容"});
    assert.deepEqual(captured, {scope: "dictation", text: "听写内容"});
    assert.equal(c2.calls.starts.length, 1);
    assert.equal(c2.calls.starts[0].text, "听写内容");
});

test("transform: 新请求取消旧请求（§3.7）", async () => {
    const {controller, calls} = makeController();
    await bind(controller);
    await controller.start("selection"); // requestId 101
    await controller.start("selection"); // 新请求自动取消旧的

    assert.deepEqual(calls.cancels, [101]);
    assert.equal(calls.starts.length, 2);
    assert.equal(controller.phase, "running");
});

// ── SourceEngine range freeze/replace（0.23.4 §3.4 Engine 侧校验）────────

/** 最小 textarea 假体（无头环境；execCommand 不可用 → replaceRange 走降级直写） */
function fakeTextarea(initial) {
    return {
        value: initial,
        hidden: false,
        selectionStart: 0,
        selectionEnd: 0,
        listeners: {},
        addEventListener(type, fn) {
            this.listeners[type] = fn;
        },
        removeEventListener() {},
        focus() {},
        setSelectionRange(s, e) {
            this.selectionStart = s;
            this.selectionEnd = e;
        },
        scrollTop: 0,
        clientHeight: 100,
    };
}

test("engine: createSelectionRangeHandle 冻结锚点与文本", () => {
    const el = fakeTextarea("第一段。\n第二段。");
    const engine = new SourceEngine({element: el, initialText: el.value, onChange: () => {}});
    el.setSelectionRange(0, 4);
    const handle = engine.createSelectionRangeHandle();
    assert.deepEqual(
        {kind: handle.kind, start: handle.start, end: handle.end, text: handle.text, blockSafe: handle.blockSafe},
        {kind: "source", start: 0, end: 4, text: "第一段。", blockSafe: true},
    );
    // 空选区 → null
    el.setSelectionRange(2, 2);
    assert.equal(engine.createSelectionRangeHandle(), null);
});

test("engine: createTextRangeHandle 定位文本；找不到返回 null", () => {
    const el = fakeTextarea("第一段。\n第二段。");
    const engine = new SourceEngine({element: el, initialText: el.value, onChange: () => {}});
    const handle = engine.createTextRangeHandle("第二段");
    assert.equal(handle.start, 5);
    assert.equal(handle.end, 8);
    assert.equal(handle.text, "第二段");
    assert.equal(engine.createTextRangeHandle("不存在"), null);
    assert.equal(engine.createTextRangeHandle(""), null);
});

test("engine: replaceRange 复核通过后替换并通知；冻结文本不符拒绝", () => {
    const changes = [];
    const el = fakeTextarea("第一段。\n第二段。");
    const engine = new SourceEngine({
        element: el,
        initialText: el.value,
        onChange: () => changes.push(el.value),
    });

    // 冻结文本匹配：替换成功（无头环境 execCommand 不可用 → 降级直写 + 手动通知）
    const handle = {kind: "source", start: 0, end: 4, text: "第一段。"};
    assert.equal(engine.replaceRange(handle, "首段整理。"), true);
    assert.equal(el.value, "首段整理。\n第二段。");
    assert.equal(changes.length, 1, "降级路径手动通知 revision");

    // 冻结文本与当前内容不符：拒绝且零修改
    const staleHandle = {kind: "source", start: 0, end: 4, text: "旧内容"};
    assert.equal(engine.replaceRange(staleHandle, "x"), false);
    assert.equal(el.value, "首段整理。\n第二段。");
    assert.equal(changes.length, 1, "拒绝路径不触发 revision");

    // 越界锚点拒绝
    assert.equal(engine.replaceRange({kind: "source", start: 0, end: 999, text: "x"}, "y"), false);
});

test("transform: 早到完成事件（start 响应前）被采纳，不卡 running", async () => {
    const {controller, handlers} = makeController();
    await bind(controller);
    // 不 await start——模拟 IPC 响应未返回时事件先到
    const starting = controller.start("selection");
    assert.equal(controller.phase, "running");
    assert.equal(controller.runRequestId, null, "start 响应尚未回填 requestId");

    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 101, scope: "selection",
        revisedText: "整理稿", revision: 5,
    }});
    assert.equal(controller.phase, "candidate", "早到事件被采纳");
    assert.equal(controller.candidate.revisedText, "整理稿");

    await starting;
    assert.equal(controller.runRequestId, null, "候选态不写回运行槽（已完成 id 不得占用）");
    assert.equal(controller.phase, "candidate");
});

// ── 0.23.6 §5.7：AI 启动期取消追认 ───────────────────────────────────

test("transform: start 响应前取消 → 迟到响应追认取消，不复活 running", async () => {
    const {controller, calls} = makeController();
    await bind(controller);

    // 可控的 start 响应：模拟 IPC 迟迟未返回
    let releaseStart;
    controller._api.startEditorTransform = async (req) => {
        calls.starts.push(req);
        return new Promise((resolve) => {
            releaseStart = () => resolve({requestId: 202});
        });
    };

    const starting = controller.start("selection");
    await new Promise((r) => setTimeout(r, 0));
    assert.equal(controller.phase, "running");
    assert.equal(controller.runRequestId, null);

    // 响应未返回时取消（ESC/视图切换/关闭）
    const cancelling = controller.cancel({silent: true});
    assert.equal(controller.phase, "idle", "取消立即生效，不等待响应");

    releaseStart();
    await Promise.all([starting, cancelling]);

    assert.deepEqual(calls.cancels, [202], "响应到达后补发取消（追认）");
    assert.equal(controller.phase, "idle");
    assert.equal(controller.runRequestId, null, "迟到响应不得写回 requestId");
});

test("transform: 新请求使旧 start 迟到响应被追认取消，新请求不受影响", async () => {
    const {controller, calls} = makeController();
    await bind(controller);

    let releaseFirst;
    controller._api.startEditorTransform = async (req) => {
        calls.starts.push(req);
        if (calls.starts.length === 1) {
            return new Promise((resolve) => {
                releaseFirst = () => resolve({requestId: 301});
            });
        }
        return {requestId: 302};
    };

    const first = controller.start("selection");
    await new Promise((r) => setTimeout(r, 0));

    // 新请求：cancel 清 running（旧请求 requestId 尚未回填），随后 start 第二次
    const second = controller.start("selection");
    await new Promise((r) => setTimeout(r, 0));
    assert.equal(controller.phase, "running");

    releaseFirst();
    await Promise.all([first, second]);

    assert.deepEqual(calls.cancels, [301], "旧请求在迟到响应到达后被追认取消");
    assert.equal(controller.runRequestId, 302, "新请求 requestId 正常回填");
    assert.equal(controller.phase, "running");
});

test("transform: 已取消请求的迟到完成/失败事件被忽略（retired 过滤）", async () => {
    const {controller, handlers, calls, events} = makeController();
    await bind(controller);
    await controller.start("selection"); // requestId 101
    await controller.cancel({silent: true});
    assert.deepEqual(calls.cancels, [101]);

    // 迟到完成事件：不构建候选
    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 101, scope: "selection",
        revisedText: "迟到候选", revision: 5,
    }});
    assert.equal(controller.candidate, null, "已取消请求的迟到候选被忽略");
    assert.equal(controller.phase, "idle");

    // 新请求复用 fake 的同一 id（生产 id 单调不复用；此处验证退役标记不误伤新请求）
    await controller.start("selection");
    assert.equal(controller.phase, "running");
    handlers[FAILED]({payload: {sessionRef: "ed_t", generation: 2, requestId: 999, code: "cancelled"}});
    // 999 从未属于本控制器 → 忽略，不污染
    assert.equal(controller.phase, "running");
});

// ── 0.23.6 二次 Review：连续极早完成不遗留旧 requestId ───────────────

test("transform: 连续极早完成（A 放弃候选后 B）不遗留旧 id，不卡 running", async () => {
    const {controller, handlers, calls} = makeController();
    await bind(controller);

    let nextId = 401;
    controller._api.startEditorTransform = async (req) => {
        calls.starts.push(req);
        return {requestId: nextId++};
    };

    // 请求 A：完成事件早于 start 响应 → 采纳进 candidate
    const startA = controller.start("selection");
    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 401, scope: "selection",
        revisedText: "A 稿", revision: 5,
    }});
    assert.equal(controller.phase, "candidate");
    await startA;
    assert.equal(controller.runRequestId, null, "候选态不回填已完成 id");

    // 用户放弃 A 的候选 → idle
    await controller.discard();
    assert.equal(controller.phase, "idle");
    assert.equal(controller.runRequestId, null);

    // 请求 B：完成事件同样早于响应；不得被 A 遗留的旧 id 拒绝
    const startB = controller.start("selection");
    assert.equal(controller.phase, "running");
    handlers[COMPLETED]({payload: {
        sessionRef: "ed_t", generation: 2, requestId: 402, scope: "selection",
        revisedText: "B 稿", revision: 5,
    }});
    assert.equal(controller.phase, "candidate", "B 的早到完成事件被采纳");
    assert.equal(controller.candidate?.revisedText, "B 稿");

    await startB;
    assert.equal(controller.phase, "candidate", "B 响应确认候选后不回退 running");
    assert.equal(controller.runRequestId, null);
});

test("transform: retired 记录有界，长会话不无界增长", async () => {
    const {controller} = makeController();
    await bind(controller);

    let nextId = 500;
    controller._api.startEditorTransform = async () => ({requestId: nextId++});

    for (let i = 0; i < 64; i++) {
        await controller.start("selection");
        await controller.cancel({silent: true});
    }

    assert.ok(
        controller._retiredRequestIds.size <= EditorTransformController.RETIRED_CAP,
        `retired 记录应有界（实际 ${controller._retiredRequestIds.size}）`,
    );
});

// ── 0.23.6 §5.7：dictation 使用冻结的本轮听写范围 handle ─────────────

test("transform: start(dictation, {handle}) 直用冻结范围，不做 Engine 内定位", async () => {
    let freezeCalled = false;
    const {controller, calls} = makeController();
    controller._adapter.freezeRange = () => {
        freezeCalled = true;
        return null;
    };

    const handle = {
        handle: {kind: "source", start: 3, end: 8, text: "本轮听写", blockSafe: true},
        text: "本轮听写",
        blockSafe: true,
    };
    await controller.start("dictation", {handle});

    assert.equal(freezeCalled, false, "提供 handle 时不走 indexOf 定位");
    assert.equal(calls.starts.length, 1);
    assert.equal(calls.starts[0].text, "本轮听写");
    assert.equal(JSON.parse(calls.starts[0].rangeHandle).start, 3, "rangeHandle 为冻结的真实范围");
    assert.equal(controller.pendingRun.sourceText, "本轮听写");
});

// ── 0.23.7：整理等待态（spinner / 真实经过时间 / 超阈值提示 / 可取消）────

test("transform 等待态纯函数：10s 阈值与真实经过时间", () => {
    assert.equal(STILL_WORKING_AFTER_SECONDS, 10);
    assert.equal(runningLabelKey(0), "editor.transform.running");
    assert.equal(runningLabelKey(9), "editor.transform.running");
    assert.equal(runningLabelKey(10), "editor.transform.stillWorking");
    assert.equal(runningLabelKey(600), "editor.transform.stillWorking");

    assert.equal(elapsedSeconds(null, 1000), 0);
    assert.equal(elapsedSeconds(1000, null), 0);
    // 回拨/边界不做负数
    assert.equal(elapsedSeconds(1000, 999), 0);
    assert.equal(elapsedSeconds(1000, 4999), 3);
    assert.equal(elapsedSeconds(1000, 5000), 4);
});

test("transform 等待态：spinner + 真实经过时间，超 10s 提示仍在处理，且无虚假百分比", async () => {
    let clock = 1_000;
    const timers = {
        list: [],
        start(fn, ms) {
            this.list.push({fn, ms, cleared: false});
            return this.list.length;
        },
        clear(id) {
            this.list[id - 1].cleared = true;
        },
    };
    const el = makeCardStub();
    const {controller} = makeController({el, now: () => clock, timers});

    await controller.start("selection");
    assert.equal(controller.phase, "running");
    // 等待视觉语言：共享 spinner 组件 + 自定义经过时间行
    assert.match(el.body.innerHTML, /class="spinner spinner-sm"/);
    assert.match(el.body.innerHTML, /editor-diff-running-label/);
    assert.doesNotMatch(el.body.innerHTML, /%/, "不展示虚假百分比");
    // 尚不可用的应用/复制禁用；放弃按钮改为取消语义；关闭入口仍在
    assert.equal(el.applyBtn.disabled, true);
    assert.equal(el.copyBtn.disabled, true);
    assert.equal(el.discardBtn.textContent, realT("editor.transform.cancel"));
    assert.ok(el.closeBtn, "等待态保留关闭入口");
    // 心跳 1s 一次
    assert.equal(timers.list.length, 1);
    assert.equal(timers.list[0].ms, 1000);

    // 9s：仍是常态文案
    clock += 9_000;
    timers.list[0].fn();
    assert.equal(el._runningLabel.textContent, realT("editor.transform.running"));
    assert.equal(el._runningElapsed.textContent, realT("editor.transform.elapsed", {seconds: 9}));

    // 12s：切到"仍在处理中"，经过时间同步（合计 12s）
    clock += 3_000;
    timers.list[0].fn();
    assert.equal(el._runningLabel.textContent, realT("editor.transform.stillWorking"));
    assert.equal(el._runningElapsed.textContent, realT("editor.transform.elapsed", {seconds: 12}));
});

test("transform 等待态：离开 running 立即停表，无悬挂计时器", async () => {
    const timers = {
        list: [],
        start(fn, ms) {
            this.list.push({fn, ms, cleared: false});
            return this.list.length;
        },
        clear(id) {
            this.list[id - 1].cleared = true;
        },
    };
    const el = makeCardStub();
    const {controller, handlers} = makeController({el, now: () => 5_000, timers});
    await bind(controller);
    await controller.start("selection");
    assert.equal(timers.list.length, 1);

    handlers[COMPLETED]({
        payload: {
            sessionRef: "ed_t",
            generation: 2,
            requestId: 101,
            scope: "selection",
            revisedText: "第一段。",
            revision: 5,
            rangeHandle: "x",
        },
    });

    assert.equal(controller.phase, "candidate");
    assert.equal(timers.list[0].cleared, true, "进入候选态必须停表");
    assert.equal(controller.runningElapsedSeconds, 0);
    // 候选态的"放弃"恢复原语义
    assert.equal(el.discardBtn.textContent, realT("editor.transform.discard"));
});

test("transform 等待态：运行中点放弃/关闭即取消请求", async () => {
    const el = makeCardStub();
    const {controller, calls} = makeController({el});
    await bind(controller);
    await controller.start("selection");
    assert.equal(controller.runRequestId, 101);

    await controller.discard(); // 等待态下的"取消"（与关闭按钮同一入口）

    assert.equal(controller.phase, "idle");
    assert.deepEqual(calls.cancels, [101], "取消必须真实下发后端，释放全局 AI 单槽");
});

test("transform 等待态：取消后的迟到完成事件不重开候选", async () => {
    const el = makeCardStub();
    const {controller, handlers} = makeController({el});
    await bind(controller);
    await controller.start("selection");
    await controller.discard();

    handlers[COMPLETED]({
        payload: {
            sessionRef: "ed_t",
            generation: 2,
            requestId: 101,
            scope: "selection",
            revisedText: "已取消，不应成为候选",
            revision: 5,
            rangeHandle: "x",
        },
    });

    assert.equal(controller.phase, "idle");
    assert.equal(controller.candidate, null);
});
