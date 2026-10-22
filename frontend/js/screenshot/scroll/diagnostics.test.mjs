import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';

globalThis.savedReplayFiles = [];
globalThis.window = {
    location: {search: ''},
    localStorage: {getItem: () => '1'},
};
globalThis.document = {
    getElementById: () => null,
    createElement: () => ({
        getContext: () => ({
            putImageData() {
            }
        }),
        toBlob: (callback) => callback(new Blob([new Uint8Array([1, 2, 3])], {type: 'image/png'})),
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
const stateUrl = dataUrl(`export const ss = globalThis.scrollDiagnosticState = { screenshotConfig: { scrollDebug: true } };`);
const source = (await readFile(new URL('./diagnostics.js', import.meta.url), 'utf8'))
    .replace("'./tracker.js'", JSON.stringify(trackerUrl))
    .replace("'../../shared/api.js'", JSON.stringify(apiUrl))
    .replace("'../ss-state.js'", JSON.stringify(stateUrl))
    .replace("'./preview.js'", JSON.stringify(dataUrl('export function positionScrollDiagnostics() {}')));
const {exportScrollReplay, resetScrollDiagnostics, recordScrollDiagnostic,
    recordScrollEvent, buildCalibrationSummary} = await import(dataUrl(source));

const frame = {width: 1, height: 1, data: new Uint8ClampedArray([0, 0, 0, 255])};
const decision = {
    expectedDirection: 1,
    accepted: true,
    reason: 'matched',
    source: 'adjacent',
    confidence: 0.8,
    calibration: {
        positionedOverlap: {status: 'conflict', score: 12, threshold: 8},
    },
};
const result = await exportScrollReplay({
    scrollReplayFrames: [{
        frame, capturedAtMs: 10, settle: {stable: true}, decision,
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

// 分位数包含输入到抓帧前的等待，且不把缺失/非法值混入统计。
const summary = buildCalibrationSummary(Array.from({length: 20}, (_, i) => ({
    timing: {preCaptureMs: i + 1, totalLatencyMs: (i + 1) * 10, captureMs: -1},
})));
assert.equal(summary.timing.preCaptureMs.p50, 10);
assert.equal(summary.timing.totalLatencyMs.p95, 190);
assert.equal(summary.timing.captureMs.count, 0);

// 帧像素预算用尽后继续有界保留纯计时/时间线，显式报告截断数量。
const session = {};
resetScrollDiagnostics(session);
for (let i = 0; i < 1100; i++) recordScrollDiagnostic(session, frame, decision, {
    timing: {totalLatencyMs: i},
});
assert.equal(session.scrollReplayFrames.length, 256);
assert.equal(session.scrollReplayBytes, 256 * 4);
assert.equal(session.scrollTimingSamples.length, 1024);
assert.equal(session.scrollDroppedTimingSamples, 76);
for (let i = 0; i < 1100; i++) recordScrollEvent(session, 'wheel');
assert.equal(session.scrollTimeline.length, 2048);
assert.equal(session.scrollDroppedTimelineEvents, 152);
savedReplayFiles.length = 0;
await exportScrollReplay(session);
const bounded = JSON.parse(new TextDecoder().decode(savedReplayFiles.at(-1).data));
assert.equal(bounded.timingSampleCount, 1024);
assert.equal(bounded.droppedTimingSamples, 76);
assert.equal(bounded.timeline.length, 2048);
assert.equal(bounded.droppedTimelineEvents, 152);
assert.equal(bounded.timingSummary.totalLatencyMs.min, 76);
assert.equal(bounded.timingSummary.totalLatencyMs.p95, 1048);

// 默认不开启诊断时，不保留像素、输入时间线或计时样本。
scrollDiagnosticState.screenshotConfig.scrollDebug = false;
resetScrollDiagnostics(session);
recordScrollDiagnostic(session, frame, decision, {timing: {totalLatencyMs: 20}});
recordScrollEvent(session, 'wheel');
assert.equal(session.scrollReplayFrames.length, 0);
assert.equal(session.scrollTimingSamples.length, 0);
assert.equal(session.scrollTimeline.length, 0);

console.log('scroll diagnostics tests passed');
