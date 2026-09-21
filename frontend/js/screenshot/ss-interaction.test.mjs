import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';

// ss-interaction.js 导入 ../shared/api.js → tauri.js，后者在模块加载时读 window.__TAURI__。
// Node 测试环境没有 window，需要先 mock。
globalThis.window = globalThis.window || {
    __TAURI__: {internal: {invoke: () => Promise.resolve()}},
    alert() {
    }, confirm() {
    }, prompt() {
    },
};

const {getSelectionHandle} = await import('./ss-interaction.js');
const {ss} = await import('./ss-state.js');
const {
    magnifierSampleRegion,
    screenPointToBitmap,
    shouldStartFreeSelection,
} = await import('./ss-selection-geometry.js');

assert.equal(shouldStartFreeSelection(10, 10, 12, 12), false, '阈值内保持 pending-snap');
assert.equal(shouldStartFreeSelection(10, 10, 13, 10), true, '达到 3 CSS px 转自由框选');

assert.deepEqual(
    magnifierSampleRegion(0, 0, 100, 100),
    {readX: 0, readY: 0, gridOffsetX: 8, gridOffsetY: 4, width: 8, height: 5},
    '左上角采样应保留中心格偏移',
);
assert.deepEqual(
    magnifierSampleRegion(99, 99, 100, 100),
    {readX: 91, readY: 95, gridOffsetX: 0, gridOffsetY: 0, width: 9, height: 5},
    '右下角采样应裁剪读取尺寸',
);

const dpi200Meta = {vx: -1920, vy: 720, renderScaleX: 2, renderScaleY: 2};
assert.deepEqual(screenPointToBitmap(-1820, 820, dpi200Meta), {x: 100, y: 100});
assert.deepEqual(screenPointToBitmap(-1819, 821, dpi200Meta), {x: 101, y: 101});
assert.deepEqual(screenPointToBitmap(-1818, 822, dpi200Meta), {x: 102, y: 102},
    '物理光标在 200% 屏幕上移动 1px 时，bitmap 采样也必须只移动 1px');

const rect = {x: 100, y: 100, w: 200, h: 120};
assert.equal(getSelectionHandle(100, 100, rect), 'nw');
assert.equal(getSelectionHandle(300, 220, rect), 'se');
assert.equal(getSelectionHandle(200, 100, rect), 'n');
assert.equal(getSelectionHandle(200, 160, rect), null);

// ── 0.23.15：computeInteractionRect（move / resize / Shift 1:1） ──────────
//
// 交付要求"鼠标松开时先用 release 事件的最新坐标同步计算最终矩形，不能依赖最后
// 一次 rAF 已经运行"。pointermove 与 release 共用本函数，因此这里的断言就是
// 两条路径不会漂移的守门条件。

const {computeInteractionRect, beginSelectionInteraction, updateSelectionInteraction, finishSelectionInteraction} =
    await import('./ss-interaction.js');

