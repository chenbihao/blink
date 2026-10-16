import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import {createConfigRefresher} from '../shared/config-sync.js';

const load = name => readFileSync(new URL(name, import.meta.url), 'utf8')
    .replace(/^import[\s\S]*?from\s+["'][^"']+["'];\s*/gm, '').replace(/^export /gm, '');
const flush = async () => { for (let i = 0; i < 8; i++) await Promise.resolve(); };

function setup() {
    const listeners = new Map();
    const calls = {enter: 0, reset: 0, search: 0, focus: 0, configs: []};
    const reads = [];
    let active = false, resolveSnapshot;
    const snapshot = new Promise(resolve => { resolveSnapshot = resolve; });
    const noop = () => {};
    const node = () => ({value: '', offsetHeight: 100, style: {}, addEventListener: noop, focus: () => calls.focus++});
    const context = vm.createContext({
        listNavigation: {reset: noop, refreshFromConfigData: cfg => calls.configs.push(cfg.wheel_mode)},
        appEl: node(), queryEl: node(), aiQueryEl: node(), console,
        EVENTS: new Proxy({}, {get: (_, key) => key}),
        listen: (name, callback) => { listeners.set(name, callback); return Promise.resolve(noop); },
        invoke: name => name === 'get_main_window_layout_state' ? snapshot : new Promise((resolve, reject) => reads.push({resolve, reject})),
        createConfigRefresher,
        applyThemeFromConfigData: noop, applyGlassOpacityFromConfigData: noop, applyI18nFromConfigData: noop,
        autosuggestConfig: {refreshFromConfigData: noop},
        chord: {refreshFromConfigData: () => Promise.resolve()},
        resetSuggestionSession: () => Promise.resolve(),
        resizeWindow: () => Promise.resolve(), requestAnimationFrame: () => 1, cancelAnimationFrame: noop,
        document: {addEventListener: noop},
        clipboardMode: {
            enter: () => { if (!active) { active = true; calls.enter++; } },
            reset: () => { if (active) { active = false; calls.reset++; } },
            isActive: () => active,
        },
        inputState: {inChordStandby: () => false, forceClearReadOnly: noop, onShown: noop, reevaluate: noop},
        aiMode: {isActive: () => false}, cmdMode: {reset: noop},
        ghost: {unfreeze: noop}, dictation: {resetOnShown: noop},
        search: {reset: noop, fetchContextSuggestions: () => calls.search++},
        results: {clear: noop, refreshMaxResultsFromConfigData: noop},
    });
    vm.runInContext(load('./window-size.js') + '\n' + load('./lifecycle.js'), context);
    context.init();
    return {calls, reads, resolveSnapshot, emit: (name, payload) => listeners.get(name)({payload})};
}

{
    const h = setup();
    h.resolveSnapshot({revision: 1, visible: true, clipboard: true});
    await flush();
    assert.equal(h.calls.enter, 1, '初次 SHOWN 丢失时从快照恢复剪贴板模式');
    assert.equal(h.calls.search, 0, '快照恢复不启动普通上下文搜索');
}

{
    const h = setup();
    h.emit('SHOWN', {revision: 3, clipboard: true});
    h.resolveSnapshot({revision: 1, visible: false, clipboard: false});
    await flush();
    assert.equal(h.calls.reset, 0, '迟到初始化快照不清理新会话');
    h.emit('HIDDEN', {revision: 4});
    h.emit('SHOWN', {revision: 5, clipboard: true});
    h.emit('HIDDEN', {revision: 4});
    h.emit('SHOWN', {revision: 3, clipboard: false});
    assert.equal(h.calls.enter, 2);
    assert.equal(h.calls.reset, 1, '旧 HIDDEN/SHOWN 不清理后来唤起的列表');
}

{
    const h = setup();
    h.emit('SHOWN', {revision: 1, clipboard: false});
    assert.equal(h.calls.search, 1);
    h.emit('CHORD_ENTER_MODE', {revision: 1, mode: 'clipboard', preserveQuery: true});
    assert.equal(h.calls.enter, 1, '可见窗口无需再次 SHOWN 即可切换模式');
    assert.equal(h.calls.search, 1, '切模式不重复启动普通搜索');
    h.emit('HIDDEN', {revision: 2});
    h.emit('CHORD_ENTER_MODE', {revision: 1, mode: 'clipboard'});
    assert.equal(h.calls.enter, 1, '旧会话模式事件不能重新激活剪贴板');
}

{
    const h = setup();
    await flush();
    assert.equal(h.reads.length, 1, '先订阅配置广播，再做启动读取');
    h.emit('SHOWN', {revision: 1, clipboard: true});
    h.emit('CONFIG_CHANGED', {key: 'app.search'});
    h.reads[0].resolve({wheel_mode: 'page'});
    await flush();
    assert.deepEqual(h.calls.configs, [], '配置广播前的旧读取不能覆盖新设置');
    assert.equal(h.reads.length, 2, '并发失效合并为一次最新读取');
    h.reads[1].resolve({wheel_mode: 'step'});
    await flush();
    assert.deepEqual(h.calls.configs, ['step'], '剪贴板 SHOWN 也刷新配置');
    h.emit('CONFIG_CHANGED', {key: 'app.search'});
    await flush();
    h.reads[2].resolve({wheel_mode: 'page'});
    await flush();
    assert.deepEqual(h.calls.configs, ['step', 'page'], '可见会话无需重开即响应配置');
    assert.equal(h.calls.enter, 1, '热刷新不重建剪贴板会话');
    h.emit('HIDDEN', {revision: 2});
    h.emit('CONFIG_CHANGED', {key: 'app.search'});
    await flush();
    h.reads[3].resolve({wheel_mode: 'step'});
    await flush();
    assert.equal(h.calls.configs.at(-1), 'step', '隐藏期间配置仍同步');
}

console.log('main-window lifecycle snapshots, revisions and live configuration passed');
