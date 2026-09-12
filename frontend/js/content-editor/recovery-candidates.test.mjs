/**
 * 恢复候选编排测试（0.23.6 恢复链路收口）。
 *
 * 覆盖：
 * - RecoveryConflictResolver 四动作语义：keep/restore/copy/dismiss，
 *   关闭/Esc/遮罩类出口（dismiss）不写剪贴板、不清理候选、不动正文；
 * - 复制失败保留候选并报错；复制成功只按冻结墙清理旧候选 + flush 当前；
 * - 围栏（session/generation/来源实例/revision/正文）不匹配的迟到响应被丢弃；
 * - 端到端竞态：弹窗停留超过 debounce → 自动保存完成 → 选"保留当前" →
 *   立即模拟崩溃 → 当前正文仍可从磁盘恢复（不被旧候选清理误删）；
 * - RecoveryCandidates 列表：过滤当前键、orphan 徽标信息、恢复前围栏/落盘
 *   校验、删除携带候选自身完整版本墙、无会话禁用恢复。
 */

// tauri.js 顶层引用 window——先备好全局再 import
globalThis.window = globalThis;

const {test} = await import("node:test");
const assert = (await import("node:assert/strict")).default;
const {RecoveryConflictResolver, RecoveryCandidates, candidatePreview, candidateSourceKind} =
    await import("./recovery-candidates.js");
const {RecoveryDraft} = await import("./recovery-draft.js");
const {fnv1a64Hex} = await import("./canonical-buffer.js");

/** 可推进假时钟（与 recovery-draft.test.mjs 相同语义） */
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
    };
}

const settle = () => new Promise((resolve) => setImmediate(resolve));

/** 草稿 api 假体：带版本墙校验的 save/clear/orphan（贴近后端行为） */
function makeDraftApi() {
    const record = {saves: [], clears: [], orphans: [], clipboard: []};
    let draft = null;
    return {
        record,
        get disk() {
            return draft;
        },
        async saveEditorDraft(req) {
            record.saves.push({...req});
            draft = {orphaned: false, ...req};
            return {storedRevision: req.revision};
        },
        async loadEditorDraft(key) {
            return draft && draft.key === key ? {...draft} : null;
        },
        async clearEditorDraft(req) {
            record.clears.push({...req});
            if (draft
                && draft.key === req.key
                && draft.sessionRef === req.sessionRef
                && draft.generation === req.generation
                && draft.revision === req.expectedRevision
                && draft.hash === req.expectedHash) {
                draft = null;
                return true;
            }
            // 模拟后端身份墙：不匹配 → 保留当前草稿
            return false;
        },
        async orphanEditorDraft(req) {
            record.orphans.push({...req});
            if (draft && draft.key === req.key && draft.revision === req.expectedRevision
                && draft.hash === req.expectedHash) {
                draft.orphaned = true;
                return true;
            }
            return false;
        },
        async copyToClipboard(text) {
            record.clipboard.push(text);
        },
    };
}

/** 构造真实 RecoveryDraft + 外部可变的 view 状态 */
function makeDraftHarness(api, overrides = {}) {
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
        ...overrides,
    });
    return {draft, view, clock};
}

function fenceOf(view, extra = {}) {
    return {
        sessionActive: true,
        sessionRef: "s1",
        generation: 1,
        key: "editor:src-a",
        sourceInstanceId: "src-a",
        sourceRevision: view.sourceRevision,
        adapterRevision: view.revision,
        body: view.text,
        bodyHash: fnv1a64Hex(view.text),
        ...extra,
    };
}

