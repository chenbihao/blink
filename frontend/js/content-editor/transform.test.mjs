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
const {EditorTransformController} = await import("./transform.js");
const {SourceEngine} = await import("./engines/source-engine.js");

const COMPLETED = "blink://editor-transform-completed";
const FAILED = "blink://editor-transform-failed";

/** 构造控制器 + fake api/adapter/listen */
function makeController({
    frozen = undefined,
    noFreeze = false,
    replaceResult = true,
    startError = null,
    revision = 5,
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
    const events = {phases: [], status: [], errors: []};
    const controller = new EditorTransformController(
        {
            api,
            adapter,
            getSession: () => session,
            listen,
            copyToClipboard: async (text) => calls.copies.push(text),
            el: {}, // 无头模式
        },
        {
            onPhaseChanged: (p) => events.phases.push(p),
            onStatus: (m) => events.status.push(m),
            onError: (m) => events.errors.push(m),
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
    assert.equal(controller.runRequestId, 101);
});
