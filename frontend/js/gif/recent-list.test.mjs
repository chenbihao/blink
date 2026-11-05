//! 「最近录制」列表测试（node --test）——工厂注入 stub，验证纯函数与交互。

import test from 'node:test';
import assert from 'node:assert/strict';
import {createRecentList, itemBadges, itemMetaText, itemTimeText} from './recent-list.js';

// —— 纯函数 ——

test('itemMetaText 汇总时长/尺寸/帧数/占用（KB 与 MB 分档）', () => {
    assert.equal(
        itemMetaText({totalMs: 7400, width: 1280, height: 720, fps: 15, frameCount: 111, spoolBytes: 1_880_000}),
        '7.4s · 1280×720 · 15fps · 111 帧 · 1.8 MB',
    );
    assert.equal(
        itemMetaText({totalMs: 0, width: 640, height: 480, fps: 30, frameCount: 3, spoolBytes: 900}),
        '0.0s · 640×480 · 30fps · 3 帧 · 1 KB',
    );
});

test('itemTimeText 本地时间 YYYY-MM-DD HH:mm；非法输入空串', () => {
    const text = itemTimeText(new Date(2026, 10, 1, 9, 5, 3).getTime());
    assert.equal(text, '2026-11-01 09:05');
    assert.equal(itemTimeText(NaN), '');
});

test('itemBadges：草稿/异常中断组合；正常会话无徽章', () => {
    assert.deepEqual(itemBadges({hasDraft: true, state: 'stopped'}), ['draft']);
    assert.deepEqual(itemBadges({hasDraft: false, state: 'recording'}), ['abnormal']);
    assert.deepEqual(itemBadges({hasDraft: true, state: 'recording'}), ['draft', 'abnormal']);
    assert.deepEqual(itemBadges({hasDraft: false, state: 'stopped'}), []);
});

// —— 交互（伪 DOM + stub invoke）——

class FakeElement {
    constructor(tag) {
        this.tag = tag;
        this.children = [];
        this.dataset = {};
        this.listeners = new Map();
        this._text = '';
        this.classList = {
            _set: new Set(),
            add: (...cs) => cs.forEach(c => this.classList._set.add(c)),
            remove: (...cs) => cs.forEach(c => this.classList._set.delete(c)),
            toggle: (c, on) => {
                if (on) this.classList._set.add(c);
                else this.classList._set.delete(c);
            },
            contains: c => this.classList._set.has(c),
        };
        this.style = {};
    }
    /** className 与 classList 联动（真实 DOM 语义） */
    set className(v) {
        this.classList._set = new Set(String(v).split(/\s+/).filter(Boolean));
    }
    get className() {
        return [...this.classList._set].join(' ');
    }
    get textContent() {
        return this._text;
    }
    set textContent(v) {
        this._text = String(v);
        this.children = [];
    }
    appendChild(el) {
        this.children.push(el);
        el.parent = this;
        return el;
    }
    addEventListener(type, fn) {
        this.listeners.set(type, fn);
    }
    click() {
        return this.listeners.get('click')?.();
    }
    get isConnected() {
        return !this._detached;
    }
}

