import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';

const load = async (name) => import('data:text/javascript;base64,'
    + Buffer.from(await readFile(new URL(name, import.meta.url))).toString('base64'));
const {ScrollCaptureSession} = await load('./session.js');
const {finalizeScrollCapture, recordTailStability} = await load('./finalize.js');
const deferred = () => {
    let resolve;
    const promise = new Promise((done) => { resolve = done; });
    return {promise, resolve};
};
function setup(results = [{moved: true}, {reason: 'unchanged'}]) {
    const session = new ScrollCaptureSession();
    session.scrollCapturePhase = 'capturing';
    const order = [];
    const options = {
        waitForSettle: async () => { order.push('settle'); return {stable: true}; },
        captureFrame: async () => { order.push('capture'); return results.shift() || {reason: 'unchanged'}; },
        delay: async () => {},
    };
    return {session, order, options};
}

// 完成发生在防抖阶段：必须取消旧 timer 并主动抓尾帧，再复核静止。
{
    const {session, order, options} = setup();
    session._scrollCaptureTimer = setTimeout(() => assert.fail('旧防抖任务不得执行'), 1000);
    const result = await finalizeScrollCapture(session, options);
    assert.equal(result.ok, true);
    assert.deepEqual(order, ['settle', 'capture', 'settle', 'capture']);
    assert.equal(session.scrollCapturePhase, 'finalizing');
    assert.equal(session._scrollCaptureTimer, 0);
}
// 已发出的 wheel 和抓帧未结束：收尾不得与它们并发；重复完成共用同一任务。
{
    const {session, order, options} = setup();
    const wheel = deferred();
    const frame = deferred();
    session.wheelForwardInFlight = wheel.promise;
    session.captureInFlight = frame.promise;
    const first = finalizeScrollCapture(session, options);
    assert.equal(finalizeScrollCapture(session, options), first);
    await Promise.resolve();
    assert.deepEqual(order, []);
    wheel.resolve(); frame.resolve();
    assert.equal((await first).ok, true);
}
// 二次定位确认不能被完成动作绕过。
{
    const {session, options, order} = setup([
        {reason: 'pending-confirmation'}, {moved: true}, {reason: 'unchanged'},
    ]);
    assert.equal((await finalizeScrollCapture(session, options)).ok, true);
    assert.equal(order.filter(x => x === 'capture').length, 3);
}
// 不可靠尾部不得输出；自动滚动停止，回到可重试采集态。
{
    const {session, options} = setup([{reason: 'no-overlap'}]);
    session.autoScroll = true;
    const result = await finalizeScrollCapture(session, options);
    assert.equal(result.ok, false);
    assert.equal(session.autoScroll, false);
    assert.equal(session.captureFinalizing, false);
    assert.equal(session.scrollCapturePhase, 'capturing');
}
// 总截止时间覆盖未返回的 IPC，超时后迟到结果必须过期。
{
    const {session, options} = setup();
    const waiting = deferred();
    session.captureInFlight = waiting.promise;
    let expire;
    options.setTimer = (fn) => { expire = fn; return 1; };
    options.clearTimer = () => {};
    const task = finalizeScrollCapture(session, options);
    const generation = session.captureGeneration;
    expire();
    assert.equal((await task).reason, 'timeout');
    assert.ok(session.captureGeneration > generation);
    waiting.resolve();
}
// 达到上限可明确截断输出，不应把用户困在无法完成的会话内。
{
    const {session, options} = setup([{reason: 'frame-limit'}]);
    assert.deepEqual(await finalizeScrollCapture(session, options), {ok: true, truncated: true});
    assert.equal(session.scrollCapturePhase, 'finalizing');
}
// 收尾的逐帧耗时必须从原输入起算，包含排队/等待，而非只测最后一次抓帧。
{
    const {session, options} = setup();
    session.scrollWheelStartedAtMs = 12;
    const capture = options.captureFrame;
    options.captureFrame = (...args) => {
        assert.equal(args[2].wheelStartedAtMs, 12);
        assert.equal(args[2].finalizing, true);
        return capture(...args);
    };
    assert.equal((await finalizeScrollCapture(session, options)).ok, true);
}
// 等待中退出再开启：旧收尾不得将新会话置为 finalizing，也不得清其 busy 标记。
{
    const {session, options, order} = setup();
    const waiting = deferred();
    options.waitForSettle = () => waiting.promise;
    const task = finalizeScrollCapture(session, options);
    await Promise.resolve(); await Promise.resolve();
    session.reset();
    session.scrollCapturePhase = 'capturing';
    session.captureFinalizing = true;
    waiting.resolve({stable: true});
    assert.equal((await task).ok, false);
    assert.deepEqual(order, []);
    assert.equal(session.scrollCapturePhase, 'capturing');
    assert.equal(session.captureFinalizing, true);
}
// 稳定探针已经观察足够久时，不再额外睡90ms；仍须两次独立尾帧确认。
{
    const {session, options, order} = setup([{reason: 'unchanged'}, {reason: 'unchanged'}]);
    let clock = 0;
    const delays = [];
    options.now = () => clock;
    options.waitForSettle = async () => { order.push('settle'); clock += 205; return {stable: true}; };
    const capture = options.captureFrame;
    options.captureFrame = (...args) => { clock += 70; return capture(...args); };
    options.delay = async ms => { delays.push(ms); clock += ms; };
    assert.equal((await finalizeScrollCapture(session, options)).ok, true);
    assert.equal(clock, 550, '两轮205ms稳定观察和70ms尾帧确认不应再叠加90ms');
    assert.deepEqual(delays, []);
    assert.equal(order.filter(x => x === 'capture').length, 2);
}
// 探针提前返回时只补足90ms间隔，迟到滚动仍要重新确认，不能把首个unchanged当结束。
{
    const {session, options} = setup();
    let clock = 0, top = 0;
    const captures = [], delays = [];
    options.now = () => clock;
    options.waitForSettle = async () => { clock += 10; return {stable: true}; };
    options.captureFrame = async () => {
        captures.push(top); clock += 5;
        return captures.length === 2 ? {moved: true} : {reason: 'unchanged'};
    };
    options.delay = async ms => { delays.push(ms); clock += ms; top = 18; };
    assert.equal((await finalizeScrollCapture(session, options)).ok, true);
    assert.deepEqual(captures, [0, 18, 18]);
    assert.deepEqual(delays, [80, 80], '只补足观察间隔，不能省掉短探针后的等待');
}
// 补足间隔期间退出，旧收尾不得再发尾帧请求。
{
    const {session, options, order} = setup([{reason: 'unchanged'}]);
    options.now = () => 0;
    options.delay = async () => { session.reset(); session.scrollCapturePhase = 'capturing'; };
    assert.equal((await finalizeScrollCapture(session, options)).reason, 'aborted');
    assert.equal(order.filter(x => x === 'capture').length, 1);
}
function stableHistory(session) {
    for (const at of [0, 100, 200]) recordTailStability(session,
        {decision: {reason: 'unchanged'}, trackingState: 'tracking'},
        {captureStartedAt: at, wheelVersion: session.manualWheelVersion});
}
// 最近持续静止时复用一轮观察；仍抓两张新尾帧，另一轮探针照常验证。
{
    const {session, options, order} = setup([{reason: 'unchanged'}, {reason: 'unchanged'}]);
    stableHistory(session);
    let clock = 250;
    options.now = () => clock;
    options.waitForSettle = async () => { order.push('settle'); clock += 205; return {stable: true}; };
    const capture = options.captureFrame;
    options.captureFrame = (...args) => { clock += 70; return capture(...args); };
    options.delay = async ms => { clock += ms; };
    assert.equal((await finalizeScrollCapture(session, options)).ok, true);
    assert.deepEqual(order, ['capture', 'settle', 'capture']);
    assert.equal(clock - 250, 345, '复用近期稳定观察后省去一轮205ms探针');
}
// 快路径仍须有点击后的观察窗口，迟到滚动会推进尾段并再确认一次。
{
    const {session, options, order} = setup([
        {reason: 'unchanged'}, {moved: true}, {reason: 'unchanged'},
    ]);
    stableHistory(session);
    let clock = 250;
    const capturedAt = [];
    options.now = () => clock;
    options.waitForSettle = async () => { order.push('settle'); clock += 125; return {stable: true}; };
    const capture = options.captureFrame;
    options.captureFrame = (...args) => { capturedAt.push(clock); return capture(...args); };
    options.delay = async ms => { clock += ms; };
    assert.equal((await finalizeScrollCapture(session, options)).ok, true);
    assert.deepEqual(capturedAt, [250, 430, 555]);
    assert.deepEqual(order, ['capture', 'settle', 'capture', 'settle', 'capture']);
}
// 证据过期、刚滚动、失去定位、待确认跳跃、自动滚动和短观察都必须完整检测。
for (const invalidate of [
    (session, options) => { options.now = () => 551; },
    session => { session.manualWheelVersion++; },
    session => { session.scrollTrackingState = 'lost'; },
    session => { session.scrollPendingJump = {}; },
    session => { session.autoScroll = true; },
    session => { session.tailStability.since = 100; },
]) {
    const {session, options, order} = setup([{reason: 'unchanged'}, {reason: 'unchanged'}]);
    stableHistory(session);
    options.now = () => 250;
    invalidate(session, options);
    options.captureFrame = async () => {
        order.push('capture'); session.scrollTrackingState = 'tracking'; session.scrollPendingJump = null;
        return {reason: 'unchanged'};
    };
    assert.equal((await finalizeScrollCapture(session, options)).ok, true);
    assert.deepEqual(order, ['settle', 'capture', 'settle', 'capture']);
}
// 稳定历史不能越过新尾帧的定位失败；快路径补时期间退出也必须中止。
{
    const {session, options, order} = setup([{reason: 'no-overlap'}]);
    stableHistory(session);
    options.now = () => 250;
    assert.equal((await finalizeScrollCapture(session, options)).ok, false);
    assert.deepEqual(order, ['capture']);
}
{
    const {session, options, order} = setup([{reason: 'unchanged'}]);
    stableHistory(session);
    options.now = () => 250;
    options.delay = async () => { session.reset(); session.scrollCapturePhase = 'capturing'; };
    assert.equal((await finalizeScrollCapture(session, options)).reason, 'aborted');
    assert.deepEqual(order, ['capture', 'settle']);
}
// 运动/拒绝/断档/旧输入结果不能累计稳定时间；新会话不继承历史。
{
    const session = new ScrollCaptureSession();
    const result = {decision: {reason: 'unchanged'}, trackingState: 'tracking'};
    for (const [changedResult, metadata] of [
        [{decision: {reason: 'moved'}, trackingState: 'tracking'}, {}],
        [{...result, pendingJump: {}}, {}],
        [{...result, trackingState: 'lost'}, {}],
        [result, {discontinuity: true}],
        [result, {wheelVersion: -1}],
        [result, {finalizing: true}],
    ]) {
        stableHistory(session);
        recordTailStability(session, changedResult, {captureStartedAt: 250, wheelVersion: 0, ...metadata});
        assert.equal(session.tailStability, null);
    }
    stableHistory(session);
    recordTailStability(session, result, {captureStartedAt: 551, wheelVersion: 0});
    assert.equal(session.tailStability.since, 551, '采集断档不能累计旧静止时长');
    session.reset();
    assert.equal(session.tailStability, null);
}
console.log('scroll finalization tests passed');
