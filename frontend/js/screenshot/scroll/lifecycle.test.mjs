//! 驱动真实 index/capture-driver/tracker/stitch，以可控时钟和屏幕桩验证完整调度。
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {ClockedWorker} from './scroll-test-support.mjs';

const dataUrl = source => 'data:text/javascript;base64,' + Buffer.from(source).toString('base64');
const loadSource = name => readFile(new URL(name, import.meta.url), 'utf8');
globalThis.ImageData = class {
    constructor(data, w, h) {
        this.width = typeof data === 'number' ? data : w;
        this.height = typeof data === 'number' ? w : h;
        this.data = typeof data === 'number' ? new Uint8ClampedArray(data * w * 4) : data;
    }
};
const {ScrollCaptureSession} = await import(dataUrl(await loadSource('./session.js')));
const session = new ScrollCaptureSession();
let clock = 0, timerId = 0;
const timers = new Map();
const originalSetTimeout = globalThis.setTimeout, originalClearTimeout = globalThis.clearTimeout;
globalThis.setTimeout = (fn, ms) => { const id = ++timerId; timers.set(id, {fn, at: clock + ms}); return id; };
globalThis.clearTimeout = id => timers.delete(id);
globalThis.performance = {now: () => clock};
globalThis.window = {__blinkScreenMeta: {vx: 0, vy: 0}};
globalThis.document = {getElementById: () => null};
const hints = [], outputs = [], edits = [];
const ss = {scrollSession: session, editorSession: {reset() {}}, _showTransientHint: s => hints.push(s)};
ss._enterCanvasImageEditor = image => edits.push(image);
ss._compositeSelection = async () => edits.at(-1);
function frame(top) {
    const result = new ImageData(36, 90);
    for (let y = 0; y < 90; y++) for (let x = 0; x < 36; x++) {
        const i = (y * 36 + x) * 4, yy = top + y;
        result.data[i] = (yy * 17 + Math.floor(yy / 7) * 29 + x * 3) & 255;
        result.data[i + 1] = (yy * 7 + Math.floor(yy / 11) * 43 + x * 11) & 255;
        result.data[i + 2] = (yy * 13 + Math.floor(yy / 17) * 61 + x * 5) & 255;
        result.data[i + 3] = 255;
    }
    return result;
}
const h = globalThis.scrollLifecycleHarness = {ss, top: 0, captures: 0, captureGate: null, token: 0,
    async band() {
        this.captures++;
        const pixels = frame(this.top).data.buffer;
        if (this.captureGate) await this.captureGate;
        return pixels;
    },
};
globalThis.Worker = class extends ClockedWorker {
    postMessage(data, transfer) {
        if (data.type === 'init' && h.limits) data.payload.limits = h.limits;
        super.postMessage(data, transfer);
    }
};
const api = dataUrl(`const h = globalThis.scrollLifecycleHarness;
export const screenshotCaptureBand = () => h.band();
export const screenshotCaptureProbe = async () => new Uint8Array(100).buffer;
export const screenshotForwardWheel = async () => { h.top += 18; };
export const screenshotSetCaptureExclusion = async (exclude) => exclude ? ++h.token : null;
export const screenshotCancel = () => {};
`);
const stitch = dataUrl(await loadSource('./stitch.js'));
const tracker = dataUrl((await loadSource('./tracker.js')).replace("'./stitch.js'", JSON.stringify(stitch)));
const driver = dataUrl((await loadSource('./capture-driver.js'))
    .replace("'../../shared/api.js'", JSON.stringify(api))
    .replace("'./stability.js'", JSON.stringify(dataUrl(await loadSource('./stability.js')))));
