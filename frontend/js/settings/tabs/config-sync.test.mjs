import {test} from "node:test";
import assert from "node:assert/strict";
import {connectConfigActivity, createConfigRefresher} from "../../shared/config-sync.js";

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
const nodes = new Map();
function element() {
    return {
        checked: false, disabled: false, value: "", textContent: "", hidden: false,
        listeners: new Map(), dataset: {},
        classList: {toggle() {}},
        setAttribute() {}, removeAttribute() {},
        addEventListener(name, listener) {
            const list = this.listeners.get(name) ?? [];
            list.push(listener); this.listeners.set(name, list);
        },
        closest() { return {after(node) { nodes.set(node.id, node); }}; },
    };
}
const checkbox = element();
nodes.set("auto-start", checkbox);
const chordEnabled = element(), chordHint = element();
nodes.set("chord-enabled", chordEnabled);
nodes.set("chord-hint-visible", chordHint);
const doc = new EventTarget();
doc.getElementById = (id) => nodes.get(id) ?? null;
doc.createElement = element;
globalThis.document = doc;
globalThis.window = globalThis;
let persisted = {auto_start: false, auto_start_registration_skipped: false};
let performSave;
let saves = 0;
globalThis.__TAURI__ = {core: {invoke: async (command, args) => {
    if (command === "get_config") return structuredClone(persisted);
    if (command === "set_config") { saves++; return performSave(args); }
    throw new Error(command);
}}};
const {initGeneralTab, applyGeneralConfig} = await import("./general.js");
const {setCurrentConfig, getCurrentConfig} = await import("../shared/state.js");
const refresh = createConfigRefresher({read: async () => structuredClone(persisted), apply: (cfg) => { setCurrentConfig(cfg); applyGeneralConfig(cfg); }});
connectConfigActivity(refresh, doc);
await refresh.refresh();
initGeneralTab(persisted);
const change = () => checkbox.listeners.get("change")[0]({target: checkbox});

test("重复回填更新 checkbox，不重新注册保存监听", async () => {
    persisted.auto_start = true;
    for (let i = 0; i < 10; i++) await refresh.refresh();
    assert.equal(checkbox.checked, true);
    assert.equal(checkbox.listeners.get("change").length, 1);
    assert.equal(saves, 0);
});

test("自启动 pending 禁用；目标值在 await 前捕获，完成后权威回填", async () => {
    persisted.auto_start = false;
    await refresh.refresh();
    let finish;
    performSave = (args) => new Promise((resolve) => { finish = () => { persisted.auto_start = args.value; resolve(); }; assert.equal(args.expected, false); });
    checkbox.checked = true;
    const saving = change();
    assert.equal(checkbox.disabled, true);
    checkbox.checked = false; // 模拟在途 DOM 值变化，不应改变本次目标。
    await refresh.refresh();
    finish(); await saving;
    assert.equal(checkbox.disabled, false);
    assert.equal(getCurrentConfig().auto_start, true);
    await tick(); await tick();
    assert.equal(checkbox.checked, true);
});

test("冲突失败解除等待、显示错误并读取另一窗口已保存的状态", async () => {
    persisted.auto_start = true;
    await refresh.refresh();
    performSave = async () => { throw {code: "config_conflict", message: "配置已变化，请重新读取后重试"}; };
    checkbox.checked = false;
    await change();
    await tick(); await tick();
    assert.equal(checkbox.disabled, false);
    assert.equal(checkbox.checked, true);
    assert.match(nodes.get("auto-start-status").textContent, /配置已变化/);
    assert.equal(getCurrentConfig().auto_start, true);
});

test("连续切换 Chord 两个字段串行使用确认基线，最后意图保留", async () => {
    persisted.chord_enabled = false;
    persisted.chord_hint_visible = true;
    await refresh.refresh();
    const requests = [];
    let finish;
    performSave = async (args) => {
        requests.push(args);
        if (requests.length === 1) await new Promise(resolve => { finish = resolve; });
        assert.deepEqual(args.expected, {chordEnabled: persisted.chord_enabled, chordHintVisible: persisted.chord_hint_visible});
        persisted.chord_enabled = args.value.chordEnabled;
        persisted.chord_hint_visible = args.value.chordHintVisible;
    };
    chordEnabled.checked = true;
    const first = chordEnabled.listeners.get("change")[0]();
    await tick();
    chordHint.checked = false;
    const second = chordHint.listeners.get("change")[0]();
    await tick();
    assert.equal(requests.length, 1);
    await refresh.refresh();
    assert.equal(chordHint.checked, false);
    finish();
    await Promise.all([first, second]);
    await tick(); await tick();
    assert.equal(requests.length, 2);
    assert.equal(persisted.chord_enabled, true);
    assert.equal(persisted.chord_hint_visible, false);
    assert.equal(chordEnabled.checked, true);
    assert.equal(chordHint.checked, false);
});

test("旧 Chord 保存失败不回滚排队的新意图，保存队列可继续", async () => {
    persisted.chord_enabled = false;
    persisted.chord_hint_visible = true;
    await refresh.refresh();
    let failFirst;
    let requests = 0;
    performSave = async (args) => {
        if (++requests === 1) await new Promise((resolve, reject) => { failFirst = () => reject(new Error("temporary failure")); });
        assert.deepEqual(args.expected, {chordEnabled: false, chordHintVisible: true});
        persisted.chord_enabled = args.value.chordEnabled;
        persisted.chord_hint_visible = args.value.chordHintVisible;
    };
    chordEnabled.checked = true;
    const first = chordEnabled.listeners.get("change")[0]();
    await tick();
    chordHint.checked = false;
    const second = chordHint.listeners.get("change")[0]();
    failFirst();
    await Promise.all([first, second]);
    await tick(); await tick();
    assert.equal(persisted.chord_enabled, true);
    assert.equal(persisted.chord_hint_visible, false);
    assert.equal(chordHint.checked, false);
});
