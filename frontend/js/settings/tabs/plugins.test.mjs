import assert from "node:assert/strict";
import {test} from "node:test";
import {TestElement, weatherCardFixture} from "./weather-settings-test-helpers.mjs";

globalThis.window ??= {};
const {bindPluginCardEvents} = await import("./plugins.js");

function cardFixture() {
    const {card, root} = weatherCardFixture();
    const enabled = new TestElement("plugin-enabled");
    enabled.checked = true;
    const save = new TestElement("plugin-save");
    const unit = new TestElement("plugin-field");
    unit.value = "fahrenheit";
    unit.dataset.key = "temperature_unit";
    const secret = new TestElement("plugin-field plugin-secret-field");
    secret.value = "fixture-unsaved-key";
    secret.dataset.key = "caiyun_app_secret";
    const toggle = new TestElement("trigger-tag-btn");
    toggle.dataset.keyword = "天气";
    const toggleTag = new TestElement("trigger-tag");
    toggle.closest = () => toggleTag;
    const remove = new TestElement("trigger-tag-btn-delete");
    remove.dataset.keyword = "forecast";
    const removeTag = new TestElement("trigger-tag");
    remove.closest = () => removeTag;
    const add = new TestElement("trigger-add-inline-input");
    const row = new TestElement("plugin-triggers-row");
    const message = new TestElement("plugin-save-msg");
    for (const [selector, element] of [
        [".plugin-enabled", enabled], [".weather-save, .plugin-save", [save]],
        ['.plugin-field[data-key="temperature_unit"]', unit],
        ['.plugin-field[data-key="caiyun_app_secret"]', secret],
        ['.plugin-field.plugin-secret-field[data-key="caiyun_app_secret"]', secret],
        [".plugin-field", [unit, secret]], [".plugin-field.plugin-secret-field", [secret]],
        [".trigger-tag-btn:not(.trigger-tag-btn-delete)", [toggle]],
        [".trigger-tag-btn-delete", [remove]], [".trigger-add-inline-input", add],
        [".plugin-triggers-row", row], [".plugin-save-msg", message],
    ]) card.selectors.set(selector, element);
    const plugin = {id: "builtin.weather", revision: 4, enabled: true, settings_schema: [
        {key: "temperature_unit", type: "enum"},
        {key: "caiyun_app_secret", type: "string", secret: true},
    ], settings: {temperature_unit: "celsius", caiyun_app_secret: "", default_location: {
        name: "Guangzhou", latitude: 23.13, longitude: 113.26, coordinate_system: "wgs84", source: "qweather",
    }}};
    const document = {
        querySelector: () => card, getElementById: () => null,
        createElement: () => {
            const tag = new TestElement("trigger-tag");
            tag.selectors.set(".trigger-tag-btn-delete", new TestElement("trigger-tag-btn-delete"));
            return tag;
        },
    };
    return {card, root, enabled, save, unit, secret, toggle, toggleTag, remove, removeTag, add, row, message, plugin, document};
}

