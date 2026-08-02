import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const { ScrollCaptureSession, attachScrollSessionFacade } = await import(
  'data:text/javascript;base64,'
  + Buffer.from(await readFile(new URL('./session.js', import.meta.url))).toString('base64')
);

const session = new ScrollCaptureSession();
assert.equal(session.scrollCapturePhase, 'idle');
assert.deepEqual(session.scrollFrames, []);
assert.equal(session.active, false);

session.scrollCapturePhase = 'capturing';
session.scrollFrames.push({ top: 0 });
session.queuedManualWheel = { delta: 120 };
session.manualWheelVersion = 4;
const oldGeneration = session.captureGeneration;
session.reset();
assert.ok(session.captureGeneration > oldGeneration, 'reset 应使旧异步任务失效');
assert.equal(session.invalidate(), session.captureGeneration, 'invalidate 应返回新代际供异步入口捕获');
assert.equal(session.scrollCapturePhase, 'idle');
assert.deepEqual(session.scrollFrames, []);
assert.equal(session.queuedManualWheel, null);
assert.equal(session.manualWheelVersion, 0);

const shared = {};
const attached = attachScrollSessionFacade(shared);
shared.scrollCurrentTop = 240;
assert.equal(attached.scrollCurrentTop, 240, '旧 ss 访问应写入统一 session');
attached.scrollTrackingState = 'lost';
assert.equal(shared.scrollTrackingState, 'lost', 'session 写入应从旧 ss 门面可见');
assert.equal(shared.scrollSession, attached);
let restoreSelection;
attached.exitHandler = (restore) => { restoreSelection = restore; };
attached.exit(false);
assert.equal(restoreSelection, false, '跨模块退出应经过 session 的唯一生命周期入口');

console.log('scroll session tests passed');