function makeResolverHarness({choice, api} = {}) {
    const harness = makeDraftHarness(api ?? makeDraftApi());
    harness.draft.bind({
        ...harness.view.identity,
        key: "editor:src-a",
        sourceInstanceId: "src-a",
    });
    const status = [];
    const restores = [];
    const resolver = new RecoveryConflictResolver({
        api: {copyToClipboard: async (text) => {
            if (api) return api.copyToClipboard(text);
            throw new Error("inject api");
        }},
        draft: harness.draft,
        getFenceState: () => fenceOf(harness.view),
        restoreIntoEditor: (text) => {
            restores.push(text);
            return true;
        },
        showDialog: async () => choice,
        onStatus: (m) => status.push(m),
        t: (key) => key,
    });
    return {resolver, harness, status, restores};
}

const CANDIDATE_BODY = "崩溃前未保存的长正文";

test("resolver: dismiss（关闭/Esc/遮罩）不写剪贴板、不清理、不动正文", async () => {
    const api = makeDraftApi();
    // 磁盘上有旧候选
    await api.saveEditorDraft({
        key: "editor:src-a", sessionRef: "s1", generation: 1, revision: 1,
        hash: fnv1a64Hex("旧候选"), body: "旧候选", baseDigest: "d", baseRevision: 1,
        sourceInstanceId: "src-a", schemaVersion: 1,
    });
    const {resolver, harness, status, restores} = makeResolverHarness({choice: "dismiss", api});
    const frozen = fenceOf(harness.view);
    const result = await resolver.resolve(frozen, {
        text: CANDIDATE_BODY,
        identity: {key: "editor:src-a", sessionRef: "s1", generation: 1, revision: 1, hash: fnv1a64Hex("旧候选")},
    });
    assert.equal(result.action, "dismiss");
    assert.equal(api.record.clipboard.length, 0, "关闭弹窗绝不能写剪贴板");
    assert.equal(api.record.clears.length, 0, "未明确选择删除/放弃不得清理候选");
    assert.equal(restores.length, 0, "正文不动");
    assert.ok(status.includes("editor.draft.deferred"));
    assert.equal(api.disk.body, "旧候选", "候选原样保留");
});

test("resolver: keep 精确清理冻结候选并立即 flush 当前正文", async () => {
    const api = makeDraftApi();
    await api.saveEditorDraft({
        key: "editor:src-a", sessionRef: "s1", generation: 1, revision: 1,
        hash: fnv1a64Hex("旧候选"), body: "旧候选", baseDigest: "d", baseRevision: 1,
        sourceInstanceId: "src-a", schemaVersion: 1,
    });
    const {resolver, harness} = makeResolverHarness({choice: "keep", api});
    // 当前正文已 dirty
    harness.view.dirty = true;
    harness.view.text = "当前正在写的正文";
    harness.view.revision = 5;
    const frozen = fenceOf(harness.view);
    const result = await resolver.resolve(frozen, {
        text: CANDIDATE_BODY,
        identity: {key: "editor:src-a", sessionRef: "s1", generation: 1, revision: 1, hash: fnv1a64Hex("旧候选")},
    });
    assert.equal(result.action, "kept");
    // 旧候选按冻结墙被清理，随后当前正文落盘
    assert.equal(api.record.clears.length, 1);
    const clearReq = api.record.clears[0];
    assert.equal(clearReq.expectedRevision, 1, "清理必须携带候选自身 revision");
    assert.equal(clearReq.expectedHash, fnv1a64Hex("旧候选"));
    assert.equal(api.disk.body, "当前正在写的正文", "清理后当前正文立即重新落盘");
    assert.equal(api.disk.revision, 5);
});

