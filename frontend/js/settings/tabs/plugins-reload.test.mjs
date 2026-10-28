import assert from "node:assert/strict";
import {test} from "node:test";
import {TestElement} from "./weather-settings-test-helpers.mjs";

globalThis.window = {addEventListener() {}};
const {initPluginsTab} = await import("./plugins.js");

test("deferred cross-window plugin config reload resumes after focus leaves the card", {timeout: 2000}, async () => {
    const container = new TestElement();
    const card = new TestElement();
    const input = {};
    const doc = new TestElement();
    doc.activeElement = null;
    doc.getElementById = id => id === "plugins-container" ? container : null;
    doc.querySelector = () => card;
    container.contains = element => element === input;
    globalThis.document = doc;
    let changed;
    let revision = 1;
    let reads = 0;
    window.__TAURI__ = {
        core: {invoke: async command => {
            assert.equal(command, "get_plugins");
            reads++;
            return [{id: "test.sync", name: "Sync", revision, version: `0.${revision}.0`, settings_schema: []}];
        }},
        event: {listen: (_event, listener) => { changed = listener; return Promise.resolve(() => {}); }},
    };
    initPluginsTab();
    await new Promise(resolve => setImmediate(resolve));
    assert.match(container.innerHTML, /v0\.1\.0/);

    doc.activeElement = input;
    revision = 2;
    changed({payload: {key: "plugin:test.sync"}});
    await new Promise(resolve => setImmediate(resolve));
    assert.match(container.innerHTML, /v0\.1\.0/, "focused controls keep their current contents");
    const before = reads;
    await container.fire("focusout");
    await new Promise(resolve => setTimeout(resolve, 10));
    assert.match(container.innerHTML, /v0\.1\.0/, "moving focus within the card must not replace it");
    const afterInternalMove = reads;
    // Native focusout fires before document.activeElement moves to the new target.
    const leaving = container.fire("focusout");
    doc.activeElement = null;
    await leaving;
    await new Promise(resolve => setTimeout(resolve, 10));
    await new Promise(resolve => setImmediate(resolve));
    assert.ok(afterInternalMove >= before);
    assert.equal(reads, afterInternalMove + 1);
    assert.match(container.innerHTML, /v0\.2\.0/);
});
