import {test} from 'node:test';
import assert from 'node:assert/strict';

// Node 环境最小 DOM 替身（brush-size.test.mjs 同款模式）。
globalThis.window = globalThis.window || {};
window.__blinkScreenMeta = {
    vx: 0, vy: 0,
    renderScaleX: 1, renderScaleY: 1,
    physicalDisplays: [{x: 0, y: 0, w: 100, h: 60, dpi: 96}],
};
// captureLineSnapshot 走 document.createElement('canvas')
globalThis.document = {
    createElement: () => ({width: 0, height: 0, getContext: () => makeCtx()}),
};

const {ss, TOOL_CAPS} = await import('./ss-state.js');
const annot = await import('./annotation-engine.js');
const draw = await import('./ss-draw.js');

/** 记录型 2D 上下文替身：属性直接赋值，方法按序记录。 */
function makeCtx() {
    const calls = [];
    return {
        calls,
        canvas: null,
        save: () => calls.push(['save']),
        restore: () => calls.push(['restore']),
        beginPath: () => calls.push(['beginPath']),
        closePath: () => calls.push(['closePath']),
        moveTo: (x, y) => calls.push(['moveTo', x, y]),
        lineTo: (x, y) => calls.push(['lineTo', x, y]),
        stroke: () => calls.push(['stroke']),
        fill: () => calls.push(['fill']),
        arc: (x, y, r) => calls.push(['arc', x, y, r]),
        setLineDash: (d) => calls.push(['setLineDash', d]),
        clearRect: () => calls.push(['clearRect']),
        drawImage: () => calls.push(['drawImage']),
        fillRect: () => calls.push(['fillRect']),
        strokeRect: () => calls.push(['strokeRect']),
        ellipse: () => calls.push(['ellipse']),
        fillText: () => calls.push(['fillText']),
        measureText: () => ({width: 10}),
    };
}

// ── 箭头几何（0.28.26 预览/成品共用）─────────────────────────────

test('drawArrowShape：实心三角头部单次 fill，虚线只作用箭杆', () => {
    const ctx = makeCtx();
    annot.drawArrowShape(ctx, {x: 0, y: 0}, {x: 100, y: 0}, {color: '#ff0000', width: 4, dashed: true});
    const strokes = ctx.calls.filter((c) => c[0] === 'stroke');
    const fills = ctx.calls.filter((c) => c[0] === 'fill');
    assert.equal(strokes.length, 1, '箭杆只有一次 stroke');
    assert.equal(fills.length, 1, '头部是一次 fill（实心三角，无两翼拼接）');
    const dashIdx = ctx.calls.findIndex((c) => c[0] === 'setLineDash' && Array.isArray(c[1]) && c[1].length === 2);
    const strokeIdx = ctx.calls.findIndex((c) => c[0] === 'stroke');
    const clearDashIdx = ctx.calls.findIndex((c) => c[0] === 'setLineDash' && Array.isArray(c[1]) && c[1].length === 0);
    const fillIdx = ctx.calls.findIndex((c) => c[0] === 'fill');
    assert.ok(dashIdx >= 0 && dashIdx < strokeIdx, '虚线在箭杆 stroke 前设置');
    assert.ok(clearDashIdx > strokeIdx && clearDashIdx < fillIdx, '头部 fill 前清除虚线');
    // 头部子路径：moveTo 尖端 + 两个 lineTo 两翼 + closePath
    const tip = ctx.calls.findIndex((c) => c[0] === 'moveTo' && c[1] === 100 && c[2] === 0);
    assert.ok(tip >= 0, '头部从尖端起笔');
    assert.equal(ctx.calls.filter((c, i) => i > tip && c[0] === 'lineTo').length, 2, '两翼各一条 lineTo');
    assert.ok(ctx.calls.findIndex((c) => c[0] === 'closePath') > tip, '头部闭合子路径');
});

test('drawArrowShape：头部按总长限幅，零长退化为实心圆点', () => {
    // 极短箭头：headLen 被限幅为总长，头部不越过起点反向
    const shortCtx = makeCtx();
    annot.drawArrowShape(shortCtx, {x: 5, y: 5}, {x: 8, y: 5}, {color: '#000', width: 4});
    const lineTos = shortCtx.calls.filter((c) => c[0] === 'lineTo');
    for (const c of lineTos) {
        assert.ok(c[1] >= 5, '头部几何不越过起点 X');
    }
    // 零长：arc 圆点
    const dotCtx = makeCtx();
    annot.drawArrowShape(dotCtx, {x: 3, y: 3}, {x: 3, y: 3}, {color: '#000', width: 4});
    assert.ok(dotCtx.calls.some((c) => c[0] === 'arc'), '零长退化为 arc 圆点');
    assert.ok(dotCtx.calls.some((c) => c[0] === 'fill'), '圆点为实心');
    assert.equal(dotCtx.calls.filter((c) => c[0] === 'stroke').length, 0, '零长不画箭杆');
});

