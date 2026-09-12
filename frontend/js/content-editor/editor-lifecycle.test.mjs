/**
 * EditorLifecycle / EditorExit 测试（0.23.6 恢复链路修复）。
 *
 * 覆盖：
 * - endSession(retainDraft) 完整成功路径：flushVerified → markOrphaned →
 *   end 成功 → 才解绑草稿（orphan）；
 * - flush 失败 / orphan 标记失败 / end 失败：返回 {ok:false}，不解绑、
 *   不清理、编辑器保持可编辑（自动保存仍绑定）；
 * - end 返回 stale（后端已复位）同样视为成功并解绑；
 * - clean 会话 retainDraft 不需要 flush/orphan；
 * - clearDraft / clean 收尾走 discardCurrent / discardIfBodyEquals；
 * - EditorExit：无会话/clean 快速放行；确认后 flushVerified 失败 →
 *   confirmed:false 阻止退出；等待期间继续编辑 / 会话切换 → 不批准；
 *   全成功 → confirmed:true；重复请求去重；应答失败不上抛。
 */

// tauri.js 顶层引用 window——先备好全局再 import
globalThis.window = globalThis;

const {test} = await import("node:test");
const assert = (await import("node:assert/strict")).default;
const {EditorExit, EditorLifecycle} = await import("./editor-lifecycle.js");

const settle = () => new Promise((resolve) => setImmediate(resolve));

function makeHarness({
    active = true,
    dirty = true,
    flush = {ok: true, saved: true},
    orphan = {marked: true},
    endResult = {ok: true},
    body = "未保存正文",
} = {}) {
    const calls = {flushVerified: 0, markOrphaned: 0, orphan: 0, discardCurrent: 0,
        discardIfBodyEquals: [], ends: [], locks: []};
    const session = {
        isActive: active,
        sessionRef: active ? "s1" : null,
        generation: 1,
        async end(reason) {
            calls.ends.push(reason);
            if (endResult instanceof Error) throw endResult;
            return endResult;
        },
    };
    const adapter = {
        isDirty: () => dirty,
        getText: () => body,
    };
    const draft = {
        async flushVerified() {
            calls.flushVerified += 1;
            return flush;
        },
        async markOrphaned() {
            calls.markOrphaned += 1;
            return orphan;
        },
        orphan() {
            calls.orphan += 1;
        },
        async discardCurrent() {
            calls.discardCurrent += 1;
            return {cleared: true};
        },
        async discardIfBodyEquals(b) {
            calls.discardIfBodyEquals.push(b);
            return {cleared: true};
        },
    };
    const lifecycle = new EditorLifecycle({
        session, adapter, draft,
        setEditingLocked: (locked) => calls.locks.push(locked),
    });
    return {lifecycle, session, adapter, draft, calls};
}

// ── EditorLifecycle.endSession ────────────────────────────────────────

test("lifecycle: retainDraft 完整成功——落盘→orphan 标记→end 成功后才解绑", async () => {
    const {lifecycle, calls} = makeHarness();
    const result = await lifecycle.endSession("abandoned", {retainDraft: true});
    assert.deepEqual(result, {ok: true});
    assert.equal(calls.flushVerified, 1, "dirty 正文必须先验证式落盘");
    assert.equal(calls.markOrphaned, 1, "落盘后显式转存 orphan");
    assert.deepEqual(calls.ends, ["abandoned"]);
    assert.equal(calls.orphan, 1, "end 成功后才解除草稿绑定");
    assert.equal(calls.discardCurrent, 0, "retain 路径不清理草稿");
    assert.deepEqual(calls.locks, [true, false], "落盘、orphan、end 必须处于同一输入锁内");
});

test("lifecycle: flush 未确认 → 阻止结束，草稿绑定与排期不动", async () => {
    const {lifecycle, calls} = makeHarness({flush: {ok: false, reason: "error"}});
    const result = await lifecycle.endSession("abandoned", {retainDraft: true});
    assert.equal(result.ok, false);
    assert.equal(result.error.code, "draft_flush_failed");
    assert.equal(calls.markOrphaned, 0, "落盘未确认不得转存");
    assert.equal(calls.ends.length, 0, "不得发起 end：会话保持活跃");
    assert.equal(calls.orphan, 0, "自动保存不得解绑");
});

