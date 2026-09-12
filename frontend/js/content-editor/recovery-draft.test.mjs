/**
 * RecoveryDraft 单测：与 Ctrl+S 分离的实时恢复草稿机制。
 *
 * 覆盖：
 * - 防抖落盘与合并（输入后 300–750ms）；
 * - 关键边界强制 flush；
 * - 单调 revision/hash 水位——旧异步写入不得覆盖新正文、迟到写入不改水位；
 * - 生命周期：bind / restore / discardCurrent（提交成功、放弃、结束）/ discardCandidate（冻结候选）；
 * - 草稿链路绝不触发文件/剪贴板/便签等保存目标副作用。
 */

import {test} from "node:test";
import assert from "node:assert/strict";
import {
    DRAFT_MAX_DEBOUNCE_MS, DRAFT_MIN_DEBOUNCE_MS, DRAFT_DEBOUNCE_MS,
    RecoveryDraft,
    draftKeyForSource,
    recoveryFenceMatches,
    recoverySourceMatches,
} from "./recovery-draft.js";
import {fnv1a64Hex} from "./canonical-buffer.js";

/** 可推进的假时钟（防抖确定性） */
function makeClock() {
    let now = 0;
    let seq = 0;
    const timers = new Map();
    return {
        setTimeout: (fn, ms) => {
            seq += 1;
            timers.set(seq, {fn, at: now + ms});
            return seq;
        },
        clearTimeout: (id) => timers.delete(id),
        advance: (ms) => {
            now += ms;
            for (const [id, t] of [...timers]) {
                if (t.at <= now) {
                    timers.delete(id);
                    t.fn();
                }
            }
        },
        pending: () => timers.size,
    };
}

function settle() {
    return new Promise((resolve) => setImmediate(resolve));
}

/** api 假体：只暴露草稿通道 + 目标副作用探针 */
function makeApi() {
    const record = {saves: [], clears: [], orphans: [], loads: [], draft: null};
    const side = {commit: 0, end: 0, clipboard: 0, sticky: 0, file: 0};
    return {
        record,
        side,
        async saveEditorDraft(req) {
            record.saves.push({...req});
            record.draft = {
                key: req.key,
                revision: req.revision,
                hash: req.hash,
                body: req.body,
                sessionRef: req.sessionRef,
                generation: req.generation,
                baseDigest: req.baseDigest,
                baseRevision: req.baseRevision,
                sourceInstanceId: req.sourceInstanceId,
                schemaVersion: req.schemaVersion,
                orphaned: false,
            };
            return {storedRevision: req.revision};
        },
        async loadEditorDraft(key) {
            record.loads.push(key);
            if (!record.draft || record.draft.key !== key) return null;
            return {...record.draft};
        },
        async clearEditorDraft(req) {
            record.clears.push({...req});
            if (record.draft
                && record.draft.key === req.key
                && record.draft.sessionRef === req.sessionRef
                && record.draft.generation === req.generation
                && record.draft.revision === req.expectedRevision
                && record.draft.hash === req.expectedHash) {
                record.draft = null;
                return true;
            }
            return false;
        },
        async orphanEditorDraft(req) {
            record.orphans.push({...req});
            if (record.draft
                && record.draft.key === req.key
                && record.draft.sessionRef === req.sessionRef
                && record.draft.generation === req.generation
                && record.draft.revision === req.expectedRevision
                && record.draft.hash === req.expectedHash) {
                record.draft.orphaned = true;
                return true;
            }
            return false;
        },
        async commitContentEditor() {
            side.commit += 1;
        },
        async endContentEditor() {
            side.end += 1;
        },
        async copyToClipboard() {
            side.clipboard += 1;
        },
        async createStickyNote() {
            side.sticky += 1;
        },
        async writeTextFile() {
            side.file += 1;
        },
    };
}

/** 构造被测控制器：text/revision/dirty 由外部变量驱动 */
function makeDraft(api, extra = {}) {
    const clock = makeClock();
    const view = {
        text: "初始",
        checkpoint: "初始",
        sourceRevision: 1,
        revision: 0,
        dirty: false,
        identity: {sessionRef: "s1", generation: 1},
    };
    const draft = new RecoveryDraft({
        api,
        getIdentity: () => (view.identity ? {...view.identity} : null),
        getText: () => view.text,
        getRevision: () => view.revision,
        getCheckpoint: () => view.checkpoint,
        getSourceRevision: () => view.sourceRevision,
        isDirty: () => view.dirty,
        hashOf: fnv1a64Hex,
        timers: {setTimeout: clock.setTimeout, clearTimeout: clock.clearTimeout},
        ...extra,
    });
    return {draft, view, clock};
}