{
    const MON = {x: 0, y: 0, w: 1920, h: 1080};
    const moveInteraction = {
        kind: 'move', handle: null,
        startX: 500, startY: 400,
        original: {x: 400, y: 300, w: 200, h: 150},
        monitor: MON,
    };
    assert.deepEqual(
        computeInteractionRect(moveInteraction, 520, 430, false, null),
        {x: 420, y: 330, w: 200, h: 150},
        'move: 平移量按指针位移，宽高不变',
    );
    assert.deepEqual(
        computeInteractionRect(moveInteraction, -1000, -1000, false, null),
        {x: 0, y: 0, w: 200, h: 150},
        'move: 左上越界钳制到屏内',
    );
    assert.deepEqual(
        computeInteractionRect(moveInteraction, 5000, 5000, false, null),
        {x: 1720, y: 930, w: 200, h: 150},
        'move: 右下越界钳制到屏内',
    );

    const base = {x: 400, y: 300, w: 200, h: 150};
    const resize = (handle) => ({
        kind: 'resize', handle, startX: 600, startY: 450, original: base, monitor: MON,
    });

    assert.deepEqual(
        computeInteractionRect(resize('se'), 700, 500, false, null),
        {x: 400, y: 300, w: 300, h: 200},
        'resize se: 右下角外拖',
    );
    assert.deepEqual(
        computeInteractionRect(resize('nw'), 350, 250, false, null),
        {x: 350, y: 250, w: 250, h: 200},
        'resize nw: 左上角外拖',
    );
    assert.deepEqual(
        computeInteractionRect(resize('se'), 100, 100, false, null),
        {x: 400, y: 300, w: 5, h: 5},
        'resize se: 反向内拖钳制到最小边长',
    );
    assert.deepEqual(
        computeInteractionRect(resize('n'), 600, 280, false, null),
        {x: 400, y: 280, w: 200, h: 170},
        'resize n: 只改上边',
    );
    assert.deepEqual(
        computeInteractionRect(resize('w'), 350, 450, false, null),
        {x: 350, y: 300, w: 250, h: 150},
        'resize w: 只改左边',
    );
    // 八向 handle 全覆盖：每个方向都必须产出合法矩形（w/h ≥ 最小边长）
    for (const handle of ['n', 's', 'e', 'w', 'nw', 'ne', 'sw', 'se']) {
        const r = computeInteractionRect(resize(handle), 900, 700, false, null);
        assert.ok(r.w >= 5 && r.h >= 5, `resize ${handle}: 宽高不小于最小边长`);
        assert.ok(r.y >= MON.y && r.y + r.h <= MON.y + MON.h, `resize ${handle}: 纵向不越界`);
        assert.ok(r.x >= MON.x && r.x + r.w <= MON.x + MON.w, `resize ${handle}: 横向不越界`);
    }

    // Shift 1:1：宽高必须相等（回归项——applySquareResize 曾被调用但未导入，
    // 该路径会抛 ReferenceError，Shift + 缩放实际不可用）
    const env1x = {meta: {vx: 0, vy: 0, renderScaleX: 1, renderScaleY: 1}, canvasW: 1920, canvasH: 1080};
    const sq1 = computeInteractionRect(resize('se'), 700, 350, true, env1x);
    assert.equal(sq1.w, sq1.h, 'Shift resize: 1× 渲染下宽高相等');
    assert.deepEqual(sq1, {x: 400, y: 300, w: 300, h: 300}, 'Shift resize: 取较大位移作边长');
    // 200% 缩放（renderScale=2）下同样必须 1:1，且换算回 CSS 不产生比例错误
    const env2x = {meta: {vx: 0, vy: 0, renderScaleX: 2, renderScaleY: 2}, canvasW: 3840, canvasH: 2160};
    const sq2 = computeInteractionRect(resize('se'), 650, 340, true, env2x);
    assert.equal(sq2.w, sq2.h, 'Shift resize: 200% 渲染下宽高相等');
    // 未按 Shift 时不约束
    assert.deepEqual(
        computeInteractionRect(resize('se'), 700, 350, false, env1x),
        {x: 400, y: 300, w: 300, h: 50},
        '未按 Shift: 不施加等边约束',
    );

    console.log('✓ computeInteractionRect tests passed');
}

// ── 0.23.15：release 坐标优先（move / new 两条路径） ──────────────────────

