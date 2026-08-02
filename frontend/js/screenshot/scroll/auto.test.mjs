import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const source = await readFile(new URL('./auto.js', import.meta.url), 'utf8');
const { nextAutoWheelState, runAutoScrollController } = await import(
  'data:text/javascript;base64,' + Buffer.from(source).toString('base64')
);

const lowConfidence = nextAutoWheelState(
  { delta: -180, lowConfidenceCount: 0 },
  { positionShift: 120, decision: { confidence: 0.4 } },
  300,
);
assert.deepEqual(
  lowConfidence,
  { delta: -120, lowConfidenceCount: 1 },
  '低置信确认帧应减速而不是继续放大滚轮',
);

const timedOut = nextAutoWheelState(
  { delta: -120, lowConfidenceCount: 1 },
  { positionShift: 110, decision: { confidence: 0.9 } },
  300,
  { timedOut: true },
);
assert.deepEqual(timedOut, { delta: -90, lowConfidenceCount: 2 });

const recovering = nextAutoWheelState(
  { delta: -90, lowConfidenceCount: 2 },
  { positionShift: 90, decision: { confidence: 0.9 } },
  300,
);
assert.deepEqual(
  recovering,
  { delta: -120, lowConfidenceCount: 1 },
  '恢复期间只能缓慢提速并逐轮消除低置信状态',
);

let active = true;
let forwarded = 0;
let predicted = 0;
let unchangedCaptures = 0;
const forwardModes = [];
let stopReason = null;
await runAutoScrollController({
  generation: 1,
  session: {
    scrollTrackingState: 'tracking', scrollBandH: 300, _scrollCapturing: false,
    autoWheelDelta: -120, autoLowConfidenceCount: 0,
  },
  isActive: () => active,
  waitForSettle: async () => ({ stable: true }),
  captureFrame: async () => {
    unchangedCaptures++;
    return { moved: false, reason: 'unchanged' };
  },
  forwardWheel: async (positionCursor, forceMessage) => {
    forwarded++;
    forwardModes.push({ positionCursor, forceMessage });
  },
  previewWheel: () => { predicted++; },
  stop: async (reason) => { stopReason = reason; active = false; },
  delay: async () => {},
});
assert.equal(forwarded, 5, '分级重试五次后才判定到底并停止注入滚轮');
assert.equal(unchangedCaptures, 10, '每次注入后 unchanged 应延迟复核一次再决定继续滚动');
assert.equal(predicted, 5, '每次自动滚轮都应先给预览预测反馈');
assert.deepEqual(forwardModes.slice(0, 3), [
  { positionCursor: true, forceMessage: false },
  { positionCursor: true, forceMessage: false },
  { positionCursor: true, forceMessage: true },
]);
assert.match(stopReason, /滚动到底/);

active = true;
const order = [];
let captures = 0;
await runAutoScrollController({
  generation: 2,
  session: {
    scrollTrackingState: 'recovering', scrollBandH: 300, _scrollCapturing: false,
    autoWheelDelta: -120, autoLowConfidenceCount: 0,
  },
  isActive: () => active,
  waitForSettle: async () => ({ stable: true }),
  captureFrame: async (direction) => {
    order.push(`capture:${direction}`);
    captures++;
    return captures === 1
      ? { moved: true, reason: 'relocalized', positionShift: -100, decision: { confidence: 0.9 } }
      : { moved: false, reason: 'low-confidence' };
  },
  forwardWheel: async () => { order.push('forward'); },
  stop: async () => { active = false; },
  delay: async () => {},
});
assert.deepEqual(
  order.slice(0, 2),
  ['capture:0', 'forward'],
  'recovering / lost 状态必须先恢复定位再注入滚轮',
);

console.log('scroll auto tests passed');