const stubs = {
    '../ss-state.js': `export const ss = globalThis.scrollLifecycleHarness.ss;`,
    '../../shared/api.js': null,
    '../ss-hover.js': `export const findWindowForRect = () => ({hwnd: 1, targetX: 1, targetY: 1});`,
    './diagnostics.js': `export const bindScrollDiagnostics = () => {}; export const recordScrollDiagnostic = () => {}; export const recordScrollEvent = () => {}; export const resetScrollDiagnostics = () => {}; export const canRetainScrollReplayFrame = () => false;`,
    '../ss-editor-policy.js': `export const isLongScreenshotPan = () => false;`,
    '../ss-output.js': `export const encodeImageDataPng = async image => image;
        export const outputScreenshotPng = async (action, image) => {
            const h = globalThis.scrollLifecycleHarness; h.lastOutputAt = performance.now(); return h.output(image);
        };`,
    '../ss-selection-geometry.js': `export const cssPointToScreen = (x, y) => ({x,y}); export const cssRectToBitmap = r => r;`,
    '../ss-display.js': `export const findDisplayCssAt = () => null;`,
    '../image-editor-session.js': `export const IMAGE_SOURCE = {LONG_SCREENSHOT: 'long'};`,
    './preview.js': ['hideCaptureFrame','logScrollUiDiagnostics','positionPreview','positionScrollToolbar',
        'resetPreviewRendering','scrollCaptureInputRegions','setScrollCapturePresentation','showCaptureFrame','showScrollInputFeedback','updatePreview']
        .map(name => `export const ${name} = () => 0;`).join('\n'),
};
h.output = image => outputs.push(image);
const urls = {
    '../../shared/api.js': api, './stitch.js': stitch, './tracker.js': tracker,
    './capture-driver.js': driver, './auto.js': dataUrl(await loadSource('./auto.js')),
    './finalize.js': dataUrl(await loadSource('./finalize.js')),
    './continuous.js': dataUrl(await loadSource('./continuous.js')),
    './tracking-client.js': dataUrl((await loadSource('./tracking-client.js'))
        .replace("new URL('./tracking-worker.js', import.meta.url)", "'test:tracking-worker'")),
};
for (const [name, source] of Object.entries(stubs)) if (source) urls[name] = dataUrl(source);
let index = await loadSource('./index.js');
for (const [name, url] of Object.entries(urls)) index = index.replaceAll(`'${name}'`, JSON.stringify(url));
const {enterScrollCapture, onScrollWheel, outputLongImage, enterScrollEdit, resetScrollCaptureSession, toggleAutoScroll} = await import(dataUrl(index));
const tick = async () => { for (let i = 0; i < 100; i++) await Promise.resolve(); };
async function advance(ms) {
    const end = clock + ms;
    await tick();
    for (;;) {
        const next = [...timers].filter(([, t]) => t.at <= end).sort((a,b) => a[1].at - b[1].at)[0];
        if (!next) break;
        clock = next[1].at; timers.delete(next[0]); next[1].fn(); await tick();
    }
    clock = end; await tick();
}
const wheel = () => onScrollWheel({deltaY: 120, deltaMode: 0, clientX: 10, clientY: 10});
async function begin() { h.top = 0; await enterScrollCapture({x: 0, y: 0, w: 36, h: 90}); }

