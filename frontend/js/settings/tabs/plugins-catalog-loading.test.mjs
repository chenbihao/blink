import test from 'node:test';
import assert from 'node:assert/strict';
import {TestElement} from './weather-settings-test-helpers.mjs';

globalThis.window = {addEventListener() {}};
const {initPluginsTab} = await import('./plugins.js');

test('late catalog response upgrades dictionary install to automatic download without a config revision change', async () => {
    const container = new TestElement();
    container.contains = () => false;
    const catalogContainer = new TestElement();
    const card = new TestElement();
    const document = new TestElement();
    document.activeElement = null;
    document.getElementById = id => id === 'plugins-container' ? container : catalogContainer;
    document.querySelector = () => card;
    globalThis.document = document;
    let resolveCatalog;
    window.__TAURI__ = {
        core: {invoke: async command => {
            if (command === 'get_plugin_catalog') return new Promise(resolve => { resolveCatalog = resolve; });
            if (command === 'get_plugins') return [{id: 'builtin.dictionary', name: 'Dictionary', revision: 0, version: '0.1.0', settings_schema: []}];
            if (command === 'get_plugin_surface_status') return [{plugin_id: 'builtin.dictionary', datasets: [{id: 'dictionary-basic'}]}];
            throw new Error(`unexpected command: ${command}`);
        }},
        event: {listen: () => Promise.resolve(() => {})},
    };
    initPluginsTab();
    await new Promise(resolve => setImmediate(resolve));
    assert.doesNotMatch(container.innerHTML, /data-catalog="1"/);
    assert.doesNotMatch(container.innerHTML, /plugin-dataset-install-local/);
    resolveCatalog({devMode: false, catalog: {plugins: [], datasets: [{plugin_id: 'builtin.dictionary', dataset_id: 'dictionary-basic', version: '1.0.0'}]}});
    await new Promise(resolve => setImmediate(resolve));
    assert.match(container.innerHTML, /data-catalog="1"/);
    assert.doesNotMatch(container.innerHTML, /plugin-dataset-install-local/);
});
