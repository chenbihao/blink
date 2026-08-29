/**
 * 清理确认 modal 纯函数测试（0.22.5 H3）。
 *
 * 测试覆盖：
 * 1. aggregateSharedTargets：跨引擎聚合、去重、合并 affected_engine_ids
 * 2. createCleanupModal：open/close/dispose 生命周期
 * 3. createCleanupModal：空 targets / current / blocked / shared 选择逻辑
 * 4. createCleanupModal：confirm 成功关闭、失败保留、重复提交防护
 * 5. createCleanupModal：mode 传递正确
 *
 * 通过 mock window.__TAURI__ + 最小 document mock 后动态导入模块，
 * 不引入 jsdom/bundler。
 */

import assert from "node:assert/strict";

// ── mock 基础设施 ───────────────────────────────────────────────────────────

if (!globalThis.window) {
    globalThis.window = {};
}

globalThis.window.__TAURI__ = {
    core: {invoke: async () => []},
    event: {listen: async () => () => {}},
};

// ── 最小 document mock ────────────────────────────────────────────────────

function matchesSelector(el, sel) {
    if (!el || !el.dataset) return false;
    const attrMatch = sel.match(/\[data-([a-z-]+)="([a-z-]+)"\]/);
    if (attrMatch) return el.dataset[attrMatch[1]] === attrMatch[2];
    if (sel.startsWith(".")) return el.classList?.contains(sel.slice(1));
    if (el.tagName === sel.toUpperCase()) return true;
    if (sel.startsWith("input")) return el.tagName === "INPUT";
    if (sel.startsWith("button")) return el.tagName === "BUTTON";
    return false;
}

function queryInChildren(children, sel, findAll) {
    const results = [];
    for (const child of children || []) {
        if (matchesSelector(child, sel)) {
            if (!findAll) return child;
            results.push(child);
        }
        if (child._children) {
            const sub = queryInChildren(child._children, sel, findAll);
            if (!findAll && sub) return sub;
            if (Array.isArray(sub)) results.push(...sub);
        }
    }
    return findAll ? results : null;
}

function mockElement(tag) {
    const el = {
        tagName: (tag || "div").toUpperCase(),
        hidden: true,
        disabled: false,
        checked: false,
        type: "",
        title: "",
        dataset: {},
        _children: [],
        _textContent: "",
        _listeners: {},
        _attrs: {},
        classList: {
            _set: new Set(),
            add(c) { this._set.add(c); },
            remove(c) { this._set.delete(c); },
            contains(c) { return this._set.has(c); },
        },
        style: {},
        _parent: null,
        scrollTop: 0,
        scrollHeight: 0,
    };
    el.setAttribute = (k, v) => { el._attrs[k] = v; if (k.startsWith("data-")) el.dataset[k.slice(5)] = v; };
    el.getAttribute = (k) => el._attrs[k] ?? null;
    el.hasAttribute = (k) => k in el._attrs;
    el.removeAttribute = (k) => { delete el._attrs[k]; };
    el.appendChild = (child) => { child._parent = el; el._children.push(child); return child; };
    el.removeChild = (child) => { const i = el._children.indexOf(child); if (i >= 0) el._children.splice(i, 1); };
    el.remove = () => { if (el._parent) el._parent.removeChild(el); };
    el.addEventListener = (type, fn) => { (el._listeners[type] ||= []).push(fn); };
    el.removeEventListener = (type, fn) => { const a = el._listeners[type]; if (a) { const i = a.indexOf(fn); if (i >= 0) a.splice(i, 1); } };
    el.focus = () => {};
    el.scrollIntoView = () => {};
    el.click = () => { for (const h of el._listeners["click"] || []) h({preventDefault() {}, stopPropagation() {}}); };
    el.querySelector = (sel) => queryInChildren(el._children, sel, false);
    el.querySelectorAll = (sel) => queryInChildren(el._children, sel, true);
    el.closest = () => null;
    Object.defineProperty(el, "textContent", {
        get: () => el._textContent,
        set: (v) => { el._textContent = String(v); el._children = []; },
    });
    Object.defineProperty(el, "innerHTML", {
        get: () => el._textContent,
        set: (v) => { el._textContent = String(v); el._children = []; },
    });
    return el;
}

globalThis.document = {
    createElement: (tag) => mockElement(tag),
    createTextNode: (text) => ({textContent: String(text)}),
    getElementById: () => null,
    querySelector: () => null,
    querySelectorAll: () => [],
    activeElement: null,
};

// ── 动态导入被测模块（mock 设置后）────────────────────────────────────────

const {createCleanupModal, aggregateSharedTargets} = await import("./local-engine-cleanup-modal.js");

// ── 测试框架 ─────────────────────────────────────────────────────────────

let testCount = 0;
let passCount = 0;

function test(name, fn) {
    testCount++;
    try {
        fn();
        passCount++;
        console.log(`  ✓ ${name}`);
    } catch (e) {
        console.error(`  ✗ ${name}`);
        console.error(`    ${e.message}`);
        throw e;
    }
}