for (const action of ["toggle", "delete", "add"]) {
    test(`${action} trigger advances the save revision and preserves unsaved weather fields`, {timeout: 2000}, async (t) => {
        const fixture = cardFixture();
        const oldDocument = globalThis.document;
        const oldTauri = window.__TAURI__;
        globalThis.document = fixture.document;
        let revision = 4;
        const writes = [];
        window.__TAURI__ = {core: {invoke: async (command, args) => {
            if (command === "get_plugin_secret_hint") return null;
            if (command === "get_plugins") return [{...fixture.plugin, revision,
                settings: {...writes.at(-1).settings, caiyun_app_secret: ""}}];
            if (command === "set_config") {
                assert.equal(args.value.expectedRevision, revision);
                writes.push(structuredClone(args.value));
                revision++;
                return;
            }
            assert.ok(["toggle_default_trigger", "delete_custom_trigger", "add_custom_trigger"].includes(command));
            assert.equal(args.expectedRevision, revision, "trigger uses its displayed config snapshot");
            assert.equal(args.pluginId, fixture.plugin.id);
            return ++revision;
        }}};
        t.after(() => {
            clearTimeout(fixture.message._t);
            globalThis.document = oldDocument;
            window.__TAURI__ = oldTauri;
        });
        bindPluginCardEvents(fixture.plugin);
        await fixture.root.fire("click", {target: fixture.root.querySelector(".weather-edit-toggle")});
        const region = fixture.root.querySelector(".weather-region");
        region.value = "mainland";
        await fixture.root.fire("change", {target: region});
        if (action === "toggle") {
            await fixture.toggle.fire("click");
            assert.ok(fixture.toggleTag.classList.contains("trigger-tag-disabled"));
        } else if (action === "delete") {
            await fixture.remove.fire("click");
            assert.equal(fixture.removeTag.isConnected, false);
        } else {
            fixture.add.value = "forecast2";
            await fixture.add.fire("keydown", {key: "Enter"});
            assert.equal(fixture.row.children.length, 1);
            const added = fixture.row.children[0];
            await added.querySelector(".trigger-tag-btn-delete").fire("click");
            assert.equal(added.isConnected, false, "newly added tags use the same revision path");
        }
        assert.equal(fixture.plugin.revision, revision);
        assert.equal(fixture.secret.value, "fixture-unsaved-key");
        assert.equal(fixture.unit.value, "fahrenheit");
        assert.equal(writes.length, 0, "trigger mutation does not save settings drafts");
        await fixture.save.fire("click");
        assert.equal(writes.length, 1);
        assert.equal(writes[0].settings.temperature_unit, undefined, "location save does not submit unrelated drafts");
        assert.equal(writes[0].settings.caiyun_app_secret, undefined);
        assert.equal(fixture.secret.value, "fixture-unsaved-key");
        assert.equal(writes[0].settings.default_location.in_mainland_china, true);
        assert.equal(writes[0].settings.default_location.source, "qweather");
    });
}

test("a settings save cannot race an in-flight trigger mutation", {timeout: 2000}, async (t) => {
    const fixture = cardFixture();
    const oldDocument = globalThis.document;
    const oldTauri = window.__TAURI__;
    globalThis.document = fixture.document;
    let complete;
    let settingsWrites = 0;
    window.__TAURI__ = {core: {invoke: async (command) => {
        if (command === "get_plugin_secret_hint") return null;
        if (command === "toggle_default_trigger") return new Promise((resolve) => { complete = resolve; });
        if (command === "set_config") { settingsWrites++; return; }
        else throw new Error(`Unexpected IPC ${command}`);
    }}};
    t.after(() => {
        complete?.(5);
        clearTimeout(fixture.message._t);
        globalThis.document = oldDocument;
        window.__TAURI__ = oldTauri;
    });
    bindPluginCardEvents(fixture.plugin);
    const pending = fixture.toggle.fire("click");
    const queuedSave = fixture.save.fire("click");
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(settingsWrites, 0);
    assert.equal(fixture.secret.value, "fixture-unsaved-key");
    complete(5);
    await pending;
    await queuedSave;
    assert.equal(settingsWrites, 1);
    assert.equal(fixture.plugin.revision, 6);
});

test("rapid field changes are queued with fresh revisions without saving the location draft", {timeout: 2000}, async (t) => {
    const fixture = cardFixture();
    const previousDocument = globalThis.document;
    const previousTauri = window.__TAURI__;
    globalThis.document = fixture.document;
    const writes = [];
    let release;
    window.__TAURI__ = {core: {invoke: async (command, args) => {
        if (command === "get_plugin_secret_hint") return null;
        assert.equal(command, "set_config");
        writes.push(structuredClone(args.value));
        if (writes.length === 1) await new Promise(resolve => { release = resolve; });
    }}};
    t.after(() => {
        clearTimeout(fixture.message._t);
        globalThis.document = previousDocument;
        window.__TAURI__ = previousTauri;
    });
    bindPluginCardEvents(fixture.plugin);
    await fixture.root.fire("click", {target: fixture.root.querySelector(".weather-edit-toggle")});
    const region = fixture.root.querySelector(".weather-region");
    region.value = "mainland";
    await fixture.root.fire("change", {target: region});
    const first = fixture.unit.fire("change");
    fixture.unit.value = "celsius";
    const second = fixture.unit.fire("change");
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(writes.length, 1);
    release();
    await Promise.all([first, second]);
    assert.deepEqual(writes.map(write => write.expectedRevision), [4, 5]);
    assert.deepEqual(writes.map(write => write.settings), [{temperature_unit:"fahrenheit"}, {temperature_unit:"celsius"}]);
    assert.equal(fixture.root.querySelector("[data-weather-editor]")["aria-hidden"], "false");
    assert.equal(fixture.secret.value, "fixture-unsaved-key");
});