test("draft: 防抖窗口落在 300–750ms，默认 500ms", () => {
    assert.ok(DRAFT_DEBOUNCE_MS >= DRAFT_MIN_DEBOUNCE_MS && DRAFT_DEBOUNCE_MS <= DRAFT_MAX_DEBOUNCE_MS);
    assert.ok(DRAFT_MIN_DEBOUNCE_MS >= 300);
    assert.ok(DRAFT_MAX_DEBOUNCE_MS <= 750);
});

test("draft: 草稿键跨重启稳定且按来源区分", () => {
    assert.equal(draftKeyForSource({kind: "sticky", stickyId: "s1"}), "sticky:s1");
    assert.equal(draftKeyForSource({kind: "empty"}), null, "临时来源必须由后端下发键");
    assert.equal(draftKeyForSource({kind: "selection"}), null);
    assert.equal(draftKeyForSource({kind: "clipboard_item", itemRef: "c9"}), null);
    assert.equal(draftKeyForSource({kind: "capability_result", capabilityId: "x"}), null);
    assert.equal(
        draftKeyForSource({kind: "empty"}, {draftKey: "editor:src_a"}),
        "editor:src_a",
        "临时来源使用后端快照键",
    );
    assert.equal(draftKeyForSource(null), null);
    assert.equal(draftKeyForSource({kind: "sticky"}), null, "缺 stickyId 不得产生歧义键");
});

test("draft: 读取围栏区分正文竞态与会话/来源切换", () => {
    const frozen = {
        sessionRef: "s1",
        generation: 1,
        key: "editor:src-a",
        sourceInstanceId: "src-a",
        sourceRevision: null,
        adapterRevision: 0,
        body: "初始",
        bodyHash: fnv1a64Hex("初始"),
    };
    const current = {sessionActive: true, ...frozen};
    assert.equal(recoverySourceMatches(frozen, current), true);
    assert.equal(recoveryFenceMatches(frozen, current), true);
    assert.equal(
        recoveryFenceMatches(frozen, {...current, body: "用户输入", bodyHash: fnv1a64Hex("用户输入")}),
        false,
        "用户输入只触发冲突候选，不得自动覆盖",
    );
    assert.equal(
        recoverySourceMatches(frozen, {...current, key: "editor:src-b", sourceInstanceId: "src-b"}),
        false,
        "换源后的迟到响应必须丢弃",
    );
    assert.equal(recoveryFenceMatches(frozen, {...current, sessionActive: false}), false);
});

test("draft: 输入后防抖落盘，连续输入合并为一次写入", async () => {
    const api = makeApi();
    const {draft, view, clock} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:s1"});

    view.dirty = true;
    view.text = "第一版";
    view.revision = 1;
    draft.schedule();
    assert.equal(clock.pending(), 1, "应排期一个防抖任务");

    // 未到窗口不写盘
    clock.advance(400);
    await settle();
    assert.equal(api.record.saves.length, 0);

    // 连续输入：重置窗口（合并为一次写入）
    view.text = "第二版";
    view.revision = 2;
    draft.schedule();
    clock.advance(499);
    await settle();
    assert.equal(api.record.saves.length, 0, "窗口未到不得写盘");

    clock.advance(1);
    await settle();
    assert.equal(api.record.saves.length, 1, "连续输入应合并为一次写入");
    assert.equal(api.record.saves[0].body, "第二版");
    assert.equal(api.record.saves[0].revision, 2);
});

test("draft: 关键边界 flush 立即落盘并取消排期", async () => {
    const api = makeApi();
    const {draft, view, clock} = makeDraft(api);
    draft.bind({...view.identity, key: "empty"});

    view.dirty = true;
    view.text = "切换前的正文";
    view.revision = 3;
    draft.schedule();
    await draft.flush();

    assert.equal(api.record.saves.length, 1);
    assert.equal(api.record.saves[0].body, "切换前的正文");
    assert.equal(api.record.saves[0].baseDigest, fnv1a64Hex("初始"));
    assert.equal(api.record.saves[0].baseRevision, 1);
    assert.equal(clock.pending(), 0, "flush 后不得残留防抖任务");

    // 防抖窗口过去也不重复写
    clock.advance(1000);
    await settle();
    assert.equal(api.record.saves.length, 1, "同一水位不重复写盘");
});

