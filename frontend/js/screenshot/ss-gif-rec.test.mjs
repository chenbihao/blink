//! 录制叠层模式纯逻辑测试（node --test）。

import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

// 模块导入链（ss-display/ss-toolbar/shared）在顶层访问 window——
// 先注入最小 stub 再动态导入。
globalThis.window = globalThis.window || {devicePixelRatio: 1, __blinkScreenMeta: {vx: 0, vy: 0}};
const {clampRegionMove, placeTextOutsideRegion, unsupportedKindsOf} = await import('./ss-gif-rec.js');

test('unsupportedKindsOf：底图依赖效果去重收集，水印独立计入', () => {
    assert.deepEqual(
        unsupportedKindsOf([
            {type: 'pencil'}, {type: 'blur'}, {type: 'pixelate'}, {type: 'blur'}, {type: 'arrow'},
        ], false),
        ['blur', 'pixelate'],
    );
    assert.deepEqual(unsupportedKindsOf([{type: 'text'}], true), ['watermark']);
    assert.deepEqual(unsupportedKindsOf([{type: 'rect'}, {type: 'eraser'}], false), []);
    assert.deepEqual(unsupportedKindsOf([], false), []);
    // 录制可用的绘制工具不误报
    assert.deepEqual(
        unsupportedKindsOf([
            {type: 'pencil'}, {type: 'arrow'}, {type: 'rect'}, {type: 'ellipse'},
            {type: 'highlight-multiply'}, {type: 'highlight-translucent'},
            {type: 'text'}, {type: 'number'}, {type: 'eraser'},
        ], false),
        [],
    );
});

test('placeTextOutsideRegion：区域内挪到正下方，区域外保持原位', () => {
    const sel = {x: 100, y: 100, w: 400, h: 300};
    // 区域内（含边界）→ 区域正下方 8px
    assert.deepEqual(placeTextOutsideRegion({x: 150, y: 200}, sel), {x: 150, y: 408});
    assert.deepEqual(placeTextOutsideRegion({x: sel.x, y: sel.y}, sel), {x: 100, y: 408});
    assert.deepEqual(placeTextOutsideRegion({x: sel.x + sel.w, y: sel.y + sel.h}, sel), {x: 500, y: 408});
    // 区域外原位
    assert.deepEqual(placeTextOutsideRegion({x: 50, y: 200}, sel), {x: 50, y: 200});
    assert.deepEqual(placeTextOutsideRegion({x: 150, y: 500}, sel), {x: 150, y: 500});
    // 无选区原样返回
    assert.deepEqual(placeTextOutsideRegion({x: 10, y: 10}, null), {x: 10, y: 10});
});

test('clampRegionMove：钳制到虚拟屏内（含负坐标多屏），无边距丢失', () => {
    const origin = {x: 100, y: 100, w: 400, h: 300};
    const vs = {vx: -1920, vy: 0, vw: 5760, vh: 1440};
    assert.deepEqual(clampRegionMove(origin, 50, 60, vs), {x: 150, y: 160});
    // 左/上边界（负坐标合法）
    assert.deepEqual(clampRegionMove(origin, -10000, 0, vs), {x: -1920, y: 100});
    assert.deepEqual(clampRegionMove(origin, 0, -10000, vs), {x: 100, y: 0});
    // 右/下边界按右下角贴边（w/h 保持）
    assert.deepEqual(clampRegionMove(origin, 100000, 0, vs), {x: 5760 - 1920 - 400, y: 100});
    assert.deepEqual(clampRegionMove(origin, 0, 100000, vs), {x: 100, y: 1440 - 300});
    // 无虚拟屏信息（边界未知）→ 不钳制
    assert.deepEqual(clampRegionMove(origin, -100000, 0, null), {x: -99900, y: 100});
});

test('录制就绪等待透明层绘制；旧会话 idle 不清理新会话，结束时释放定时器', async () => {
    const classes = () => {
        const values = new Set();
        return {add: (...cs) => cs.forEach(c => values.add(c)),
            remove: (...cs) => cs.forEach(c => values.delete(c)),
            contains: c => values.has(c)};
    };
    const toolbar = {classList: classes(), dataset: {},
        getBoundingClientRect: () => ({x: 0, y: 200, width: 200, height: 40})};
    const body = {classList: classes()};
    const root = {classList: classes()};
    const paints = [], calls = [], timers = new Set();
    let timerId = 0;
    const context = vm.createContext({
        ss: {toolbar, selCss: {x: 0, y: 0, w: 100, h: 100}},
        window: {__blinkScreenMeta: {vx: 0, vy: 0, w: 1920, h: 1080}},
        document: {body, documentElement: root, getElementById: () => null, querySelectorAll: () => []},
        annot: {getTool: () => 'pencil'}, positionToolbar() {}, selectTool() {},
        getRenderScale: () => ({scaleX: 1, scaleY: 1}), cssPointToScreen: (x, y) => ({x, y}),
        listen: async () => () => {}, EVENTS: {}, performance: {now: () => 1000},
        setInterval: () => { timers.add(++timerId); return timerId; },
        clearInterval: id => timers.delete(id), clearTimeout() {},
        requestAnimationFrame: fn => paints.push(fn),
        invoke: async (cmd, args) => {
            calls.push({cmd, args});
            if (cmd === 'gif_recorder_state') return {sessionId: 'new', phase: 'recording', elapsedMs: 0};
        },
    });
    const source = readFileSync(new URL('./ss-gif-rec.js', import.meta.url), 'utf8')
        .replace(/^import .*;\r?$/gm, '').replace(/export /g, '');
    vm.runInContext(`${source}\nglobalThis.fixture = {state, enterGifRecordOverlay, applyRecorderState};`, context);
    const f = context.fixture;
    const task = f.enterGifRecordOverlay({x: 0, y: 0, w: 100, h: 100}, 'new');
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(root.classList.contains('gif-recording'), true);
    assert.equal(calls.some(c => c.cmd === 'gif_overlay_ready'), false);
    paints.shift()();
    assert.equal(calls.some(c => c.cmd === 'gif_overlay_ready'), false);
    paints.shift()();
    await task;
    assert.deepEqual(calls.map(c => c.cmd), ['gif_overlay_input', 'gif_overlay_ready', 'gif_recorder_state']);
    f.state.cancelArmed = true;
    f.applyRecorderState({sessionId: 'old', phase: 'idle'});
    assert.equal(f.state.active, true);
    assert.equal(timers.size, 2);
    f.applyRecorderState({sessionId: 'new', phase: 'idle'});
    assert.equal(f.state.active, false);
    assert.equal(f.state.cancelArmed, false);
    assert.equal(timers.size, 0);
    assert.equal(root.classList.contains('gif-recording'), false);
});
