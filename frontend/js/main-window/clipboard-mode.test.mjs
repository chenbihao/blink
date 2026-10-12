import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import * as selection from './clipboard-selection.js';
import {parse as parseColor} from '../shared/color.js';

// 执行实际状态机，替换 DOM/IPC 边界；可手动推进防抖和任意顺序完成请求。
const source = readFileSync(new URL('./clipboard-mode.js', import.meta.url), 'utf8')
    .replace(/^import[\s\S]*?from\s+["'][^"']+["'];\s*/gm, '')
    .replace(/^export /gm, '');
const deferred = () => {
    let resolve, reject;
    const promise = new Promise((yes, no) => {resolve = yes; reject = no;});
    return {promise, resolve, reject};
};
const flush = async () => {for (let i = 0; i < 5; i++) await Promise.resolve();};

function setup() {
    selection._resetForTest();
    const timers = new Map(), requests = [], renders = [], hits = [], pastes = [];
    let timerId = 0, feedback = null;
    const noop = () => {};
    const queryEl = {value: '', placeholder: '', focus: noop};
    const context = vm.createContext({
        queryEl, resultsEl: {querySelectorAll: () => []}, selection, parseColor,
        results: {setClipboardMode: noop, clear: noop, render: entries => renders.push(entries), refreshStatusbar: noop},
        ghost: {clear: noop}, suggestionBar: {clear: noop},
        statusbar: {clearOverride: noop, setOverride: noop},
        document: {body: {classList: {add: noop, remove: noop}}},
        dismissFeedback: () => {feedback = null;}, showFeedback: message => {feedback = message;},
        syncWindowSize: noop, resyncWindowSize: noop, t: key => key,
        setTimeout: callback => {timers.set(++timerId, callback); return timerId;},
        clearTimeout: id => timers.delete(id),
        searchClipboard: (query, seq, kind) => {
            const request = {...deferred(), query, seq, kind};
            requests.push(request);
            return request.promise;
        },
        getClipboardTextBatch: async keys => keys.map(key => ({text: `text:${key}`})),
        recordClipboardHit: async key => {hits.push(key);},
        pasteToInput: text => {const request = {...deferred(), text}; pastes.push(request); return request.promise;},
        showActionError: () => {feedback = 'error';},
        console: {log: noop, error: noop}, Date,
    });
    vm.runInContext(source, context);
    context.enter();
    return {
        context, queryEl, requests, renders, pastes, hits,
        input(value) {queryEl.value = value; context.handleInput(value);},
        tick() {const pending = [...timers.values()]; timers.clear(); pending.forEach(callback => callback());},
        get feedback() {return feedback;}, set feedback(value) {feedback = value;},
        get timerCount() {return timers.size;},
    };
}

{
    const h = setup();
    h.input('latest');
    h.context.setKind('text');
    assert.equal(h.requests.at(-1).query, 'latest');
    assert.equal(h.requests.at(-1).kind, 'text');
    assert.equal(h.timerCount, 0, '类型切换取消旧防抖');
    h.requests[0].resolve({entries: [{name: 'stale'}]});
    h.requests.at(-1).resolve({entries: [{name: 'latest'}]});
    await flush();
    assert.equal(h.renders.length, 1);
    assert.equal(h.renders[0][0].name, 'latest');
}
{
    const h = setup();
    h.input('new');
    h.requests[0].resolve({entries: [{name: 'old'}]});
    await flush();
    assert.equal(h.renders.length, 0, '防抖期间旧响应也不能回流');
    h.input('');
    h.tick();
    assert.equal(h.requests.at(-1).query, '', '退回旧 query 也要撤销待发的新 query');
}
{
    const h = setup();
    h.input('#ff0000');
    assert.equal(h.renders.at(-1)[0].source, 'color');
    h.context.setKind('image');
    assert.equal(h.requests.at(-1).kind, 'image');
    h.context.setKind('text');
    assert.equal(h.renders.at(-1)[0].source, 'color', '类型切换复用颜色字面量逻辑');
}
{
    const h = setup();
    selection.toggleSelection('a'); selection.toggleSelection('b');
    const pending = h.context.batchPaste();
    await flush();
    assert.equal(h.pastes[0].text, 'text:a\ntext:b');
    h.context.reset();
    assert.equal(h.feedback, null, '隐藏时收掉持久 loading');
    h.context.enter();
    selection.toggleSelection('new');
    h.feedback = 'new feedback';
    h.pastes[0].resolve();
    await pending;
    assert.deepEqual(selection.getSelectedKeys(), ['new']);
    assert.equal(h.feedback, 'new feedback');
    assert.deepEqual(h.hits, ['a', 'b'], '旧成功仍记录命中');
}
for (const fail of [false, true]) {
    const h = setup();
    selection.toggleSelection('a');
    const pending = h.context.batchPaste();
    await flush();
    if (fail) h.pastes[0].reject(new Error('busy')); else h.pastes[0].resolve();
    await pending;
    assert.equal(selection.hasSelection(), fail, '当前会话成功清选，失败保留');
    assert.equal(h.feedback, fail ? 'error' : null);
}
console.log('剪贴板模式：防抖、类型切换、响应乱序和批量上屏会话隔离通过');