test("resolver: copy 是显式动作；失败保留候选，成功后清理候选 + flush 当前", async () => {
    // 失败路径
    const failApi = makeDraftApi();
    await failApi.saveEditorDraft({
        key: "editor:src-a", sessionRef: "s1", generation: 1, revision: 1,
        hash: fnv1a64Hex("旧候选"), body: "旧候选", baseDigest: "d", baseRevision: 1,
        sourceInstanceId: "src-a", schemaVersion: 1,
    });
    failApi.copyToClipboard = async () => {
        throw new Error("clipboard locked");
    };
    const failing = makeResolverHarness({choice: "copy", api: failApi});
    const r1 = await failing.resolver.resolve(fenceOf(failing.harness.view), {
        text: CANDIDATE_BODY,
        identity: {key: "editor:src-a", sessionRef: "s1", generation: 1, revision: 1, hash: fnv1a64Hex("旧候选")},
    });
    assert.equal(r1.action, "copy-failed");
    assert.equal(failApi.disk.body, "旧候选", "复制失败候选必须保留");
    assert.equal(failApi.record.clears.length, 0);
    assert.ok(failing.status.includes("editor.draft.copyFailed"));

    // 成功路径
    const okApi = makeDraftApi();
    await okApi.saveEditorDraft({
        key: "editor:src-a", sessionRef: "s1", generation: 1, revision: 1,
        hash: fnv1a64Hex("旧候选"), body: "旧候选", baseDigest: "d", baseRevision: 1,
        sourceInstanceId: "src-a", schemaVersion: 1,
    });
    const ok = makeResolverHarness({choice: "copy", api: okApi});
    ok.harness.view.dirty = true;
    ok.harness.view.text = "当前正文";
    ok.harness.view.revision = 3;
    const r2 = await ok.resolver.resolve(fenceOf(ok.harness.view), {
        text: CANDIDATE_BODY,
        identity: {key: "editor:src-a", sessionRef: "s1", generation: 1, revision: 1, hash: fnv1a64Hex("旧候选")},
    });
    assert.equal(r2.action, "copied");
    assert.deepEqual(okApi.record.clipboard, [CANDIDATE_BODY], "只有显式复制才写剪贴板");
    assert.equal(okApi.disk.body, "当前正文", "当前正文在候选清理后重新落盘");
});

test("resolver: restore 只替换正文，不清理候选", async () => {
    const api = makeDraftApi();
    await api.saveEditorDraft({
        key: "editor:src-a", sessionRef: "s1", generation: 1, revision: 1,
        hash: fnv1a64Hex("旧候选"), body: "旧候选", baseDigest: "d", baseRevision: 1,
        sourceInstanceId: "src-a", schemaVersion: 1,
    });
    const {resolver, harness, restores} = makeResolverHarness({choice: "restore", api});
    const r = await resolver.resolve(fenceOf(harness.view), {
        text: CANDIDATE_BODY,
        identity: {key: "editor:src-a", sessionRef: "s1", generation: 1, revision: 1, hash: fnv1a64Hex("旧候选")},
    });
    assert.equal(r.action, "restored");
    assert.deepEqual(restores, [CANDIDATE_BODY]);
    assert.equal(api.record.clears.length, 0, "恢复不是删除：候选保留");
    assert.equal(api.record.clipboard.length, 0);
});

test("resolver: 弹窗后围栏不匹配（迟到响应）→ 只提示，零副作用", async () => {
    const api = makeDraftApi();
    const {resolver, harness, status, restores} = makeResolverHarness({choice: "keep", api});
    const frozen = fenceOf(harness.view);
    // 弹窗等待期间用户继续输入：当前状态已偏离 frozen
    harness.view.text = "弹窗期间的新输入";
    harness.view.revision = 9;
    const r = await resolver.resolve(frozen, {
        text: CANDIDATE_BODY,
        identity: {key: "editor:src-a", sessionRef: "s1", generation: 1, revision: 1, hash: fnv1a64Hex("x")},
    });
    assert.equal(r.action, "stale");
    assert.equal(api.record.clears.length, 0);
    assert.equal(api.record.clipboard.length, 0);
    assert.equal(restores.length, 0);
    assert.ok(status.includes("editor.draft.staleFence"));
});

