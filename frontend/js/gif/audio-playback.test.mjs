import test from 'node:test';
import assert from 'node:assert/strict';
globalThis.window ||= {};
const {locateAudio, createAudioPlayback, softLimit, mixLimiterCurve} = await import('./audio-playback.js');
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
    const gains = [], shapers = [], connections = [];
    const node = extra => ({...extra, connect(target) { connections.push([this, target]); return target; }, disconnect() {}});
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
        createGain() { const n = node({gain: {value: 1, setTargetAtTime(value) { this.value = value; }}}); gains.push(n); return n; }
        createWaveShaper() { const n = node({}); shapers.push(n); return n; }
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
    assert.equal(gains[0].gain.value, 0.25);
    assert.equal(shapers.length, 1);
    assert.equal(shapers[0].oversample, '4x');
    assert.ok(connections.some(([from,to]) => from === gains[1] && to === gains[0]), '音轨必须先进入同一混音总线');
    const louder = [{gain: 2, muted: false}, {gain: 2, muted: false}];
    p.sync(26, true, spans, louder);
    assert.equal(shapers.length, 1, '两路混音只限幅一次');
    assert.equal(gains[1].gain.value, 2);
    assert.equal(gains[2].gain.value, 2);
    assert.ok(connections.some(([from,to]) => from === gains[2] && to === gains[0]));
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

test('双源200%混音软限幅有界，普通音量保持线性，曲线与后端数值一致', () => {
    // 相同输入/输出锚点也由 Rust 测试校验，防止预览/导出曲线漂移。
    const fixtures = [[0,0], [0.5,0.5], [0.9,0.9], [1,0.957079616], [2,0.979999915], [4,0.98]];
    const curve = mixLimiterCurve();
    for (const [x,y] of fixtures) {
        assert.ok(Math.abs(softLimit(x)-y)<1e-7);
        for (const sign of [-1,1]) {
            const index = (x*sign/4+1)/2*(curve.length-1), a = Math.floor(index), f = index-a;
            const rendered = curve[a]*(1-f)+(curve[Math.min(a+1,curve.length-1)])*f;
            assert.ok(Math.abs(rendered-y*sign)<1e-6);
        }
    }
    assert.equal(softLimit(NaN), 0);
    for (let i=1; i<curve.length; i++) {
        assert.ok(curve[i]>=curve[i-1]); assert.ok(Math.abs(curve[i])<=0.980001);
    }
});

test('rapid pause and enable serialize context transitions and ignore an old aborted play',async()=>{
    const elements=[],contexts=[],errors=[],plays=[];
    const flush=async()=>{for(let i=0;i<8;i++)await Promise.resolve();};
    globalThis.Audio=class{
        constructor(){this.events={};this.currentTime=0;this.paused=true;elements.push(this);}
        addEventListener(k,fn){this.events[k]=fn;}load(){queueMicrotask(()=>this.events.loadedmetadata?.());}
        pause(){this.paused=true;}removeAttribute(){}
        play(){this.paused=false;return new Promise((resolve,reject)=>plays.push({resolve,reject}));}
    };
    const node=()=>({gain:{value:1,setTargetAtTime(){}},connect(other){return other;},disconnect(){}});
    globalThis.AudioContext=class{
        constructor(){this.state='running';this.destination={};this.suspends=[];this.resumes=[];contexts.push(this);}
        createGain(){return node();}createWaveShaper(){return node();}createMediaElementSource(){return node();}
        suspend(){return new Promise(resolve=>this.suspends.push(()=>{this.state='suspended';resolve();}));}
        resume(){return new Promise(resolve=>this.resumes.push(()=>{this.state='running';resolve();}));}
        close(){this.state='closed';return Promise.resolve();}
    };
    const p=createAudioPlayback({onError:e=>errors.push(e)});
    await p.load('toggle',[true,false]);
    const spans=[{key:'one',sourceMs:0,speed:1,soundMs:1000,silenceMs:0,outputMs:1000}], mix=[{gain:1,muted:false}];
    p.sync(10,true,spans,mix);
    const context=contexts[0];
    p.pause();assert.equal(context.suspends.length,1);
    p.sync(20,true,spans,mix);assert.equal(plays.length,2);
    plays[0].reject({name:'AbortError'});await flush();
    p.sync(21,true,spans,mix);assert.equal(plays.length,2,'an old rejection must not clear a newer play attempt');
    context.suspends[0]();await flush();
    assert.equal(context.resumes.length,1,'the late suspension must be followed by the requested resume');
    for(let i=0;i<10;i++)p.sync(22+i,true,spans,mix);
    assert.equal(context.resumes.length,1,'progress ticks coalesce while resume is pending');
    context.resumes[0]();plays[1].resolve();await flush();
    assert.equal(context.state,'running');assert.equal(elements[0].paused,false);assert.equal(errors.length,0);
    for(let i=0;i<3;i++) {
        p.sync(40+i,false,spans,mix);assert.equal(elements[0].paused,true);
        p.sync(50+i,true,spans,mix);assert.equal(elements[0].currentTime,(50+i)/1000);
        plays.at(-1).resolve();await flush();
        assert.equal(elements[0].paused,false);
    }
    assert.equal(context.suspends.length,1,'preview muting does not suspend the still-playing context');
    p.dispose();
});
