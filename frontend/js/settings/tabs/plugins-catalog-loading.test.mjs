import test from 'node:test';
import assert from 'node:assert/strict';
import {TestElement} from './weather-settings-test-helpers.mjs';

globalThis.window = {addEventListener() {}};
const {setLang, t} = await import('../../i18n/index.js');
const {initPluginsTab} = await import('./plugins.js');

test('late catalog response upgrades dictionary install to automatic download without a config revision change', async () => {
    const container = new TestElement();
    container.contains = () => false;
    const catalogContainer = new TestElement();
    const card = new TestElement();
    const datasets = new TestElement();
    card.selectors.set("[data-plugin-datasets]", datasets);
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
    assert.match(datasets.innerHTML, /data-catalog="1"/);
    assert.doesNotMatch(container.innerHTML, /data-catalog="1"/, "card and credential controls are preserved");
    assert.doesNotMatch(container.innerHTML, /plugin-dataset-install-local/);
});

test('catalog connection failures show localized guidance without backend diagnostics', async () => {
    const container = new TestElement();
    container.contains = () => false;
    const catalogContainer = new TestElement();
    const document = new TestElement();
    document.activeElement = null;
    document.getElementById = id => id === 'plugins-container' ? container : catalogContainer;
    document.querySelector = () => null;
    globalThis.document = document;
    let failure = {code: 'catalog_unavailable', message: '插件目录读取失败: HTTP 503 https://internal.example/catalog.json'};
    window.__TAURI__ = {
        core: {invoke: async command => {
            if (command === 'get_plugin_catalog') throw failure;
            if (command === 'get_plugins' || command === 'get_plugin_surface_status') return [];
            throw new Error(`unexpected command: ${command}`);
        }},
        event: {listen: () => Promise.resolve(() => {})},
    };
    const originalWarn = console.warn;
    console.warn = () => {};
    try {
        for (const lang of ['zh', 'en']) {
            setLang(lang);
            initPluginsTab();
            await new Promise(resolve => setImmediate(resolve));
            assert.ok(catalogContainer.innerHTML.includes(t('plugin.catalog_unavailable')));
            assert.match(catalogContainer.innerHTML, /plugin-catalog-refresh/);
            assert.doesNotMatch(catalogContainer.innerHTML, /目录读取失败|HTTP 503|internal\.example/);
        }
        failure = {code: 'internal', message: 'internal implementation details'};
        initPluginsTab();
        await new Promise(resolve => setImmediate(resolve));
        assert.ok(catalogContainer.innerHTML.includes(t('plugin.catalog_load_failed')));
        assert.doesNotMatch(catalogContainer.innerHTML, /internal implementation details/);
    } finally {
        console.warn = originalWarn;
        setLang('zh');
    }
});
