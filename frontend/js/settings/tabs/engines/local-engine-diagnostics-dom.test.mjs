/**
 * 诊断面板孤儿进程渲染测试（最小 DOM shim，沿 local-engine-card-dom.test.mjs 范式）。
 *
 * 回归锚点：renderDiagnosticContent 的 orphan actionable 分支此前引用了
 * 不在作用域的 controller（ReferenceError），导致诊断面板渲染中断——
 * 本测试保证 actionable orphan 时面板正常渲染停止按钮。
 */

import assert from "node:assert/strict";

// ── Tauri mock（须在 import 前设置，tauri.js 需要全局 window）───────────────
globalThis.window = {
    __TAURI__: {
        core: {invoke: async () => ({})},
        event: {listen: async () => () => {}},
    },
};

// ── 最小 DOM shim（覆盖被测模块所需特性）────────────────────────────────────

class ShimElement {
    constructor(tag) {
        this.tagName = tag.toUpperCase();
        this.nodeType = 1;
        this.className = "";
        this.id = "";
        this.hidden = false;
        this._children = [];
        this._parent = null;
        this._text = "";
        this._attrs = {};
        this._listeners = {};
        this.value = "";
        this.disabled = false;
        const self = this;
        this.dataset = new Proxy({}, {
            set(obj, prop, value) {
                obj[prop] = value;
                self._attrs[`data-${String(prop).replace(/[A-Z]/g, (m) => `-${m.toLowerCase()}`)}`] = String(value);
                return true;
            },
            get(obj, prop) {
                return obj[prop];
            },
        });
    }

    appendChild(child) {
        if (child._parent) child._parent.removeChild(child);
        child._parent = this;
        this._children.push(child);
        return child;
    }

    removeChild(child) {
        const idx = this._children.indexOf(child);
        if (idx >= 0) this._children.splice(idx, 1);
        child._parent = null;
        return child;
    }

    remove() {
        if (this._parent) this._parent.removeChild(this);
    }

    get textContent() {
        return this._text + this._children.map((c) => c.textContent).join("");
    }

    set textContent(value) {
        this._children = [];
        this._text = String(value);
    }

    setAttribute(name, value) {
        this._attrs[name] = String(value);
    }

    getAttribute(name) {
        return name in this._attrs ? this._attrs[name] : null;
    }

    addEventListener(type, fn) {
        (this._listeners[type] = this._listeners[type] || []).push(fn);
    }

    _fire(type, event = {}) {
        event.target = event.target || this;
        event.type = type;
        for (const fn of this._listeners[type] || []) fn(event);
    }

    click() {
        this._fire("click");
    }
}

function matchesSelector(el, selector) {
    if (!el || el.nodeType !== 1) return false;
    const s = selector.trim();
    if (s.startsWith(".")) {
        const classes = s.slice(1).split(".");
        const have = String(el.className || "").split(/\s+/).filter(Boolean);
        return classes.every((c) => have.includes(c));
    }
    return el.tagName === s.toUpperCase();
}

function queryAll(root, selector) {
    const out = [];
    const walk = (el) => {
        if (matchesSelector(el, selector)) out.push(el);
        for (const c of el._children) walk(c);
    };
    for (const c of root._children) walk(c);
    return out;
}

ShimElement.prototype.querySelector = function (selector) {
    return queryAll(this, selector)[0] || null;
};

ShimElement.prototype.querySelectorAll = function (selector) {
    return queryAll(this, selector);
};

const documentShim = {
    createElement: (tag) => new ShimElement(tag),
    createElementNS: (_ns, tag) => new ShimElement(tag),
    querySelector: () => null,
    querySelectorAll: () => [],
    body: new ShimElement("body"),
};
globalThis.document = documentShim;

// ── 被测模块 ─────────────────────────────────────────────────────────────────

const {showEngineDiagnostics} = await import("./local-engine-diagnostics.js");

async function flushMicrotasks() {
    for (let i = 0; i < 4; i++) await Promise.resolve();
}

function makeDiag() {
    return {
        engine_id: "funasr",
        environment: "ready",
        process: {state: "running"},
        service: "healthy",
        model: "ready",
        adapter_diagnostics: [],
        recent_logs: [],
        orphan_recovery: {present: true, actionable: true, reason: "lease_stale"},
    };
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

let testCount = 0;
let passCount = 0;

async function test(name, fn) {
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

await test("orphan actionable：渲染停止按钮且不抛 ReferenceError", async () => {
    const entry = {catalog: {engine_id: "funasr"}};
    const controller = {
        getDiagnostics: async () => makeDiag(),
        stopOrphan: async () => ({stopped: true}),
        refreshStatus: async () => {},
    };
    const diagPanel = documentShim.createElement("div");
    diagPanel.hidden = true; // 初始折叠（与卡片装配一致）
    const anchorBtn = documentShim.createElement("button");

    showEngineDiagnostics(entry, controller, undefined, anchorBtn, diagPanel);
    await flushMicrotasks();

    const stopBtn = diagPanel.querySelector(".le-orphan-stop");
    assert.ok(stopBtn, "actionable orphan 应渲染停止按钮");
    assert.ok(diagPanel.textContent.includes("停止孤儿进程"), "停止按钮文案在面板内");
    assert.ok(!diagPanel.querySelector(".le-diagnostic-error"), "不应落入错误分支");
    assert.ok(diagPanel.textContent.includes("引擎部署"), "检查清单正常渲染");
});

await test("orphan 非 actionable：不渲染停止按钮", async () => {
    const entry = {catalog: {engine_id: "funasr"}};
    const diag = makeDiag();
    diag.orphan_recovery = {present: false, actionable: false, reason: "no_lease"};
    const controller = {getDiagnostics: async () => diag};
    const diagPanel = documentShim.createElement("div");
    diagPanel.hidden = true;
    const anchorBtn = documentShim.createElement("button");

    showEngineDiagnostics(entry, controller, undefined, anchorBtn, diagPanel);
    await flushMicrotasks();

    assert.equal(diagPanel.querySelector(".le-orphan-stop"), null, "非 actionable 不渲染停止按钮");
});

console.log(`\n${passCount}/${testCount} passed`);
if (passCount !== testCount) process.exit(1);
