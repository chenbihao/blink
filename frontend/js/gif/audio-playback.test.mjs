import test from 'node:test';
import assert from 'node:assert/strict';
globalThis.window ||= {};
const {locateAudio, createAudioPlayback} = await import('./audio-playback.js');
const {enabledSources, meterLevel} = await import('../screenshot/ss-gif-audio.js');
test('四种模式与电平分贝门限', () => {
    assert.deepEqual(['silent','microphone','system','both'].map(enabledSources), [[false,false],[true,false],[false,true],[true,true]]);
    assert.equal(meterLevel(0), 0); assert.equal(meterLevel(1), 1); assert.equal(meterLevel(10), 1);
    assert.ok(meterLevel(0.5) > meterLevel(0.1));
});
test('修剪/删帧后的原始区间、速度与新增停留静音', () => {
    const spans = [{sourceMs: 100, soundMs: 100, silenceMs: 200, outputMs: 150}, {sourceMs: 500, soundMs: 100, silenceMs: 0, outputMs: 50}];
    assert.deepEqual(locateAudio(spans, 25), {span: 0, sourceMs: 150, rate: 2, silent: false});
    assert.deepEqual(locateAudio(spans, 75), {span: 0, sourceMs: 200, rate: 2, silent: true});
    assert.deepEqual(locateAudio(spans, 175), {span: 1, sourceMs: 550, rate: 2, silent: false});
    assert.equal(locateAudio(spans, 200), null);
});
test('播放器只从统一时钟同步、静音/暂停不播放，释放旧会话', async () => {
    const elements = [];
    globalThis.Audio = class {
        constructor() { this.currentTime = 0; this.events = {}; this.paused = true; elements.push(this); }
        addEventListener(name, cb) { this.events[name] = cb; }
        load() { queueMicrotask(() => this.events.loadedmetadata?.()); }
        pause() { this.paused = true; }
        play() { this.paused = false; return Promise.resolve(); }
        removeAttribute() {}
    };
    globalThis.AudioContext = class {
        constructor() { this.state = 'running'; this.destination = {}; }
        createMediaElementSource() { return {connect(g) {return g;}, disconnect() {}}; }
        createGain() { return {gain: {value: 1}, connect() {}, disconnect() {}}; }
        suspend() { this.state = 'suspended'; return Promise.resolve(); }
        resume() { this.state = 'running'; return Promise.resolve(); }
        close() { return Promise.resolve(); }
    };
    window.__TAURI__ = {core: {invoke: async () => new ArrayBuffer(44)}};
    const p = createAudioPlayback(); await p.load('one', [true, true]);
    const spans = [{sourceMs: 1000, soundMs: 100, silenceMs: 100, outputMs: 100}];
    const mix = [{gain: 1, muted: false}, {gain: 0.5, muted: true}];
    p.sync(25, true, spans, mix);
    assert.equal(elements[0].currentTime, 1.05); assert.equal(elements[0].playbackRate, 2);
    assert.equal(elements[0].preservesPitch, true); assert.equal(elements[0].paused, false); assert.equal(elements[1].paused, true);
    p.sync(75, true, spans, mix); assert.equal(elements[0].paused, true);
    p.sync(25, false, spans, mix); assert.equal(elements[0].paused, true);
    const paged = (sourceMs,key,outputMs=11) => [{sourceMs,key,speed:1.5,soundMs:17,silenceMs:0,outputMs}];
    p.sync(0,true,paged(2000,'rev:0'),mix);
    assert.equal(elements[0].playbackRate,1.5);
    elements[0].currentTime=2.017;
    p.sync(0,true,paged(2034,'rev:17'),mix);
    assert.equal(elements[0].currentTime,2.034,'a deleted 17ms gap must seek even below the drift tolerance');
    p.sync(0,true,paged(2051,'rev:17',12),mix);
    assert.equal(elements[0].playbackRate,1.5,'millisecond frame rounding must not modulate audio playback rate');
    p.clear(); assert.ok(elements.every(e => e.paused));
    delete window.__TAURI__;
});
