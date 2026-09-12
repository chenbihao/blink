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
            if (api._endError) throw api._endError;
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

test("session: 每次显式提交携带唯一 mutationId（0.23.6 二次 Review）", async () => {
    const {api, adapter, callbacks, calls} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    // 同 revision、同正文的两次显式提交：意图标识必须不同——后端据此把
    // 幂等早退限定为"同一 mutation 的精确重放"，不吞新的显式输出
    await s.commit();
    await s.commit();

    assert.equal(calls.commits.length, 2);
    const [first, second] = calls.commits;
    assert.ok(first.mutationId, "commit 请求携带 mutationId");
    assert.ok(second.mutationId);
    assert.notEqual(first.mutationId, second.mutationId, "重复保存也是新提交意图");
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
    await new Promise((r) => setTimeout(r, 0)); // mutation 队列：commit 在微任务中发起
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

test("session: end 经后端确认后清空本地与外围控制器", async () => {
    const {api, adapter, callbacks, calls, adapterLog, events} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    const result = await s.end("saved");
    assert.equal(result.ok, true);
    assert.equal(s.sessionRef, null);
    assert.equal(adapterLog.resets, 1);
    assert.equal(events.cleared, 1);
    assert.equal(calls.ends.length, 1);
    assert.equal(calls.ends[0].reason, "saved");
    assert.equal(calls.ends[0].sessionRef, "ed_aaa");
});

test("session: end 失败保留正文与会话，允许重试", async () => {
    const {api, adapter, callbacks, adapterLog} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();
    api._endError = {code: "io", message: "temporary failure", retryable: true};

    const result = await s.end("abandoned");
    assert.equal(result.ok, false);
    assert.equal(s.sessionRef, "ed_aaa");
    assert.equal(adapterLog.resets, 0);
});

test("session: reset 后迟到的旧 generation 快照不能复活旧会话", async () => {
    const {api, adapter, callbacks, adapterLog} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();
    await s.end("abandoned");

    s.applySnapshot({...snapshotA, body: "迟到旧正文"});
    assert.equal(s.sessionRef, null);
    assert.equal(adapterLog.loaded.length, 1);

    s.applySnapshot({...snapshotA, sessionRef: "ed_new", generation: 2, body: "新正文"});
    assert.equal(s.sessionRef, "ed_new");
    assert.equal(adapterLog.loaded.length, 2);
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

test("session: commit 携带目标覆盖并回填 commitTarget（0.23.2）", async () => {
    const {api, adapter, callbacks, calls} = makeDeps();
    api._snapshot = snapshotA;
    api.commitEditorSession = async (request) => {
        calls.commits.push(request);
        assert.equal(request.target.kind, "save_to_file");
        assert.equal(request.target.path, "D:/out.md");
        return {sourceRevision: null, commitTarget: {kind: "confirmed_file", path: "D:/out.md"}};
    };
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    const result = await s.commit({kind: "save_to_file", path: "D:/out.md"});
    assert.equal(result.ok, true);
    assert.deepEqual(s.target, {kind: "confirmed_file", path: "D:/out.md"});
});

test("session: syncExternalContent 可前移便签冲突基线（0.23.2）", async () => {
    const {api, adapter, callbacks} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();
    const revisionBefore = s.sourceRevision;

    s.syncExternalContent("外部最新内容", 999);
    assert.equal(adapter.text, "外部最新内容");
    assert.equal(s.sourceRevision, 999);
    assert.notEqual(s.sourceRevision, revisionBefore);

    // 不带 revision 的调用不改动基线
    s.syncExternalContent("再同步");
    assert.equal(s.sourceRevision, 999);
});

test("session: applySnapshot 读取 commitTarget，reset 清空（0.23.2）", async () => {
    const {api, adapter, callbacks} = makeDeps();
    api._snapshot = {...snapshotA, commitTarget: {kind: "update_sticky", stickyId: "st1"}};
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();
    assert.deepEqual(s.target, {kind: "update_sticky", stickyId: "st1"});

    await s.end("abandoned");
    assert.equal(s.target, null);
});

// ── 0.23.6 §5.7：便签异步回载双重版本墙 ──────────────────────────────────

test("session: 便签读取期间用户开始输入 → 非 force 回流丢弃，不覆盖新输入", async () => {
    const {api, adapter, callbacks} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    let resolveNote;
    const getNote = () => new Promise((r) => {
        resolveNote = r;
    });
    const pending = s.reloadFromSticky({stickyId: "s1", getNote});

    // 读取期间用户编辑（dirty）
    adapter.isDirtyValue = true;
    adapter.text = "用户的新输入";
    resolveNote({content: "便签外部新内容", updatedAt: 200});
    const result = await pending;

    assert.equal(result.applied, false);
    assert.equal(result.dirty, true);
    assert.equal(adapter.text, "用户的新输入", "旧回流不得覆盖读取期间的新输入");
    assert.equal(s.sourceRevision, 100, "冲突基线不前移");
});

test("session: 便签异步回载后二次校验身份（§5.7 双重版本墙）", async () => {
    const {api, adapter, callbacks} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    // 便签来源切换（当前来源已是别的便签）→ 丢弃
    s.source = {kind: "sticky", stickyId: "s2"};
    const mismatched = await s.reloadFromSticky({
        stickyId: "s1",
        getNote: async () => ({content: "别的便签内容", updatedAt: 200}),
    });
    assert.equal(mismatched.applied, false);
    assert.equal(mismatched.stale, true, "stickyId 与当前来源不符时丢弃");

    // 正常路径：身份一致 → 应用
    s.source = {kind: "sticky", stickyId: "s1"};
    adapter.isDirtyValue = false;
    const ok = await s.reloadFromSticky({
        stickyId: "s1",
        getNote: async () => ({content: "外部新内容", updatedAt: 200}),
    });
    assert.equal(ok.applied, true);
    assert.equal(adapter.text, "外部新内容");
    assert.equal(s.sourceRevision, 200);
});

test("session: 同便签重开（新 generation）后旧读取不写入新会话", async () => {
    const {api, adapter, callbacks} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    let resolveNote;
    const getNote = () => new Promise((r) => {
        resolveNote = r;
    });
    const pending = s.reloadFromSticky({stickyId: "s1", getNote});

    // 读取期间同便签被重开：新 sessionRef + generation
    s.sessionRef = "ed_new";
    s.generation = 9;
    resolveNote({content: "旧读取结果", updatedAt: 300});
    const result = await pending;

    assert.equal(result.applied, false);
    assert.equal(result.stale, true, "旧 generation 的同便签读取必须被身份墙丢弃");
    assert.equal(adapter.text, "正文", "新会话正文未被旧读取覆盖");
});

test("session: force 重载跳过 dirty 检查但受身份墙约束", async () => {
    const {api, adapter, callbacks} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    // force：即使 dirty 也应用（用户显式放弃修改并重载）
    adapter.isDirtyValue = true;
    const forced = await s.reloadFromSticky({
        stickyId: "s1",
        force: true,
        getNote: async () => ({content: "DB 真源", updatedAt: 300}),
    });
    assert.equal(forced.applied, true);
    assert.equal(adapter.text, "DB 真源");

    // force 的身份墙：发起后切会话，结果丢弃
    let resolveNote;
    const getNote = () => new Promise((r) => {
        resolveNote = r;
    });
    const pending = s.reloadFromSticky({stickyId: "s1", force: true, getNote});
    s.sessionRef = "ed_other";
    s.generation = 5;
    resolveNote({content: "迟到强制重载", updatedAt: 400});
    const stale = await pending;
    assert.equal(stale.applied, false);
    assert.equal(stale.stale, true);
});

// ── 0.23.6 §5.7：commit/end 同会话串行化 ─────────────────────────────────

test("session: 保存中发起 end → 等待 commit 确定结果后才 end（串行化）", async () => {
    const {api, adapter, callbacks, calls} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    let releaseCommit;
    api._commitGate = () => new Promise((resolve) => {
        releaseCommit = resolve;
    });

    const saving = s.commit();
    await new Promise((r) => setTimeout(r, 0));
    assert.equal(calls.commits.length, 1);
    assert.equal(calls.ends.length, 0, "commit 未决时 end 不得发出");

    const ending = s.end("abandoned");
    await new Promise((r) => setTimeout(r, 0));
    assert.equal(calls.ends.length, 0, "end 排队等待 commit 完成");

    releaseCommit({sourceRevision: null});
    await Promise.all([saving, ending]);
    assert.equal(calls.ends.length, 1, "commit 确定后 end 才发出");
    assert.equal(calls.ends[0].sessionRef, "ed_aaa");
});

test("session: commit 失败后 end 仍按序执行，不产生迟到写入窗口", async () => {
    const {api, adapter, callbacks, calls} = makeDeps();
    api._snapshot = snapshotA;
    const s = new EditorSession({api, adapter}, callbacks);
    await s.activate();

    api._commitError = {code: "source_conflict", message: "冲突", retryable: true};
    const saving = s.commit();
    const ending = s.end("abandoned");
    const [saveResult, endResult] = await Promise.all([saving, ending]);

    assert.equal(saveResult.ok, false);
    assert.equal(endResult.ok, true);
    assert.equal(calls.commits.length, 1);
    assert.equal(calls.ends.length, 1);
});

// ── 0.23.6 §5.7：stale_revision 分类词条 ────────────────────────────────

test("i18n dictionaries contain staleRevision key (zh/en)", async () => {
    const {zh} = await import("../i18n/zh.js");
    const {en} = await import("../i18n/en.js");
    assert.ok(typeof zh["editor.staleRevision"] === "string", "缺少 zh 词条");
    assert.ok(typeof en["editor.staleRevision"] === "string", "缺少 en 词条");
});
