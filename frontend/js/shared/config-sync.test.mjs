import {test} from "node:test";
import assert from "node:assert/strict";
import {affectsSharedConfig, connectConfigActivity, createConfigRefresher, withConfigActivity} from "./config-sync.js";

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
function deferred() {
    let resolve, reject;
    const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
    return {promise, resolve, reject};
}

test("事件波次合并；旧读取不能污染共享状态或 DOM", async () => {
    const first = deferred(), second = deferred();
    const commits = [];
    let reads = 0, currentConfig, checkbox;
    const refresh = createConfigRefresher({
        read: () => (++reads === 1 ? first.promise : second.promise),
        apply: (cfg) => { currentConfig = cfg; checkbox = cfg.auto_start; commits.push(cfg); },
    });
    const initial = refresh.refresh();
    await tick();
    refresh.refresh(); refresh.refresh(); refresh.refresh();
    first.resolve({auto_start: false});
    await tick();
    assert.equal(reads, 2);
    assert.equal(currentConfig, undefined);
    second.resolve({auto_start: true});
    await initial;
    assert.equal(checkbox, true);
    assert.deepEqual(commits, [{auto_start: true}]);
});

test("初始化等待首个有效提交；首读被编辑打断后仍能完成初始化", async () => {
    const stale = deferred();
    let reads = 0, initialized = false;
    const refresh = createConfigRefresher({read: () => ++reads === 1 ? stale.promise : Promise.resolve("latest"), apply() {}});
    refresh.refresh();
    const initial = refresh.ready().then((value) => { initialized = true; return value; });
    await tick();
    const release = refresh.hold();
    stale.resolve("stale");
    await tick();
    assert.equal(initialized, false);
    release();
    assert.equal(await initial, "latest");
});

test("旧请求错误不回滚新值，也不显示过期错误", async () => {
    const old = deferred();
    let reads = 0, confirmed;
    const errors = [];
    const refresh = createConfigRefresher({read: () => ++reads === 1 ? old.promise : Promise.resolve("latest"), apply: (v) => { confirmed = v; }, onError: (e) => errors.push(e)});
    const request = refresh.refresh();
    await tick();
    refresh.refresh();
    old.reject(new Error("old failure"));
    await request;
    assert.equal(confirmed, "latest");
    assert.deepEqual(errors, []);
});

test("首读失败结束所有初始化等待；重试成功后可继续初始化", async () => {
    let fail = true, shown;
    const error = new Error("app.chord damaged");
    const refresh = createConfigRefresher({
        read: async () => { if (fail) throw error; return {chord_enabled: true}; },
        apply: (value) => { shown = value; }, onError() {},
    });
    const first = assert.rejects(refresh.ready(), error);
    const second = assert.rejects(refresh.ready(), error);
    await refresh.refresh();
    await Promise.all([first, second]);
    await assert.rejects(refresh.ready(), error);
    assert.equal(shown, undefined);
    fail = false;
    const retry = refresh.refresh();
    const ready = refresh.ready();
    await retry;
    assert.deepEqual(await ready, {chord_enabled: true});
    assert.deepEqual(shown, {chord_enabled: true});
});

test("保存、录制、编辑期间推迟回填；结束后读最新值，不循环发请求", async () => {
    let persisted = "initial", shown, reads = 0;
    const refresh = createConfigRefresher({read: async () => { reads++; return persisted; }, apply: (v) => { shown = v; }});
    await refresh.refresh();
    const save = refresh.hold(), recording = refresh.hold(), editing = refresh.hold();
    persisted = "confirmed";
    await refresh.refresh(); await refresh.refresh();
    save(); recording();
    await tick();
    assert.equal(shown, "initial");
    assert.equal(reads, 1);
    editing(); editing();
    await tick();
    assert.equal(shown, "confirmed");
    assert.equal(reads, 2);
});

test("保存打断在途读取；结束后的权威读同时更新两个窗口", async () => {
    const stale = deferred();
    let value = false, reads = 0;
    const views = [null, null];
    const first = createConfigRefresher({read: () => ++reads === 1 ? stale.promise : Promise.resolve(value), apply: (v) => { views[0] = v; }});
    const second = createConfigRefresher({read: async () => value, apply: (v) => { views[1] = v; }});
    const request = first.refresh();
    await tick();
    const done = first.hold();
    value = true;
    first.refresh(); await second.refresh();
    stale.resolve(false);
    await request;
    assert.deepEqual(views, [null, true]);
    done();
    await tick();
    assert.deepEqual(views, [true, true]);
});

test("监听配置 payload 兼容空值、旧逻辑 key、分片 key", () => {
    for (const value of [null, {}, {source: "feature_catalog"}, {key: "auto_start"}, {key: "app.chord"}, {key: "app.disable"}]) assert.equal(affectsSharedConfig(value), true);
    assert.equal(affectsSharedConfig({key: "ai_config"}), false);
});

test("保存失败的结束事件会补读；输入草稿保留至 change/focusout", async () => {
    const target = new EventTarget();
    const previous = globalThis.document;
    globalThis.document = target;
    let value = false, shown, reads = 0;
    const refresh = createConfigRefresher({read: async () => { reads++; return value; }, apply: (v) => { shown = v; }});
    connectConfigActivity(refresh, target);
    await refresh.refresh();
    const control = {matches: () => true};
    const send = (name) => {
        const event = new Event(name);
        Object.defineProperty(event, "target", {value: control});
        target.dispatchEvent(event);
    };
    send("input");
    await assert.rejects(withConfigActivity("auto_start", async () => { value = true; await refresh.refresh(); throw new Error("failed"); }));
    await tick();
    assert.equal(shown, false);
    send("change");
    await tick(); await tick();
    assert.equal(shown, true);
    assert.equal(reads, 2);
    globalThis.document = previous;
});
