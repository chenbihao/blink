import assert from "node:assert/strict";
import {test} from "node:test";

// The editor's production module imports the Tauri bridge, whose compatibility
// shim installs its dialog guards on window when loaded.
globalThis.window ??= {};

const {buildWeatherSettingsPatch, isWeatherSearchCurrent} = await import("./weather-settings.js");

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
