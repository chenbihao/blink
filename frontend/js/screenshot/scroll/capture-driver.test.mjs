import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';

const apiUrl = 'data:text/javascript;base64,' + Buffer.from(`
  export const screenshotCaptureBand = () => null;
  export const screenshotCaptureProbe = () => null;
  export const screenshotForwardWheel = () => null;
`).toString('base64');
const stabilityUrl = 'data:text/javascript;base64,' + Buffer.from(
    'export const isProbeStable = () => ({ stable: true, score: 0 });',
).toString('base64');
const source = (await readFile(new URL('./capture-driver.js', import.meta.url), 'utf8'))
    .replace("'../../shared/api.js'", JSON.stringify(apiUrl))
    .replace("'./stability.js'", JSON.stringify(stabilityUrl));
const {shouldCompleteVisualSettle} = await import(
'data:text/javascript;base64,' + Buffer.from(source).toString('base64')
    );

assert.equal(shouldCompleteVisualSettle(89, 2, false), false);
assert.equal(shouldCompleteVisualSettle(90, 2, false), true, '静止页面应走 90ms 快路径');
assert.equal(shouldCompleteVisualSettle(120, 2, true), false);
assert.equal(shouldCompleteVisualSettle(180, 2, true), true, '观察到运动后必须保留安全等待');
assert.equal(shouldCompleteVisualSettle(180, 1, true), false, '仍需连续两个稳定样本');

console.log('scroll capture driver tests passed');
