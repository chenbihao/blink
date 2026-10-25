import assert from "node:assert/strict";
import {test} from "node:test";

// ── DOM 桩：捕获 document 级 keydown 监听 + 最小元素结构 ──────────────────
const keydownListeners = [];
class FakeElement {
    constructor(tag = "div") {
        this.tagName = tag;
        this.hidden = false;
        this.children = [];
        this.classList = {
            _set: new Set(),
            add(...cs) { for (const c of cs) this._set.add(c); },
            remove(c) { this._set.delete(c); },
            contains(c) { return this._set.has(c); },
        };
        this.dataset = {};
        this.style = {};
    }
    querySelectorAll(selector) {
        // 支持 ".flyout-nav-item" 简单类选择器
        const cls = selector.replace(/^\./, "");
        const found = [];
        const walk = (el) => {
            for (const child of el.children) {
                if (child.classList.contains(cls)) found.push(child);
                walk(child);
            }
        };
        walk(this);
        return found;
    }
    scrollIntoView() {}
}
globalThis.document = {
    addEventListener(type, listener) {
        if (type === "keydown") keydownListeners.push(listener);
    },
    getElementById() { return null; },
};
globalThis.window = {__TAURI__: {core: {invoke: () => Promise.resolve(true)}}};

const {registerFlyoutKeynav, clearFlyoutKeyActive, handleFlyoutKeynav, wasConsumedByKeynav} =
    await import("./flyout-keynav.js");

function makePanel(items) {
    const panel = new FakeElement("div");
    panel.children = items;
    return panel;
}

function keyEvent(key, {isComposing = false, target = null} = {}) {
    return {
        key,
        isComposing,
        shiftKey: false,
        target,
        preventDefault() { this.prevented = true; },
        stopPropagation() { this.stopped = true; },
        stopImmediatePropagation() { this.immediateStopped = true; },
    };
}

/** 可编辑目标桩（textarea / 面板内嵌 input）——closest 命中可编辑选择器。 */
function editableTarget() {
    return {closest: (sel) => (sel.includes("textarea") ? {} : null)};
}

test("arrow keys move highlight, Enter adopts, Escape closes", () => {
    const a = new FakeElement(); a.dataset.effort = ""; a.classList.add("flyout-nav-item");
    const b = new FakeElement(); b.dataset.effort = "high"; b.classList.add("flyout-nav-item");
    const panel = makePanel([a, b]);
    const adopted = [];
    let closed = 0;
    registerFlyoutKeynav("t1", {
        container: () => panel,
        isActive: () => !panel.hidden,
        adopt: (el) => adopted.push(el),
        close: () => { closed++; },
    });

    const down = keyEvent("ArrowDown");
    keydownListeners.at(-1)(down);
    assert.ok(down.prevented, "ArrowDown 应 preventDefault");
    assert.ok(a.classList.contains("flyout-key-active"), "首项高亮");

    const down2 = keyEvent("ArrowDown");
    keydownListeners.at(-1)(down2);
    assert.ok(b.classList.contains("flyout-key-active"), "第二项高亮");
    assert.ok(!a.classList.contains("flyout-key-active"), "前一项取消高亮");

    // 环回：再按下回到第一项
    keydownListeners.at(-1)(keyEvent("ArrowDown"));
    assert.ok(a.classList.contains("flyout-key-active"), "末项环回首项");

    const enter = keyEvent("Enter");
    keydownListeners.at(-1)(enter);
    assert.equal(adopted.length, 1, "Enter 采纳当前高亮项");
    assert.equal(adopted[0], a);

    const esc = keyEvent("Escape");
    keydownListeners.at(-1)(esc);
    assert.equal(closed, 1, "Esc 关闭面板");
    panel.hidden = true; // 真实环境由 openExclusive 互斥；测试手动收起
});

test("IME composition events are never intercepted", () => {
    const a = new FakeElement(); a.dataset.effort = ""; a.classList.add("flyout-nav-item");
    const panel = makePanel([a]);
    const adopted = [];
    let closed = 0;
    registerFlyoutKeynav("t2", {
        container: () => panel,
        isActive: () => !panel.hidden,
        adopt: (el) => adopted.push(el),
        close: () => { closed++; },
    });
    for (const key of ["Enter", "ArrowDown", "Escape"]) {
        const e = keyEvent(key, {isComposing: true});
        keydownListeners.at(-1)(e);
        assert.ok(!e.prevented, `IME 组合中 ${key} 不应被拦截`);
    }
    assert.equal(adopted.length, 0);
    assert.equal(closed, 0);
    panel.hidden = true;
});

