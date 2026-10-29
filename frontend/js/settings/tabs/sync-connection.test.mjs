import {test} from "node:test";
import assert from "node:assert/strict";

const tick = () => new Promise(resolve => setTimeout(resolve, 0));
const element = (dataset = {}) => ({
    dataset, value: "", disabled: false, readOnly: false, textContent: "", handlers: {},
    classList: {add() {}, remove() {}, toggle() {}},
    addEventListener(name, handler) { this.handlers[name] = handler; },
});
let fields, buttons, statuses;
const decode = value => value.replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&amp;/g, "&");
const container = {
    set innerHTML(html) {
        fields = new Map([...html.matchAll(/<input[^>]+data-key="([^"]+)"[^>]*>/g)].map(([tag, key]) => {
            const input = element({key});
            input.value = decode(tag.match(/value="([^"]*)"/)?.[1] || "");
            return [key, input];
        }));
        buttons = new Map([...html.matchAll(/<button[^>]+data-action="([^"]+)"[^>]*>/g)].map(([tag, action]) => {
            const button = element({action}); button.disabled = tag.includes("disabled"); return [action, button];
        }));
        statuses = new Map(["sync-conn-status", "sync-run-status"].map(id => [id, element()]));
    },
    querySelector(selector) {
        return selector.startsWith("#") ? statuses.get(selector.slice(1)) : fields.get(selector.match(/data-key="([^"]+)"/)?.[1]);
    },
    querySelectorAll(selector) { return [...(selector === ".plugin-field" ? fields : buttons).values()]; },
};
globalThis.window = globalThis;
globalThis.document = {getElementById: id => id === "sync-container" ? container : null, querySelectorAll: () => []};
let saved = {url: "", username: "", has_password: false};
const calls = [];
let saveError = false, testError = false, finishTest;
window.__TAURI__ = {core: {invoke: async (cmd, args) => {
    calls.push({cmd, args});
    if (cmd === "get_sync_settings") return {...saved};
    if (cmd === "save_sync_settings") {
        if (saveError) throw new Error("保存失败");
        saved = {url: args.url, username: args.username, has_password: Boolean(args.password) || saved.has_password};
        return {...saved};
    }
    if (cmd === "test_sync_connection") {
        if (testError) throw new Error("连接失败");
        return new Promise(resolve => { finishTest = () => resolve({status: "empty_server"}); });
    }
    if (cmd === "run_sync") throw new Error("同步失败");
    throw new Error(cmd);
}}};
const {initSyncTab} = await import("./sync.js");
const {setLang} = await import("../../i18n/index.js");
initSyncTab(); await tick();
const fill = (key, value) => { const input = fields.get(key); input.value = value; input.handlers.input(); };
const click = action => buttons.get(action).handlers.click();

test("保存并测试使用当前表单；失败保留草稿，重复点击不会重入", async () => {
    fill("sync_url", "https://example.test/dav"); fill("sync_username", "user"); fill("sync_password", " secret ");
    saveError = true;
    await click("test-conn");
    assert.equal(calls.filter(c => c.cmd === "test_sync_connection").length, 0);
    assert.equal(fields.get("sync_password").value, " secret ");
    assert.match(statuses.get("sync-conn-status").textContent, /保存失败/);
    saveError = false; testError = true;
    await click("test-conn");
    assert.deepEqual(calls.slice(-2).map(c => c.cmd), ["save_sync_settings", "test_sync_connection"]);
    assert.equal(fields.get("sync_password").value, " secret ");
    assert.match(statuses.get("sync-conn-status").textContent, /连接失败/);
    setLang("en");
    assert.equal(fields.get("sync_password").value, " secret ", "重绘保留密码草稿");
    assert.equal(fields.get("sync_url").value, "https://example.test/dav");
    testError = false;
    const testing = click("test-conn"); await tick();
    const count = calls.length;
    await click("test-conn");
    assert.equal(calls.length, count);
    assert.equal(fields.get("sync_url").readOnly, true);
    finishTest(); await testing;
    assert.equal(fields.get("sync_password").value, "", "成功后只显示已保存密码占位");
    assert.equal(fields.get("sync_url").readOnly, false);
    assert.equal(buttons.get("test-conn").disabled, false);
    fill("sync_url", "https://changed.test/dav"); fill("sync_password", "draft");
    await click("run-sync");
    assert.equal(fields.get("sync_url").value, "https://changed.test/dav", "同步失败重绘也保留草稿");
    assert.equal(fields.get("sync_password").value, "draft");
});