test('executeCommand 的 arrow 与预览共用同一几何（实心头部）', () => {
    const ctx = makeCtx();
    const canvas = {width: 100, height: 100, getContext: () => ctx};
    annot.init(canvas);
    annot.reset(100, 100, null);
    annot.executeCommand({type: 'arrow', points: [{x: 0, y: 0}, {x: 60, y: 0}], color: '#123456', width: 4, style: 'solid'}, ctx);
    assert.equal(ctx.calls.filter((c) => c[0] === 'fill').length, 1, '最终命令同样走实心头部 fill');
    assert.equal(ctx.calls.filter((c) => c[0] === 'stroke').length, 1, '箭杆一次 stroke');
});

// ── Shift 连续直线状态 ──────────────────────────────────────────

test('Shift 连线：提交段独立撤销、零长不产生空历史、清理后不跨会话连接', () => {
    const ctx = makeCtx();
    const canvas = {width: 100, height: 100, getContext: () => ctx};
    annot.init(canvas);
    annot.reset(100, 100, null);
    annot.setTool('pencil');
    annot.setColor('#123456');
    annot.setStrokeWidth(7);
    annot.setStrokeStyle('dashed');

    // 无锚点提交 → null（第一次 Shift 点击只设起点）
    assert.equal(annot.commitLineSegment({x: 10, y: 10}), null);
    assert.equal(annot.getCommands().length, 0);

    annot.setLineAnchor({x: 10, y: 10});
    const seg1 = annot.commitLineSegment({x: 50, y: 10});
    assert.ok(seg1, '有锚点时提交一段');
    assert.equal(seg1.type, 'pencil');
    assert.deepEqual(seg1.points, [{x: 10, y: 10}, {x: 50, y: 10}]);
    assert.equal(seg1.color, '#123456');
    assert.equal(seg1.width, 7);
    assert.equal(seg1.style, 'dashed', '复用当前虚线样式');
    assert.deepEqual(annot.getLineAnchor(), {x: 50, y: 10}, '锚点更新为段终点');

    annot.commitLineSegment({x: 50, y: 40});
    assert.equal(annot.getCommands().length, 2, '连续折线每段独立命令');

    // 零长提交：不产生命令
    assert.equal(annot.commitLineSegment({x: 50, y: 40}), null);
    assert.equal(annot.getCommands().length, 2, '零长点不写入空命令');

    // 每段独立撤销
    assert.equal(annot.undo(), true);
    assert.equal(annot.getCommands().length, 1);
    assert.equal(annot.redo(), true);
    assert.equal(annot.getCommands().length, 2);
    assert.equal(annot.undo(), true);
    assert.equal(annot.undo(), true);
    assert.equal(annot.getCommands().length, 0);

    // clearLineState 后锚点消失
    annot.setLineAnchor({x: 1, y: 1});
    annot.clearLineState();
    assert.equal(annot.getLineAnchor(), null);
    assert.equal(annot.commitLineSegment({x: 2, y: 2}), null, '清理后不跨操作连接');

    // reset（重新选区/编辑源切换）清理锚点
    annot.setLineAnchor({x: 1, y: 1});
    annot.reset(100, 100, null);
    assert.equal(annot.getLineAnchor(), null);
});

test('Shift 连线预览：不入撤销栈，提交段后快照按版本重拍', () => {
    const ctx = makeCtx();
    ss.annotCanvas = {width: 100, height: 100, getContext: () => ctx};
    ss.annotCtx = ctx;
    ss.selCss = {x: 0, y: 0, w: 100, h: 100};
    const canvas = ss.annotCanvas;
    annot.init(canvas);
    annot.reset(100, 100, null);
    annot.setTool('pencil');

    ss._lineSnapshot = null;
    ss._lineSnapshotVer = -1;
    annot.setLineShiftHeld(true);
    annot.setLineAnchor({x: 5, y: 5});
    annot.setLinePreviewEnd({x: 40, y: 30});

    const verBefore = annot.getCommitVersion();
    draw.redrawAnnotLinePreview();
    assert.equal(annot.getCommands().length, 0, '预览不写入撤销栈');
    assert.equal(ss._lineSnapshotVer, verBefore, '首帧拍快照记录版本');
    assert.ok(ctx.calls.some((c) => c[0] === 'lineTo' && c[1] === 40 && c[2] === 30), '预览画到锚点→鼠标位置');

    // 提交一段后版本推进，预览重拍快照
    const seg = annot.commitLineSegment({x: 40, y: 30});
    assert.ok(seg);
    assert.ok(annot.getCommitVersion() > verBefore, '提交段推进版本');
    draw.redrawAnnotLinePreview();
    assert.equal(ss._lineSnapshotVer, annot.getCommitVersion(), '版本失效后重拍快照');

    // 清除预览恢复已提交内容，命令栈不变
    draw.clearLinePreview();
    assert.equal(annot.getCommands().length, 1, '清除预览不影响已提交段');
    annot.setLineShiftHeld(false);
    annot.clearLineState();
});