async function asyncTest(name, fn) {
    testCount++;
    try {
        await fn();
        passCount++;
        console.log(`  ✓ ${name}`);
    } catch (e) {
        console.error(`  ✗ ${name}`);
        console.error(`    ${e.message}`);
        throw e;
    }
}

// 便捷工厂
function makeModal(overrides = {}) {
    const modalEl = mockElement("div");
    const bodyEl = mockElement("div");
    const confirmBtn = mockElement("button");
    const opts = {modalEl, bodyEl, confirmBtn, onConfirm: async () => {}, ...overrides};
    const modal = createCleanupModal(opts);
    return {modal, modalEl, bodyEl, confirmBtn};
}

function makeTarget(overrides = {}) {
    return {
        target_id: "gen:old",
        kind: "engine_generation",
        label_fallback: "上一环境",
        size_bytes: 2000 * 1024 * 1024,
        current: false,
        previous: true,
        removable: true,
        shared: false,
        blocked_reason: null,
        ...overrides,
    };
}

// ── 1. aggregateSharedTargets ─────────────────────────────────────────────

test("aggregateSharedTargets：空状态返回空数组", () => {
    assert.equal(aggregateSharedTargets(new Map()).length, 0);
});

test("aggregateSharedTargets：只返回后端标记 shared 的 target", () => {
    const state = new Map([
        ["funasr", {storage: {targets: [
            {target_id: "slot:1", kind: "engine_generation", shared: false, engine_id: "funasr"},
            {target_id: "shared:python_venv:py312", kind: "provider_shared_artifact", shared: true, engine_id: "funasr"},
            {target_id: "cache:1", kind: "engine_generation", shared: false, engine_id: "funasr"},
        ]}}],
    ]);
    const result = aggregateSharedTargets(state);
    assert.equal(result.length, 1);
    assert.equal(result[0].target_id, "shared:python_venv:py312");
});

test("aggregateSharedTargets：target.shared=true 的 generation 也被识别为共享", () => {
    const state = new Map([
        ["funasr", {storage: {targets: [
            {target_id: "slot:1", kind: "engine_generation", shared: false, engine_id: "funasr"},
            {target_id: "shared-asset", kind: "engine_generation", shared: true, engine_id: "funasr"},
        ]}}],
    ]);
    const result = aggregateSharedTargets(state);
    assert.equal(result.length, 1);
    assert.equal(result[0].target_id, "shared-asset");
});

test("aggregateSharedTargets：跨引擎同 target_id 去重并合并", () => {
    const state = new Map([
        ["funasr", {storage: {targets: [
            {target_id: "shared:python_venv:py312", kind: "provider_shared_artifact", shared: true, engine_id: "funasr"},
        ]}}],
        ["paddleocr", {storage: {targets: [
            {target_id: "shared:python_venv:py312", kind: "provider_shared_artifact", shared: true, engine_id: "paddleocr"},
        ]}}],
    ]);
    const result = aggregateSharedTargets(state);
    assert.equal(result.length, 1, "同 target_id 应去重");
    assert.ok(result[0].affected_engine_ids.includes("funasr"));
    assert.ok(result[0].affected_engine_ids.includes("paddleocr"));
});

test("aggregateSharedTargets：不同 target_id 不合并", () => {
    const state = new Map([
        ["funasr", {storage: {targets: [
            {target_id: "shared:python_venv:py312", kind: "provider_shared_artifact", shared: true, engine_id: "funasr"},
            {target_id: "download_cache:python_venv", kind: "provider_download_cache", shared: true, engine_id: "funasr"},
        ]}}],
        ["paddleocr", {storage: {targets: [
            {target_id: "shared:python_venv:py312", kind: "provider_shared_artifact", shared: true, engine_id: "paddleocr"},
        ]}}],
    ]);
    assert.equal(aggregateSharedTargets(state).length, 2);
});

test("aggregateSharedTargets：无 storage 或无 targets 的引擎被跳过", () => {
    const state = new Map([
        ["funasr", {storage: null}],
        ["paddleocr", {storage: {}}],
    ]);
    assert.equal(aggregateSharedTargets(state).length, 0);
});

// ── 2. createCleanupModal：生命周期 ────────────────────────────────────────

test("createCleanupModal：open 使 modal 可见，close 使 modal 隐藏", () => {
    const {modal, modalEl} = makeModal();
    assert.equal(modalEl.hidden, true);

    modal.open({targets: [], mode: "engine"});
    assert.equal(modalEl.hidden, false, "open 后 modal 应可见");
    assert.equal(modalEl.getAttribute("aria-hidden"), "false");
    assert.ok(modal.isOpen());

    modal.close(false);
    assert.equal(modalEl.hidden, true, "close 后 modal 应隐藏");
    assert.ok(!modal.isOpen());
});

test("createCleanupModal：dispose 强制关闭并清理", () => {
    const {modal, modalEl, bodyEl} = makeModal();
    modal.open({targets: [], mode: "engine"});
    assert.equal(modalEl.hidden, false);

    modal.dispose();
    assert.equal(modalEl.hidden, true, "dispose 后 modal 应隐藏");
    assert.equal(bodyEl.textContent, "", "body 应被清空");
});

