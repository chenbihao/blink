import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const source = await readFile(new URL('./image-editor-session.js', import.meta.url), 'utf8');
const { ImageEditorSession, IMAGE_SOURCE } = await import(
  `data:text/javascript;base64,${Buffer.from(source).toString('base64')}`
);

const session = new ImageEditorSession();
assert.equal(session.active, false);
assert.equal(session.canUseCaptureCropFastPath, false);

session.beginScreenshotSelection();
assert.equal(session.source, IMAGE_SOURCE.SCREENSHOT);
assert.equal(session.ownsScreenshotSession, true);
assert.equal(session.canUseCaptureCropFastPath, true);

const canvas = { width: 640, height: 480 };
session.beginCanvasSource(IMAGE_SOURCE.CLIPBOARD, canvas, { screenX: 12, screenY: 34 });
assert.equal(session.baseCanvas, canvas);
assert.equal(session.canvasBacked, true);
assert.equal(session.ownsScreenshotSession, false);
assert.equal(session.canUseCaptureCropFastPath, false);
assert.deepEqual([session.screenX, session.screenY], [12, 34]);

session.beginCanvasSource(IMAGE_SOURCE.LONG_SCREENSHOT, canvas);
assert.equal(session.ownsScreenshotSession, true);

assert.throws(() => session.beginCanvasSource('history', canvas), /不支持/);
assert.throws(() => session.beginCanvasSource(IMAGE_SOURCE.CLIPBOARD, { width: 0, height: 1 }), /非空/);

session.reset();
assert.equal(session.source, IMAGE_SOURCE.NONE);
assert.equal(session.baseCanvas, null);

console.log('image editor session tests passed');