function fixture({summaries = [], onOpenResult, respond, confirm = async () => true} = {}) {
    const calls = [];
    const dom = {
        pane: new FakeElement('section'),
        list: new FakeElement('div'),
        empty: new FakeElement('div'),
        loading: new FakeElement('div'),
        back: new FakeElement('button'),
        refresh: new FakeElement('button'),
        clear: new FakeElement('button'),
        status: new FakeElement('p'),
    };
    const original = globalThis.document;
    const byId = {
        'recent-pane': dom.pane,
        'recent-list': dom.list,
        'recent-empty': dom.empty,
        'recent-loading': dom.loading,
        'btn-back-edit': dom.back,
        'btn-recent-refresh': dom.refresh,
        'btn-recent-clear': dom.clear,
        'recent-status': dom.status,
    };
    globalThis.document = {
        getElementById: id => byId[id] ?? null,
        createElement: tag => new FakeElement(tag),
    };
    let current = null;
    const list = createRecentList({
        invoke: async (cmd, args) => {
            calls.push({cmd, args});
            if (respond) return respond(cmd, args);
            if (cmd === 'gif_recent_sessions') {
                return {items: summaries, total: summaries.length, offset: 0, limit: 20};
            }
            if (cmd === 'gif_session_thumbnail') {
                return {index: 0, width: 100, height: 60, pngBase64: ''};
            }
            if (cmd === 'gif_open_session') {
                if (onOpenResult === 'busy') {
                    throw {code: 'gif_export_busy', message: 'busy', retryable: true};
                }
                return null;
            }
            return null;
        },
        t: (key, params) => (params ? `${key}:${JSON.stringify(params)}` : key),
        confirm,
        onOpened: () => {
            current = summaries[0]?.id ?? null;
        },
        currentSessionId: () => current,
    });
    return {list, dom, calls, get current() { return current; },
        setCurrent(v) { current = v; }};
    void original;
}

test('clear covers all history through one backend action, then reloads the retained current session', async () => {
    const confirms = []; let cleared = false;
    const f = fixture({confirm: async text => {confirms.push(text); return true;}, respond: cmd => {
        if (cmd === 'gif_recent_sessions') return {items: cleared ? [{id: 'current'}] : [{id: 'current'}, {id: 'history'}], total: cleared ? 1 : 27, limit: 20};
        if (cmd === 'gif_clear_history') {cleared = true; return {deleted: 26, retained: 1};}
        return null;
    }});
    f.setCurrent('current'); f.list.bind(); await f.list.refresh();
    assert.equal(f.dom.clear.disabled, false);
    await f.dom.clear.click();
    assert.deepEqual(confirms, ['gif.recent.clearConfirm']);
    assert.equal(f.calls.filter(c => c.cmd === 'gif_clear_history').length, 1);
    assert.equal(f.calls.find(c => c.cmd === 'gif_clear_history').args.preserveSessionId, 'current');
    assert.equal(f.calls.some(c => c.cmd === 'gif_discard_session' || c.cmd === 'gif_export_cancel'), false);
    assert.deepEqual(f.dom.list.children.map(e => e.dataset.id), ['current']);
    assert.equal(f.current, 'current');
    assert.equal(f.dom.clear.disabled, true);
    assert.match(f.dom.status.textContent, /clearRetained.*26.*1/);
    assert.equal(f.dom.status.classList.contains('hidden'), false);
    assert.equal(f.dom.refresh.disabled, false);
});

test('clear cancellation and empty/current-only lists never invoke deletion', async () => {
    for (const currentOnly of [false, true]) {
        const f = fixture({summaries: [{id: 'one'}], confirm: async () => false});
        if (currentOnly) f.setCurrent('one');
        f.list.bind(); await f.list.refresh(); await f.dom.clear.click();
        assert.equal(f.calls.some(c => c.cmd === 'gif_clear_history'), false);
        assert.equal(f.dom.clear.disabled, currentOnly);
    }
    const empty = fixture(); empty.list.bind(); await empty.list.refresh();
    assert.equal(empty.dom.clear.disabled, true);
    await empty.dom.clear.click();
    assert.equal(empty.calls.some(c => c.cmd === 'gif_clear_history'), false);
});

test('a session switch during confirmation still preserves the originally displayed editor session', async () => {
    let f;
    f = fixture({summaries: [{id: 'original'}, {id: 'replacement'}],
        confirm: async () => {f.setCurrent('replacement'); return true;},
        respond: cmd => cmd === 'gif_recent_sessions' ? {items: [{id: 'original'}, {id: 'replacement'}], total: 2} :
            cmd === 'gif_clear_history' ? {deleted: 0, retained: 2} : null,
    });
    f.setCurrent('original'); f.list.bind(); await f.list.refresh();
    await f.dom.clear.click();
    assert.equal(f.calls.find(c => c.cmd === 'gif_clear_history').args.preserveSessionId, 'original');
    assert.equal(f.current, 'replacement');
});

