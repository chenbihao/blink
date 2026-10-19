import {test} from 'node:test';
import assert from 'node:assert/strict';

const tick = () => new Promise(resolve => setTimeout(resolve, 0));
const flush = async () => { await tick(); await tick(); await tick(); };
function element(id) {
    return {id, value: '', checked: false, disabled: false, hidden: false, dataset: {}, textContent: '',
        listeners: new Map(), classList: {add() {}, remove() {}, toggle() {}},
        addEventListener(name, listener) { this.listeners.set(name, listener); },
        removeAttribute(name) { delete this[name]; },
    };
}
const ids = ['file-search-enabled', 'everything-port', 'managed-everything-port', 'everything-max-results',
    'everything-status', 'managed-everything-port-status', 'managed-everything-port-row', 'probe-everything',
    'file-search-active-source', 'bookmarks-enabled', 'bookmarks-keyword', 'bookmarks-mix', 'bookmarks-error',
    'bookmarks-browser-chrome', 'bookmarks-browser-edge', 'bookmarks-browser-brave', 'bookmarks-status', 'bookmarks-refresh',
    'start-menu-enabled', 'start-menu-scan-depth', 'start-menu-include-uwp', 'start-menu-include-system',
    'start-menu-discover-settings', 'system-entry-status', 'system-entry-retry'];
const nodes = new Map(ids.map(id => [id, element(id)]));
const doc = new EventTarget();
doc.hidden = false;
doc.getElementById = id => nodes.get(id) || null;
doc.querySelectorAll = () => [];
doc.querySelector = () => ({});
globalThis.document = doc;
globalThis.window = globalThis;
globalThis.addEventListener = () => {};
const intervals = [];
globalThis.setInterval = callback => { intervals.push(callback); return intervals.length; };

let fileConfig = {enabled: true, everything_port: 80, managed_everything_port: 54000, max_results: 20};
let fileStatus = {userAvailable: false, userPort: 80, installed: true, running: true, port: 52822, activeSource: 'managed'};
let bookmarkStatus = {enabled: true, scanning: false, entries: 5, failedFiles: 0, truncated: false};
let readBookmarks = async () => structuredClone(bookmarkStatus);
let refreshBookmarks = async () => ({...bookmarkStatus, scanning: true});
let refreshCalls = 0;
const saves = [];
globalThis.__TAURI__ = {event: {listen: async () => () => {}}, core: {invoke: async (command, args) => {
    if (command === 'get_engine_config') return structuredClone(fileConfig);
    if (command === 'get_file_search_status' || command === 'probe_everything') return structuredClone(fileStatus);
    if (command === 'get_calc_config') return {enabled: true};
    if (command === 'get_start_menu_config') return {enabled: false, scan_depth: 3};
    if (command === 'get_config_section') return {enabled: true, keyword: 'bm', browsers: ['chrome'], mix_into_global: false};
    if (command === 'get_bookmarks_status') return readBookmarks();
    if (command === 'refresh_bookmarks') { refreshCalls++; return refreshBookmarks(); }
    if (command === 'set_config') {
        saves.push(structuredClone(args));
        if (args.key === 'file_search') Object.assign(fileConfig, args.value);
        return;
    }
    throw new Error(command);
}}};
const {initEnginesTab} = await import('./engines.js');
initEnginesTab({});
await flush();
const control = id => nodes.get(id);
const change = id => control(id).listeners.get('change')({currentTarget: control(id)});
const clickRefresh = () => control('bookmarks-refresh').listeners.get('click')();
const poll = async () => { intervals[0](); await flush(); };

test('用户实例离线时仍可修改其端口；托管偏好与实际端口分别展示', async () => {
    assert.equal(control('everything-port').value, 80);
    assert.equal(control('managed-everything-port').value, 54000);
    assert.equal(control('managed-everything-port-row').hidden, false);
    assert.match(control('managed-everything-port-status').textContent, /52822/);
    control('everything-port').value = '7788';
    await change('everything-port');
    assert.equal(saves.at(-1).value.everything_port, 7788);
    assert.equal(Object.hasOwn(saves.at(-1).value, 'managed_everything_port'), false);
    control('managed-everything-port').value = '55000';
    await change('managed-everything-port');
    assert.equal(saves.at(-1).value.managed_everything_port, 55000);
    assert.equal(Object.hasOwn(saves.at(-1).value, 'everything_port'), false);
    assert.equal(control('managed-everything-port').value, '55000', '状态轮询不覆盖下次启动偏好');
    assert.equal(control('everything-port').value, '7788');
});

test('无效端口不保存，也不阻断其他设置字段保存', async () => {
    const count = saves.length;
    control('everything-port').value = '1.5';
    await change('everything-port');
    assert.equal(saves.length, count);
    control('file-search-enabled').checked = false;
    await change('file-search-enabled');
    assert.equal(saves.at(-1).value.enabled, false);
    assert.equal(Object.hasOwn(saves.at(-1).value, 'everything_port'), false);
});

test('手动刷新单飞、旧读取不能覆盖新状态，轮询回填完成后的条数', async () => {
    let finishOld, finishRefresh;
    readBookmarks = () => new Promise(resolve => { finishOld = resolve; });
    intervals[0](); await tick();
    refreshBookmarks = () => new Promise(resolve => { finishRefresh = resolve; });
    const pending = clickRefresh();
    assert.equal(control('bookmarks-refresh').disabled, true);
    await clickRefresh();
    assert.equal(refreshCalls, 1);
    finishOld({enabled: false, scanning: false, entries: 0}); await flush();
    assert.equal(control('bookmarks-refresh').disabled, true);
    finishRefresh({...bookmarkStatus, scanning: true}); await pending;
    assert.equal(control('bookmarks-refresh').disabled, true);
    assert.equal(control('bookmarks-status').textContent, '索引中…');
    bookmarkStatus = {...bookmarkStatus, entries: 12};
    readBookmarks = async () => structuredClone(bookmarkStatus);
    await poll();
    assert.equal(control('bookmarks-status').textContent, '已索引 12 条');
    assert.equal(control('bookmarks-refresh').disabled, false);
});

test('刷新失败可重试，部分文件失败如实显示，禁用后不发起刷新', async () => {
    refreshBookmarks = async () => { throw new Error('fixture IPC error'); };
    await clickRefresh();
    assert.match(control('bookmarks-status').textContent, /刷新请求失败/);
    assert.equal(control('bookmarks-refresh').disabled, false);
    bookmarkStatus = {...bookmarkStatus, failedFiles: 1};
    await poll();
    assert.match(control('bookmarks-status').textContent, /12.*1 个文件暂不可读/);
    bookmarkStatus = {...bookmarkStatus, enabled: false};
    await poll();
    assert.equal(control('bookmarks-refresh').disabled, true);
    const before = refreshCalls;
    await clickRefresh();
    assert.equal(refreshCalls, before);
    assert.equal(control('bookmarks-status').textContent, '已禁用');
});
