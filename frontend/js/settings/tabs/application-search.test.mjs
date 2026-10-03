import {test} from "node:test";
import assert from "node:assert/strict";
const tick = () => new Promise(resolve => setTimeout(resolve, 0));
const flush = async () => { await tick(); await tick(); await tick(); };
function element(tag = "input") {
    return {tag, checked: false, value: "", disabled: false, textContent: "", className: "", children: [], listeners: new Map(),
        classList: {add() {}}, appendChild(child) { this.children.push(child); }, focus() {}, remove() {},
        addEventListener(event, callback) { const list = this.listeners.get(event) || []; list.push(callback); this.listeners.set(event, list); },
    };
}
const ids = ["start-menu-enabled", "start-menu-scan-depth", "start-menu-include-uwp", "start-menu-include-system", "start-menu-discover-settings", "system-entry-status", "system-entry-retry"];
const nodes = new Map(ids.map(id => [id, element()]));
const doc = new EventTarget();
doc.hidden = false; doc.getElementById = id => nodes.get(id); doc.querySelectorAll = () => []; doc.createElement = element;
doc.body = {appendChild(root) { const walk = node => { if (node.tag === "button") setTimeout(() => node.listeners.get("click")[0](), 0); node.children.forEach(walk); }; walk(root); }};
globalThis.document = doc;
globalThis.window = globalThis;
const windowEvents = new Map();
globalThis.addEventListener = (name, callback) => windowEvents.set(name, callback);
globalThis.requestAnimationFrame = callback => setTimeout(callback, 0);
let persisted = {enabled: false, scan_depth: 6, include_uwp: false, include_system_shortcuts: true, discover_system_settings: false};
let save = async args => { Object.assign(persisted, args.value); };
let read = async () => structuredClone(persisted);
let statusRead = async () => ({state: "ready", extra_count: 8});
let notification, statusCalls = 0;
const saves = [];
globalThis.__TAURI__ = {event: {listen: async (_, handler) => { notification = handler; return () => {}; }}, core: {invoke: async (command, args) => {
    if (command === "get_start_menu_config") return read();
    if (command === "set_config") { saves.push(args); return save(args); }
    if (command === "get_system_entry_status") { statusCalls++; return statusRead(); }
    if (command === "refresh_system_entries") return;
    throw new Error(command);
}}};
const {initApplicationSearch} = await import("./application-search.js");
initApplicationSearch(); await flush();
const control = id => nodes.get(id);
const change = id => control(id).listeners.get("change")[0]();

test("总开关关闭时子项禁用并保留偏好，默认不读取发现状态", () => {
    assert.equal(control("start-menu-enabled").checked, false);
    assert.equal(control("start-menu-enabled").disabled, false);
    assert.equal(control("start-menu-include-system").checked, true);
    assert.equal(control("start-menu-include-system").disabled, true);
    assert.equal(control("start-menu-scan-depth").value, 6);
    assert.equal(statusCalls, 0);
});
test("字段 CAS 保存单飞，事件读取延迟到写入完成，重复回填不重复绑定", async () => {
    let finish;
    save = args => new Promise(resolve => { finish = () => { Object.assign(persisted, args.value); resolve(); }; });
    control("start-menu-enabled").checked = true;
    const writing = change("start-menu-enabled");
    assert.equal(control("start-menu-enabled").disabled, true);
    assert.deepEqual(saves.at(-1), {key: "start_menu_config", value: {enabled: true}, expected: {enabled: false}});
    await change("start-menu-enabled"); assert.equal(saves.length, 1);
    notification({payload: {key: "engine:start_menu"}});
    finish(); await writing; await flush();
    assert.equal(control("start-menu-enabled").checked, true);
    assert.equal(control("start-menu-include-uwp").checked, false);
    assert.equal(control("start-menu-include-system").disabled, false);
    for (let i = 0; i < 3; i++) { windowEvents.get("focus")(); await flush(); }
    assert.equal(control("start-menu-enabled").listeners.get("change").length, 1);
});
test("失败回滚后权威补读另一窗口的配置，未知字段不写入", async () => {
    save = async () => { persisted.scan_depth = 7; throw "config_conflict"; };
    control("start-menu-include-system").checked = false;
    await change("start-menu-include-system"); await flush();
    assert.equal(control("start-menu-include-system").checked, true);
    assert.equal(control("start-menu-scan-depth").value, 7);
    assert.deepEqual(saves.at(-1).value, {include_system_shortcuts: false});
});
test("焦点/配置事件补读，关闭后丢弃在途发现状态", async () => {
    let finishStatus;
    statusRead = () => new Promise(resolve => { finishStatus = resolve; });
    persisted.discover_system_settings = true;
    notification({payload: {key: "start_menu_config"}}); await flush();
    assert.equal(control("start-menu-discover-settings").checked, true);
    persisted.enabled = false;
    windowEvents.get("focus")(); await flush();
    finishStatus({state: "ready", extra_count: 999}); await flush();
    assert.equal(control("system-entry-status").textContent, "未启用");
    assert.equal(control("system-entry-retry").disabled, true);
});
test("过期配置读取不能覆盖新的权威响应", async () => {
    let resolveOld;
    read = () => new Promise(resolve => { resolveOld = resolve; });
    windowEvents.get("focus")(); await tick();
    persisted.enabled = true; persisted.include_system_shortcuts = false; persisted.discover_system_settings = false;
    read = async () => structuredClone(persisted);
    notification({payload: {key: "engine:start_menu"}});
    resolveOld({enabled: false, scan_depth: 1, include_uwp: true, include_system_shortcuts: true, discover_system_settings: false});
    await flush();
    assert.equal(control("start-menu-enabled").checked, true);
    assert.equal(control("start-menu-include-system").checked, false);
});