test("draft: 保存的来源基线取 checkpoint，不取当前工作正文", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:baseline"});
    view.checkpoint = "来源原文";
    view.text = "用户正在编辑的新正文";
    view.revision = 1;
    view.dirty = true;
    await draft.flush();
    assert.equal(api.record.saves[0].baseDigest, fnv1a64Hex("来源原文"));
    assert.notEqual(api.record.saves[0].baseDigest, fnv1a64Hex(view.text));
});

test("draft: clean（无改动）不落草稿", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "empty"});
    await draft.flush();
    assert.equal(api.record.saves.length, 0);
});

test("draft: 旧异步写入不得覆盖新正文（串行队列 + 水位）", async () => {
    const api = makeApi();
    const pending = [];
    api.saveEditorDraft = (req) => new Promise((resolve) => pending.push({req, resolve}));

    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "empty"});

    view.dirty = true;
    view.text = "旧正文";
    view.revision = 1;
    const first = draft.flush();

    view.text = "新正文";
    view.revision = 2;
    const second = draft.flush();

    await settle();
    assert.equal(pending.length, 1, "第二次写入必须排队等待第一次完成");

    pending[0].resolve({storedRevision: 1});
    await first;
    await settle();
    assert.equal(pending.length, 2, "第一次完成后才发起第二次写入");
    assert.equal(pending[1].req.body, "新正文", "最终落盘必须是新正文");
    pending[1].resolve({storedRevision: 2});
    await second;
});

test("draft: 会话身份变化后的迟到写入被丢弃且不改水位", async () => {
    const api = makeApi();
    const pending = [];
    api.saveEditorDraft = (req) => new Promise((resolve) => pending.push({req, resolve}));

    const {draft, view} = makeDraft(api);
    draft.bind({sessionRef: "s1", generation: 1, key: "empty"});
    view.dirty = true;
    view.text = "旧会话正文";
    view.revision = 5;
    void draft.flush();
    await settle();

    // 会话被替换（新 ref/generation）
    draft.unbind();
    view.identity = {sessionRef: "s2", generation: 2};
    view.text = "新会话正文";
    view.revision = 1;
    draft.bind({sessionRef: "s2", generation: 2, key: "empty"});
    view.dirty = true;
    const fresh = draft.flush();

    // 旧写入这时才返回 —— 必须被视为迟到结果
    pending[0].resolve({storedRevision: 5});
    await settle();
    await settle();
    assert.equal(pending.length, 2, "新会话的写入必须已发起");
    pending[1].resolve({storedRevision: 1});
    await fresh;
    await settle();

    const bodies = pending.map((p) => p.req.body);
    assert.deepEqual(bodies, ["旧会话正文", "新会话正文"], "两次写入各自携带自己的身份");
    assert.equal(pending[0].req.sessionRef, "s1");
    assert.equal(pending[1].req.sessionRef, "s2");
});

test("draft: defer 模式不排期但关键边界仍 flush", async () => {
    const api = makeApi();
    const {draft, view, clock} = makeDraft(api);
    draft.bind({...view.identity, key: "empty"});

    view.dirty = true;
    view.text = "大文档";
    view.revision = 1;
    draft.schedule({defer: true});
    assert.equal(clock.pending(), 0, "defer 不做高频物化");
    assert.equal(draft.pending, true, "但需要在下个边界 flush");

    await draft.flush();
    assert.equal(api.record.saves.length, 1);
    assert.equal(draft.pending, false);
});

test("draft: 成功提交 → discardCurrent 清空草稿且不再写盘", async () => {
    const api = makeApi();
    const {draft, view, clock} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:s1"});

    view.dirty = true;
    view.text = "已保存正文";
    view.revision = 1;
    await draft.flush();
    assert.equal(api.record.draft?.body, "已保存正文");

    // 提交成功：正文成为新基线，草稿不再需要
    view.dirty = false;
    await draft.discardCurrent();
    assert.equal(api.record.clears.length, 1);
    assert.equal(api.record.draft, null, "草稿必须被清理");

    // 排期也已取消
    clock.advance(1000);
    await settle();
    assert.equal(api.record.saves.length, 1);
});

