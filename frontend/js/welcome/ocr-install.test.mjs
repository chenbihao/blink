import {test} from "node:test";
import assert from "node:assert/strict";

class Element {
    children = []; handlers = {}; hidden = false; disabled = false;
    style = {}; classList = {toggle() {}, add() {}, remove() {}};
    setAttribute() {} removeAttribute() {}
    append(...nodes) { this.children.push(...nodes); }
    appendChild(node) { this.append(node); return node; }
    prepend(node) { this.children.unshift(node); }
    addEventListener(name, handler) { this.handlers[name] = handler; }
}
const nodes = new Map(["back-btn", "next-btn", "skip-btn", "ocr-status", "ocr-install-btn", "ocr-progress", "ocr-progress-fill", "ocr-progress-text"].map(id => [id, new Element()]));
const doc = new EventTarget(); doc.documentElement = new Element();
doc.getElementById = id => nodes.get(id) ?? null;
doc.querySelector = () => null; doc.querySelectorAll = () => []; doc.createElement = () => new Element();
globalThis.document = doc; globalThis.window = new EventTarget();
const listeners = new Map(), polls = [];
const timeout = globalThis.setTimeout;
globalThis.setTimeout = (callback, delay, ...args) => delay === 750 ? polls.push(callback) : timeout(callback, delay, ...args);
const tick = () => new Promise(resolve => timeout(resolve, 0));
let status = {environment: "missing", operation: {kind: "idle"}};
let finishInstall, installs = 0, joinExisting = false;
window.__TAURI__ = {
    core: {invoke: async cmd => {
        if (cmd === "get_config") return {theme: "dark", language: "zh", hotkey: {display: "Alt+Space"}, chord_bindings: {}};
        if (cmd === "get_global_hotkey_statuses") return [];
        if (cmd === "get_local_engine_status") return [{engine_id: "paddleocr", status: structuredClone(status)}];
        if (cmd === "install_local_engine") {
            installs++;
            if (joinExisting) throw {code: "already_running", message: "已有安装在进行"};
            return new Promise(resolve => { finishInstall = resolve; });
        }
        throw new Error(cmd);
    }},
    event: {listen: async (name, handler) => { listeners.set(name, handler); return () => {}; }},
};
await import("../welcome.js");
await tick(); await tick();
const {EVENTS} = await import("../shared/event-names.js");
const {t} = await import("../i18n/index.js");
const emit = (name, payload) => listeners.get(name)({payload: {engine_id: "paddleocr", ...payload}});
const poll = async () => { assert.ok(polls.length > 0); polls.shift()(); await tick(); };

test("引导安装超过两秒才建立任务仍可绑定；漏掉阶段和终态事件仍收敛", async () => {
    nodes.get("next-btn").handlers.click(); nodes.get("next-btn").handlers.click(); await tick();
    const installing = nodes.get("ocr-install-btn").handlers.click(); await tick();
    await nodes.get("ocr-install-btn").handlers.click();
    assert.equal(installs, 1);
    for (let n = 0; n < 4; n++) await poll(); // 3 秒后才出现 operation_id
    status.operation = {kind: "installing", operation_id: "new-op", stage: "pending"};
    await poll();
    emit(EVENTS.LOCAL_ENGINE_INSTALL_PROGRESS, {operation_id: "old-op", downloaded: 50, total: 100});
    assert.equal(nodes.get("ocr-status").textContent, t("local_engine.operation.stage.pending"));
    emit(EVENTS.LOCAL_ENGINE_INSTALL_PROGRESS, {operation_id: "new-op", downloaded: 50, total: 100});
    assert.equal(nodes.get("ocr-status").textContent, t("local_engine.operation.stage.downloading"));
    assert.equal(nodes.get("ocr-progress").hidden, false);
    emit(EVENTS.LOCAL_ENGINE_INSTALL_STAGE, {operation_id: "new-op", stage: "verifying"});
    await poll();
    assert.equal(nodes.get("ocr-status").textContent, t("local_engine.operation.stage.verifying"), "粗粒度快照不能回退实时阶段");
    status = {environment: "ready", operation: {kind: "idle"}};
    await poll();
    assert.equal(nodes.get("ocr-status").textContent, t("welcome.step3.ocr.ready"));
    finishInstall({operation_id: "new-op"}); await installing;
    assert.equal(polls.length, 0, "终态后停止轮询");
    joinExisting = true;
    status = {environment: "missing", operation: {kind: "installing", operation_id: "external-op", stage: "downloading"}};
    await nodes.get("ocr-install-btn").handlers.click(); await tick();
    assert.equal(nodes.get("ocr-status").textContent, t("local_engine.operation.stage.downloading"), "接管外部安装直接恢复阶段");
    status = {environment: "broken", operation: {kind: "idle"}};
    await poll(); // 旧代际的轮询退出
    if (polls.length) await poll();
    assert.equal(nodes.get("ocr-status").textContent, t("welcome.step3.ocr.failed"));
    assert.equal(nodes.get("ocr-install-btn").disabled, false);
    emit(EVENTS.LOCAL_ENGINE_INSTALL_PROGRESS, {operation_id: "new-op", downloaded: 80, total: 100});
    assert.equal(nodes.get("ocr-status").textContent, t("welcome.step3.ocr.failed"), "失败后迟到进度不能重新激活安装");
    assert.equal(polls.length, 0);
    globalThis.setTimeout = timeout;
});