test("竞态端到端：弹窗停留超过 debounce → 自动保存 → 保留当前 → 崩溃后当前正文可恢复", async () => {
    const api = makeDraftApi();
    const {draft, view, clock} = makeDraftHarness(api);
    draft.bind({sessionRef: "s1", generation: 1, key: "editor:src-a", sourceInstanceId: "src-a"});

    // 磁盘残留旧候选（rev 1，崩溃前一轮）
    await api.saveEditorDraft({
        key: "editor:src-a", sessionRef: "s0", generation: 0, revision: 1,
        hash: fnv1a64Hex("旧候选正文"), body: "旧候选正文", baseDigest: "d", baseRevision: 1,
        sourceInstanceId: "src-old", schemaVersion: 1,
    });

    // bind 探测读到旧候选 → 冲突弹窗
    const restored = await draft.restore({
        key: "editor:src-a",
        authoritativeBody: "初始",
        authoritativeDigest: fnv1a64Hex("初始"),
        authoritativeRevision: 1,
    });
    assert.equal(restored.status, "conflict");

    // 用户在弹窗期间持续输入（排期自动保存）
    view.dirty = true;
    view.text = "弹窗期间写的新正文";
    view.revision = 2;
    draft.schedule();

    // 弹窗停留超过 debounce（500ms）→ 自动保存完成
    clock.advance(600);
    await settle();
    assert.equal(api.disk.body, "弹窗期间写的新正文", "自动保存应已覆盖旧候选");
    assert.equal(api.disk.revision, 2);

    // 用户选择"保留当前"
    const resolver = new RecoveryConflictResolver({
        api,
        draft,
        getFenceState: () => fenceOf(view),
        restoreIntoEditor: () => true,
        showDialog: async () => "keep",
        onStatus: () => {},
        t: (k) => k,
    });
    const frozen = fenceOf(view);
    const result = await resolver.resolve(frozen, restored);
    assert.equal(result.action, "kept");

    // 立即模拟崩溃（不再有后续写入）：磁盘必须仍是当前正文
    assert.equal(api.disk.body, "弹窗期间写的新正文", "崩溃后当前正文必须可从磁盘恢复");
    assert.equal(api.disk.revision, 2);
});

test("candidates: 纯函数——来源类型与谨慎预览", () => {
    assert.equal(candidateSourceKind("sticky:s1"), "sticky");
    assert.equal(candidateSourceKind("editor:src_abc"), "temporary");
    assert.equal(candidatePreview("第一行\n第二行"), "第一行", "预览只取首行");
    assert.equal(candidatePreview("  多个   空白  "), "多个 空白", "空白压缩");
    assert.equal(candidatePreview(""), "", "空正文无预览");
    const long = "a".repeat(80);
    assert.equal(candidatePreview(long, 60).length, 61, "截断到 60 + 省略号");
});

// ── 列表控制器（DOM 桩）──────────────────────────────────────────────

function makeDom() {
    const docListeners = {};
    function makeElement(tag) {
        const el = {
            tagName: tag, className: "", _children: [], _listeners: {}, _attributes: {},
            disabled: false, dataset: {},
            classList: {
                _set: new Set(),
                add(...c) {
                    c.forEach((x) => this._set.add(x));
                },
                remove(...c) {
                    c.forEach((x) => this._set.delete(x));
                },
                toggle() {
                },
                contains(c) {
                    return this._set.has(c);
                },
            },
            setAttribute(k, v) {
                this._attributes[k] = String(v);
            },
            appendChild(child) {
                this._children.push(child);
                child._parent = this;
                return child;
            },
            remove() {
                if (this._parent) {
                    const i = this._parent._children.indexOf(this);
                    if (i >= 0) this._parent._children.splice(i, 1);
                    this._parent = null;
                }
            },
            addEventListener(t, f) {
                (this._listeners[t] ??= []).push(f);
            },
            removeEventListener() {
            },
            dispatchEvent(ev) {
                const a = this._listeners[ev?.type];
                if (a) [...a].forEach((f) => f(ev));
                return true;
            },
            focus() {
            },
            querySelector() {
                return null;
            },
            querySelectorAll() {
                return [];
            },
            get textContent() {
                return this._textContent;
            },
            set textContent(v) {
                this._textContent = String(v);
            },
        };
        return el;
    }

    globalThis.requestAnimationFrame = (cb) => setTimeout(cb, 0);
    globalThis.document = {
        createElement: makeElement,
        body: makeElement("body"),
        addEventListener(type, fn, opts) {
            const capture = opts === true || (opts && opts.capture === true);
            const key = type + (capture ? "::capture" : "");
            (docListeners[key] ??= []).push(fn);
        },
        removeEventListener(type, fn, opts) {
            const capture = opts === true || (opts && opts.capture === true);
            const key = type + (capture ? "::capture" : "");
            const arr = docListeners[key];
            if (arr) {
                const i = arr.indexOf(fn);
                if (i >= 0) arr.splice(i, 1);
            }
        },
        querySelector() {
            return null;
        },
    };
    return {
        document: globalThis.document,
        dispatchKeydown: (key) => {
            const arr = docListeners["keydown::capture"];
            const ev = {key, preventDefault() {}, stopPropagation() {}};
            if (arr) [...arr].forEach((fn) => fn(ev));
        },
        keydownCount: () => (docListeners["keydown::capture"] ?? []).length,
    };
}

