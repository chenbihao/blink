import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';

const source = await readFile(new URL('./image-editor-session.js', import.meta.url), 'utf8');
const {ImageEditorSession, IMAGE_SOURCE} = await import(
    `data:text/javascript;base64,${Buffer.from(source).toString('base64')}`
    );

const session = new ImageEditorSession();
assert.equal(session.active, false);
assert.equal(session.canUseCaptureCropFastPath, false);

session.beginScreenshotSelection();
assert.equal(session.source, IMAGE_SOURCE.SCREENSHOT);
assert.equal(session.ownsScreenshotSession, true);
assert.equal(session.canUseCaptureCropFastPath, true);

const canvas = {width: 640, height: 480};
session.beginCanvasSource(IMAGE_SOURCE.CLIPBOARD, canvas, {screenX: 12, screenY: 34});
assert.equal(session.baseCanvas, canvas);
assert.equal(session.canvasBacked, true);
assert.equal(session.ownsScreenshotSession, false);
assert.equal(session.canUseCaptureCropFastPath, false);
assert.deepEqual([session.screenX, session.screenY], [12, 34]);

session.beginCanvasSource(IMAGE_SOURCE.LONG_SCREENSHOT, canvas);
assert.equal(session.ownsScreenshotSession, true);

// HISTORY 和 PIN 是 0.20.4 新增的受支持来源
session.beginCanvasSource(IMAGE_SOURCE.HISTORY, canvas);
assert.equal(session.source, IMAGE_SOURCE.HISTORY);
assert.equal(session.ownsScreenshotSession, false);

session.beginCanvasSource(IMAGE_SOURCE.PIN, canvas);
assert.equal(session.source, IMAGE_SOURCE.PIN);
assert.equal(session.ownsScreenshotSession, false);

// 仍应拒绝未注册的来源
assert.throws(() => session.beginCanvasSource('unknown', canvas), /不支持/);
assert.throws(() => session.beginCanvasSource(IMAGE_SOURCE.CLIPBOARD, {width: 0, height: 1}), /非空/);

session.reset();
assert.equal(session.source, IMAGE_SOURCE.NONE);
assert.equal(session.baseCanvas, null);

// ── Epoch 机制测试（0.19.16）──
// reset / beginCanvasSource 必须递增 epoch，使后台异步回调能检测代际失效
const epochBeforeReset = session.epoch;
session.reset();
assert.equal(session.epoch, epochBeforeReset + 1, 'reset() 递增 epoch');

const epochAfterReset = session.epoch;
session.reset();
session.reset();
assert.equal(session.epoch, epochAfterReset + 2, '连续 reset 每次 +1');

// beginCanvasSource 也递增 epoch
const epochBeforeCanvas = session.epoch;
session.beginCanvasSource(IMAGE_SOURCE.LONG_SCREENSHOT, canvas);
assert.equal(session.epoch, epochBeforeCanvas + 1, 'beginCanvasSource 递增 epoch');

// beginScreenshotSelection 内部调 reset → epoch 递增
const epochBeforeScreenshot = session.epoch;
session.beginScreenshotSelection();
assert.equal(session.epoch, epochBeforeScreenshot + 1, 'beginScreenshotSelection 递增 epoch');

// 构造时 epoch 已初始化
const freshSession = new ImageEditorSession();
assert.ok(freshSession.epoch >= 1, '新实例 epoch >= 1');
const freshEpoch = freshSession.epoch;
freshSession.reset();
assert.equal(freshSession.epoch, freshEpoch + 1, '新实例 reset 后 epoch +1');

console.log('image editor session tests passed (incl. epoch)');
