//! 用法：node ss-scroll-replay-runner.mjs <blink-scroll-* 目录>

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { decodeReplayPng } from './ss-scroll-replay-png.mjs';

globalThis.ImageData ??= class ImageData {
  constructor(dataOrWidth, widthOrHeight, maybeHeight) {
    if (typeof dataOrWidth === 'number') {
      this.width = dataOrWidth;
      this.height = widthOrHeight;
      this.data = new Uint8ClampedArray(this.width * this.height * 4);
    } else {
      this.data = dataOrWidth;
      this.width = widthOrHeight;
      this.height = maybeHeight;
    }
  }
};

function dataUrl(source) {
  return 'data:text/javascript;base64,' + Buffer.from(source).toString('base64');
}

async function loadReplayModule() {
  const base = new URL('./', import.meta.url);
  const stitchUrl = dataUrl(await readFile(new URL('ss-scroll-stitch.js', base), 'utf8'));
  const trackerSource = (await readFile(new URL('ss-scroll-tracker.js', base), 'utf8'))
    .replace("'./ss-scroll-stitch.js'", JSON.stringify(stitchUrl));
  const trackerUrl = dataUrl(trackerSource);
  const replaySource = (await readFile(new URL('ss-scroll-replay.js', base), 'utf8'))
    .replace("'./ss-scroll-stitch.js'", JSON.stringify(stitchUrl))
    .replace("'./ss-scroll-tracker.js'", JSON.stringify(trackerUrl));
  return import(dataUrl(replaySource));
}

export async function replayExportedDirectory(directory) {
  const manifest = JSON.parse(await readFile(resolve(directory, 'manifest.json'), 'utf8'));
  if (manifest.format !== 'blink-scroll-replay' || manifest.version !== 1) {
    throw new Error('不支持的长截图回放 manifest');
  }
  const sequence = [];
  for (const captured of manifest.frames) {
    const decoded = decodeReplayPng(await readFile(resolve(directory, captured.file)));
    sequence.push({
      frame: new ImageData(decoded.data, decoded.width, decoded.height),
      expectedDirection: captured.expectedDirection,
      settle: captured.settle,
    });
  }
  const { replayScrollSequence } = await loadReplayModule();
  const result = replayScrollSequence(sequence);
  const expected = manifest.frames.map((captured) => captured.expectedDecision);
  assert.deepEqual(result.decisions, expected, '离线重放结果与录制时决策不一致');
  return result;
}

const invokedPath = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : '';
if (import.meta.url === invokedPath) {
  const directory = process.argv[2];
  if (!directory) throw new Error('请传入 blink-scroll-* 回放目录');
  const result = await replayExportedDirectory(directory);
  console.log(JSON.stringify({
    frameCount: result.decisions.length,
    confirmedTop: result.confirmedTop,
    decisions: result.decisions.map((decision, index) => ({ index, ...decision })),
  }, null, 2));
}
