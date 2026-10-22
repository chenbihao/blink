import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';

const {ScrollCaptureSession, attachScrollSessionFacade} = await import(
'data:text/javascript;base64,'
+ Buffer.from(await readFile(new URL('./session.js', import.meta.url))).toString('base64')
    );

const session = new ScrollCaptureSession();
assert.equal(session.scrollCapturePhase, 'idle');
assert.deepEqual(session.scrollFrames, []);
assert.equal(session.active, false);
assert.equal(session.capturing, false);

session.scrollCapturePhase = 'capturing';
assert.equal(session.active, true);
assert.equal(session.capturing, true);
session.scrollCapturePhase = 'editing';
assert.equal(session.active, true);
assert.equal(session.capturing, false);
session.scrollCapturePhase = 'capturing';
session.scrollFrames.push({top: 0});
session.queuedManualWheel = {delta: 120};
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
attached.exitHandler = (restore) => {
    restoreSelection = restore;
};
attached.exit(false);
assert.equal(restoreSelection, false, '跨模块退出应经过 session 的唯一生命周期入口');

// 编辑器接管后释放采集像素和 Worker，但输出/取消仍需保留来源几何与会话身份。
session.scrollCapturePhase = 'editing';
session.scrollSourceMonitor = {x: -1920, y: 0, w: 1920, h: 1080};
session.scrollBandX = -1200;
session.scrollFrames = [{image: {data: new Uint8ClampedArray(1024)}}];
session.scrollLastFrame = session.scrollFrames[0].image;
session.scrollKeyframes = [session.scrollLastFrame];
session.scrollReplayFrames = [session.scrollLastFrame];
session.scrollReplayBytes = 1024;
let disposedPipeline = 0, disposedClient = 0;
session.continuousCapture = {dispose() { disposedPipeline++; }};
session.trackingClient = {dispose() { disposedClient++; }};
const editingLifecycle = session.lifecycleId, editingGeneration = session.captureGeneration;
session.releaseCaptureResources(); session.releaseCaptureResources();
assert.equal(disposedPipeline, 1); assert.equal(disposedClient, 1);
assert.equal(session.trackingClient, null); assert.equal(session.continuousCapture, null);
assert.deepEqual(session.scrollFrames, []); assert.deepEqual(session.scrollKeyframes, []);
assert.equal(session.scrollLastFrame, null); assert.deepEqual(session.scrollReplayFrames, []);
assert.equal(session.scrollReplayBytes, 0);
assert.equal(session.scrollCapturePhase, 'editing'); assert.equal(session.active, true);
assert.equal(session.scrollBandX, -1200); assert.equal(session.scrollSourceMonitor.x, -1920);
assert.equal(session.lifecycleId, editingLifecycle);
assert.ok(session.captureGeneration > editingGeneration, '旧采集/滚轮任务不得继续执行');

console.log('scroll session tests passed');