try {
    // 首帧仍在抓屏时提前点击完成，不得让会话永久卡在 finalizing。
    let releaseFirst;
    h.captureGate = new Promise(resolve => { releaseFirst = resolve; });
    const starting = enterScrollCapture({x: 0, y: 0, w: 36, h: 90});
    await tick();
    await outputLongImage('copy');
    assert.equal(session.captureFinalizing, false);
    h.captureGate = null; releaseFirst(); await starting;
    resetScrollCaptureSession();

    // 正常连续滚动不需要停顿，甚至没有 overlay wheel 事件也能持续确认。
    await begin();
    for (let i = 1; i <= 12; i++) {
        h.top = i * 18;
        await advance(100);
        assert.equal(session.scrollCurrentTop, h.top);
    }
    assert.equal(session.scrollFrames.reduce((sum, item) => sum + item.image.height, 0), 306);
    resetScrollCaptureSession();

    // 无需等防抖：滚轮生效后立即完成，输出必须含真实新增的18行。
    await begin();
    let releaseSlowCapture;
    h.captureGate = new Promise(resolve => { releaseSlowCapture = resolve; });
    const slowCapture = session.continuousCapture.sample({expectedDirection: 0});
    await tick();
    onScrollWheel({deltaY: 120, deltaMode: 0, clientX: 10, clientY: 10,
        preventDefault() { assert.fail('采集不能同步阻止滚轮默认行为'); }});
    await tick();
    assert.equal(h.top, 18, '抓屏仍在途时也应立即转发滚轮，不等候图像处理');
    h.captureGate = null; releaseSlowCapture(); await slowCapture;
    resetScrollCaptureSession();

    await begin(); wheel();
    const finishing = outputLongImage('copy');
    await advance(1500); await finishing;
    assert.equal(outputs.at(-1).height, 108);
    assert.deepEqual(outputs.at(-1).data, new Uint8ClampedArray([
        ...frame(0).data, ...frame(18).data.slice(72 * 36 * 4),
    ]), '输出必须保留末段像素而非只满足高度');

    // 原生穿透滚动不经过overlay wheel；点击后迟到的真实滚动也必须进入最终像素。
    await begin();
    setTimeout(() => { h.top = 18; }, 180);
    const coldStartedAt = clock;
    const nativeTailOutput = outputLongImage('pin');
    await advance(1500); await nativeTailOutput;
    assert.equal(outputs.at(-1).height, 108);
    assert.deepEqual(outputs.at(-1).data, new Uint8ClampedArray([
        ...frame(0).data, ...frame(18).data.slice(72 * 36 * 4),
    ]), '优化等待后不能漏掉首个unchanged之后才生效的原生滚动');

    const coldFinalizeMs = h.lastOutputAt - coldStartedAt;
    // 真正后台采集积累静止记录；停稳后点击只省掉重复探针，仍有两次新采集。
    await begin(); await advance(1000);
    const beforeStablePin = h.captures;
    const stableStartedAt = clock;
    const stablePin = outputLongImage('pin');
    await advance(1500); await stablePin;
    assert.equal(outputs.at(-1).height, 90);
    assert.equal(h.captures - beforeStablePin, 2);
    assert.ok(h.lastOutputAt - stableStartedAt < coldFinalizeMs, '停稳后的收尾应省去重复稳定检测');

    // 已走快路径时，点击180ms后才到来的原生滚动也不能漏尾。
    await begin(); await advance(1000);
    setTimeout(() => { h.top = 18; }, 180);
    const beforeNativeTail = h.captures;
    const stableNativeTail = outputLongImage('pin');
    await advance(1500); await stableNativeTail;
    assert.ok(h.captures - beforeNativeTail >= 3, '发现迟到运动后应重新确认');
    assert.deepEqual(outputs.at(-1).data, new Uint8ClampedArray([
        ...frame(0).data, ...frame(18).data.slice(72 * 36 * 4),
    ]));

    // 连续输入仍处于旧稳定等待时完成，必须采到最后位置。
    await begin(); wheel(); await advance(70); wheel();
    const duringSettle = outputLongImage('copy');
    await advance(1500); await duringSettle;
    assert.equal(outputs.at(-1).height, 126);

    // 完成进编辑与直接输出必须使用同一末段快照；重复点击不能重复输出。
    await begin(); wheel();
    const editingClient = session.trackingClient, editingPipeline = session.continuousCapture;
    session.scrollReplayFrames = [{frame: frame(0)}];
    session.scrollReplayBytes = frame(0).data.byteLength;
    const editing = enterScrollEdit();
    await advance(1500); await editing;
    assert.equal(session.scrollCapturePhase, 'editing');
    assert.equal(edits.at(-1).height, 108);
    assert.equal(editingClient.closed, true); assert.equal(editingPipeline.closed, true);
    assert.equal(session.trackingClient, null); assert.equal(session.continuousCapture, null);
    assert.deepEqual(session.scrollFrames, []); assert.deepEqual(session.scrollReplayFrames, []);
    assert.equal(session.scrollReplayBytes, 0);
    const editingCaptures = h.captures;
    await advance(1000);
    assert.equal(h.captures, editingCaptures, '编辑态不能继续后台采集');
    h.output = () => { throw new Error('edited output failed'); };
    await assert.rejects(outputLongImage('save'), /edited output failed/);
    assert.equal(session.scrollCapturePhase, 'editing');
    assert.equal(session.trackingClient, null, '编辑态输出失败应保留编辑底图，不重建采集 Worker');
    h.output = image => outputs.push(image);
    const count = outputs.length;
    await Promise.all([outputLongImage('save'), outputLongImage('copy')]);
    assert.equal(outputs.length, count + 1);
    assert.deepEqual(outputs.at(-1).data, edits.at(-1).data, '采集资源释放后仍从编辑器底图输出');

    // 编辑器接管失败前不能丢弃采集片段；恢复后仍能完成同一长图。
    await begin(); wheel();
    const enterEditor = ss._enterCanvasImageEditor;
    ss._enterCanvasImageEditor = () => { throw new Error('editor unavailable'); };
    const failedEdit = assert.rejects(enterScrollEdit(), /editor unavailable/);
    await advance(1500); await failedEdit;
    assert.equal(session.scrollCapturePhase, 'capturing');
    assert.ok(session.scrollFrames.length > 0);
    assert.equal(session.trackingClient.closed, false);
    ss._enterCanvasImageEditor = enterEditor;
    const retriedEdit = enterScrollEdit();
    await advance(1500); await retriedEdit;
    assert.equal(edits.at(-1).height, 108);
    assert.equal(session.scrollCapturePhase, 'editing');

    // 输出失败必须恢复到可重试状态，保留已确认尾段。
    await begin(); wheel();
    h.output = () => { throw new Error('output failed'); };
    const failedOutput = assert.rejects(outputLongImage('pin'), /output failed/);
    await advance(1500); await failedOutput;
    assert.equal(session.scrollCapturePhase, 'capturing');
    assert.equal(session.captureFinalizing, false);
    assert.equal(session.outputBusy, false);
    assert.equal(session.scrollCurrentTop, 18);
    h.output = image => outputs.push(image);

    // 抓帧 IPC 迟到：旧会话的 finally/结果不能清空或改变新会话。
    await begin();
    let release;
    h.captureGate = new Promise(resolve => { release = resolve; });
    wheel(); await advance(250);
    resetScrollCaptureSession(); h.captureGate = null;
    await begin();
    const top = session.scrollCurrentTop;
    session._scrollCapturing = true;
    release(); await tick();
    assert.equal(session.scrollCurrentTop, top);
    assert.equal(session._scrollCapturing, true);
    session._scrollCapturing = false;

    // 旧手动任务的 wheelVersion 恰好与新会话相同，也不能清掉新输入意图。
    h.captureGate = new Promise(resolve => { release = resolve; });
    wheel(); await advance(250);
    resetScrollCaptureSession(); h.captureGate = null;
    await begin(); wheel();
    const startedAt = session.scrollWheelStartedAtMs;
    release(); await tick();
    assert.equal(session.scrollPendingDirection, 1);
    assert.equal(session.scrollWheelStartedAtMs, startedAt);

    // 自动滚动在稳定等待中暂停，旧任务退出后手动采集仍能推进。
    resetScrollCaptureSession(); await begin();
    await toggleAutoScroll();
    assert.equal(session._scrollCapturing, true);
    await toggleAutoScroll();
    await advance(300);
    wheel(); await advance(700);
    assert.equal(session.scrollCurrentTop, h.top, '暂停自动滚动后不能永久阻塞手动采集');
    assert.equal(session._scrollCapturing, false);

    // 背景采样已经提交运动后，自动模式不能把后续 unchanged 误判成到底。
    resetScrollCaptureSession(); await begin(); await toggleAutoScroll();
    await advance(1000);
    assert.ok(session.scrollCurrentTop >= 90);
    assert.equal(session.autoScroll, true);
    await toggleAutoScroll();

    // 总范围超限拒绝必须是事务式的，不能先推进坐标/关键帧。
    resetScrollCaptureSession(); await begin();
    h.limits = {maxHeight: 90, maxFrames: 200};
    resetScrollCaptureSession(); await begin();
    const before = {top: session.scrollCurrentTop, frames: session.scrollFrames,
        keyframes: session.scrollKeyframes, last: session.scrollLastFrame};
    wheel(); await advance(400);
    assert.equal(session.scrollCurrentTop, before.top);
    assert.equal(session.scrollFrames, before.frames);
    assert.equal(session.scrollKeyframes, before.keyframes);
    assert.equal(session.scrollLastFrame, before.last);
    assert.ok(hints.some(hint => hint.includes('上限')));
    resetScrollCaptureSession();
} finally {
    globalThis.setTimeout = originalSetTimeout;
    globalThis.clearTimeout = originalClearTimeout;
}
console.log('scroll lifecycle tests passed');
