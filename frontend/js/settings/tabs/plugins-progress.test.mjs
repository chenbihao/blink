import test from "node:test";
import assert from "node:assert/strict";
import {TestElement} from "./weather-settings-test-helpers.mjs";
import {EVENTS} from "../../shared/event-names.js";

globalThis.window = {addEventListener() {}};
const {initPluginsTab} = await import("./plugins.js");

test("dictionary progress refreshes only its sections and preserves credential drafts", async () => {
    const container = new TestElement();
    const catalog = new TestElement();
    const dictionary = new TestElement();
    const weather = new TestElement();
    const datasets = new TestElement();
    const secret = new TestElement();
    secret.value = "unsaved draft";
    weather.selectors.set('.plugin-field[data-key="api_key"]', secret);
    dictionary.selectors.set("[data-plugin-datasets]", datasets);
    const listeners = new Map();
    const calls = [];
    let phase = "building";
    const surface = () => ({plugin_id: "builtin.dictionary", datasets: [{id: "dictionary-basic"}],
        bootstrap: {supported: true, ops: [{dataset_id: "dictionary-basic", phase: {kind: "plugin_stage", stage: phase}}]}});
    globalThis.document = {
        addEventListener() {},
        getElementById: id => id === "plugins-container" ? container : catalog,
        querySelector: selector => selector.includes('data-plugin-id="builtin.dictionary"') ? dictionary : weather,
        querySelectorAll: () => [], activeElement: secret,
    };
    container.contains = () => false;
    window.__TAURI__ = {
        core: {invoke: async (command, args) => {
            calls.push({command, args});
            if (command === "get_plugins") return [
                {id: "builtin.dictionary", revision: 0, settings_schema: []},
                {id: "builtin.weather", revision: 0, settings_schema: [{key: "api_key", type: "string", secret: true}]},
            ];
            if (command === "get_plugin_catalog") return {catalog: {plugins: [], datasets: []}};
            if (command === "get_plugin_secret_hint") return "***hint";
            if (command === "get_plugin_surface_status") return [surface()];
            throw new Error(command);
        }},
        event: {listen: (name, listener) => {listeners.set(name, listener); return Promise.resolve(() => {});}},
    };
    initPluginsTab();
    await new Promise(resolve => setImmediate(resolve));
    const originalCard = container.innerHTML;
    const reads = calls.length;
    phase = "verifying";
    listeners.get(EVENTS.PLUGIN_BOOTSTRAP_PROGRESS)({payload: {plugin_id: "builtin.dictionary", phase: {kind: "plugin_stage"}}});
    await new Promise(resolve => setTimeout(resolve, 300));
    assert.deepEqual(calls.slice(reads), [{command: "get_plugin_surface_status", args: {pluginId: "builtin.dictionary"}}]);
    assert.equal(container.innerHTML, originalCard);
    assert.equal(secret.value, "unsaved draft");
    assert.match(datasets.innerHTML, /校验/);
});
