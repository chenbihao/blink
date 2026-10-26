//! GIF 控制条渲染纯逻辑测试（node --test）。
//!
//! renderState 是纯 DOM 操作函数——测试用假 DOM 元素（class list + textContent
//! 断言），不依赖浏览器。

import test from 'node:test';
import assert from 'node:assert/strict';
import {renderState} from './recorder-view.js';

function fakeDom() {
    const mk = () => ({
        classes: new Set(),
        text: '',
        classList: {
            add(c) { this._el.classes.add(c); },
            remove(c) { this._el.classes.delete(c); },
            toggle(c, on) { on ? this._el.classes.add(c) : this._el.classes.delete(c); },
            contains(c) { return this._el.classes.has(c); },
            _el: null,
        },
        set textContent(v) { this.text = v; },
        get textContent() { return this.text; },
    });
    const countdown = mk();
    const recLine = mk();
    const recDot = mk();
    const recTime = mk();
    const recFrames = mk();
    const iconA = mk();
    const iconB = mk();
    const pauseBtn = {icons: null, querySelectorAll: () => [iconA, iconB]};
    for (const el of [countdown, recLine, recDot, recTime, recFrames, iconA, iconB]) {
        el.classList._el = el;
    }
    iconB.classes.add('hidden');
    return {countdown, recLine, recDot, recTime, recFrames, pauseBtn, iconA, iconB};
}

test('倒计时阶段只显示大数字', () => {
    const dom = fakeDom();
    renderState({phase: 'countdown', countdownMs: 2500}, dom);
    assert.ok(!dom.countdown.classes.has('hidden'));
    assert.ok(dom.recLine.classes.has('hidden'));
    assert.equal(dom.countdown.textContent, '3');
    // 倒计时未开录，暂停按钮禁用
    assert.equal(dom.pauseBtn.disabled, true);
    renderState({phase: 'countdown', countdownMs: 900}, dom);
    assert.equal(dom.countdown.textContent, '1');
});

test('录制中显示有效时长与帧数', () => {
    const dom = fakeDom();
    renderState({phase: 'recording', elapsedMs: 65430, frames: 982}, dom);
    assert.ok(dom.countdown.classes.has('hidden'));
    assert.ok(!dom.recLine.classes.has('hidden'));
    assert.ok(!dom.recDot.classes.has('paused'));
    assert.equal(dom.recTime.textContent, '01:05.4');
    assert.equal(dom.recFrames.textContent, '982 帧');
    assert.equal(dom.pauseBtn.disabled, false, '录制中暂停可用');
});

test('暂停状态切换圆点与按钮图标', () => {
    const dom = fakeDom();
    renderState({phase: 'paused', elapsedMs: 1000, frames: 15}, dom);
    assert.ok(dom.recDot.classes.has('paused'));
    // 暂停时：pause 图标隐藏、play 图标显示
    assert.ok(dom.iconA.classes.has('hidden'));
    assert.ok(!dom.iconB.classes.has('hidden'));
    renderState({phase: 'recording', elapsedMs: 2000, frames: 30}, dom);
    assert.ok(!dom.iconA.classes.has('hidden'));
    assert.ok(dom.iconB.classes.has('hidden'));
});

test('时长格式含分钟与十分位', () => {
    const dom = fakeDom();
    renderState({phase: 'recording', elapsedMs: 0, frames: 0}, dom);
    assert.equal(dom.recTime.textContent, '00:00.0');
    renderState({phase: 'recording', elapsedMs: 599940, frames: 0}, dom);
    assert.equal(dom.recTime.textContent, '09:59.9');
});
