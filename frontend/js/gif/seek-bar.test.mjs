//! 录制编辑器进度定位纯逻辑测试（node --test）。

import test from 'node:test';
import assert from 'node:assert/strict';
import {
    clampPreviewLeft,
    createLatestGen,
    formatTimecode,
    keyboardStep,
    locateTime,
    pointerRatio,
    ratioToTime,
    timeAt,
} from './seek-bar.js';

/** 前缀和构造：[100, 66, 500] → [0, 100, 166, 666] */
const prefixOf = durs => durs.reduce((acc, d) => (acc.push(acc[acc.length - 1] + d), acc), [0]);

test('locateTime 按有效时长前缀和二分定位（含帧内偏移）', () => {
    const prefix = prefixOf([100, 66, 500]);
    assert.deepEqual(locateTime(prefix, 0), {playIdx: 0, inFrameMs: 0, atEnd: false});
    assert.deepEqual(locateTime(prefix, 99), {playIdx: 0, inFrameMs: 99, atEnd: false});
    assert.deepEqual(locateTime(prefix, 100), {playIdx: 1, inFrameMs: 0, atEnd: false});
    assert.deepEqual(locateTime(prefix, 165), {playIdx: 1, inFrameMs: 65, atEnd: false});
    assert.deepEqual(locateTime(prefix, 166), {playIdx: 2, inFrameMs: 0, atEnd: false});
    assert.deepEqual(locateTime(prefix, 400), {playIdx: 2, inFrameMs: 234, atEnd: false});
});

test('locateTime 末尾与越界：t>=total 落末帧末尾，负值落首帧起点', () => {
    const prefix = prefixOf([100, 66, 500]);
    assert.deepEqual(locateTime(prefix, 666), {playIdx: 2, inFrameMs: 500, atEnd: true});
    assert.deepEqual(locateTime(prefix, 9999), {playIdx: 2, inFrameMs: 500, atEnd: true});
    assert.deepEqual(locateTime(prefix, -5), {playIdx: 0, inFrameMs: 0, atEnd: false});
    assert.deepEqual(locateTime(prefix, NaN), {playIdx: 0, inFrameMs: 0, atEnd: false});
});

test('locateTime 空素材返回 null；单帧素材整段落该帧', () => {
    assert.equal(locateTime([0], 50), null);
    assert.equal(locateTime([], 50), null);
    const one = prefixOf([250]);
    assert.deepEqual(locateTime(one, 0), {playIdx: 0, inFrameMs: 0, atEnd: false});
    assert.deepEqual(locateTime(one, 120), {playIdx: 0, inFrameMs: 120, atEnd: false});
    assert.deepEqual(locateTime(one, 250), {playIdx: 0, inFrameMs: 250, atEnd: true});
});

test('locateTime 对变速/停留/删帧后的不均匀前缀同样正确（只依赖前缀和）', () => {
    // 模拟：删帧后序列为 3 帧，其中第 1 帧 hold 2000ms、全局 2× 变速使其他帧 33ms
    const prefix = prefixOf([33, 2000, 33]);
    assert.equal(locateTime(prefix, 1000).playIdx, 1);
    assert.equal(locateTime(prefix, 2032).playIdx, 1);
    assert.equal(locateTime(prefix, 2033).playIdx, 2);
    assert.equal(locateTime(prefix, 2065).playIdx, 2);
    assert.equal(locateTime(prefix, 2066).atEnd, true);
});

test('timeAt 为 locateTime 的逆映射并钳制越界序号/偏移', () => {
    const prefix = prefixOf([100, 66, 500]);
    assert.equal(timeAt(prefix, 0), 0);
    assert.equal(timeAt(prefix, 1), 100);
    assert.equal(timeAt(prefix, 2, 234), 400);
    assert.equal(timeAt(prefix, 2, 9999), 666);
    assert.equal(timeAt(prefix, -3), 0);
    assert.equal(timeAt(prefix, 50), 166);
    assert.equal(timeAt([0], 0), 0);
    for (const t of [0, 1, 99, 100, 300, 665]) {
        const loc = locateTime(prefix, t);
        assert.equal(timeAt(prefix, loc.playIdx, loc.inFrameMs), t);
    }
});

test('pointerRatio / ratioToTime 钳制到 [0,1] 与 [0,total]', () => {
    const rect = {left: 100, width: 200};
    assert.equal(pointerRatio(100, rect), 0);
    assert.equal(pointerRatio(200, rect), 0.5);
    assert.equal(pointerRatio(300, rect), 1);
    assert.equal(pointerRatio(50, rect), 0);
    assert.equal(pointerRatio(999, rect), 1);
    assert.equal(pointerRatio(150, {left: 0, width: 0}), 0);
    assert.equal(ratioToTime(0.5, 1000), 500);
    assert.equal(ratioToTime(-1, 1000), 0);
    assert.equal(ratioToTime(2, 1000), 1000);
    assert.equal(ratioToTime(0.5, 0), 0);
});

test('keyboardStep 钳制不循环；Home/End 到首尾；无关键返回 null', () => {
    assert.equal(keyboardStep(0, 5, 'ArrowLeft'), 0);
    assert.equal(keyboardStep(4, 5, 'ArrowRight'), 4);
    assert.equal(keyboardStep(2, 5, 'ArrowLeft'), 1);
    assert.equal(keyboardStep(2, 5, 'ArrowRight'), 3);
    assert.equal(keyboardStep(2, 5, 'Home'), 0);
    assert.equal(keyboardStep(2, 5, 'End'), 4);
    assert.equal(keyboardStep(2, 30, 'PageUp'), 12);
    assert.equal(keyboardStep(2, 30, 'PageDown'), 0);
    assert.equal(keyboardStep(2, 5, 'Space'), null);
    assert.equal(keyboardStep(0, 0, 'End'), null);
});

test('clampPreviewLeft 居中到指针并钳制容器两端', () => {
    assert.equal(clampPreviewLeft(100, 60, 400), 70);
    assert.equal(clampPreviewLeft(10, 60, 400), 0);
    assert.equal(clampPreviewLeft(395, 60, 400), 340);
    // 预览比容器宽：钳到 0
    assert.equal(clampPreviewLeft(50, 500, 400), 0);
});

test('formatTimecode 输出 m:ss.d', () => {
    assert.equal(formatTimecode(0), '0:00.0');
    assert.equal(formatTimecode(1234), '0:01.2');
    assert.equal(formatTimecode(61_950), '1:01.9');
    assert.equal(formatTimecode(-10), '0:00.0');
});

test('createLatestGen 只认最新代次，bump 使在途全部失效', () => {
    const gen = createLatestGen();
    const a = gen.next();
    const b = gen.next();
    assert.equal(gen.isCurrent(a), false);
    assert.equal(gen.isCurrent(b), true);
    gen.bump();
    assert.equal(gen.isCurrent(b), false);
    assert.equal(gen.current() > b, true);
});