{
    globalThis.window.__blinkScreenMeta = {
        vx: 0, vy: 0,
        renderScaleX: 1, renderScaleY: 1,
        physicalDisplays: [{x: 0, y: 0, w: 1920, h: 1080, dpi: 96}],
    };
    window.innerWidth = 1920;
    window.innerHeight = 1080;

    // 实时层替身：本组只验证几何，DOM 写入不参与断言
    const stubEl = () => ({style: {}, classList: {add() {}, remove() {}, contains: () => false}});
    ss.liveSelectionEl = stubEl();
    ss.liveMaskTop = stubEl();
    ss.liveMaskBottom = stubEl();
    ss.liveMaskLeft = stubEl();
    ss.liveMaskRight = stubEl();
    ss.liveBorderEl = stubEl();
    ss.sizeHint = null;
    ss.interactionCanvas = null;
    ss.interactionCtx = null;
    ss.canvas = {style: {}};

    const committed = [];
    ss._enterAnnotationMode = (r) => committed.push(r);
    ss._exitAnnotationMode = () => committed.push('exit');

    // move：最后一帧 pointermove 落在 (200,400)，release 落在 (250,500)
    ss.isAnnotating = true;
    ss.isDragging = false;
    ss.selCss = {x: 100, y: 100, w: 200, h: 150};
    beginSelectionInteraction('move', {offsetX: 150, offsetY: 150});
    updateSelectionInteraction({offsetX: 200, offsetY: 400, shiftKey: false});
    assert.deepEqual(ss.selCss, {x: 150, y: 350, w: 200, h: 150}, '拖动中按 pointermove 采样');
    assert.equal(finishSelectionInteraction({offsetX: 250, offsetY: 500, shiftKey: false}), true, 'move 交互被消费');
    assert.deepEqual(
        committed[committed.length - 1],
        {x: 200, y: 450, w: 200, h: 150},
        'release 坐标重算最终矩形（不依赖最后一帧 rAF 是否已运行）',
    );
    assert.equal(ss.selectionInteraction, null, '交互状态已清空');

    // new：同样以 release 坐标为准
    // （beginSelectionInteraction 有 selCss 入口守卫，'new' 分支沿用该守卫）
    ss.isAnnotating = false;
    ss.isDragging = false;
    ss.selCss = {x: 0, y: 0, w: 0, h: 0};
    beginSelectionInteraction('new', {offsetX: 300, offsetY: 300});
    updateSelectionInteraction({offsetX: 400, offsetY: 350, shiftKey: false});
    assert.equal(finishSelectionInteraction({offsetX: 420, offsetY: 380, shiftKey: false}), true, 'new 交互被消费');
    assert.deepEqual(
        committed[committed.length - 1],
        {x: 300, y: 300, w: 120, h: 80},
        'new: release 坐标决定最终矩形',
    );

    // Shift 正方形在 release 路径同样生效
    ss.isAnnotating = false;
    ss.isDragging = false;
    ss.selCss = {x: 0, y: 0, w: 0, h: 0};
    beginSelectionInteraction('new', {offsetX: 0, offsetY: 0});
    updateSelectionInteraction({offsetX: 50, offsetY: 20, shiftKey: true});
    finishSelectionInteraction({offsetX: 100, offsetY: 30, shiftKey: true});
    const last = committed[committed.length - 1];
    assert.deepEqual(last, {x: 0, y: 0, w: 100, h: 100}, 'new + Shift: release 路径仍是 1:1');

    // 未达 3px 阈值：不激活、不提交（单击语义保持）
    ss.isAnnotating = true;
    ss.selCss = {x: 10, y: 10, w: 400, h: 300};
    const before = committed.length;
    beginSelectionInteraction('move', {offsetX: 200, offsetY: 200});
    updateSelectionInteraction({offsetX: 202, offsetY: 201, shiftKey: false});
    assert.deepEqual(ss.selCss, {x: 10, y: 10, w: 400, h: 300}, '阈值内不改动选区');
    finishSelectionInteraction({offsetX: 202, offsetY: 201, shiftKey: false});
    assert.equal(committed.length, before, '阈值内松手不进入标注模式');
    assert.equal(ss.selectionInteraction, null, '阈值内松手仍结束交互');

    // 选区过小（< MIN_SELECTION_SIZE）：走退出标注模式
    ss.isAnnotating = false;
    ss.isDragging = false;
    ss.selCss = {x: 0, y: 0, w: 0, h: 0};
    beginSelectionInteraction('new', {offsetX: 50, offsetY: 50});
    updateSelectionInteraction({offsetX: 60, offsetY: 60, shiftKey: false});
    const beforeSmall = committed.length;
    finishSelectionInteraction({offsetX: 52, offsetY: 52, shiftKey: false});
    assert.equal(committed.length, beforeSmall + 1, '过小选区只触发一次收口');
    assert.equal(committed[committed.length - 1], 'exit', '过小选区退出标注模式而非进入');

    console.log('✓ release 坐标优先 / 阈值语义');
}

// 0.23.18：取色器 precision hint 的 DPI 视觉补偿（源码断言——updatePrecisionHint 未导出）
{
    const src = readFileSync(new URL('./ss-interaction.js', import.meta.url), 'utf8');
    const m = src.match(/function updatePrecisionHint\(\)[\s\S]*?\n}/);
    assert.ok(m, '找到 updatePrecisionHint');
    assert.match(m[0], /uiScaleAtCss\(window\.innerWidth \/ 2, 16/, '按视口顶部中央锚点计算 uiScale');
    assert.match(m[0], /translateX\(-50%\) scale\(/, 'transform 组合水平居中与缩放补偿');
    console.log('✓ precision hint DPI 视觉补偿');
}

console.log('ss-interaction tests passed');
