/**
 * chord.js — 全局快捷键保存/回滚/竞态防护 + 8 秒消息生命周期 + 键位展示测试。
 *
 * 测试覆盖：
 * 1. saveGlobalBinding：revision token 防竞态（旧请求被忽略）
 * 2. saveGlobalBinding：保存失败回滚 UI（toggle/radio/combo/配置区）
 * 3. saveGlobalBinding：保存成功更新 confirmedGlobalBindings
 * 4. showGlobalStatusMessage：8 秒 timer 生命周期
 * 5. showGlobalStatusMessage：新消息替换旧消息取消旧 timer
 * 6. 键位展示：normalizeCombo + renderComboHTML 不再使用手拼 formatCombo
 */

import {test, describe, before, after, afterEach} from "node:test";
import assert from "node:assert";
import fs from "node:fs";

// ── DOM 环境 mock ─────────────────────────────────────────────────────────────

globalThis.window = globalThis;
globalThis.requestAnimationFrame = (cb) => setTimeout(cb, 0);

// 保存原始 clearTimeout 防止递归
const origClearTimeout = globalThis.clearTimeout.bind(globalThis);

// 简易 DOM mock
function makeElement(tag) {
    const el = {
        tagName: tag.toUpperCase(),
        className: "",
        _children: [],
        _listeners: {},
        _attributes: {},
        _textContent: "",
        _innerHTML: "",
        _style: {},
        dataset: {},
        classList: {
            _set: new Set(),
            add(...c) { c.forEach((x) => this._set.add(x)); },
            remove(...c) { c.forEach((x) => this._set.delete(x)); },
            toggle(c, force) {
                if (force === true || (force === undefined && !this._set.has(c))) this._set.add(c);
                else this._set.delete(c);
            },
            contains(c) { return this._set.has(c); },
        },
        setAttribute(k, v) { this._attributes[k] = v; },
        getAttribute(k) { return this._attributes[k] ?? null; },
        removeAttribute(k) { delete this._attributes[k]; },
        hasAttribute(k) { return k in this._attributes; },
        appendChild(child) {
            this._children.push(child); child._parent = this;
            // 同步更新 _innerHTML 以便 outerHTML 能反映子元素
            const childHtml = child.outerHTML || child.textContent || "";
            this._innerHTML += childHtml;
            return child;
        },
        removeChild(child) {
            const i = this._children.indexOf(child);
            if (i >= 0) { this._children.splice(i, 1); child._parent = null; }
            return child;
        },
        remove() {
            if (this._parent) {
                this._parent.removeChild(this);
            }
        },
        querySelector(sel) {
            return this._children[0] || null;
        },
        querySelectorAll() { return []; },
        addEventListener(type, fn) {
            if (!this._listeners[type]) this._listeners[type] = [];
            this._listeners[type].push(fn);
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
            if (arr) arr.forEach((fn) => fn(ev));
            return true;
        },
        focus() {},
        get textContent() { return this._textContent; },
        set textContent(v) { this._textContent = String(v); this._innerHTML = String(v); },
        get innerHTML() { return this._innerHTML; },
        set innerHTML(v) { this._innerHTML = String(v); },
        get outerHTML() {
            const tag = this.tagName.toLowerCase();
            const cls = this.className ? ` class="${this.className}"` : "";
            return `<${tag}${cls}>${this._innerHTML}</${tag}>`;
        },
    };
    Object.defineProperty(el, "checked", {
        get() { return this._checked ?? false; },
        set(v) { this._checked = v; },
        configurable: true,
    });
    return el;
}

// Mock document
const chordContainer = makeElement("div");
chordContainer.id = "chord-actions-container";

globalThis.document = {
    createElement: makeElement,
    createTextNode: (text) => ({textContent: String(text), _parent: null}),
    createDocumentFragment: () => makeElement("fragment"),
    body: makeElement("body"),
    getElementById(id) {
        if (id === "chord-actions-container") return chordContainer;
        return null;
    },
    querySelector(sel) {
        if (sel && sel.startsWith(".chord-global-status")) return null;
        return null;
    },
    querySelectorAll() { return []; },
    addEventListener() {},
    removeEventListener() {},
};