function makeCandidatesHarness({
    drafts, fence, dirty = false, flushOk = true, confirm = true, clearResult = true,
} = {}) {
    const dom = makeDom();
    const calls = {lists: 0, clears: [], clipboard: [], restores: [], status: []};
    const api = {
        async listEditorDrafts() {
            calls.lists += 1;
            return typeof drafts === "function" ? drafts() : drafts;
        },
        async clearEditorDraft(req) {
            calls.clears.push({...req});
            return clearResult;
        },
        async copyToClipboard(text) {
            calls.clipboard.push(text);
        },
    };
    const controller = new RecoveryCandidates({
        api,
        getFenceState: () => fence(),
        isDirty: () => dirty,
        flushVerified: async () => ({ok: flushOk}),
        restoreIntoEditor: (text) => {
            calls.restores.push(text);
            return true;
        },
        confirmDialog: async () => confirm,
        formatTime: () => "T",
        onStatus: (m) => calls.status.push(m),
        t: (key) => key,
    });
    return {controller, calls, dom, api};
}

const SAMPLE_DRAFTS = [
    {
        key: "editor:src_old1", sessionRef: "s0", generation: 0, revision: 4,
        hash: fnv1a64Hex("临时草稿A"), body: "临时草稿A", updatedAtMs: 1000,
        orphaned: true, schemaVersion: 1,
    },
    {
        key: "sticky:s9", sessionRef: "s1", generation: 1, revision: 2,
        hash: fnv1a64Hex("便签草稿B"), body: "便签草稿B", updatedAtMs: 2000,
        orphaned: false, schemaVersion: 1,
    },
];

test("candidates: open 渲染候选（过滤当前键），Esc 关闭无副作用", async () => {
    const fence = () => ({
        sessionActive: true, sessionRef: "s1", generation: 1, key: "editor:src-current",
        sourceInstanceId: "src-current", sourceRevision: 1, adapterRevision: 0,
        body: "x", bodyHash: fnv1a64Hex("x"),
    });
    const drafts = [...SAMPLE_DRAFTS, {
        key: "editor:src-current", sessionRef: "s1", generation: 1, revision: 9,
        hash: fnv1a64Hex("当前"), body: "当前", updatedAtMs: 3000, schemaVersion: 1,
    }];
    const {controller, calls, dom} = makeCandidatesHarness({drafts, fence});
    await controller.open();
    assert.equal(controller.isOpen, true);
    assert.equal(dom.keydownCount(), 1, "打开列表注册 Esc 监听");
    const overlay = dom.document.body._children.at(-1);
    const card = overlay._children[0];
    const listEl = card._children[1];
    // 当前会话自身的键被过滤
    const rows = listEl._children.filter((c) => c.className === "recovery-item");
    assert.equal(rows.length, 2);
    // orphan 徽标出现在 meta 行
    const firstMeta = rows[0]._children[0];
    assert.ok(firstMeta._children.some((c) => c._textContent === "editor.draft.orphanBadge"));
    // meta 含类型与长度信息（翻译键直出）
    assert.match(firstMeta._textContent, /editor\.draft\.list\.meta/);

    // Esc 关闭：无清理、无剪贴板、无恢复
    dom.dispatchKeydown("Escape");
    assert.equal(controller.isOpen, false);
    assert.equal(dom.keydownCount(), 0, "关闭后监听器移除");
    assert.equal(calls.clears.length, 0);
    assert.equal(calls.clipboard.length, 0);
    assert.equal(calls.restores.length, 0);
});