test("draft: clean 清理必须再次确认草稿正文与权威来源相同", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:clean-wall"});
    view.dirty = true;
    view.text = "草稿正文";
    view.revision = 1;
    await draft.flush();

    assert.deepEqual(await draft.discardIfBodyEquals("其它权威正文"), {cleared: false});
    assert.equal(api.record.draft?.body, "草稿正文");
    assert.deepEqual(await draft.discardIfBodyEquals("草稿正文"), {cleared: true});
    assert.equal(api.record.draft, null);
});

test("draft: 清理等待在途保存后使用最新水位", async () => {
    const api = makeApi();
    let resolveSave;
    api.saveEditorDraft = async (req) => {
        recordSave(req);
        await new Promise((resolve) => { resolveSave = resolve; });
        api.record.draft = {
            key: req.key,
            revision: req.revision,
            hash: req.hash,
            body: req.body,
            sessionRef: req.sessionRef,
            generation: req.generation,
            baseDigest: req.baseDigest,
            baseRevision: req.baseRevision,
            sourceInstanceId: req.sourceInstanceId,
            schemaVersion: req.schemaVersion,
            orphaned: false,
        };
        return {storedRevision: req.revision};
    };
    const recordSave = (req) => api.record.saves.push({...req});
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:clear-wall"});
    view.dirty = true;
    view.text = "提交前最后正文";
    view.revision = 2;
    const saving = draft.flush();
    await settle();
    const clearing = draft.discardCurrent();
    resolveSave();
    await saving;
    await clearing;
    assert.equal(api.record.clears[0].expectedRevision, 2);
    assert.equal(api.record.draft, null);
});

test("draft: 放弃/结束会话 → discardCurrent；异常退出 → 草稿留在磁盘可恢复", async () => {
    const api = makeApi();

    // 场景一：正常结束（放弃修改）→ 草稿被清
    const first = makeDraft(api);
    first.draft.bind({...first.view.identity, key: "sticky:s7"});
    first.view.dirty = true;
    first.view.text = "放弃的内容";
    first.view.revision = 1;
    await first.draft.flush();
    await first.draft.discardCurrent();
    assert.equal(api.record.draft, null);

    // 场景二：异常退出（没有 discard）→ 草稿仍在
    const second = makeDraft(api);
    second.draft.bind({...second.view.identity, key: "sticky:s7"});
    second.view.dirty = true;
    second.view.text = "崩溃前的内容";
    second.view.revision = 2;
    await second.draft.flush();
    second.draft.unbind(); // 模拟进程消失：仅解绑，不清理

    assert.equal(api.record.draft?.body, "崩溃前的内容", "异常退出后草稿必须留在磁盘");

    // 重启后新会话绑定同一来源 → 基线一致时允许恢复
    const third = makeDraft(api);
    third.draft.bind({sessionRef: "s-new", generation: 9, key: "sticky:s7"});
    const restored = await third.draft.restore({
        key: "sticky:s7",
        authoritativeBody: "便签里的旧内容",
        authoritativeDigest: fnv1a64Hex("初始"),
        authoritativeRevision: 1,
    });
    assert.ok(restored, "重启后必须能恢复最新草稿");
    assert.equal(restored.status, "restore");
    assert.equal(restored.text, "崩溃前的内容");
    assert.equal(restored.revision, 2);
});

test("draft: restore 只在同键且与权威基线不同时命中", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "empty"});
    view.dirty = true;
    view.text = "草稿正文";
    view.revision = 4;
    await draft.flush();

    // 同键且来源基线一致、正文不同 → 允许恢复
    const restored = await draft.restore({
        key: "empty",
        authoritativeBody: "其它内容",
        authoritativeDigest: fnv1a64Hex("初始"),
        authoritativeRevision: 1,
    });
    assert.equal(restored?.status, "restore");
    // 与权威正文一致 → 返回 same，由调用方安全清理
    assert.equal(
        (await draft.restore({key: "empty", authoritativeBody: "草稿正文"}))?.status,
        "same",
    );
    // 异键 → 不命中
    assert.equal(await draft.restore({key: "sticky:other", authoritativeBody: "x"}), null);
    // 无草稿 → null
    api.record.draft = null;
    assert.equal(await draft.restore({key: "empty", authoritativeBody: "x"}), null);
    // 空正文草稿仍是合法恢复候选（用户可能把正文全部删空后崩溃）
    api.record.draft = {
        key: "empty",
        body: "",
        revision: 1,
        sessionRef: "s1",
        generation: 1,
        baseDigest: fnv1a64Hex("初始"),
        baseRevision: 1,
        schemaVersion: 1,
    };
    const empty = await draft.restore({
        key: "empty",
        authoritativeBody: "x",
        authoritativeDigest: fnv1a64Hex("初始"),
        authoritativeRevision: 1,
    });
    assert.equal(empty?.status, "restore");
    assert.equal(empty.text, "");
});

