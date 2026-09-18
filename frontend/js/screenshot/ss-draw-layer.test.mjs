import {test} from 'node:test';
import assert from 'node:assert/strict';

// ss-draw → ss-display / ss-live-selection 需要 window；Node 环境先补上。
globalThis.window = globalThis.window || {};
window.__blinkScreenMeta = {
    vx: 0, vy: 0,
    renderScaleX: 1, renderScaleY: 1,
    physicalDisplays: [{x: 0, y: 0, w: 100, h: 60, dpi: 96}],
};

const {ss} = await import('./ss-state.js');
const drawMod = await import('./ss-draw.js');
const {drawDimmed, drawFinalSelection} = drawMod;
const annot = await import('./annotation-engine.js');

/** 最小元素替身（只需要 style + classList） */
function fakeEl() {
    const classes = new Set();
    return {
        style: {},
        classList: {
            add: (c) => classes.add(c),
            remove: (c) => classes.delete(c),
            contains: (c) => classes.has(c),
        },
        _classes: classes,
    };
}

/** 装好 canvas / 实时层替身，返回操作计数器 */
function installLayers() {
    const ops = {baseClear: 0, baseDraw: 0, clear: 0, fill: 0, stroke: 0};
    ss.canvas = {width: 100, height: 60};
    ss.interactionCanvas = {width: 100, height: 60};
    ss.screenshot = {kind: 'source-canvas'};
    ss.screenshotOffscreen = ss.screenshot;
    ss.ctx = {
        clearRect: () => ops.baseClear++,
        drawImage: () => ops.baseDraw++,
        fillRect: () => ops.fill++,
    };
    ss.interactionCtx = {
        fillStyle: '',
        strokeStyle: '',
        lineWidth: 0,
        clearRect: () => ops.clear++,
        fillRect: () => ops.fill++,
        strokeRect: () => ops.stroke++,
    };
    ss.liveSelectionEl = fakeEl();
    ss.liveMaskTop = fakeEl();
    ss.liveMaskBottom = fakeEl();
    ss.liveMaskLeft = fakeEl();
    ss.liveMaskRight = fakeEl();
    ss.liveBorderEl = fakeEl();
    ss.sizeHint = fakeEl();
    ss.liveSelectionEl._classes.add('hidden');
    return ops;
}

test('drawDimmed keeps the screenshot layer original and masks only the interaction layer', () => {
    const baseCalls = [];
    const interactionCalls = [];
    const source = {kind: 'source-canvas'};

    ss.canvas = {width: 100, height: 60};
    ss.interactionCanvas = {width: 100, height: 60};
    ss.screenshot = source;
    ss.screenshotOffscreen = source;
    ss.ctx = {
        clearRect: (...args) => baseCalls.push(['clearRect', ...args]),
        drawImage: (...args) => baseCalls.push(['drawImage', ...args]),
        fillRect: (...args) => baseCalls.push(['fillRect', ...args]),
    };
    ss.interactionCtx = {
        fillStyle: '',
        clearRect: (...args) => interactionCalls.push(['clearRect', ...args]),
        fillRect: (...args) => interactionCalls.push(['fillRect', ...args]),
    };

    drawDimmed();

    assert.deepEqual(baseCalls, [
        ['clearRect', 0, 0, 100, 60],
        ['drawImage', source, 0, 0],
    ]);
    assert.deepEqual(interactionCalls, [
        ['clearRect', 0, 0, 100, 60],
        ['fillRect', 0, 0, 100, 60],
    ]);
    assert.equal(ss.interactionCtx.fillStyle, 'rgba(0, 0, 0, 0.45)');
});

test('0.23.15：drawDimmed / drawFinalSelection 都必须先隐藏实时 DOM 层', async () => {
    // 提交（canvas 绘制）与实时层隐藏落在同一个 JS task 内，是"松手不闪白、
    // 不出现双边框"的实现方式——因此两个 canvas 提交入口都必须内联这一步。
    const live = await import('./ss-live-selection.js');

    installLayers();
    ss.isAnnotating = false;
    live.updateLiveSelection({x: 10, y: 10, w: 40, h: 30});
    assert.equal(live.isLiveSelectionActive(), true, '拖动中实时层处于激活态');
    drawDimmed();
    assert.equal(live.isLiveSelectionActive(), false, 'drawDimmed 退出激活态');
    assert.ok(ss.liveSelectionEl._classes.has('hidden'), 'drawDimmed 隐藏实时层');

    installLayers();
    ss.isAnnotating = true;
    ss.selCss = {x: 10, y: 10, w: 40, h: 30};
    live.updateLiveSelection({x: 12, y: 12, w: 40, h: 30});
    assert.equal(live.isLiveSelectionActive(), true, '提交前实时层已激活');
    drawFinalSelection();
    assert.equal(live.isLiveSelectionActive(), false, 'drawFinalSelection 退出激活态');
    assert.ok(ss.liveSelectionEl._classes.has('hidden'), 'drawFinalSelection 隐藏实时层');
    assert.equal(ss.sizeHint._classes.has('hidden'), false, '尺寸提示随之补显');
    live.resetLiveSelection();
});

test('0.23.15：drawFinalSelection 是单次提交——一次 clear + 四块遮罩 + 一条边框', () => {
    const ops = installLayers();
    ss.isAnnotating = true;
    ss.selCss = {x: 10, y: 8, w: 40, h: 30};
    annot.setTool('select');

    drawFinalSelection();

    assert.equal(ops.clear, 1, '恰好一次全层 clearRect');
    assert.equal(ops.fill, 4 + 8, '四块遮罩 + 八个手柄填充');
    assert.equal(ops.stroke, 1 + 8, '一条选区边框 + 八个手柄描边');
    assert.equal(ops.baseDraw, 0, '不重绘静态底图');

    // 非选取工具不画手柄，仍是"一次 clear + 四块遮罩 + 一条边框"
    const ops2 = installLayers();
    annot.setTool('rect');
    drawFinalSelection();
    assert.equal(ops2.clear, 1);
    assert.equal(ops2.fill, 4);
    assert.equal(ops2.stroke, 1);
    annot.setTool('select');
});

test('0.23.15：ss-draw 不再暴露按帧调用的选区绘制入口', () => {
    // 保留旧名会让"两个独立实时队列"重新长出来；这里把删除结果钉成契约。
    for (const name of [
        'drawSelection',
        'scheduleDrawSelection',
        'cancelDrawSelectionRaf',
        'scheduleDrawFinalSelection',
        'cancelDrawFinalSelectionRaf',
    ]) {
        assert.equal(drawMod[name], undefined, `${name} 必须已删除（拖动改走 ss-live-selection）`);
    }
    assert.equal(typeof drawMod.drawFinalSelection, 'function', '保留单次提交入口');
    assert.equal(typeof drawMod.drawDimmed, 'function', '保留整屏暗罩入口');
});