test("candidates: 恢复——dirty 先验证落盘 + 确认 + 围栏复检；clean 直接恢复", async () => {
    let body = "x";
    let revision = 0;
    const fence = () => ({
        sessionActive: true, sessionRef: "s1", generation: 1, key: "editor:src-cur",
        sourceInstanceId: "cur", sourceRevision: 1, adapterRevision: revision,
        body, bodyHash: fnv1a64Hex(body),
    });
    const dirtyCase = {drafts: SAMPLE_DRAFTS, fence, dirty: true, flushOk: false, confirm: true};
    const blocked = makeCandidatesHarness(dirtyCase);
    await blocked.controller.open();
    const overlay = blocked.dom.document.body._children.at(-1);
    const rows = overlay._children[0]._children[1]._children.filter((c) => c.className === "recovery-item");
    const restoreBtn = rows[0]._children[2]._children[0];
    restoreBtn.dispatchEvent({type: "click"});
    await settle();
    assert.equal(blocked.calls.restores.length, 0, "落盘未确认不得覆盖当前正文");
    assert.ok(blocked.calls.status.includes("editor.draft.flushFailed"));

    // 落盘 OK + 用户确认 → 恢复
    const ok = makeCandidatesHarness({drafts: SAMPLE_DRAFTS, fence, dirty: true, flushOk: true, confirm: true});
    await ok.controller.open();
    const rows2 = ok.dom.document.body._children.at(-1)._children[0]._children[1]._children
        .filter((c) => c.className === "recovery-item");
    rows2[1]._children[2]._children[0].dispatchEvent({type: "click"});
    await settle();
    assert.deepEqual(ok.calls.restores, ["便签草稿B"]);
    assert.equal(ok.controller.isOpen, false, "恢复后关闭列表");
});

test("candidates: 围栏在恢复前变化 → 拒绝恢复", async () => {
    let revision = 0;
    const fence = () => ({
        sessionActive: true, sessionRef: "s1", generation: 1, key: "editor:src-cur",
        sourceInstanceId: "cur", sourceRevision: 1, adapterRevision: revision,
        body: "x", bodyHash: fnv1a64Hex("x"),
    });
    const {controller, calls, dom} = makeCandidatesHarness({drafts: SAMPLE_DRAFTS, fence, dirty: false});
    await controller.open();
    // 打开后用户在别处继续输入（revision 前进）→ 围栏不再匹配
    revision = 7;
    const overlay = dom.document.body._children.at(-1);
    const rows = overlay._children[0]._children[1]._children.filter((c) => c.className === "recovery-item");
    rows[0]._children[2]._children[0].dispatchEvent({type: "click"});
    await settle();
    assert.equal(calls.restores.length, 0, "迟到响应不得覆盖新输入");
    assert.ok(calls.status.includes("editor.draft.staleFence"));
});

