//! 指针输入工具测试（0.23.15）。
//!
//! 覆盖交付要求的"pointer capture 的开始、结束和取消路径可测试"，以及
//! "合并采样点只取最后一个"与"正常提交后的 lostpointercapture 不得二次取消"。

import assert from 'node:assert/strict';

import {
    capturePointer,
    hasActiveDragInteraction,
    latestPointerSample,
    pointerPoint,
    releasePointer,
} from './ss-pointer.js';

// ── latestPointerSample：只取最后一个采样点 ──────────────────────────────

{
    const plain = {offsetX: 1, offsetY: 2, shiftKey: false, pointerId: 7};
    assert.equal(latestPointerSample(plain), plain, '无 getCoalescedEvents 时返回原事件');

    const samples = [
        {offsetX: 10, offsetY: 10},
        {offsetX: 20, offsetY: 20},
        {offsetX: 33, offsetY: 44},
    ];
    const coalesced = {...plain, getCoalescedEvents: () => samples};
    assert.equal(latestPointerSample(coalesced), samples[2], '有合并采样时只取最后一个');

    const empty = {...plain, getCoalescedEvents: () => []};
    assert.equal(latestPointerSample(empty), empty, '空采样列表退回原事件');

    const throwing = {...plain, getCoalescedEvents: () => { throw new Error('not implemented'); }};
    assert.equal(latestPointerSample(throwing), throwing, '调用抛错时退回原事件');

    const nullish = {...plain, getCoalescedEvents: () => null};
    assert.equal(latestPointerSample(nullish), nullish, '返回 null 时退回原事件');

    assert.equal(latestPointerSample(undefined), undefined, '空事件安全');
    console.log('✓ latestPointerSample');
}

// ── pointerPoint：采样点 + 外层修饰键 ────────────────────────────────────

{
    const outer = {
        offsetX: 5, offsetY: 6, shiftKey: true, pointerId: 3,
        getCoalescedEvents: () => [
            {offsetX: 100, offsetY: 100, shiftKey: false},
            {offsetX: 200, offsetY: 210, shiftKey: false},
        ],
    };
    const point = pointerPoint(outer);
    assert.equal(point.offsetX, 200, '坐标取最后一个采样点');
    assert.equal(point.offsetY, 210, '坐标取最后一个采样点');
    assert.equal(point.shiftKey, true, '修饰键以外层事件为准（采样点不覆盖）');
    assert.equal(point.pointerId, 3, 'pointerId 透传');

    const simple = pointerPoint({offsetX: 1, offsetY: 2, shiftKey: false, pointerId: 9});
    assert.deepEqual(
        simple,
        {offsetX: 1, offsetY: 2, shiftKey: false, pointerId: 9},
        '无合并采样时原样归一',
    );
    console.log('✓ pointerPoint');
}

// ── capturePointer / releasePointer：开始与结束路径 ──────────────────────

{
    const calls = [];
    const target = {
        captured: new Set(),
        setPointerCapture(id) {
            calls.push(['set', id]);
            this.captured.add(id);
        },
        hasPointerCapture(id) {
            return this.captured.has(id);
        },
        releasePointerCapture(id) {
            calls.push(['release', id]);
            this.captured.delete(id);
        },
    };

    assert.equal(capturePointer(target, {pointerId: 4}), true, '起 capture 成功');
    assert.deepEqual(calls, [['set', 4]], 'capture 使用 e.pointerId');
    assert.equal(target.captured.has(4), true, 'capture 已持有');

    assert.equal(releasePointer(target, {pointerId: 4}), true, '释放自己持有的 capture');
    assert.deepEqual(calls, [['set', 4], ['release', 4]], '释放路径正确');
    assert.equal(target.captured.has(4), false, 'capture 已释放');

    // 幂等：未持有时不重复释放（避免误释放其它元素的 capture）
    assert.equal(releasePointer(target, {pointerId: 4}), false, '未持有时跳过释放');
    assert.deepEqual(calls, [['set', 4], ['release', 4]], '未持有时不产生额外调用');

    // pointerId 缺失 / 目标不支持：安全降级，不抛错
    assert.equal(capturePointer(target, {pointerId: undefined}), false, 'pointerId 缺失不起 capture');
    assert.equal(capturePointer(target, {}), false, '无 pointerId 字段不起 capture');
    assert.equal(capturePointer(null, {pointerId: 1}), false, '目标缺失安全');
    assert.equal(capturePointer({}, {pointerId: 1}), false, '目标无 setPointerCapture 安全');
    assert.equal(releasePointer(null, {pointerId: 1}), false, '释放时目标缺失安全');
    assert.equal(releasePointer({}, {pointerId: 1}), false, '目标无 releasePointerCapture 安全');

    // 抛错被吞掉：交互不得因为 capture 失败而中断
    const throwing = {
        setPointerCapture() { throw new Error('InvalidPointerId'); },
        releasePointerCapture() { throw new Error('InvalidPointerId'); },
        hasPointerCapture() { throw new Error('InvalidPointerId'); },
    };
    assert.equal(capturePointer(throwing, {pointerId: 1}), false, 'setPointerCapture 抛错被吞');
    assert.equal(releasePointer(throwing, {pointerId: 1}), false, 'releasePointerCapture 抛错被吞');

    // 只有 releasePointerCapture 的环境（无 hasPointerCapture）：仍然释放
    const legacy = {released: [], releasePointerCapture(id) { this.released.push(id); }};
    assert.equal(releasePointer(legacy, {pointerId: 2}), true, '无 hasPointerCapture 时直接释放');
    assert.deepEqual(legacy.released, [2], '释放调用到位');
    console.log('✓ capture / release 路径');
}

// ── hasActiveDragInteraction：取消路径的判定（含正常提交后的假信号） ──────

{
    // 正常 pointerup 提交完成后浏览器仍会派发 lostpointercapture：
    // 此时三个状态都已清空，必须判为"不需要取消"，否则会把刚提交的选区二次取消
    assert.equal(
        hasActiveDragInteraction({selectionInteraction: null, isDragging: false, pendingSnap: null}),
        false,
        '已提交（状态全空）不得触发取消',
    );
    assert.equal(
        hasActiveDragInteraction({selectionInteraction: {kind: 'move'}, isDragging: false, pendingSnap: null}),
        true,
        '选区移动/缩放进行中 → 取消',
    );
    assert.equal(
        hasActiveDragInteraction({selectionInteraction: null, isDragging: true, pendingSnap: null}),
        true,
        '新建拖选进行中 → 取消',
    );
    assert.equal(
        hasActiveDragInteraction({selectionInteraction: null, isDragging: false, pendingSnap: {winRect: {}}}),
        true,
        'pending-snap 候选等待中 → 取消',
    );
    assert.equal(
        hasActiveDragInteraction({}),
        false,
        '字段缺失按无交互处理',
    );
    console.log('✓ 取消路径判定');
}

console.log('\nss-pointer tests all passed');
