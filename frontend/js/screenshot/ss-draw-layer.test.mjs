import { test } from 'node:test';
import assert from 'node:assert/strict';

import { ss } from './ss-state.js';
import { drawDimmed } from './ss-draw.js';

test('drawDimmed keeps the screenshot layer original and masks only the interaction layer', () => {
  const baseCalls = [];
  const interactionCalls = [];
  const source = { kind: 'source-canvas' };

  ss.canvas = { width: 100, height: 60 };
  ss.interactionCanvas = { width: 100, height: 60 };
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
