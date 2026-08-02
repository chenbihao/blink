import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const { isProbeStable, probeMotionScore } = await import(
  'data:text/javascript;base64,'
  + Buffer.from(await readFile(new URL('./stability.js', import.meta.url))).toString('base64')
);

const stillA = new Uint8Array(100).fill(80);
const stillB = new Uint8Array(100).fill(81);
assert.equal(isProbeStable(stillA, stillB).stable, true, '轻微采集噪声应视为稳定');

const localAnimation = new Uint8Array(stillA);
localAnimation.fill(180, 0, 20);
assert.equal(
  isProbeStable(stillA, localAnimation).stable,
  true,
  '小面积动画不应让稳定等待永久超时',
);

const scrolling = new Uint8Array(stillA);
scrolling.fill(160, 0, 75);
assert.equal(isProbeStable(stillA, scrolling).stable, false, '大面积内容变化应判为滚动中');
assert.ok(probeMotionScore(stillA, scrolling) > 3);

assert.equal(probeMotionScore(new Uint8Array(1), new Uint8Array(2)), Infinity);

console.log('scroll stability tests passed');
