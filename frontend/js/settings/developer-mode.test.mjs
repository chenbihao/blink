import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';

const dataUrl = source => 'data:text/javascript;base64,' + Buffer.from(source).toString('base64');
const languageCallbacks = globalThis.developerModeLanguageCallbacks = [];
const i18n = dataUrl(`export const t = (key, params) => key + (params?.remaining == null ? '' : ':' + params.remaining);
export const onLangChange = callback => globalThis.developerModeLanguageCallbacks.push(callback);`);
const source = (await readFile(new URL('./developer-mode.js', import.meta.url), 'utf8'))
    .replace("'../i18n/index.js'", JSON.stringify(i18n));
const {initDeveloperMode} = await import(dataUrl(source));

function target() {
    const listeners = new Map();
    return {dataset: {}, textContent: '', hidden: false, focused: false,
        addEventListener(type, callback) {
            const list = listeners.get(type) || []; list.push(callback); listeners.set(type, list);
        },
        emit(type, event = {}) { for (const callback of listeners.get(type) || []) callback(event); },
        focus() { this.focused = true; },
    };
}
const values = new Map();
const storage = {getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value)};
const legacyValues = new Map([['blink.settings.developerMode', 'true']]);
function harness(sessionStorage = storage) {
    let time = 0;
    let timerId = 0;
    const timers = new Map();
    const version = target(), status = target(), hide = target(), controls = target();
    const nodes = new Map([['about-version', version], ['developer-mode-status', status],
        ['developer-mode-hide', hide], ['developer-mode-controls', controls]]);
    const documentRef = {documentElement: target(), getElementById: key => nodes.get(key)};
    const windowRef = {...target(), sessionStorage,
        localStorage: {removeItem: key => legacyValues.delete(key)},
        setTimeout(callback, ms) { const id = ++timerId; timers.set(id, {at: time + ms, callback}); return id; },
        clearTimeout(id) { timers.delete(id); },
    };
    const advance = ms => {
        time += ms;
        for (const [id, timer] of timers) if (timer.at <= time) { timers.delete(id); timer.callback(); }
    };
    const args = {documentRef, windowRef, now: () => time};
    const controller = initDeveloperMode(args);
    return {controller, args, version, status, hide, controls, windowRef,
        root: documentRef.documentElement,
        advance,
        tap(ms = 100) { advance(ms); version.emit('click'); },
    };
}

const h = harness();
assert.equal(h.root.dataset.developerMode, 'false', '默认隐藏全部带统一标记的调试项');
assert.equal(legacyValues.has('blink.settings.developerMode'), false, '清除旧的永久记忆，不能自动解锁新会话');
assert.equal(h.controls.hidden, true, '默认不占据额外布局空间');
for (let i = 0; i < 6; i++) h.tap();
assert.equal(h.controller.enabled, false, '六次不能提前解锁');
assert.equal(h.status.textContent, 'about.developer.remaining:1');
h.tap();
assert.equal(h.controller.enabled, true);
assert.equal(h.root.dataset.developerMode, 'true');
assert.equal(h.status.textContent, 'about.developer.enabled:7');
assert.equal(values.get('blink.settings.developerMode'), 'true');
h.tap(); assert.equal(h.controller.enabled, true, '已启用时继续点击不能意外关闭');
assert.equal(initDeveloperMode(h.args), h.controller, '设置初始化重试不能重复绑定七连击');

const restored = harness();
assert.equal(restored.controller.enabled, true, '同一窗口会话刷新后保留解锁状态');
const newSessionValues = new Map();
const newSession = harness({getItem: key => newSessionValues.get(key) ?? null,
    setItem: (key, value) => newSessionValues.set(key, value)});
assert.equal(newSession.controller.enabled, false, '新会话默认隐藏，不继承上一会话状态');
restored.hide.emit('click');
assert.equal(restored.controller.enabled, false);
assert.equal(restored.version.focused, true, '隐藏按钮消失后归还键盘焦点');
assert.equal(values.get('blink.settings.developerMode'), 'false');

const paused = harness();
for (let i = 0; i < 6; i++) paused.tap();
paused.advance(1600);
assert.equal(paused.status.textContent, '', '间隔过长应自动清除失效的剩余次数提示');
paused.tap(1600); assert.equal(paused.controller.enabled, false, '间隔过长重新计数');
for (let i = 0; i < 5; i++) paused.tap();
assert.equal(paused.controller.enabled, false);
paused.windowRef.emit('blur'); paused.tap();
assert.equal(paused.status.textContent, '', '失焦后连续点击计数清零');
let prevented = false;
paused.version.emit('keydown', {key: 'Enter', repeat: true, preventDefault: () => { prevented = true; }});
assert.equal(prevented, true, '长按键盘自动重复不能充当多次点击');
for (const callback of languageCallbacks) callback();
assert.equal(paused.root.dataset.developerMode, 'false', '语言切换不改变显隐状态');

const warnings = [], oldWarn = console.warn;
console.warn = (...args) => warnings.push(args);
try {
    const unavailable = harness({getItem() { throw new Error('blocked'); }, setItem() { throw new Error('blocked'); }});
    for (let i = 0; i < 7; i++) unavailable.tap();
    assert.equal(unavailable.controller.enabled, true, '存储不可用时当前窗口仍可解锁');
    assert.equal(warnings.length, 2, '持久化失败有上下文日志');
} finally { console.warn = oldWarn; }
console.log('settings developer mode tests passed');
