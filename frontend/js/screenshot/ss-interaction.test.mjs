import assert from 'node:assert/strict';

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

console.log('ss-interaction tests passed');