test("candidates: 删除——显式确认 + 候选自身完整版本墙", async () => {
    const fence = () => ({
        sessionActive: true, sessionRef: "s1", generation: 1, key: "editor:src-cur",
        sourceInstanceId: "cur", sourceRevision: 1, adapterRevision: 0,
        body: "x", bodyHash: fnv1a64Hex("x"),
    });
    const confirmed = makeCandidatesHarness({drafts: SAMPLE_DRAFTS, fence, confirm: true});
    await confirmed.controller.open();
    const overlay = confirmed.dom.document.body._children.at(-1);
    const rows = overlay._children[0]._children[1]._children.filter((c) => c.className === "recovery-item");
    const deleteBtn = rows[0]._children[2]._children[2];
    deleteBtn.dispatchEvent({type: "click"});
    await settle();
    assert.equal(confirmed.calls.clears.length, 1);
    assert.deepEqual(confirmed.calls.clears[0], {
        key: "editor:src_old1",
        sessionRef: "s0",
        generation: 0,
        expectedRevision: 4,
        expectedHash: fnv1a64Hex("临时草稿A"),
    }, "删除必须携带候选自身的完整版本墙，不串稿");

    // 用户取消确认 → 不删除
    const cancelled = makeCandidatesHarness({drafts: SAMPLE_DRAFTS, fence, confirm: false});
    await cancelled.controller.open();
    const rows2 = cancelled.dom.document.body._children.at(-1)._children[0]._children[1]._children
        .filter((c) => c.className === "recovery-item");
    rows2[0]._children[2]._children[2].dispatchEvent({type: "click"});
    await settle();
    assert.equal(cancelled.calls.clears.length, 0);

    // IPC 正常返回但版本墙不匹配 → 行必须保留，不能假装删除成功。
    const refused = makeCandidatesHarness({drafts: SAMPLE_DRAFTS, fence, confirm: true, clearResult: false});
    await refused.controller.open();
    const refusedRows = refused.dom.document.body._children.at(-1)._children[0]._children[1]._children
        .filter((c) => c.className === "recovery-item");
    refusedRows[0]._children[2]._children[2].dispatchEvent({type: "click"});
    await settle();
    assert.ok(refusedRows[0]._parent, "版本墙拒绝时候选行必须保留");
    assert.ok(refused.calls.status.includes("editor.draft.deleteFailed"));
});

test("candidates: 复制走剪贴板；无会话时恢复禁用", async () => {
    const fence = () => ({sessionActive: false, sessionRef: null, generation: 0, key: null,
        sourceInstanceId: null, sourceRevision: null, adapterRevision: 0, body: "", bodyHash: fnv1a64Hex("")});
    const {controller, calls, dom} = makeCandidatesHarness({drafts: SAMPLE_DRAFTS, fence});
    await controller.open();
    const overlay = dom.document.body._children.at(-1);
    const rows = overlay._children[0]._children[1]._children.filter((c) => c.className === "recovery-item");
    const restoreBtn = rows[0]._children[2]._children[0];
    assert.equal(restoreBtn.disabled, true, "无会话不得恢复到编辑器");
    const copyBtn = rows[0]._children[2]._children[1];
    copyBtn.dispatchEvent({type: "click"});
    await settle();
    assert.deepEqual(calls.clipboard, ["临时草稿A"]);
});

test("candidates: 空列表 / 读取失败不弹层", async () => {
    const fence = () => ({
        sessionActive: true, sessionRef: "s1", generation: 1, key: "k",
        sourceInstanceId: "i", sourceRevision: 1, adapterRevision: 0,
        body: "x", bodyHash: fnv1a64Hex("x"),
    });
    const empty = makeCandidatesHarness({drafts: [], fence});
    await empty.controller.open();
    assert.equal(empty.controller.isOpen, false);
    assert.ok(empty.calls.status.includes("editor.draft.list.empty"));

    const failing = makeCandidatesHarness({drafts: [], fence});
    failing.api.listEditorDrafts = async () => {
        throw new Error("io");
    };
    const controller = failing.controller;
    const calls = failing.calls;
    await controller.open();
    assert.equal(controller.isOpen, false);
    assert.ok(calls.status.includes("editor.draft.list.failed"));
});
