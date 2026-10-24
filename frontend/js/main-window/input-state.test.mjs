import assert from "node:assert/strict";
import {readFileSync} from "node:fs";
import {test} from "node:test";
import vm from "node:vm";
import {createInputStateCore} from "./input-state-core.js";

const source = readFileSync(new URL("./input-state.js", import.meta.url), "utf8")
    .replace(/^import .*;\r?$/gm, "")
    .replace(/^export /gm, "");

function setup() {
    const classes = new Set();
    const page = {focused: true, visibilityState: "visible"};
    let focusCalls = 0;
    let revision = 0;
    const queryEl = {readOnly: false, focus: () => focusCalls++};
    const context = vm.createContext({
        createInputStateCore, queryEl,
        chord: {isEnabled: () => true, notifyVisibilityChange() {}},
        aiMode: {isActive: () => false},
        clipboardMode: {isActive: () => false},
        cmdMode: {isActive: () => false},
        document: {
            hasFocus: () => page.focused,
            get visibilityState() { return page.visibilityState; },
            body: {classList: {
                contains: (key) => classes.has(key),
                toggle: (key, enabled) => enabled ? classes.add(key) : classes.delete(key),
            }},
        },
    });
    vm.runInContext(source + `
        globalThis.apply = (state) => {
            if (core.applyState(state)) projectUi();
        };
    `, context);
    return {
        page, queryEl,
        get focusCalls() { return focusCalls; },
        apply(altDown, windowVisible = true) {
            context.apply({revision: ++revision, altDown, windowVisible, exclusiveChordActive: false});
        },
    };
}

test("主窗隐藏后迟到的 Alt 松开只恢复可编辑，不向后台搜索框设焦", () => {
    const h = setup();
    h.apply(true);
    assert.equal(h.queryEl.readOnly, true);
    h.page.focused = false;
    // 实机：native 已隐藏，WebView document.visibilityState 仍是 visible。
    h.apply(false, false);
    assert.equal(h.queryEl.readOnly, false);
    assert.equal(h.focusCalls, 0);
});

test("主窗仍可见但用户已切到编辑器时，不恢复搜索框焦点", () => {
    const h = setup();
    h.apply(true);
    h.page.focused = false;
    h.apply(false);
    assert.equal(h.queryEl.readOnly, false);
    assert.equal(h.focusCalls, 0);
});

test("后端已隐藏主窗时，即使 DOM 焦点也未及时更新仍不设焦", () => {
    const h = setup();
    h.apply(true);
    h.apply(false, false);
    assert.equal(h.focusCalls, 0);
});

test("前台可见主窗退出待命态仍恢复输入焦点，重复状态不反复设焦", () => {
    const h = setup();
    h.apply(true);
    assert.equal(h.queryEl.readOnly, true);
    h.apply(false);
    assert.equal(h.queryEl.readOnly, false);
    assert.equal(h.focusCalls, 1);
    h.apply(false);
    assert.equal(h.focusCalls, 1);
});