test("inactive panel does not consume keys; no active highlight Enter closes", () => {
    const a = new FakeElement(); a.dataset.effort = ""; a.classList.add("flyout-nav-item");
    const panel = makePanel([a]);
    let closed = 0;
    registerFlyoutKeynav("t3", {
        container: () => panel,
        isActive: () => !panel.hidden,
        adopt: () => {},
        close: () => { closed++; },
    });
    panel.hidden = true;
    const e = keyEvent("ArrowDown");
    keydownListeners.at(-1)(e);
    assert.ok(!e.prevented, "面板关闭时不应消费按键");

    panel.hidden = false;
    clearFlyoutKeyActive(panel);
    const enter = keyEvent("Enter");
    keydownListeners.at(-1)(enter);
    assert.equal(closed, 1, "无高亮时 Enter 关闭面板（不误采纳）");
    panel.hidden = true;
});

test("disabled items are skipped during navigation", () => {
    const a = new FakeElement(); a.dataset.effort = ""; a.classList.add("flyout-nav-item");
    const disabled = new FakeElement(); disabled.dataset.effort = "x";
    disabled.classList.add("flyout-nav-item", "flyout-nav-item-disabled");
    const c = new FakeElement(); c.dataset.effort = "high"; c.classList.add("flyout-nav-item");
    const panel = makePanel([a, disabled, c]);
    const adopted = [];
    registerFlyoutKeynav("t4", {
        container: () => panel,
        isActive: () => !panel.hidden,
        adopt: (el) => adopted.push(el),
        close: () => {},
    });
    keydownListeners.at(-1)(keyEvent("ArrowDown"));
    assert.ok(a.classList.contains("flyout-key-active"), "从首项开始");
    keydownListeners.at(-1)(keyEvent("ArrowDown"));
    assert.ok(
        c.classList.contains("flyout-key-active"),
        "禁用项被跳过，直接到第三项"
    );
    panel.hidden = true;
});

// ── 0.27.15 review 修正：可编辑目标仲裁 + 消费标记 ─────────────────────────

test("editable target: non-Escape keys pass through at document level, Escape still closes", () => {
    const a = new FakeElement(); a.dataset.effort = ""; a.classList.add("flyout-nav-item");
    const panel = makePanel([a]);
    const adopted = [];
    let closed = 0;
    registerFlyoutKeynav("t5", {
        container: () => panel,
        isActive: () => !panel.hidden,
        adopt: (el) => adopted.push(el),
        close: () => { closed++; },
    });
    // composer textarea / 面板内嵌 input：↑/↓/Enter 原生放行（不劫持光标/输入）
    for (const key of ["ArrowUp", "ArrowDown", "Enter", "Tab"]) {
        const e = keyEvent(key, {target: editableTarget()});
        keydownListeners.at(-1)(e);
        assert.ok(!e.prevented, `可编辑目标的 ${key} 不应在 document 层被拦截`);
    }
    assert.equal(adopted.length, 0);
    assert.equal(closed, 0);
    // Esc 仍关面板（即使焦点在输入框内）
    const esc = keyEvent("Escape", {target: editableTarget()});
    keydownListeners.at(-1)(esc);
    assert.equal(closed, 1, "可编辑目标的 Esc 仍关面板");
    panel.hidden = true;
});

test("consumed keys are marked for same-node arbitration (wasConsumedByKeynav)", () => {
    const a = new FakeElement(); a.dataset.effort = ""; a.classList.add("flyout-nav-item");
    const panel = makePanel([a]);
    registerFlyoutKeynav("t6", {
        container: () => panel,
        isActive: () => !panel.hidden,
        adopt: () => {},
        close: () => {},
    });
    const esc = keyEvent("Escape");
    keydownListeners.at(-1)(esc);
    assert.ok(wasConsumedByKeynav(esc), "被消费的 Esc 应可被 handleEsc 仲裁识别");
    assert.ok(esc.immediateStopped, "消费按键应 stopImmediatePropagation");
    const other = keyEvent("Escape");
    assert.ok(!wasConsumedByKeynav(other), "未消费的事件不误报");
    panel.hidden = true;
});

test("handleFlyoutKeynav routes at target phase regardless of editable target", () => {
    const a = new FakeElement(); a.dataset.effort = "high"; a.classList.add("flyout-nav-item");
    const panel = makePanel([a]);
    const adopted = [];
    registerFlyoutKeynav("t7", {
        container: () => panel,
        isActive: () => !panel.hidden,
        adopt: (el) => adopted.push(el),
        close: () => {},
    });
    // composer 在 target 阶段调用（焦点在 textarea）：面板打开时 Enter 采纳而非发送
    assert.equal(handleFlyoutKeynav(keyEvent("ArrowDown", {target: editableTarget()})), true);
    const enter = keyEvent("Enter", {target: editableTarget()});
    assert.equal(handleFlyoutKeynav(enter), true, "面板打开时 Enter 应被面板消费");
    assert.equal(adopted.length, 1);
    // 面板关闭时不拦截——composer 正常 Enter 发送
    panel.hidden = true;
    const enter2 = keyEvent("Enter", {target: editableTarget()});
    assert.equal(handleFlyoutKeynav(enter2), false, "面板关闭时 Enter 归还 composer");
});
