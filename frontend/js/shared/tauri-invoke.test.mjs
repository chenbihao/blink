//! tauri.js invoke 第三参透传回归测试（0.23.12）。
//!
//! 回归背景：0.22.6 将裸导出的 Tauri invoke 换成包装函数时，签名只声明了
//! (cmd, args)，第三参 options（raw IPC headers）被静默丢弃，导致
//! screenshot_pin / screenshot_copy_rgba / ocr_image 等所有依赖 headers 的
//! raw IPC 命令隐形失效。本测试钉死 options 必须原样透传。

import assert from 'node:assert/strict';

// tauri.js 顶层会替换 window.alert/confirm/prompt、运行期动态读取 __TAURI__，
// 先放置 stub window 再动态 import。
const calls = [];
const stubInvoke = (cmd, args, options) => {
    calls.push({cmd, args, options});
    return Promise.resolve('ok');
};

globalThis.window = {
    __TAURI__: {core: {invoke: stubInvoke}},
    alert: () => {
    },
    confirm: () => true,
    prompt: () => '',
};

const {invoke} = await import('./tauri.js');

// 1. 普通两参调用：options 为 undefined 透传
await invoke('search_apps', {query: 'a'});
assert.equal(calls.length, 1);
assert.equal(calls[0].cmd, 'search_apps');
assert.deepEqual(calls[0].args, {query: 'a'});
assert.equal(calls[0].options, undefined);
console.log('✓ 两参调用透传且 options=undefined');

// 2. raw IPC 三参调用：headers 必须原样到达（钉图回归场景）
const png = new Uint8Array([1, 2, 3]);
const opts = {headers: {'screen-x': '100', 'screen-y': '200', 'show-translating': 'false'}};
await invoke('screenshot_pin', png, opts);
assert.equal(calls[1].cmd, 'screenshot_pin');
assert.equal(calls[1].args, png);
assert.deepEqual(calls[1].options, opts, '第三参 options 必须透传（headers 不得丢失）');
console.log('✓ 三参 raw IPC 调用 options/headers 原样透传');

// 3. __TAURI__ 未注入 → 拒绝且带命令名
delete globalThis.window.__TAURI__;
await assert.rejects(
    invoke('screenshot_pin', png, opts),
    (e) => e.message.includes('invoke 不可用') && e.message.includes('screenshot_pin'),
);
console.log('✓ __TAURI__ 未注入时拒绝并带命令名');

// 4. 旧形态兜底：TAU.invoke（无 core 命名空间）同样透传三参
globalThis.window.__TAURI__ = {invoke: stubInvoke};
await invoke('screenshot_save', png, {headers: {path: 'C:/tmp/a.png'}});
assert.equal(calls[2].cmd, 'screenshot_save');
assert.deepEqual(calls[2].options, {headers: {path: 'C:/tmp/a.png'}});
console.log('✓ TAU.invoke 旧形态兜底且三参透传');

console.log('\n4/4 tests passed');