// ── 3. createCleanupModal：targets 选择逻辑 ─────────────────────────────

test("createCleanupModal：空 targets 列表禁用确认按钮", () => {
    const {modal, confirmBtn} = makeModal();
    modal.open({targets: [], mode: "engine"});
    assert.equal(confirmBtn.disabled, true);
});

test("createCleanupModal：正常 targets 渲染后启用确认按钮", () => {
    const {modal, confirmBtn} = makeModal();
    modal.open({targets: [makeTarget()], mode: "engine"});
    assert.equal(confirmBtn.disabled, false, "有可选 target 时确认按钮应可用");
});

test("createCleanupModal：current generation 不可选 → 确认按钮禁用", () => {
    const {modal, confirmBtn} = makeModal();
    modal.open({
        targets: [makeTarget({current: true, removable: false, blocked_reason: "current_generation"})],
        mode: "engine",
    });
    assert.equal(confirmBtn.disabled, true);
});

test("createCleanupModal：blocked target 不可选 → 确认按钮禁用", () => {
    const {modal, confirmBtn} = makeModal();
    modal.open({
        targets: [makeTarget({blocked_reason: "process_running"})],
        mode: "engine",
    });
    assert.equal(confirmBtn.disabled, true);
});

test("createCleanupModal：shared target 默认不选但有非 shared 时按钮可用", () => {
    const {modal, confirmBtn} = makeModal();
    modal.open({
        targets: [
            makeTarget(),
            makeTarget({target_id: "shared:py", kind: "provider_shared_artifact", shared: true, label_fallback: "共享 Python"}),
        ],
        mode: "engine",
    });
    assert.equal(confirmBtn.disabled, false, "有非 shared 可选 target 时确认按钮应可用");
});

test("createCleanupModal：只有 shared target → 按钮禁用（默认不选 shared）", () => {
    const {modal, confirmBtn} = makeModal();
    modal.open({
        targets: [makeTarget({target_id: "shared:py", kind: "provider_shared_artifact", shared: true})],
        mode: "engine",
    });
    assert.equal(confirmBtn.disabled, true, "只有 shared target 时默认不选 → 按钮应禁用");
});

// ── 4. createCleanupModal：confirm 行为 ──────────────────────────────────
// async test 需要在顶层 await，否则汇总代码会在 async test 完成前执行

await asyncTest("createCleanupModal：confirm 成功后关闭 modal", async () => {
    let called = false;
    const {modal, modalEl, confirmBtn} = makeModal({
        onConfirm: async (ids, mode) => {
            called = true;
            assert.deepEqual(ids, ["gen:old"]);
            assert.equal(mode, "engine");
        },
    });
    modal.open({targets: [makeTarget()], mode: "engine"});
    confirmBtn.click();
    await new Promise((r) => setTimeout(r, 50));
    assert.ok(called, "onConfirm 应被调用");
    assert.equal(modalEl.hidden, true, "confirm 成功后 modal 应关闭");
});

await asyncTest("createCleanupModal：confirm 失败保留 modal 并展示错误", async () => {
    const {modal, modalEl, bodyEl, confirmBtn} = makeModal({
        onConfirm: async () => { throw {message: "清理失败：进程占用", action_hint: "请先停止引擎"}; },
    });
    modal.open({targets: [makeTarget()], mode: "engine"});
    confirmBtn.click();
    await new Promise((r) => setTimeout(r, 50));
    assert.equal(modalEl.hidden, false, "confirm 失败后 modal 应保持打开");
    assert.ok(bodyEl._children.length > 0, "body 应有错误内容");
});

await asyncTest("createCleanupModal：confirm 期间禁用重复提交", async () => {
    let count = 0;
    let resolveFn;
    const {modal, confirmBtn} = makeModal({
        onConfirm: async () => { count++; return new Promise((r) => { resolveFn = r; }); },
    });
    modal.open({targets: [makeTarget()], mode: "engine"});
    confirmBtn.click();
    assert.equal(count, 1, "第一次点击应触发");
    confirmBtn.click();
    assert.equal(count, 1, "提交中第二次点击不应触发");
    resolveFn();
    await new Promise((r) => setTimeout(r, 50));
});

await asyncTest("createCleanupModal：shared 模式传入 mode=shared", async () => {
    let receivedMode = null;
    const {modal, confirmBtn} = makeModal({
        onConfirm: async (_, mode) => { receivedMode = mode; },
    });
    // 用 engine_model_cache kind（非 shared kind）确保默认选中
    modal.open({
        targets: [makeTarget({target_id: "model:old", kind: "engine_model_cache", shared: false})],
        mode: "shared",
    });
    confirmBtn.click();
    await new Promise((r) => setTimeout(r, 50));
    assert.equal(receivedMode, "shared");
});

// ── 汇总 ──────────────────────────────────────────────────────────────────

console.log(`\n${passCount}/${testCount} tests passed`);
if (passCount !== testCount) process.exit(1);
console.log("local-engine-cleanup-modal tests passed");