test("draft: 来源基线变化或旧 schema 只能生成冲突候选，不能自动恢复", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:s2"});
    view.dirty = true;
    view.text = "草稿";
    view.revision = 1;
    await draft.flush();

    const changed = await draft.restore({
        key: "sticky:s2",
        authoritativeBody: "别处的新正文",
        authoritativeDigest: fnv1a64Hex("已更新的来源"),
        authoritativeRevision: 2,
    });
    assert.equal(changed?.status, "conflict");
    assert.equal(changed?.trusted, false);

    api.record.draft.schemaVersion = 0;
    const legacy = await draft.restore({
        key: "sticky:s2",
        authoritativeBody: "其它",
        authoritativeDigest: fnv1a64Hex("初始"),
        authoritativeRevision: 1,
    });
    assert.equal(legacy?.status, "conflict");
    assert.equal(legacy?.untrusted, true);
});

test("draft: 冲突候选在返回 UI 前迁移到独立 orphan 键，后续自动保存不覆盖", async () => {
    const api = makeApi();
    const archived = [];
    api.archiveEditorDraft = async (req) => {
        const current = api.record.draft;
        if (!current || current.key !== req.key || current.revision !== req.expectedRevision
            || current.hash !== req.expectedHash) return null;
        const archivedKey = "orphan:test:1";
        archived.push({...current, key: archivedKey, orphaned: true});
        api.record.draft = null;
        return archivedKey;
    };
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:isolate"});
    api.record.draft = {
        key: "sticky:isolate", sessionRef: "old", generation: 1, revision: 4,
        hash: fnv1a64Hex("旧候选"), body: "旧候选", baseDigest: "different",
        baseRevision: 1, sourceInstanceId: "sticky:isolate", schemaVersion: 1,
        orphaned: false,
    };

    const candidate = await draft.restore({
        key: "sticky:isolate",
        authoritativeBody: "当前来源",
        authoritativeDigest: fnv1a64Hex("当前来源"),
        authoritativeRevision: 2,
    });
    assert.equal(candidate.status, "conflict");
    assert.equal(candidate.identity.key, "orphan:test:1");
    assert.equal(archived[0].body, "旧候选");
    assert.equal(archived[0].orphaned, true);

    view.dirty = true;
    view.text = "弹窗期间的新正文";
    view.revision = 5;
    await draft.flush();
    assert.equal(api.record.draft.key, "sticky:isolate");
    assert.equal(api.record.draft.body, "弹窗期间的新正文");
    assert.equal(archived[0].body, "旧候选", "独立候选不得被当前自动保存覆盖");
});

test("draft: 磁盘读取迟到时，解绑/换源会丢弃响应", async () => {
    const api = makeApi();
    let release;
    const gate = new Promise((resolve) => { release = resolve; });
    api.loadEditorDraft = async (key) => {
        await gate;
        return {
            key,
            body: "迟到草稿",
            revision: 4,
            sessionRef: "s1",
            generation: 1,
            baseDigest: fnv1a64Hex("初始"),
            baseRevision: 1,
            schemaVersion: 1,
        };
    };
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "editor:src-a"});
    const pending = draft.restore({
        key: "editor:src-a",
        authoritativeBody: "初始",
        authoritativeDigest: fnv1a64Hex("初始"),
        authoritativeRevision: 1,
    });
    draft.bind({sessionRef: "s2", generation: 2, key: "editor:src-b"});
    release();
    assert.equal(await pending, null, "来源/会话改变后迟到草稿不得回流");
});

test("draft: 来源失效转 orphan，仍可在候选列表中保留且不自动恢复", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:gone"});
    view.dirty = true;
    view.text = "来源删除前的正文";
    view.revision = 1;
    await draft.flush();
    assert.equal((await draft.markOrphaned()).marked, true);
    draft.orphan();
    assert.equal(api.record.draft.orphaned, true);

    const next = makeDraft(api);
    next.draft.bind({sessionRef: "new", generation: 3, key: "sticky:gone"});
    const candidate = await next.draft.restore({
        key: "sticky:gone",
        authoritativeBody: "新空白会话",
        authoritativeDigest: fnv1a64Hex("初始"),
        authoritativeRevision: 1,
    });
    assert.equal(candidate?.status, "conflict");
    assert.equal(candidate?.orphaned, true);
});

