import test from "node:test";
import assert from "node:assert/strict";
import {TestElement} from "./weather-settings-test-helpers.mjs";
import {EVENTS} from "../../shared/event-names.js";

globalThis.window = {addEventListener() {}};
globalThis.CSS = {escape: value => value};
const {initPluginsTab} = await import("./plugins.js");

test("automatic catalog source drives install arguments and completion refreshes once", async () => {
    const container = new TestElement();
    const catalog = new TestElement();
    const install = new TestElement();
    install.dataset.pluginId = "builtin.dictionary";
    catalog.selectors.set(".plugin-catalog-install", [install]);
    globalThis.document = {
        addEventListener() {},
        getElementById: id => id === "plugins-container" ? container : catalog,
        querySelector: () => null, querySelectorAll: () => [], activeElement: null,
    };
    const listeners = new Map();
    let resolveCatalog, resolveInstall;
    const calls = [];
    window.__TAURI__ = {
        core: {invoke: async (command, args) => {
            calls.push({command, args});
            if (command === "get_plugins" || command === "get_plugin_surface_status") return [];
            if (command === "get_plugin_catalog") return new Promise(resolve => {resolveCatalog = resolve;});
            if (command === "install_plugin_from_catalog") return new Promise(resolve => {resolveInstall = resolve;});
            throw new Error(command);
        }},
        event: {listen: (name, listener) => {listeners.set(name, listener); return Promise.resolve(() => {});}},
    };
    initPluginsTab();
    assert.match(catalog.innerHTML, /role="status"/);
    assert.match(catalog.innerHTML, /icon-refresh-cw/);
    resolveCatalog({source: "local", devMode: true, localDir: "test-pack", catalog: {plugins: [], datasets: []}});
    await new Promise(resolve => setImmediate(resolve));
    assert.doesNotMatch(catalog.innerHTML, /source-mode|pick-dir/);
    const pending = install.listeners.get("click")[0]();
    assert.deepEqual(calls.find(call => call.command === "install_plugin_from_catalog").args,
        {pluginId: "builtin.dictionary", localDir: "test-pack"});
    listeners.get(EVENTS.PLUGIN_CENTER_PROGRESS)({payload: {id: "builtin.dictionary", phase: "done"}});
    resolveInstall({});
    await pending;
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(calls.filter(call => call.command === "get_plugins").length, 2);
});
