//! GIF 编辑器纯逻辑测试（node --test）。

import test from 'node:test';
import assert from 'node:assert/strict';
import {nextPlayStep, frameState, srcToPlayIndex, playToSrc, commands, formatFrameDur, formatSeconds} from './timeline-view.js';

test('nextPlayStep 循环回到首帧', () => {
    const frames = [{srcIdx: 0, durMs: 100}, {srcIdx: 1, durMs: 66}];
    // 时长跟游标走：返回的是**下一帧**的停留时长（新帧按自己的时长调度）
    assert.deepEqual(nextPlayStep(frames, 0), {cursor: 1, durMs: 66, atEnd: false});
    assert.deepEqual(nextPlayStep(frames, 1), {cursor: 0, durMs: 100, atEnd: false});
});

test('nextPlayStep 长帧停顿不再错位到后一帧', () => {
    // 帧 A=500ms、B=50ms：推进到 B 时调度 50ms（旧实现错用 A 的 500ms，
    // 把长帧的停顿落在 B 身上——正是"长帧卡顿但高亮在下一帧"的根因）
    const frames = [{srcIdx: 0, durMs: 500}, {srcIdx: 1, durMs: 50}];
    assert.equal(nextPlayStep(frames, 0).durMs, 50);
    assert.equal(nextPlayStep(frames, 1).durMs, 500);
});

test('nextPlayStep 空序列与越界游标', () => {
    assert.deepEqual(nextPlayStep([], 0), {cursor: 0, durMs: 0, atEnd: true});
    // 越界游标被夹回（例如删除帧后游标超限）
    assert.deepEqual(nextPlayStep([{srcIdx: 0, durMs: 50}], 9), {cursor: 0, durMs: 50, atEnd: false});
});

test('frameState 标注修剪/删除/停留', () => {
    const view = {
        trimStart: 2,
        trimEnd: 5,
        deleted: [4],
        holds: {5: 1200},
        frames: [{srcIdx: 2}, {srcIdx: 3}, {srcIdx: 5}],
    };
    assert.deepEqual(frameState(view, 3), {inTrim: true, deleted: false, held: false, visible: true});
    assert.deepEqual(frameState(view, 4), {inTrim: true, deleted: true, held: false, visible: false});
    assert.deepEqual(frameState(view, 5), {inTrim: true, deleted: false, held: true, visible: true});
    assert.deepEqual(frameState(view, 1), {inTrim: false, deleted: false, held: false, visible: false});
});

test('src/play 双向映射', () => {
    const view = {frames: [{srcIdx: 2}, {srcIdx: 3}, {srcIdx: 5}]};
    assert.equal(srcToPlayIndex(view, 3), 1);
    assert.equal(srcToPlayIndex(view, 4), -1, '已删/不在序列的帧返回 -1');
    assert.equal(playToSrc(view, 2), 5);
    assert.equal(playToSrc(view, 9), null);
});

test('commands 构造与后端 serde tag 一致', () => {
    assert.deepEqual(commands.trimStart(3), {type: 'trim_start', frame: 3});
    assert.deepEqual(commands.trimEnd(5), {type: 'trim_end', frame: 5});
    assert.deepEqual(commands.deleteFrame(4), {type: 'delete_frame', frame: 4});
    assert.deepEqual(commands.setSpeed(1.5), {type: 'set_speed', factor: 1.5});
    assert.deepEqual(commands.setHold(5, 1000), {type: 'set_hold', frame: 5, ms: 1000});
    assert.deepEqual(commands.setLoop(3), {type: 'set_loop', count: 3});
    assert.deepEqual(commands.setLoop(null), {type: 'set_loop', count: null});
    assert.deepEqual(commands.undo(), {type: 'undo'});
    assert.deepEqual(commands.redo(), {type: 'redo'});
});

test('时长格式化', () => {
    assert.equal(formatSeconds(1200), '1.2s');
    assert.equal(formatSeconds(12500), '12.5s');
    assert.equal(formatFrameDur(66), '66ms');
    assert.equal(formatFrameDur(500), '500ms');
    assert.equal(formatFrameDur(1500), '1.5s');
});