test("draft: 草稿链路绝不触发保存目标副作用", async () => {
    const api = makeApi();
    const {draft, view, clock} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:s1"});

    view.dirty = true;
    view.text = "正文";
    view.revision = 1;
    draft.schedule();
    clock.advance(600);
    await settle();
    await draft.flush();
    await draft.restore({key: "sticky:s1", authoritativeBody: "x"});
    await draft.discardCurrent();

    assert.deepEqual(
        api.side,
        {commit: 0, end: 0, clipboard: 0, sticky: 0, file: 0},
        "草稿保存不得触发文件/剪贴板/便签/提交等外部副作用",
    );
});

test("draft: 连续快速 flush（连续 Ctrl+S / 快速切换）不乱序，最终为最新正文", async () => {
    const api = makeApi();
    const order = [];
    api.saveEditorDraft = async (req) => {
        order.push(req.body);
        return {storedRevision: req.revision};
    };
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "empty"});
    view.dirty = true;

    for (let i = 1; i <= 6; i += 1) {
        view.text = `第${i}版`;
        view.revision = i;
        void draft.flush();
    }
    await settle();
    await settle();

    assert.deepEqual(order, ["第1版", "第2版", "第3版", "第4版", "第5版", "第6版"], "写入必须按版本顺序串行");
    assert.equal(order.at(-1), "第6版", "最终落盘必须是最新正文");
});

test("draft: 未绑定会话时不落草稿", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    view.dirty = true;
    assert.equal(draft.schedule(), false);
    assert.deepEqual(await draft.flush(), {saved: false});
    assert.equal(api.record.saves.length, 0);
});

// ── 0.23.6：候选清理与当前清理分离（恢复链路收口）──────────────────────

test("draft: discardCandidate 只按冻结版本墙清理，不借执行时的 _loadedDraft", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "editor:src-a"});

    // 磁盘上是旧候选（rev 1）
    view.dirty = true;
    view.text = "旧候选正文";
    view.revision = 1;
    await draft.flush();

    // 弹窗停留期间自动保存把当前正文写入（rev 5）
    view.text = "弹窗期间的新正文";
    view.revision = 5;
    await draft.flush();
    assert.equal(api.record.draft.body, "弹窗期间的新正文");

    // 用户选择"保留当前"：清理冻结的旧候选（rev 1 墙）
    const frozenCandidate = {
        key: "editor:src-a",
        sessionRef: "s1",
        generation: 1,
        revision: 1,
        hash: fnv1a64Hex("旧候选正文"),
    };
    assert.deepEqual(await draft.discardCandidate(frozenCandidate), {cleared: false});
    // 旧候选墙与磁盘现状不匹配 → 拒绝清理，自动保存的草稿必须保留
    assert.notEqual(api.record.draft, null, "新自动保存不得被旧候选清理误删");
    assert.equal(api.record.draft.body, "弹窗期间的新正文");

    // 墙完全匹配时才真正清理
    const exact = {
        key: "editor:src-a",
        sessionRef: "s1",
        generation: 1,
        revision: 5,
        hash: fnv1a64Hex("弹窗期间的新正文"),
    };
    assert.deepEqual(await draft.discardCandidate(exact), {cleared: true});
    assert.equal(api.record.draft, null);
});

test("draft: discardCandidate 拒绝不完整的版本墙（宁可不清理）", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "editor:src-a"});
    view.dirty = true;
    view.text = "正文";
    view.revision = 1;
    await draft.flush();

    await draft.discardCandidate({key: "editor:src-a"}); // 只有 key：拒绝
    await draft.discardCandidate({
        key: "editor:src-a",
        sessionRef: "s1",
        generation: 1,
        revision: 1,
        // 缺 hash：拒绝
    });
    await draft.discardCandidate(null);
    assert.notEqual(api.record.draft, null, "缺墙字段不得触发清理");
    assert.equal(api.record.clears.length, 0, "没有发出任何 clear IPC");
});

