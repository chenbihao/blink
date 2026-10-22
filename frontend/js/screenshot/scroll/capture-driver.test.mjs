import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';

const apiUrl = 'data:text/javascript;base64,' + Buffer.from(`
  export const screenshotCaptureBand = () => null;
  export const screenshotCaptureProbe = () => null;
  export const screenshotForwardWheel = (...args) => globalThis.wheelForwardTest?.(...args);
`).toString('base64');
const stabilityUrl = 'data:text/javascript;base64,' + Buffer.from(
    'export const isProbeStable = () => ({ stable: true, score: 0 });',
).toString('base64');
const source = (await readFile(new URL('./capture-driver.js', import.meta.url), 'utf8'))
    .replace("'../../shared/api.js'", JSON.stringify(apiUrl))
    .replace("'./stability.js'", JSON.stringify(stabilityUrl));
const {shouldCompleteVisualSettle, queueManualWheel} = await import(
'data:text/javascript;base64,' + Buffer.from(source).toString('base64')
    );

assert.equal(shouldCompleteVisualSettle(89, 2, false), false);
assert.equal(shouldCompleteVisualSettle(90, 2, false), true, '静止页面应走 90ms 快路径');
assert.equal(shouldCompleteVisualSettle(120, 2, true), false);
assert.equal(shouldCompleteVisualSettle(180, 2, true), true, '观察到运动后必须保留安全等待');
assert.equal(shouldCompleteVisualSettle(180, 1, true), false, '仍需连续两个稳定样本');

// 转发 IPC 繁忙时不等待匹配，也不能截断密集输入的总滚动量。
let releaseWheel;
const firstWheel = new Promise(resolve => { releaseWheel = resolve; });
const forwarded = [];
globalThis.wheelForwardTest = (...args) => {
    forwarded.push(args);
    return forwarded.length === 1 ? firstWheel : Promise.resolve();
};
const session = {lifecycleId: 1, scrollCapturePhase: 'capturing', autoScroll: false,
    scrollHwnd: 1, scrollInputToken: 42};
queueManualWheel(session, 120, 10, 20);
for (let i = 0; i < 10; i++) queueManualWheel(session, 120, 10, 20);
assert.equal(session.queuedManualWheel.delta, 1200, '在途输入不能被截断到480');
releaseWheel(); await session.wheelForwardInFlight;
assert.deepEqual(forwarded.map(args => args[1]), [120, 480, 480, 240]);
assert.equal(forwarded.reduce((sum, args) => sum + args[1], 0), 1320);
assert.ok(forwarded.every(args => args[4] === 72 && args[7] === 42), '保持短时穿透与会话隔离参数');
assert.equal(session.queuedManualWheel, null);

console.log('scroll capture driver tests passed');
