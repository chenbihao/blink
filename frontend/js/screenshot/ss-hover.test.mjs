import assert from 'node:assert/strict';

globalThis.window = {
  __blinkScreenMeta: { vx: -100, vy: 0 },
  devicePixelRatio: 1,
  innerWidth: 300,
  innerHeight: 200,
};

const { ss } = await import('./ss-state.js');
const {
  clearPickableWindows,
  findWindowForRect,
  loadPickableWindows,
  normalizePickableWindows,
} = await import('./ss-hover.js');

const rawWindows = [
  { hwnd: 1, x: -150, y: -20, w: 120, h: 100, title: 'partial', process_name: 'partial-app' },
  { hwnd: 2, x: 400, y: 0, w: 50, h: 50, title: 'outside', process_name: 'outside-app' },
];
const normalized = normalizePickableWindows(
  rawWindows,
  window.__blinkScreenMeta,
  window.innerWidth,
  window.innerHeight,
);
assert.deepEqual(normalized, [{
  hwnd: 1,
  title: 'partial',
  processName: 'partial-app',
  x: 0,
  y: 0,
  w: 70,
  h: 80,
}], '部分越界窗口应裁剪，完全越界窗口应过滤');

clearPickableWindows();
const currentGeneration = ss.windowListGen;
await loadPickableWindows(currentGeneration, async () => [{
  hwnd: 9, x: -90, y: 10, w: 80, h: 80, title: 'current', process_name: 'current-app',
}]);
assert.equal(findWindowForRect({ x: 10, y: 10, w: 20, h: 20 })?.hwnd, 9);

// 旧代请求晚到的失败不得清空新代已经成功写入的列表。
ss.windowListGen = currentGeneration + 1;
await loadPickableWindows(ss.windowListGen, async () => [{
  hwnd: 10, x: -80, y: 20, w: 60, h: 60, title: 'new', process_name: 'new-app',
}]);
await loadPickableWindows(currentGeneration, async () => { throw new Error('stale failure'); });
assert.equal(
  findWindowForRect({ x: 20, y: 20, w: 20, h: 20 })?.hwnd,
  10,
  '旧请求失败不应覆盖新列表',
);

console.log('ss-hover tests passed');
