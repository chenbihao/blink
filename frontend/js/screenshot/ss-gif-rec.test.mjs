//! 录制叠层模式纯逻辑测试（node --test）。

import test from 'node:test';
import assert from 'node:assert/strict';

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
