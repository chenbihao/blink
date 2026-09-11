/**
 * EditorSession（前端会话状态机）测试（0.23.1）。
 *
 * 覆盖验收（phase 文档 §6.2）：
 * - command 校验之外的会话身份语义：旧异步结果不能污染新会话；
 * - 同 sessionRef 重复 bound 不重置正文；
 * - commit 代际防护、错误分类、检查点前移；
 * - end 本地清空 + IPC 通知；外部 ended 事件防御性清空。
 *
 * 注：editor-session.js 依赖链含 tauri.js（模块顶层写 window），
 * 必须 `globalThis.window = globalThis` 先于 import。
 */

// tauri.js 顶层引用 window——先备好全局再 import
globalThis.window = globalThis;

const {test} = await import("node:test");
const assert = (await import("node:assert/strict")).default;
const {EditorSession} = await import("./editor-session.js");

function makeDeps() {
    const calls = {commits: [], ends: [], snapshots: 0};
    const adapterLog = {loaded: [], resets: 0, checkpoints: []};
    const api = {
        async getEditorSession() {
            calls.snapshots += 1;
            return api._snapshot ?? null;
        },
        async commitEditorSession(request) {
            calls.commits.push(request);
            if (api._commitGate) return api._commitGate();
            if (api._commitError) throw api._commitError;
            return {sourceRevision: null};
        },
        async endEditorSession(request) {
            calls.ends.push(request);
        },
    };
    const adapter = {
        revision: 7,
        text: "正文",
        isDirtyValue: false,
        loadInitial(opts) {
            adapterLog.loaded.push(opts);
        },
        reset() {
            adapterLog.resets += 1;
        },
        setCheckpoint(body) {
            adapterLog.checkpoints.push(body);
        },
        syncFromExternal(text) {
            adapter.text = text;
        },
        getText() {
            return adapter.text;
        },
        isDirty() {
            return adapter.isDirtyValue;
        },
        focus() {},
    };
    const events = {titles: [], statuses: [], cleared: 0, snapshotApplied: 0};
    const callbacks = {
        onTitle: (v) => events.titles.push(v),
        onStatus: (v) => events.statuses.push(v),
        onSessionCleared: () => {
            events.cleared += 1;
        },
        onSnapshotApplied: () => {
            events.snapshotApplied += 1;
        },
    };
    return {api, adapter, callbacks, calls, adapterLog, events};
}

const snapshotA = {
    sessionRef: "ed_aaa",
    generation: 1,
    title: "编辑便签内容",
    body: "A 内容",
    source: {kind: "sticky", stickyId: "s1"},
    sourceRevision: 100,
    markdownPolicy: "preferred",
};

test("session: 应用快照载入 Adapter，同 ref 重复事件不重置", async () => {
    const {api, adapter, callbacks, adapterLog, events} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);

    await s.activate();
    assert.equal(s.sessionRef, "ed_aaa");
    assert.equal(s.generation, 1);
    assert.equal(s.sourceRevision, 100);
    assert.equal(adapterLog.loaded.length, 1);
    assert.equal(adapterLog.loaded[0].body, "A 内容");
    assert.equal(events.snapshotApplied, 1);

    // 同 ref 再触发（窗口重新激活）→ 不重置正文
    s.applySnapshot({...snapshotA});
    assert.equal(adapterLog.loaded.length, 1);
});

test("session: bound 新 ref → 拉快照；ended → 防御性清空", async () => {
    const {api, adapter, callbacks, adapterLog, events} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    // 后端发出 bound(ed_bbb) 时，后端的当前会话已是 ed_bbb——快照随之更新
    api._snapshot = {...snapshotA, sessionRef: "ed_bbb", generation: 2, body: "B 内容"};
    s.handleSessionEvent({kind: "bound", sessionRef: "ed_bbb", generation: 2});
    await new Promise((r) => setTimeout(r, 0)); // activate 是异步拉取
    assert.equal(s.sessionRef, "ed_bbb");
    assert.equal(adapterLog.loaded.length, 2);
    assert.equal(adapterLog.loaded[1].body, "B 内容");

    s.handleSessionEvent({kind: "ended"});
    assert.equal(s.sessionRef, null);
    assert.equal(adapterLog.resets, 1);
    assert.equal(events.cleared, 1);
});

test("session: commit 成功 → 检查点前移、revision 提交", async () => {
    const {api, adapter, callbacks, calls, adapterLog} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    const result = await s.commit();
    assert.equal(result.ok, true);
    assert.equal(calls.commits.length, 1);
    const sent = calls.commits[0];
    assert.equal(sent.sessionRef, "ed_aaa");
    assert.equal(sent.generation, 1);
    assert.equal(sent.revision, 7);
    assert.equal(sent.body, "正文");
    assert.deepEqual(adapterLog.checkpoints, ["正文"]);
});

test("session: commit 期间会话切换 → 迟到结果不落状态", async () => {
    const {api, adapter, callbacks, calls, adapterLog} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    // 挂起 commit，期间会话切到 B
    let releaseGate;
    api._commitGate = () => new Promise((resolve) => {
        releaseGate = resolve;
    });

    const pending = s.commit();
    assert.equal(calls.commits.length, 1);

    api._snapshot = {...snapshotA, sessionRef: "ed_bbb", generation: 2};
    s.sessionRef = "ed_bbb"; // 模拟新快照已应用
    s.generation = 2;

    releaseGate({sourceRevision: null});
    const result = await pending;

    assert.equal(result.ok, true);
    assert.equal(result.stale, true);
    assert.deepEqual(adapterLog.checkpoints, [], "迟到结果不得前移检查点");
});

test("session: commit 结构化错误按 code 透传，saving 复位", async () => {
    const {api, adapter, callbacks} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    api._commitError = {code: "source_conflict", message: "内容已被外部修改", retryable: true};
    const result = await s.commit();
    assert.equal(result.ok, false);
    assert.equal(result.error.code, "source_conflict");
    assert.equal(s.saving, false);
});

test("session: end 先本地清空再通知后端", async () => {
    const {api, adapter, callbacks, calls, adapterLog} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    await s.end("saved");
    assert.equal(s.sessionRef, null);
    assert.equal(adapterLog.resets, 1);
    assert.equal(calls.ends.length, 1);
    assert.equal(calls.ends[0].reason, "saved");
    assert.equal(calls.ends[0].sessionRef, "ed_aaa");
});

test("session: 无会话时 commit/end 不动作", async () => {
    const {api, adapter, callbacks, calls} = makeDeps();
    const s = new EditorSession({api, adapter}, callbacks);

    const result = await s.commit();
    assert.equal(result.ok, false);
    await s.end("abandoned");
    assert.equal(calls.commits.length, 0);
    assert.equal(calls.ends.length, 0);
});
