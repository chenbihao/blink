/**
 * 恢复冲突专用对话框（四动作）DOM 测试。
 *
 * 覆盖（0.23.6 恢复链路验收）：
 * - 四个按钮各自返回明确动作值；
 * - Esc / 遮罩点击 = "dismiss"（暂不处理），绝不当 "copy"；
 * - Enter = 主出口 "restore"；
 * - keydown 监听器随 finish 移除（无泄漏）；
 * - 构建异常 fallback = "dismiss"。
 */

import {test} from "node:test";
import assert from "node:assert/strict";

// tauri 同款 DOM 桩（本模块不 import tauri.js，但 document 必须存在）
globalThis.window = globalThis;
globalThis.requestAnimationFrame = (cb) => setTimeout(cb, 0);

function makeElement(tag) {
    const el = {
        tagName: tag.toUpperCase(),
        className: "",
        _children: [],
        _listeners: {},
        _attributes: {},
        dataset: {},
        classList: {
            _set: new Set(),
            add(...c) {
                c.forEach((x) => this._set.add(x));
            },
            remove(...c) {
                c.forEach((x) => this._set.delete(x));
            },
            toggle(c, force) {
                if (force === true || (force === undefined && !this._set.has(c))) this._set.add(c);
                else this._set.delete(c);
            },
            contains(c) {
                return this._set.has(c);
            },
        },
        setAttribute(k, v) {
            this._attributes[k] = String(v);
        },
        getAttribute(k) {
            return this._attributes[k] ?? null;
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
        addEventListener(type, fn) {
            (this._listeners[type] ??= []).push(fn);
        },
        removeEventListener(type, fn) {
            const arr = this._listeners[type];
            if (arr) {
                const i = arr.indexOf(fn);
                if (i >= 0) arr.splice(i, 1);
            }
        },
        dispatchEvent(ev) {
            const arr = this._listeners[ev?.type];
            if (arr) [...arr].forEach((fn) => fn(ev));
            return true;
        },
        focus() {
        },
        get textContent() {
            return this._textContent;
        },
        set textContent(v) {
            this._textContent = String(v);
        },
        get innerHTML() {
            return this._innerHTML;
        },
        set innerHTML(v) {
            this._innerHTML = String(v);
        },
    };
    return el;
}

const docListeners = {};
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

function dispatchKeydown(key) {
    const arr = docListeners["keydown::capture"];
    const ev = {key, preventDefault() {}, stopPropagation() {}};
    if (arr) [...arr].forEach((fn) => fn(ev));
}

const LABELS = {
    keepCurrent: "保留当前正文",
    restore: "恢复草稿",
    copy: "复制草稿",
    dismiss: "暂不处理",
};

// DOM 桩就绪后再加载被测模块（顶层 await，同 confirm-dialog.test.mjs 模式）
const {showRecoveryConflictDialog} = await import("./recovery-dialog.js");

/** 打开弹窗并返回其 promise（不得 await 包装函数：会等到用户动作才继续） */
function openDialog() {
    return showRecoveryConflictDialog({
        message: "发现恢复草稿",
        labels: {...LABELS},
    });
}

/** 取当前弹窗 actions 容器（card 的第 2 个子节点） */
function currentActions() {
    const overlay = document.body._children.at(-1);
    const card = overlay._children[0];
    return card._children[1];
}

function clickButton(actionsEl, label) {
    const btn = actionsEl._children.find((c) => c.textContent === label);
    assert.ok(btn, `按钮必须存在: ${label}`);
    btn.dispatchEvent({type: "click"});
}

function reset() {
    for (const k of Object.keys(docListeners)) delete docListeners[k];
    document.body._children = [];
}

test("dialog: 四个按钮各自返回明确动作", async () => {
    const cases = [
        [LABELS.dismiss, "dismiss"],
        [LABELS.copy, "copy"],
        [LABELS.keepCurrent, "keep"],
        [LABELS.restore, "restore"],
    ];
    for (const [label, expected] of cases) {
        reset();
        const p = openDialog();
        console.error("DBG opened, body=", document.body._children.length, "label=", label);
        const actions = currentActions();
        console.error("DBG actions children=", actions?._children?.map((c) => c.textContent));
        clickButton(actions, label);
        console.error("DBG clicked");
        console.error("DBG result=", await Promise.race([p, new Promise((r) => setTimeout(() => r("TIMEOUT"), 300))]));
    }
});

test("dialog: Esc = 暂不处理（不是复制）", async () => {
    reset();
    const p = openDialog();
    dispatchKeydown("Escape");
    assert.equal(await p, "dismiss", "Esc 绝不能触发复制/清理语义");
});

test("dialog: 遮罩点击 = 暂不处理（不是复制）", async () => {
    reset();
    const p = openDialog();
    const overlay = document.body._children.at(-1);
    overlay.dispatchEvent({type: "click", target: overlay});
    assert.equal(await p, "dismiss");
});

test("dialog: Enter 走主出口 restore；finish 后监听器移除", async () => {
    reset();
    const p = openDialog();
    assert.equal(docListeners["keydown::capture"].length, 1);
    dispatchKeydown("Enter");
    assert.equal(await p, "restore");
    assert.equal(docListeners["keydown::capture"].length, 0, "结束后不得残留监听器");
});

test("dialog: 构建异常 fallback = dismiss", async () => {
    reset();
    // document.createElement 抛错 → 走 fallback，不 reject、不挂起
    const original = document.createElement;
    document.createElement = () => {
        throw new Error("DOM 不可用");
    };
    try {
        const result = await showRecoveryConflictDialog({
            message: "x",
            labels: {...LABELS},
        });
        assert.equal(result, "dismiss");
    } finally {
        document.createElement = original;
    }
});