test("lifecycle: orphan 标记失败 → 阻止结束（不谎称已保留恢复草稿）", async () => {
    const {lifecycle, calls} = makeHarness({orphan: {marked: false}});
    const result = await lifecycle.endSession("abandoned", {retainDraft: true});
    assert.equal(result.ok, false);
    assert.equal(result.error.code, "draft_orphan_failed");
    assert.equal(calls.ends.length, 0);
    assert.equal(calls.orphan, 0);
});

test("lifecycle: end 失败 → 正文与草稿身份都保留，不解绑", async () => {
    const {lifecycle, calls, session} = makeHarness({
        endResult: {ok: false, error: {code: "io", message: "ipc broken"}},
    });
    const result = await lifecycle.endSession("abandoned", {retainDraft: true});
    assert.equal(result.ok, false);
    assert.equal(result.error.code, "io");
    assert.equal(calls.orphan, 0, "end 失败不得解除自动保存绑定");
    assert.equal(session.isActive, true, "会话身份保留");
    assert.equal(calls.discardCurrent, 0);
});

test("lifecycle: end 返回 stale（后端已复位）→ 视为成功并解绑", async () => {
    const {lifecycle, calls} = makeHarness({endResult: {ok: true, stale: true}});
    const result = await lifecycle.endSession("abandoned", {retainDraft: true});
    assert.equal(result.ok, true);
    assert.equal(calls.orphan, 1);
});

test("lifecycle: clean 会话 retainDraft 跳过落盘与 orphan 标记", async () => {
    const {lifecycle, calls} = makeHarness({dirty: false});
    const result = await lifecycle.endSession("abandoned", {retainDraft: true});
    assert.equal(result.ok, true);
    assert.equal(calls.flushVerified, 0, "clean 无可丢失正文");
    assert.equal(calls.markOrphaned, 0);
    assert.equal(calls.orphan, 1);
});

test("lifecycle: clearDraft（明确放弃/提交成功）→ end 成功后 discardCurrent", async () => {
    const {lifecycle, calls} = makeHarness({dirty: false});
    const result = await lifecycle.endSession("abandoned", {clearDraft: true});
    assert.equal(result.ok, true);
    assert.equal(calls.discardCurrent, 1);
    assert.equal(calls.discardIfBodyEquals.length, 0);
});

test("lifecycle: 普通结束且 clean → 只在磁盘草稿等于权威正文时清理", async () => {
    const {lifecycle, calls} = makeHarness({dirty: false, body: "权威正文"});
    const result = await lifecycle.endSession("abandoned");
    assert.equal(result.ok, true);
    assert.deepEqual(calls.discardIfBodyEquals, ["权威正文"]);
    assert.equal(calls.discardCurrent, 0);
});

// ── EditorExit ────────────────────────────────────────────────────────

function makeExit({
    active = true,
    dirty = true,
    choice = "ok",
    flush = {ok: true, saved: true},
    resolveError = null,
} = {}) {
    const calls = {resolves: [], flushes: 0, dialogs: 0, status: [], locks: []};
    let session = {
        isActive: active,
        sessionRef: active ? "s1" : null,
        generation: 1,
    };
    const exit = new EditorExit({
        api: {
            resolveEditorExit: async (req) => {
                calls.resolves.push({...req});
                if (resolveError) throw resolveError;
                return true;
            },
        },
        getSession: () => session,
        isDirty: () => dirty,
        flushVerified: async () => {
            calls.flushes += 1;
            return flush;
        },
        showDialog: async () => {
            calls.dialogs += 1;
            return choice;
        },
        setEditingLocked: (locked) => calls.locks.push(locked),
        onStatus: (m) => calls.status.push(m),
        t: (k) => k,
    });
    return {exit, calls, setSession: (next) => {
        session = next;
    }};
}

test("exit: 无会话 → 快速放行（不弹窗不落盘）", async () => {
    const {exit, calls} = makeExit({active: false});
    await exit.handleRequest({requestId: "r1"});
    assert.deepEqual(calls.resolves, [{requestId: "r1", confirmed: true}]);
    assert.equal(calls.dialogs, 0);
    assert.equal(calls.flushes, 0);
});

