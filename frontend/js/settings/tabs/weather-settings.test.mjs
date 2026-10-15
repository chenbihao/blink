import assert from "node:assert/strict";
import {test} from "node:test";
import {TestElement, weatherCardFixture} from "./weather-settings-test-helpers.mjs";

// The editor's production module imports the Tauri bridge, whose compatibility
// shim installs its dialog guards on window when loaded.
globalThis.window ??= {};

const {buildWeatherSettingsPatch, isWeatherSearchCurrent, mountWeatherSettings} = await import("./weather-settings.js");

test("saved location region can be confirmed without selecting the location again", {timeout: 2000}, async () => {
    const location = {name: "Guangzhou", latitude: 23.13, longitude: 113.26, coordinate_system: "wgs84",
        source: "qweather", source_id: "qweather:101280101", timezone: "Asia/Shanghai"};
    const {card, root} = weatherCardFixture();
    const editor = mountWeatherSettings(card, {settings: {default_location: location}});
    try {
        await root.fire("click", {target: root.querySelector(".weather-edit-toggle")});
        assert.deepEqual(editor.collect(), {}, "opening does not create a write");
        const region = root.querySelector(".weather-region");
        assert.equal(region.value, "unknown");
        region.value = "mainland";
        await root.fire("change", {target: region});
        assert.deepEqual(editor.collect().default_location, {...location, in_mainland_china: true});
        assert.equal(location.in_mainland_china, undefined, "saved snapshot is unchanged before save");
        await root.fire("click", {target: new TestElement("weather-cancel")});
        assert.deepEqual(editor.collect(), {}, "cancel discards the region draft");
    } finally { editor.dispose(); }
});

test("opening restores the saved region, and clearing the location cannot be undone by region edits", {timeout: 2000}, async () => {
    for (const [saved, shown] of [[true, "mainland"], [false, "outside"]]) {
        const location = {latitude: 0, longitude: 0, coordinate_system: "wgs84", in_mainland_china: saved};
        const {card, root} = weatherCardFixture();
        const editor = mountWeatherSettings(card, {enabled: false, settings: {default_location: location}});
        try {
            await root.fire("click", {target: root.querySelector(".weather-edit-toggle")});
            const region = root.querySelector(".weather-region");
            assert.equal(region.value, shown);
            region.value = "unknown";
            await root.fire("change", {target: region});
            assert.equal(editor.collect().default_location.in_mainland_china, null);
            await root.fire("click", {target: new TestElement("weather-clear-location")});
            region.value = "mainland";
            await root.fire("change", {target: region});
            assert.equal(editor.collect().default_location, null);
        } finally { editor.dispose(); }
    }
});

test("new manual coordinates do not inherit a saved location's region automatically", {timeout: 2000}, async () => {
    const {card, root} = weatherCardFixture();
    const editor = mountWeatherSettings(card, {settings: {default_location: {
        latitude: 23.13, longitude: 113.26, coordinate_system: "wgs84", in_mainland_china: true,
    }}});
    try {
        await root.fire("click", {target: root.querySelector(".weather-edit-toggle")});
        root.querySelector(".weather-latitude").value = "0";
        root.querySelector(".weather-longitude").value = "0";
        await root.fire("click", {target: new TestElement("weather-manual-use")});
        assert.equal(editor.collect().default_location.in_mainland_china, null);
        const region = root.querySelector(".weather-region");
        region.value = "outside";
        await root.fire("change", {target: region});
        assert.equal(editor.collect().default_location.in_mainland_china, false);
    } finally { editor.dispose(); }
});

test("weather location patch keeps untouched settings out of the write", () => {
    assert.deepEqual(buildWeatherSettingsPatch(undefined, false), {});
});

test("weather location patch distinguishes a selected place from an explicit clear", () => {
    const location = {
        name: "Chaoyang",
        display_name: "Chaoyang, Liaoning, China",
        latitude: 41.57,
        longitude: 120.45,
        coordinate_system: "wgs84",
        country: "China",
        country_code: "CN",
        administrative_area: "Liaoning",
        administrative_areas: ["Liaoning", "Chaoyang"],
        timezone: "Asia/Shanghai",
        kind: "city_center",
        source: "open_meteo",
        source_id: "example",
        resolved_at: "2026-10-15T00:00:00Z",
    };

    // 0.26.2 起 default_provider 由通用设置控件管理，地点 patch 不再写死来源。
    assert.deepEqual(buildWeatherSettingsPatch(location), {
        settings_version: 1,
        default_location: location,
        default_city: "",
    });
    assert.deepEqual(buildWeatherSettingsPatch(null), {
        settings_version: 1,
        default_location: null,
        default_city: "",
    });
});

test("weather search results are accepted only for the current live editor lease", () => {
    const current = {
        disposed: false,
        editorOpen: true,
        enabled: true,
        revision: 4,
        currentRevision: 4,
        rootConnected: true,
    };
    assert.equal(isWeatherSearchCurrent(current), true);

    for (const stale of [
        {currentRevision: 5},
        {editorOpen: false},
        {enabled: false},
        {disposed: true},
        {rootConnected: false},
    ]) {
        assert.equal(isWeatherSearchCurrent({...current, ...stale}), false, JSON.stringify(stale));
    }
});

test("weather location patch carries the region declaration when present", () => {
    // 区域声明随地点一起保存（彩云按大陆/非大陆选择坐标系）；未确认时为 null
    const confirmed = {
        name: "Shanghai",
        display_name: "Shanghai · China",
        latitude: 31.18,
        longitude: 121.53,
        coordinate_system: "wgs84",
        in_mainland_china: true,
    };
    assert.deepEqual(buildWeatherSettingsPatch(confirmed), {
        settings_version: 1,
        default_location: confirmed,
        default_city: "",
    });
    const unconfirmed = {...confirmed, in_mainland_china: null};
    assert.equal(
        buildWeatherSettingsPatch(unconfirmed).default_location.in_mainland_china,
        null,
    );
});