// Mock CSS.escape
globalThis.CSS = {
    escape: (s) => String(s).replace(/[^a-zA-Z0-9_-]/g, "\\$&"),
};

// Mock navigator for kbd.js (navigator is read-only, use defineProperty)
Object.defineProperty(globalThis, "navigator", {
    value: {platform: "Win32", userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"},
    writable: true,
    configurable: true,
});

// Mock invoke — 用于 chord.js 的 saveGlobalBinding
let mockConfig = {};
let mockSaveConfigShouldFail = false;

// set_config 需要写回 mockConfig，以模拟后端持久化（串行化测试需要）
function setMockConfigValue(key, value) {
    // 模拟后端按 key 路由到对应分片
    if (key === "chord_bindings") {
        mockConfig.chord_bindings = value;
    } else if (key === "clipboard_config") {
        mockConfig.clipboard = value;
    } else if (key === "screenshot_config") {
        // screenshot_config 走 screenshot:config 分片，不影响 mockConfig
    } else if (key === "disabled_chord_actions") {
        mockConfig.disabled_chord_actions = value;
    } else {
        mockConfig[key] = value;
    }
}

globalThis.window.__TAURI__ = {
    core: {
        invoke(cmd, args) {
            if (cmd === "get_config") {
                return Promise.resolve(JSON.parse(JSON.stringify(mockConfig)));
            }
            if (cmd === "get_config_section") {
                return Promise.resolve({});
            }
            if (cmd === "list_all_chord_actions") {
                return Promise.resolve([]);
            }
            if (cmd === "get_global_hotkey_statuses") {
                return Promise.resolve([]);
            }
            // set_config — chord.js 调用 saveConfig 最终走 invoke("set_config", ...)
            if (cmd === "set_config") {
                if (mockSaveConfigShouldFail) {
                    return Promise.reject({code: "conflict", message: "快捷键被占用", retryable: false});
                }
                setMockConfigValue(args?.key, args?.value);
                return Promise.resolve(null);
            }
            return Promise.resolve(null);
        },
    },
    event: {
        listen() { return Promise.resolve(() => {}); },
    },
};

// ── 导入被测模块 ─────────────────────────────────────────────────────────────

const chordModule = await import("./chord.js");
const __test__ = chordModule.__test__;

// ── 测试 ─────────────────────────────────────────────────────────────────────

describe("saveGlobalBinding — revision token 竞态防护", () => {
    before(() => {
        // 每组测试前重置串行化链
        __test__.resetChordBindingsWriteChain();
    });

    test("快速连续操作：旧请求的迟到响应被忽略", async () => {
        __test__.resetChordBindingsWriteChain();
        mockConfig = {
            chord_bindings: {
                chat: {key: "a", modifiers: ["alt"]},
            },
        };
        mockSaveConfigShouldFail = false;

        // 模拟第一次调用（会自增 revision 到 1）
        const p1 = __test__.saveGlobalBinding("chat", {mode: "follow_chord"});
        // 在 p1 的串行化任务执行前发起第二次（revision 自增到 2）
        // 串行化链保证 p1 先执行：p1 检测到 rev(1) !== current(2) → 跳过
        const p2 = __test__.saveGlobalBinding("chat", {mode: "custom", modifiers: ["ctrl", "alt"], key: "b"});

        const [r1, r2] = await Promise.all([p1, p2]);
        // 第一次请求的 revision (1) !== 当前 revision (2)，所以返回 false
        assert.equal(r1, false, "旧请求（revision 1）应被忽略");
        // 第二次请求的 revision (2) === 当前 revision (2)，所以返回 true
        assert.equal(r2, true, "新请求（revision 2）应成功");
    });

    test("保存成功后 confirmedGlobalBindings 更新", async () => {
        __test__.resetChordBindingsWriteChain();
        mockConfig = {
            chord_bindings: {
                screenshot: {key: "s", modifiers: ["alt"]},
            },
        };
        mockSaveConfigShouldFail = false;

        const result = await __test__.saveGlobalBinding("screenshot", {mode: "follow_chord"});
        assert.equal(result, true);
        const confirmed = __test__.confirmedGlobalBindings.get("screenshot");
        assert.deepEqual(confirmed, {mode: "follow_chord"});
    });

    test("保存失败后 confirmedGlobalBindings 不更新", async () => {
        __test__.resetChordBindingsWriteChain();
        mockConfig = {
            chord_bindings: {
                clipboard_history: {key: "c", modifiers: ["alt"]},
            },
        };

        // 先保存一次成功（建立 confirmed state）
        mockSaveConfigShouldFail = false;
        await __test__.saveGlobalBinding("clipboard_history", {mode: "follow_chord"});
        const confirmedBefore = __test__.confirmedGlobalBindings.get("clipboard_history");

        // 再保存失败
        mockSaveConfigShouldFail = true;
        const result = await __test__.saveGlobalBinding("clipboard_history", {mode: "custom", modifiers: ["ctrl"], key: "x"});
        assert.equal(result, false, "保存失败返回 false");

        const confirmedAfter = __test__.confirmedGlobalBindings.get("clipboard_history");
        assert.deepEqual(confirmedAfter, confirmedBefore, "confirmedGlobalBindings 不应改变");
    });

    test("串行化：并发写入不会 last-writer-wins 覆盖", async () => {
        __test__.resetChordBindingsWriteChain();
        // 模拟后端 config：两个 chord 动作的 global 字段
        mockConfig = {
            chord_bindings: {
                chat: {key: "a", modifiers: ["alt"], global: {mode: "follow_chord"}},
                screenshot: {key: "s", modifiers: ["alt"]},
            },
        };
        mockSaveConfigShouldFail = false;

        // 同一事件循环中并发提交两个不同 action；per-action revision 不得让
        // 后发的 screenshot 请求取消先发的 chat 请求。
        const p1 = __test__.saveGlobalBinding("chat", {
            mode: "custom",
            modifiers: ["ctrl", "alt"],
            key: "b",
        });
        const p2 = __test__.saveGlobalBinding("screenshot", {mode: "follow_chord"});
        const [r1, r2] = await Promise.all([p1, p2]);

        assert.equal(r1, true, "第一个保存应成功");
        assert.equal(r2, true, "第二个保存应成功");

        // 验证 mockConfig 中的两个 action 的 global 都被正确设置
        // （串行化保证第二次写入基于第一次写入后的状态）
        assert.deepEqual(
            mockConfig.chord_bindings.chat.global,
            {mode: "custom", modifiers: ["ctrl", "alt"], key: "b"},
            "chat global 保留第一个写入",
        );
        assert.deepEqual(
            mockConfig.chord_bindings.screenshot.global,
            {mode: "follow_chord"},
            "screenshot global 保留第二个写入",
        );
    });

    test("初始后端配置会建立 confirmed 回滚快照", () => {
        __test__.replaceConfirmedGlobalBindings({
            chat: {global: {mode: "follow_chord"}},
            screenshot: {key: "s", modifiers: ["alt"]},
        });
        assert.deepEqual(__test__.confirmedGlobalBindings.get("chat"), {
            mode: "follow_chord",
        });
        assert.equal(__test__.confirmedGlobalBindings.get("screenshot"), null);
    });
});

describe("showGlobalStatusMessage — 8 秒 timer 生命周期", () => {
    test("显示消息后启动 8 秒 timer", () => {
        // 清理 timers
        __test__.statusMessageTimers.clear();

        // 创建 mock 状态元素
        const statusEl = makeElement("div");
        statusEl.className = "chord-global-status";
        statusEl.dataset.id = "test-action";
        // 覆盖 document.querySelector 返回此元素
        const origQS = globalThis.document.querySelector;
        globalThis.document.querySelector = (sel) => {
            if (sel && sel.includes("chord-global-status")) return statusEl;
            return origQS.call(globalThis.document, sel);
        };

        __test__.showGlobalStatusMessage("test-action", "快捷键被占用");

        assert.ok(statusEl.innerHTML.includes("is-warn"), "应显示 is-warn 消息");
        assert.ok(statusEl.innerHTML.includes("快捷键被占用"), "消息内容正确");
        assert.ok(__test__.statusMessageTimers.has("test-action"), "timer 已注册");

        // 清理
        const timer = __test__.statusMessageTimers.get("test-action");
        if (timer) origClearTimeout(timer);
        __test__.statusMessageTimers.clear();
        globalThis.document.querySelector = origQS;
    });

    test("新消息替换旧消息时取消旧 timer", () => {
        __test__.statusMessageTimers.clear();

        const statusEl = makeElement("div");
        statusEl.dataset.id = "test-action2";
        const origQS = globalThis.document.querySelector;
        globalThis.document.querySelector = (sel) => {
            if (sel && sel.includes("chord-global-status")) return statusEl;
            return origQS.call(globalThis.document, sel);
        };

        __test__.showGlobalStatusMessage("test-action2", "第一条消息");
        const timer1 = __test__.statusMessageTimers.get("test-action2");
        assert.ok(timer1, "第一条消息的 timer 已注册");

        __test__.showGlobalStatusMessage("test-action2", "第二条消息");
        const timer2 = __test__.statusMessageTimers.get("test-action2");
        assert.ok(timer2, "第二条消息的 timer 已注册");
        assert.notEqual(timer1, timer2, "timer 已更新为新 timer");

        // 清理
        origClearTimeout(timer2);
        __test__.statusMessageTimers.clear();
        globalThis.document.querySelector = origQS;
    });

    test("空消息不显示", () => {
        __test__.statusMessageTimers.clear();

        const statusEl = makeElement("div");
        const origQS = globalThis.document.querySelector;
        globalThis.document.querySelector = (sel) => {
            if (sel && sel.includes("chord-global-status")) return statusEl;
            return origQS.call(globalThis.document, sel);
        };

        __test__.showGlobalStatusMessage("test-action3", "");
        assert.equal(statusEl.innerHTML, "", "空消息不修改 DOM");
        assert.ok(!__test__.statusMessageTimers.has("test-action3"), "空消息不注册 timer");

        __test__.showGlobalStatusMessage("test-action3", null);
        assert.ok(!__test__.statusMessageTimers.has("test-action3"), "null 消息不注册 timer");

        globalThis.document.querySelector = origQS;
    });
});

// 在顶层导入 kbd.js 以便测试使用
const {renderComboHTML, normalizeCombo} = await import("../../shared/kbd.js");

describe("键位展示 — formatCombo 已移除，使用 normalizeCombo + renderComboHTML", () => {
    test("chord.js 不再定义 formatCombo", () => {
        const moduleText = fs.readFileSync(new URL("./chord.js", import.meta.url), "utf8");
        assert.ok(!moduleText.includes("function formatCombo"), "formatCombo 应已删除");
        assert.ok(moduleText.includes("renderComboHTML"), "应使用 renderComboHTML");
        assert.ok(moduleText.includes("normalizeCombo"), "应使用 normalizeCombo");
    });

    test("renderComboHTML 产出 <kbd> 元素结构", () => {
        const html = renderComboHTML(normalizeCombo(["ctrl", "alt"], "a"));
        assert.ok(html.includes("<kbd"), "应包含 <kbd> 元素");
        assert.ok(html.includes("Ctrl"), "应包含 Ctrl");
        assert.ok(html.includes("Alt"), "应包含 Alt");
        assert.ok(html.includes("A"), "应包含 A");
    });

    test("Space 键正确显示", () => {
        const html = renderComboHTML(normalizeCombo(["alt"], " "));
        assert.ok(html.includes("Space"), "空格应显示为 Space");
    });

    test("Win/Meta 键正确显示", () => {
        const html = renderComboHTML(normalizeCombo(["meta"], "x"));
        assert.ok(html.includes("Win"), "meta 应显示为 Win（Windows 平台）");
    });

    test("左右修饰键别名归一化", () => {
        assert.equal(normalizeCombo(["lctrl", "lalt"], "k"), "Ctrl+Alt+K");
        assert.equal(normalizeCombo(["rctrl", "ralt"], "k"), "Ctrl+Alt+K");
        assert.equal(normalizeCombo(["control"], "x"), "Ctrl+X");
        assert.equal(normalizeCombo(["win"], "x"), "Win+X");
    });
});