test("a revision conflict keeps drafts and stops queued writes until explicit reload", {timeout:2000}, async (t) => {
    const fixture = cardFixture();
    const previousDocument = globalThis.document;
    const previousTauri = window.__TAURI__;
    globalThis.document = fixture.document;
    t.mock.method(console,"error",() => {});
    let writes = 0;
    window.__TAURI__ = {core:{invoke:async command => {
        if (command === "get_plugin_secret_hint") return null;
        assert.equal(command,"set_config");
        writes++;
        throw {code:"config_conflict",message:"config_conflict"};
    }}};
    t.after(() => {
        clearTimeout(fixture.message._t);
        globalThis.document = previousDocument;
        window.__TAURI__ = previousTauri;
    });
    bindPluginCardEvents(fixture.plugin);
    const first = fixture.unit.fire("change");
    fixture.unit.value = "celsius";
    const second = fixture.unit.fire("change");
    await Promise.all([first,second]);
    assert.equal(writes,1);
    assert.equal(fixture.plugin.revision,4);
    assert.equal(fixture.unit.value,"celsius");
    assert.equal(fixture.secret.value,"fixture-unsaved-key");
    assert.equal(fixture.card.dataset.configConflict,"true");
    assert.equal(fixture.message.children[0].className,"btn-small plugin-reload");
});

// ── 0.28.4：数据包（词库）区渲染 ────────────────────────────────────────────

const {renderDatasetsSection} = await import("./plugins.js");

