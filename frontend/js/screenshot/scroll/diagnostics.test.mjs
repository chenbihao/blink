import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

globalThis.savedReplayFiles = [];
globalThis.window = {
  location: { search: '' },
  localStorage: { getItem: () => '1' },
};
globalThis.document = {
  getElementById: () => null,
  createElement: () => ({
    getContext: () => ({ putImageData() {} }),
    toBlob: (callback) => callback(new Blob([new Uint8Array([1, 2, 3])], { type: 'image/png' })),
  }),
};

function dataUrl(source) {
  return 'data:text/javascript;base64,' + Buffer.from(source).toString('base64');
}

const trackerUrl = dataUrl('export const SCROLL_DECISION_SCHEMA_VERSION = 2;');
const apiUrl = dataUrl(`
  export async function screenshotSaveReplayFile(directoryName, fileName, data) {
    globalThis.savedReplayFiles.push({ directoryName, fileName, data });
    return 'C:/AppData/blink/logs/scroll-replays/' + directoryName;
  }
`);
const stateUrl = dataUrl(`export const ss = { screenshotConfig: { scrollDebug: true } };`);
const source = (await readFile(new URL('./diagnostics.js', import.meta.url), 'utf8'))
  .replace("'./tracker.js'", JSON.stringify(trackerUrl))
  .replace("'../../shared/api.js'", JSON.stringify(apiUrl))
  .replace("'../ss-state.js'", JSON.stringify(stateUrl));
const { exportScrollReplay } = await import(dataUrl(source));

const frame = { width: 1, height: 1, data: new Uint8ClampedArray([0, 0, 0, 255]) };
const decision = {
  expectedDirection: 1,
  accepted: true,
  reason: 'matched',
  source: 'adjacent',
  confidence: 0.8,
  calibration: {
    positionedOverlap: { status: 'conflict', score: 12, threshold: 8 },
  },
};
const result = await exportScrollReplay({
  scrollReplayFrames: [{
    frame, capturedAtMs: 10, settle: { stable: true }, decision,
    timing: {
      settleMs: 92, captureMs: 4.2, trackMs: 7.1,
      commitMs: 1.3, previewMs: 2.4, totalLatencyMs: 151.5,
    },
    tracking: {
      before: 'recovering', after: 'lost', failureCount: 2, becameLost: true,
    },
    calibration: decision.calibration,
  }],
});

assert.equal(result.count, 1);
assert.match(result.directory, /scroll-replays\/blink-scroll-/);
assert.equal(globalThis.savedReplayFiles.length, 2, '应先写 PNG，再写完整 manifest');
assert.equal(globalThis.savedReplayFiles[0].fileName, 'frame-0000.png');
assert.equal(globalThis.savedReplayFiles[1].fileName, 'manifest.json');
const manifest = JSON.parse(new TextDecoder().decode(globalThis.savedReplayFiles[1].data));
assert.equal(manifest.frameCount, 1);
assert.equal(manifest.frames[0].expectedDecision.accepted, true);
assert.equal(manifest.frames[0].calibration.positionedOverlap.score, 12);
assert.equal(manifest.frames[0].timing.totalLatencyMs, 151.5);
assert.equal(manifest.frames[0].tracking.after, 'lost');
assert.equal(manifest.calibrationSummary.positionedOverlap.conflictCount, 1);
assert.equal(manifest.calibrationSummary.tracking.transitions['recovering->lost'], 1);
assert.equal(manifest.calibrationSummary.tracking.becameLostCount, 1);
assert.equal(manifest.calibrationSummary.confidence.average, 0.8);
assert.equal(manifest.calibrationSummary.timing.previewMs.average, 2.4);
assert.equal(manifest.calibrationSummary.timing.totalLatencyMs.max, 151.5);

console.log('scroll diagnostics tests passed');