test('setLineShiftHeld：松开结束连线链及预览（锚点一并清理）', () => {
    annot.clearLineState();
    annot.setLineShiftHeld(true);
    assert.equal(annot.isLinePreviewActive(), false, '无锚点不激活预览');
    annot.setLineAnchor({x: 1, y: 1});
    assert.equal(annot.isLinePreviewActive(), true);
    annot.setLinePreviewEnd({x: 2, y: 2});
    annot.setLineShiftHeld(false);
    assert.equal(annot.getLinePreviewEnd(), null, '松开 Shift 清预览终点');
    assert.equal(annot.isLinePreviewActive(), false);
    assert.equal(annot.getLineAnchor(), null, '松开 Shift 结束当前连续连线，锚点不保留');
    assert.equal(annot.commitLineSegment({x: 2, y: 2}), null, '松开后不隔空续上旧链');
    annot.clearLineState();
});

// ── 线段工具（0.28.26 补充）─────────────────────────────────

test('线段工具 caps：走 stroke 宽度/虚线配置，无笔画光标圈', () => {
    assert.equal(TOOL_CAPS.line.points, 'box');
    assert.equal(TOOL_CAPS.line.widthCat, 'stroke', '线宽/虚线控件按 stroke 类生效');
    assert.equal(TOOL_CAPS.line.hasCursor, false);
    assert.equal(TOOL_CAPS.line.supportMode, false);
    assert.equal(TOOL_CAPS.line.modeGroup, null);
});

test('线段工具：isLinePending 真值表（锚点 + line 工具无需 Shift）', () => {
    annot.clearLineState();
    annot.setTool('pencil');
    annot.setLineAnchor({x: 1, y: 1});
    assert.equal(annot.isLinePending(), false, '画笔无 Shift 不算待定（维持 Shift 连线语义）');
    annot.setLineShiftHeld(true);
    assert.equal(annot.isLinePending(), true, '画笔 Shift 按住即待定');
    annot.setLineShiftHeld(false);          // 松开 Shift 清锚点（既有语义）
    annot.setLineAnchor({x: 1, y: 1});      // 重新设锚，模拟线段工具普通点击设起点
    annot.setTool('line');
    assert.equal(annot.isLinePending(), true, '线段工具有锚点即待定，无需修饰键');
    annot.clearLineState();
    assert.equal(annot.isLinePending(), false, '无锚点不算待定');
    annot.setTool('pencil');
});

test('线段工具：点两下提交两点 pencil 段，渲染/导出零改动', () => {
    const ctx = makeCtx();
    const canvas = {width: 100, height: 100, getContext: () => ctx};
    annot.init(canvas);
    annot.reset(100, 100, null);
    annot.setTool('line');
    annot.setColor('#00ff00');
    annot.setStrokeWidth(3);

    // 第一击设起点（index.js pointerdown 语义）后即待定
    annot.setLineAnchor({x: 8, y: 8});
    assert.equal(annot.isLinePending(), true);
    const seg = annot.commitLineSegment({x: 60, y: 20});
    assert.ok(seg, '第二击提交一段');
    assert.equal(seg.type, 'pencil', '复用 pencil 类型：两点即直线段');
    assert.deepEqual(seg.points, [{x: 8, y: 8}, {x: 60, y: 20}]);
    assert.equal(seg.color, '#00ff00');
    assert.equal(seg.width, 3);

    // index.js 非 Shift 提交后清锚点（普通模式各段独立，下一击重新设起点）
    annot.setLineAnchor(null);
    assert.equal(annot.getLineAnchor(), null);
    assert.equal(annot.isLinePending(), false);
    assert.equal(annot.undo(), true, '提交段独立可撤销');
    annot.setTool('pencil');
});

test('setLineShiftHeld：未持有时不误清线段工具待定锚点（补充回归）', () => {
    annot.clearLineState();
    annot.setTool('line');
    annot.setLineAnchor({x: 3, y: 4});
    assert.equal(annot.setLineShiftHeld(false), false, '未持有时返回 changed=false');
    assert.deepEqual(annot.getLineAnchor(), {x: 3, y: 4}, '无关的 Shift 起落不吃掉待定锚点');
    assert.equal(annot.isLinePending(), true, '待定状态不受影响');
    // Shift 真实起落（keydown→keyup）仍结束链条
    annot.setLineShiftHeld(true);
    assert.equal(annot.setLineShiftHeld(false), true);
    assert.equal(annot.getLineAnchor(), null, '真实松开 Shift 清锚点');
    annot.clearLineState();
    annot.setTool('pencil');
});
