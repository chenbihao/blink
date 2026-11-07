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
const {drawDimmed, drawFinalSelection, syncInteractionCanvasSize} = drawMod;
const annot = await import('./annotation-engine.js');

test('长图编辑释放未使用的交互遮罩，回到普通截图时恢复尺寸', () => {
    ss.canvas = {width: 930, height: 17935};
    ss.interactionCanvas = {width: 1920, height: 1080};
    ss.editorSession.beginCanvasSource('long-screenshot', ss.canvas);
    syncInteractionCanvasSize();
    assert.deepEqual([ss.interactionCanvas.width, ss.interactionCanvas.height], [0, 0]);
    ss.editorSession.beginScreenshotSelection();
    ss.canvas = {width: 1920, height: 1080};
    syncInteractionCanvasSize();
    assert.deepEqual([ss.interactionCanvas.width, ss.interactionCanvas.height], [1920, 1080]);
});

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

// ── 荧光笔正片叠底实时预览同层合并（0.28.26 补充）─────────────────────────

/** 记录型 2D 上下文（annotation-line-arrow.test.mjs 同款 + globalAlpha 追踪）。 */
function makeRecordingCtx() {
    const calls = [];
    const ctx = {calls, _alpha: 1, canvas: null};
    Object.defineProperty(ctx, 'globalAlpha', {
        get: () => ctx._alpha,
        set: (v) => {
            ctx._alpha = v;
            calls.push(['globalAlpha', v]);
        },
    });
    for (const m of ['save', 'restore', 'beginPath', 'closePath', 'stroke', 'fill', 'clearRect', 'drawImage', 'fillRect', 'strokeRect']) {
        ctx[m] = (...args) => calls.push([m, ...args]);
    }
    ctx.moveTo = (x, y) => calls.push(['moveTo', x, y]);
    ctx.lineTo = (x, y) => calls.push(['lineTo', x, y]);
    ctx.arc = (x, y, r) => calls.push(['arc', x, y, r]);
    ctx.setLineDash = (d) => calls.push(['setLineDash', d]);
    ctx.fillText = () => calls.push(['fillText']);
    ctx.measureText = () => ({width: 10});
    return ctx;
}

/** 可设尺寸的假 canvas：每个实例独立 recording ctx。 */
function makeFakeCanvas(w, h) {
    const ctx = makeRecordingCtx();
    return {width: w, height: h, getContext: () => ctx, _ctx: ctx};
}

test('荧光笔正片叠底预览：拆「底图+按色层」，实时笔迹并入同色层统一 0.55 合成', () => {
    // begin/composite 经引擎 renderCommandsTo/acquireCanvas 需要 document.createElement
    globalThis.document = {createElement: () => makeFakeCanvas(0, 0)};

    const canvas = makeFakeCanvas(100, 60);
    annot.init(canvas);
    annot.reset(100, 60, null);
    annot.setTool('highlight-multiply');
    annot.setColor('#ff0000');
    annot.startDraw(0, 0);
    annot.moveDraw(30, 0);
    annot.endDraw(30, 0);
    annot.setColor('#ffd700');
    annot.startDraw(0, 10);
    annot.moveDraw(20, 10);
    annot.endDraw(20, 10);
    assert.equal(annot.getCommands().length, 2, '前置：两笔已提交（红/黄各一）');

    ss.annotCanvas = makeFakeCanvas(100, 60);
    ss.annotCtx = makeRecordingCtx();
    ss._hlLivePreview = null;

    assert.equal(drawMod.beginHighlightLivePreview(), true, '分层构建成功');
    const hl = ss._hlLivePreview;
    assert.ok(hl, '预览分层已挂到 ss');
    assert.deepEqual(hl.order, ['#ff0000', '#ffd700'], '已提交高亮按色分组且按出现顺序');
    assert.ok(hl.layers.get('#ff0000')._ctx.calls.some((c) => c[0] === 'stroke'), '红层收已提交红笔');
    assert.equal(
        hl.base._ctx.calls.filter((c) => c[0] === 'drawImage').length, 0,
        '底图不含 brush 模式高亮层（避免预览双重合成）',
    );

    // 模拟进行中红笔（引擎 currentPoints）
    annot.setColor('#ff0000');
    annot.startDraw(10, 20);
    annot.moveDraw(50, 20);

    const comp = ss.annotCtx;
    assert.equal(drawMod.compositeHighlightLivePreview(), true, '合成接管本帧');
    assert.ok(comp.calls.some((c) => c[0] === 'clearRect'), '合成前清画布');
    // 两个颜色层各一次 0.55 drawImage（红=merged 含实时笔迹；黄=已提交层）
    const alphaHits = comp.calls.filter((c) => c[0] === 'globalAlpha' && c[1] === 0.55);
    assert.equal(alphaHits.length, 2, '各色层一次性 0.55 贴回（同色重叠不加深）');
    assert.equal(comp.calls.filter((c) => c[0] === 'drawImage').length, 3, '底图 1 次 + 色层 2 次');
    assert.ok(hl.live._ctx.calls.some((c) => c[0] === 'stroke'), '实时笔迹画进 live 层（满 alpha）');
    assert.ok(hl.merged._ctx.calls.some((c) => c[0] === 'drawImage'), 'live 并入同色 merged 层');
    assert.equal(annot.getCommands().length, 2, '预览不写撤销栈');

    drawMod.endHighlightLivePreview();
    assert.equal(ss._hlLivePreview, null, '释放分层');
    annot.setTool('pencil');
    annot.reset(100, 60, null);
});