test("renderDatasetsSection renders install states and actions", () => {
    const plugin = {id: "builtin.dictionary"};

    // 无 surface / 无 datasets 声明 → 不渲染
    assert.equal(renderDatasetsSection(plugin, undefined), "");
    assert.equal(renderDatasetsSection(plugin, {datasets: []}), "");

    const base = {
        datasets: [
            {id: "dictionary-basic", optional: false, min_version: "1.0.0"},
            {id: "dictionary-etymology", optional: true, min_version: null},
        ],
    };

    // 未安装：必备 → “未安装”；可选 → “未安装（可选）” + 可选标记；只有安装按钮
    let html = renderDatasetsSection(plugin, {...base, readiness: {ready: false, blockers: [
        {kind: "missing_dataset", field: "dictionary-basic", hint: "缺少数据包"},
    ]}});
    assert.match(html, /plugin-dataset-row/);
    assert.match(html, /data-dataset="dictionary-basic"/);
    assert.match(html, /plugin-dataset-status">未安装</);          // 必备包“未安装”
    assert.match(html, /未安装（可选）/);                            // 可选包
    assert.match(html, /plugin-dataset-optional/);
    assert.equal((html.match(/plugin-dataset-install/g) || []).length, 2);
    assert.doesNotMatch(html, /plugin-dataset-uninstall/); // 未装无卸载按钮

    // 已安装：版本徽标 + 更新/卸载按钮
    html = renderDatasetsSection(plugin, {...base, readiness: {ready: true, blockers: []},
        datasets: [
            {...base.datasets[0], installed: {version: "1.2.0", state: "active"}},
            {...base.datasets[1], installed: null},
        ]});
    assert.match(html, /plugin-dataset-ready">v1\.2\.0</);
    assert.match(html, /更新…/);
    assert.match(html, /plugin-dataset-uninstall/);

    // 损坏/过低：readiness blocker 命中该数据包 → 修复徽标
    html = renderDatasetsSection(plugin, {...base, readiness: {ready: false, blockers: [
        {kind: "dataset_corrupt", field: "dictionary-basic", hint: "损坏"},
    ]}, datasets: [
        {...base.datasets[0], installed: {version: "1.0.0", state: "active"}},
    ]});
    assert.match(html, /已损坏，重装修复/);
    html = renderDatasetsSection(plugin, {...base, readiness: {ready: false, blockers: [
        {kind: "dataset_outdated", field: "dictionary-basic", hint: "过低"},
    ]}, datasets: [
        {...base.datasets[0], installed: {version: "0.9.0", state: "active"}},
    ]});
    assert.match(html, /版本过低，请更新/);

    // 待清理状态：无卸载按钮（防重复操作）
    html = renderDatasetsSection(plugin, {...base, readiness: {ready: false, blockers: []},
        datasets: [
            {...base.datasets[0], installed: {version: "1.0.0", state: "pending_remove"}},
        ]});
    assert.match(html, /待清理（重启后完成）/);
    assert.doesNotMatch(html, /plugin-dataset-uninstall/);
});

// ── 0.28.5：官方插件中心（目录区渲染 + 版本比较 + 目录模式数据包） ──────────

const {renderCatalogSection, compareVersions} = await import("./plugins.js");

test("compareVersions follows dotted numeric semantics", () => {
    assert.equal(compareVersions("0.28.1", "0.28.2") < 0, true);
    assert.equal(compareVersions("0.28", "0.28.0"), 0);
    assert.equal(compareVersions("1.0.0", "0.99.99") > 0, true);
    assert.equal(compareVersions(undefined, "0.1") < 0, true);
});

test("renderCatalogSection covers loading/error/catalog states", () => {
    // 加载中 / 错误态
    assert.match(renderCatalogSection(null, new Map()), /plugin-catalog-loading/);
    assert.match(renderCatalogSection({error: "目录不可达"}, new Map()), /plugin-catalog-error/);
    assert.match(renderCatalogSection({error: "目录不可达"}, new Map()), /plugin-catalog-refresh/);

    const catalog = {plugins: [
        {plugin_id: "builtin.dictionary", version: "0.2.0", asset: {url: "https://x/y.zip", sha256: "a".repeat(64), bytes: 5 * 1024 * 1024}},
        {plugin_id: "builtin.translate", version: "0.4.0", asset: {url: "https://x/t.zip", sha256: "b".repeat(64), bytes: 300 * 1024}},
    ], datasets: []};
    const surfaces = new Map([
        // 已通过目录安装 0.2.0 → 已最新
        ["builtin.dictionary", {install: {version: "0.2.0"}, version: "0.1.0"}],
        // 只有随包 0.3.0 → 目录 0.4.0 可更新
        ["builtin.translate", {version: "0.3.0"}],
        // 不在 surface（未安装插件）→ 安装按钮
    ]);
    const html = renderCatalogSection({catalog, stale: true, devMode: false}, surfaces);
    assert.match(html, /plugin-catalog-stale/);                       // 离线缓存标记
    assert.match(html, /data-plugin-id="builtin\.dictionary"/);
    assert.match(html, /已最新/);                                      // 0.2.0 安装记录 ≥ 目录版
    assert.match(html, /更新/);                                        // 随包 0.3.0 < 目录 0.4.0
    const notLoaded = renderCatalogSection({catalog, stale: false}, new Map());
    assert.equal((notLoaded.match(/plugin-catalog-install/g) || []).length, 2); // 未安装 → 安装按钮
    assert.match(notLoaded, /5\.0 MB/);                                // 大小展示
});

test("renderDatasetsSection prefers catalog source and gates local install by dev mode", () => {
    const plugin = {id: "builtin.dictionary"};
    const surface = {
        datasets: [{id: "dictionary-basic", optional: false, min_version: "1.0.0", installed: null}],
        readiness: {ready: false, blockers: []},
    };
    const catalog = {datasets: [
        {plugin_id: "builtin.dictionary", dataset_id: "dictionary-basic", version: "1.1.0",
         asset: {url: "https://x/d.zip", sha256: "c".repeat(64), bytes: 107000000}},
    ]};

    // 目录有条目 + dev：主按钮走目录（data-catalog），本地 zip 按钮并存
    let html = renderDatasetsSection(plugin, surface, catalog, true);
    assert.match(html, /plugin-dataset-install" data-dataset="dictionary-basic" data-catalog="1"/);
    assert.match(html, /data-progress-id="builtin\.dictionary\/dictionary-basic"/);
    assert.match(html, /plugin-dataset-install-local/);

    // 目录有条目 + 生产：只有目录按钮，无本地入口
    html = renderDatasetsSection(plugin, surface, catalog, false);
    assert.match(html, /data-catalog="1"/);
    assert.doesNotMatch(html, /plugin-dataset-install-local/);

    // 无目录 + 生产：无安装按钮，提示无安装源
    html = renderDatasetsSection(plugin, surface, null, false);
    assert.doesNotMatch(html, /plugin-dataset-install/);
    assert.match(html, /暂无安装源/);

    // 无目录 + dev：保持 0.28.4 本地 zip 行为（两参调用兼容）
    html = renderDatasetsSection(plugin, surface);
    assert.equal((html.match(/plugin-dataset-install\b/g) || []).length, 1);
    assert.doesNotMatch(html, /data-catalog/);
});