test("draft: discardCandidate 不取消当前排期、不改当前水位", async () => {
    const api = makeApi();
    const {draft, view, clock} = makeDraft(api);
    draft.bind({...view.identity, key: "editor:src-a"});
    view.dirty = true;
    view.text = "当前正文";
    view.revision = 2;
    draft.schedule();
    assert.equal(clock.pending(), 1);

    await draft.discardCandidate({
        key: "editor:other",
        sessionRef: "s-old",
        generation: 0,
        revision: 9,
        hash: fnv1a64Hex("别的候选"),
    });
    assert.equal(clock.pending(), 1, "候选清理不得取消当前防抖排期");

    clock.advance(600);
    await settle();
    assert.equal(api.record.saves.length, 1, "当前正文仍按防抖正常落盘");
    assert.equal(api.record.saves[0].body, "当前正文");
});

test("draft: restore 返回 identity 版本墙供调用方冻结", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:s1"});
    api.record.draft = {
        key: "sticky:s1",
        body: "崩溃前正文",
        revision: 4,
        sessionRef: "ed_old",
        generation: 2,
        baseDigest: fnv1a64Hex("初始"),
        baseRevision: 1,
        schemaVersion: 1,
    };
    const restored = await draft.restore({
        key: "sticky:s1",
        authoritativeBody: "别的",
        authoritativeDigest: fnv1a64Hex("初始"),
        authoritativeRevision: 1,
    });
    assert.deepEqual(restored.identity, {
        key: "sticky:s1",
        sessionRef: "ed_old",
        generation: 2,
        revision: 4,
        hash: fnv1a64Hex("崩溃前正文"),
    });
});

// ── 0.23.6：flushVerified 验证式落盘（强制结束会话 / 退出确认）──────────

test("draft: flushVerified——clean 直接放行，dirty 落盘成功确认水位", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:s1"});

    // clean：无可丢失正文
    view.dirty = false;
    assert.deepEqual(await draft.flushVerified(), {ok: true, clean: true});

    // dirty：真实写入后水位覆盖冻结正文
    view.dirty = true;
    view.text = "要确认的正文";
    view.revision = 3;
    const verified = await draft.flushVerified();
    assert.equal(verified.ok, true);
    assert.equal(verified.saved, true);
    assert.equal(api.record.saves.length, 1);

    // 水位已覆盖同一 revision+hash：再次验证直接确认
    assert.deepEqual(await draft.flushVerified(), {ok: true, alreadyPersisted: true});
});

test("draft: flushVerified——写入失败不得确认", async () => {
    const api = makeApi();
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:s1"});
    api.saveEditorDraft = async () => {
        throw {code: "io", message: "磁盘满"};
    };
    view.dirty = true;
    view.text = "正文";
    view.revision = 1;
    const verified = await draft.flushVerified();
    assert.equal(verified.ok, false);
    assert.equal(verified.reason, "error");
    assert.equal(verified.error.code, "io");
});

test("draft: flushVerified——写入返回 stale（身份中途变化）不得确认", async () => {
    const api = makeApi();
    let resolveSave;
    api.saveEditorDraft = () => new Promise((resolve) => {
        resolveSave = resolve;
    });
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:s1"});
    view.dirty = true;
    view.text = "旧正文";
    view.revision = 2;
    const pending = draft.flushVerified();
    await settle();
    // 写在途时会话被替换 → 写入结果 stale
    draft.unbind();
    view.identity = {sessionRef: "s2", generation: 2};
    resolveSave({storedRevision: 2});
    const verified = await pending;
    assert.equal(verified.ok, false);
    assert.equal(verified.reason, "stale");
});

test("draft: flushVerified——等待落盘期间继续编辑，旧结果不得批准", async () => {
    const api = makeApi();
    let resolveSave;
    api.saveEditorDraft = (req) => new Promise((resolve) => {
        resolveSave = () => resolve({storedRevision: req.revision});
    });
    const {draft, view} = makeDraft(api);
    draft.bind({...view.identity, key: "sticky:s1"});
    view.dirty = true;
    view.text = "冻结时正文";
    view.revision = 5;
    const pending = draft.flushVerified();
    await settle();
    // 写盘等待期间用户继续输入（revision 前进）
    view.text = "继续编辑的新正文";
    view.revision = 6;
    resolveSave();
    const verified = await pending;
    assert.equal(verified.ok, false);
    assert.equal(verified.reason, "changed", "旧正文的落盘结果不得批准危险动作");
});