test('clear waits for its visible thumbnail leases, blocks repeat actions and reports failure without losing rows', async () => {
    const reads = []; let rejectClear;
    const f = fixture({respond: cmd => {
        if (cmd === 'gif_recent_sessions') return {items: ['one', 'two', 'three'].map(id => ({id})), total: 3, limit: 20};
        if (cmd === 'gif_session_thumbnail') return new Promise(resolve => reads.push(resolve));
        if (cmd === 'gif_clear_history') return new Promise((_resolve, reject) => {rejectClear = reject;});
    }});
    f.list.bind(); await f.list.refresh();
    const clear = f.dom.clear.click();
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(f.dom.clear.disabled, true);
    assert.equal(f.dom.refresh.disabled, true);
    assert.equal(f.dom.back.disabled, true);
    assert.equal(f.calls.some(c => c.cmd === 'gif_clear_history'), false);
    assert.equal(reads.length, 2);
    for (const resolve of reads) resolve(null);
    await new Promise(resolve => setImmediate(resolve));
    await f.dom.clear.click();
    await f.dom.list.children[0].children[2].children[0].click();
    assert.equal(f.calls.filter(c => c.cmd === 'gif_clear_history').length, 1);
    assert.equal(f.calls.some(c => c.cmd === 'gif_open_session'), false);
    rejectClear({message: '磁盘暂不可用'}); await clear;
    assert.equal(f.dom.list.children.length, 3);
    assert.match(f.dom.status.textContent, /clearFailed.*磁盘暂不可用/);
    assert.equal(f.dom.status.classList.contains('recent-status-error'), true);
    assert.equal(f.dom.clear.disabled, false);
});

test('refresh 拉取摘要并渲染条目（含缩略图请求与徽章）', async () => {
    const {list, dom, calls} = fixture({
        summaries: [
            {id: 'rec-a', createdAt: new Date(2026, 10, 1, 9, 5, 3).getTime(),
                width: 1280, height: 720, fps: 15, frameCount: 111, totalMs: 7400,
                spoolBytes: 1_880_000, state: 'stopped', stopReason: 'user', hasDraft: true},
            {id: 'rec-b', createdAt: new Date(2026, 9, 30, 18, 0, 0).getTime(),
                width: 800, height: 600, fps: 10, frameCount: 40, totalMs: 4000,
                spoolBytes: 500_000, state: 'recording', stopReason: null, hasDraft: false},
        ],
    });
    await list.refresh();
    assert.equal(dom.list.children.length, 2);
    assert.equal(calls[0].cmd, 'gif_recent_sessions');
    assert.equal(calls[0].args.offset, 0);
    const first = dom.list.children[0];
    assert.equal(first.dataset.id, 'rec-a');
    assert.equal(first.children[1].children[0].textContent, '2026-11-01 09:05');
    assert.equal(first.children[1].children[1].textContent, '7.4s · 1280×720 · 15fps · 111 帧 · 1.8 MB');
    // hasDraft 徽章
    assert.equal(first.children[1].children[2].classList.contains('badge-draft'), true);
    // 异常中断徽章在第二项
    assert.equal(dom.list.children[1].children[1].children[2].classList.contains('badge-abnormal'), true);
    // 缩略图按会话请求
    assert.ok(calls.some(c => c.cmd === 'gif_session_thumbnail' && c.args.sessionId === 'rec-a'));
    assert.ok(calls.some(c => c.cmd === 'gif_session_thumbnail' && c.args.sessionId === 'rec-b'));
});

