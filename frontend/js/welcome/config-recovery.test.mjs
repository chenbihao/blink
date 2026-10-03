import {test} from "node:test";
import assert from "node:assert/strict";

class Element {
    children = [];
    listeners = new Map();
    hidden = false;
    disabled = false;
    classList = {toggle() {}, add() {}, remove() {}};
    setAttribute() {}
    removeAttribute() {}
    append(...children) { this.children.push(...children); }
    appendChild(child) { this.append(child); return child; }
    prepend(child) { this.children.unshift(child); }
    addEventListener(name, handler) {
        const handlers = this.listeners.get(name) ?? [];
        handlers.push(handler); this.listeners.set(name, handlers);
    }
}
const nodes = new Map(["back-btn", "next-btn", "skip-btn"].map(id => [id, new Element()]));
const body = new Element();
const doc = new EventTarget();
doc.documentElement = new Element();
doc.getElementById = id => nodes.get(id) ?? null;
doc.querySelector = selector => selector === ".welcome-body" ? body : null;
doc.querySelectorAll = () => [];
doc.createElement = () => new Element();
globalThis.document = doc;
globalThis.window = new EventTarget();
let fail = true, closed = 0, reads = 0;
const listeners = new Map();
window.__TAURI__ = {
    core: {invoke: async command => {
        if (command === "get_config") {
            reads++;
            if (fail) throw new Error("损坏的 app.chord");
            return {theme: "dark", language: "zh", hotkey: {display: "Alt+Space"}, chord_bindings: {}};
        }
        if (command === "complete_onboarding") throw new Error("配置损坏，无法保存引导状态");
        if (command === "get_global_hotkey_statuses") return [];
        throw new Error(command);
    }},
    event: {listen: async (name, handler) => {
        const handlers = listeners.get(name) ?? [];
        handlers.push(handler); listeners.set(name, handlers);
        return () => {};
    }},
    window: {getCurrentWindow: () => ({close() { closed++; }})},
};

await import("../welcome.js");
const tick = () => new Promise(resolve => setTimeout(resolve, 0));
await tick(); await tick();

test("配置首读失败仍可导航和退出；重试初始化不重复监听", async () => {
    const {EVENTS} = await import("../shared/event-names.js");
    const notice = body.children[0];
    assert.equal(notice.hidden, false);
    assert.match(notice.children[0].textContent, /损坏的 app.chord/);
    assert.equal(nodes.get("next-btn").listeners.get("click").length, 1);
    nodes.get("next-btn").listeners.get("click")[0]();
    assert.equal(nodes.get("back-btn").disabled, false);
    await nodes.get("skip-btn").listeners.get("click")[0]();
    assert.equal(closed, 1, "持久化失败也必须允许退出");

    fail = false;
    const retryButton = notice.children[1];
    const retry = retryButton.listeners.get("click")[0]();
    assert.equal(retryButton.disabled, true);
    await retryButton.listeners.get("click")[0]();
    await retry;
    assert.equal(reads, 2, "重复点击重试不能启动第二轮初始化");
    assert.equal(notice.hidden, true);
    assert.equal(retryButton.disabled, false);
    assert.equal(listeners.get(EVENTS.CONFIG_CHANGED).length, 1);
    assert.equal(listeners.get(EVENTS.GLOBAL_HOTKEY_STATUS).length, 1);
    assert.equal(nodes.get("skip-btn").listeners.get("click").length, 1);
});