test("exit: clean 会话 → 快速放行", async () => {
    const {exit, calls} = makeExit({dirty: false});
    await exit.handleRequest({requestId: "r2"});
    assert.deepEqual(calls.resolves, [{requestId: "r2", confirmed: true}]);
    assert.equal(calls.dialogs, 0);
});

test("exit: 确认 + 落盘成功 → confirmed:true（真正等到落盘）", async () => {
    const {exit, calls} = makeExit({choice: "ok"});
    await exit.handleRequest({requestId: "r3"});
    assert.equal(calls.dialogs, 1);
    assert.equal(calls.flushes, 1, "退出前必须 await flushVerified");
    assert.deepEqual(calls.resolves, [{requestId: "r3", confirmed: true}]);
    assert.deepEqual(calls.locks, [true], "退出获准后保持锁定直到进程退出");
});

test("exit: 用户取消 → confirmed:false，不落盘", async () => {
    const {exit, calls} = makeExit({choice: "cancel"});
    await exit.handleRequest({requestId: "r4"});
    assert.equal(calls.flushes, 0);
    assert.deepEqual(calls.resolves, [{requestId: "r4", confirmed: false}]);
});

test("exit: 确认但落盘失败 → confirmed:false 阻止退出并提示", async () => {
    const {exit, calls} = makeExit({choice: "ok", flush: {ok: false, reason: "error"}});
    await exit.handleRequest({requestId: "r5"});
    assert.deepEqual(calls.resolves, [{requestId: "r5", confirmed: false}]);
    assert.deepEqual(calls.locks, [true, false], "落盘失败必须恢复编辑");
    assert.ok(calls.status.includes("editor.draft.exitFlushFailed"));
});

test("exit: 等待期间继续编辑（flush 未确认）→ 不批准退出", async () => {
    const {exit, calls} = makeExit({choice: "ok", flush: {ok: false, reason: "changed"}});
    await exit.handleRequest({requestId: "r6"});
    assert.deepEqual(calls.resolves, [{requestId: "r6", confirmed: false}],
        "旧正文的落盘结果不得批准退出");
});

test("exit: 等待期间会话被替换 → 不批准退出", async () => {
    const {exit, calls, setSession} = makeExit({choice: "ok"});
    // showDialog 返回后、flushVerified 之前会话已切换
    const originalShow = exit._showDialog;
    exit._showDialog = async () => {
        setSession({isActive: true, sessionRef: "s2", generation: 2});
        return originalShow();
    };
    await exit.handleRequest({requestId: "r7"});
    assert.equal(calls.flushes, 0, "会话已换：不得拿旧会话的落盘结果批准");
    assert.deepEqual(calls.resolves, [{requestId: "r7", confirmed: false}]);
});

test("exit: 弹窗期间重复请求被去重（后端超时兜底，迟到应答被忽略）", async () => {
    let releaseDialog;
    const gate = new Promise((resolve) => {
        releaseDialog = resolve;
    });
    const calls = {resolves: [], flushes: 0, dialogs: 0, status: []};
    let session = {isActive: true, sessionRef: "s1", generation: 1};
    const exit = new EditorExit({
        api: {resolveEditorExit: async (req) => {
            calls.resolves.push({...req});
            return true;
        }},
        getSession: () => session,
        isDirty: () => true,
        flushVerified: async () => {
            calls.flushes += 1;
            return {ok: true, saved: true};
        },
        showDialog: async () => {
            calls.dialogs += 1;
            await gate;
            return "ok";
        },
        onStatus: () => {},
        t: (k) => k,
    });
    const first = exit.handleRequest({requestId: "r8"});
    await settle();
    const second = exit.handleRequest({requestId: "r9"});
    await second;
    assert.equal(calls.dialogs, 1, "已有确认在展示时忽略重复请求");
    releaseDialog();
    await first;
    assert.deepEqual(calls.resolves, [{requestId: "r8", confirmed: true}],
        "重复请求被丢弃（后端超时放弃 r9）");
});

test("exit: 应答 IPC 失败不上抛（后端超时自行放弃）", async () => {
    const {exit, calls} = makeExit({
        choice: "ok",
        resolveError: new Error("ipc gone"),
    });
    await exit.handleRequest({requestId: "r10"}); // 不得 reject
    assert.equal(calls.resolves.length, 1);
});