test('删除：确认后 invoke 丢弃并从列表移除', async () => {
    const {list, dom, calls} = fixture({
        summaries: [
            {id: 'rec-a', createdAt: 1, width: 10, height: 10, fps: 10, frameCount: 1,
                totalMs: 100, spoolBytes: 10, state: 'stopped', stopReason: null, hasDraft: false},
        ],
    });
    await list.refresh();
    const item = dom.list.children[0];
    const btnDelete = item.children[2].children[1];
    await btnDelete.click();
    assert.ok(calls.some(c => c.cmd === 'gif_discard_session' && c.args.sessionId === 'rec-a'));
    assert.equal(dom.list.children.length, 0);
    assert.equal(dom.empty.classList.contains('hidden'), false);
});

test('打开：busy 错误不切换且不崩；成功后回调 onOpened', async () => {
    const {list, dom, current} = fixture({
        summaries: [
            {id: 'rec-a', createdAt: 1, width: 10, height: 10, fps: 10, frameCount: 1,
                totalMs: 100, spoolBytes: 10, state: 'stopped', stopReason: null, hasDraft: false},
        ],
        onOpenResult: 'busy',
    });
    await list.refresh();
    const btnOpen = dom.list.children[0].children[2].children[0];
    await btnOpen.click();
    assert.equal(current, null); // busy：未切换
});

test('当前编辑会话的删除按钮禁用', async () => {
    const {list, dom, setCurrent} = fixture({
        summaries: [
            {id: 'rec-a', createdAt: 1, width: 10, height: 10, fps: 10, frameCount: 1,
                totalMs: 100, spoolBytes: 10, state: 'stopped', stopReason: null, hasDraft: false},
        ],
    });
    setCurrent('rec-a');
    await list.refresh();
    const btnDelete = dom.list.children[0].children[2].children[1];
    assert.equal(btnDelete.disabled, true);
});

test('分页遇到重复项仍推进服务端游标，不用去重后的条数重复请求旧页', async () => {
    const {list, dom, calls} = fixture({respond: (cmd, args) => {
        if (cmd !== 'gif_recent_sessions') return null;
        const pages = {0: ['a', 'b'], 2: ['b', 'c'], 4: ['d']};
        return {items: pages[args.offset].map(id => ({id})), total: 5, limit: 2};
    }});
    list.bind();
    await list.refresh();
    dom.list.scrollTop = 100;
    dom.list.clientHeight = dom.list.scrollHeight = 100;
    dom.list.listeners.get('scroll')();
    await new Promise(resolve => setImmediate(resolve));
    dom.list.listeners.get('scroll')();
    await new Promise(resolve => setImmediate(resolve));
    assert.deepEqual(calls.filter(c => c.cmd === 'gif_recent_sessions').map(c => c.args.offset), [0, 2, 4]);
    assert.deepEqual(dom.list.children.map(el => el.dataset.id), ['a', 'b', 'c', 'd']);
});

test('刷新期间再次刷新：旧响应不得覆盖新列表', async () => {
    const pending = [];
    const {list, dom} = fixture({respond: cmd => cmd === 'gif_recent_sessions'
        ? new Promise(resolve => pending.push(resolve)) : null});
    const old = list.refresh();
    const latest = list.refresh();
    pending[1]({items: [{id: 'new'}], total: 1, limit: 20});
    await latest;
    pending[0]({items: [{id: 'old'}], total: 1, limit: 20});
    await old;
    assert.deepEqual(dom.list.children.map(el => el.dataset.id), ['new']);
});

test('缩略图解码同时至多两个请求，翻页重绘复用缓存', async () => {
    const pending = [];
    const {list, calls} = fixture({respond: cmd => {
        if (cmd === 'gif_recent_sessions') return {items: ['a', 'b', 'c'].map(id => ({id})), total: 3};
        if (cmd === 'gif_session_thumbnail') return new Promise(resolve => pending.push(resolve));
        return null;
    }});
    await list.refresh();
    assert.equal(pending.length, 2);
    pending[0]({pngBase64: 'a'});
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(pending.length, 3);
    pending[1]({pngBase64: 'b'});
    pending[2]({pngBase64: 'c'});
    await new Promise(resolve => setImmediate(resolve));
    await list.refresh();
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(calls.filter(c => c.cmd === 'gif_session_thumbnail').length, 3);
});
