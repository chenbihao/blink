//! 录制叠层模式纯逻辑测试（node --test）。

import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import {createRecordingSession} from './recording-session.js';

// 模块导入链（ss-display/ss-toolbar/shared）在顶层访问 window——
// 先注入最小 stub 再动态导入。
globalThis.window = globalThis.window || {devicePixelRatio: 1, __blinkScreenMeta: {vx: 0, vy: 0}};
const {placeTextOutsideRegion, unsupportedKindsOf, recordingFrameGeometry, recordingDragDelta} = await import('./ss-gif-rec.js');

test('the shared grip applies one clamped delta to capture area and toolbar',()=>{
    const region={x:100,y:80,w:640,h:300},viewport={w:1920,h:1080};
    assert.deepEqual(recordingDragDelta(region,20,30,viewport),{dx:20,dy:30});
    assert.deepEqual(recordingDragDelta(region,-500,-300,viewport),{dx:-100,dy:-80});
    assert.deepEqual(recordingDragDelta(region,2000,2000,viewport),{dx:1180,dy:700});
});

test('capture dragging coalesces native updates and cannot move a replacement recording',async()=>{
    for(const replace of [false,true]) {
        const state=createRecordingSession();state.reset('ready');state.sessionId='one';state.setMode('recording');
        state.regionPhys={x:100,y:80,w:640,h:300};
        const calls=[];let resolveMove;
        const ss={selCss:{x:100,y:80,w:640,h:300}};
        const context=vm.createContext({ss,state,window:{innerWidth:1920,innerHeight:1080,__blinkScreenMeta:{}},
            document:{getElementById:()=>null,querySelectorAll:()=>[]},
            closeRecordingPanel(){},retainToolbarAnchor(){},renderRecordingUi(){},selectTool(){},
            getRenderScale:()=>({scaleX:1,scaleY:1}),cssPointToScreen:(x,y)=>({x,y}),
            screenRectToCss:r=>r,clearInterval(){},setInterval:()=>0,showRecordingMessage:assert.fail,
            invoke:async(cmd,args)=>{
                calls.push({cmd,args});
                if(cmd==='gif_move_recording_region' && !resolveMove) await new Promise(r=>{resolveMove=r;});
                if(cmd==='gif_recorder_state') return {sessionId:'one',region:{x:300,y:80,w:640,h:300}};
            },
        });
        const source=readFileSync(new URL('./ss-gif-rec.js',import.meta.url),'utf8').replace(/^import .*;\r?$/gm,'').replace(/export /g,'');
        vm.runInContext(source,context);
        const drag=context.beginRecordingGripDrag();
        drag.move(50,0);drag.move(100,0);drag.move(200,0);
        const ending=drag.end();
        if(replace) {state.reset('ready');state.sessionId='two';state.setMode('recording');}
        resolveMove();await ending;
        const moves=calls.filter(c=>c.cmd==='gif_move_recording_region');
        assert.equal(moves.length,replace?1:2);
        assert.equal(moves[0].args.sessionId,'one');
        if(!replace) assert.equal(moves[1].args.x,300);
        else assert.equal(calls.some(c=>c.cmd==='gif_recorder_state'),false);
    }
});

test('unsupportedKindsOf：底图依赖效果去重收集，水印独立计入', () => {
    assert.deepEqual(
        unsupportedKindsOf([
            {type: 'pencil'}, {type: 'blur'}, {type: 'pixelate'}, {type: 'blur'}, {type: 'arrow'},
        ], false),
        ['blur', 'pixelate'],
    );
    assert.deepEqual(unsupportedKindsOf([{type: 'text'}], true), ['watermark']);
    assert.deepEqual(unsupportedKindsOf([{type: 'rect'}, {type: 'eraser'}], false), []);
    assert.deepEqual(unsupportedKindsOf([], false), []);
    // 录制可用的绘制工具不误报
    assert.deepEqual(
        unsupportedKindsOf([
            {type: 'pencil'}, {type: 'arrow'}, {type: 'rect'}, {type: 'ellipse'},
            {type: 'highlight-multiply'}, {type: 'highlight-translucent'},
            {type: 'text'}, {type: 'number'}, {type: 'eraser'},
        ], false),
        [],
    );
});

test('placeTextOutsideRegion：区域内挪到正下方，区域外保持原位', () => {
    const sel = {x: 100, y: 100, w: 400, h: 300};
    // 区域内（含边界）→ 区域正下方 8px
    assert.deepEqual(placeTextOutsideRegion({x: 150, y: 200}, sel), {x: 150, y: 408});
    assert.deepEqual(placeTextOutsideRegion({x: sel.x, y: sel.y}, sel), {x: 100, y: 408});
    assert.deepEqual(placeTextOutsideRegion({x: sel.x + sel.w, y: sel.y + sel.h}, sel), {x: 500, y: 408});
    // 区域外原位
    assert.deepEqual(placeTextOutsideRegion({x: 50, y: 200}, sel), {x: 50, y: 200});
    assert.deepEqual(placeTextOutsideRegion({x: 150, y: 500}, sel), {x: 150, y: 500});
    // 无选区原样返回
    assert.deepEqual(placeTextOutsideRegion({x: 10, y: 10}, null), {x: 10, y: 10});
});

test('text input stays visible at the bottom edge, in fullscreen and across scaled viewports', () => {
    const viewport = {w: 1920, h: 1080}, size = {w: 320, h: 32};
    assert.deepEqual(placeTextOutsideRegion({x: 200, y: 940}, {x: 100, y: 900, w: 800, h: 180}, viewport, size),
        {x: 200, y: 860});
    assert.deepEqual(placeTextOutsideRegion({x: 1900, y: 1070}, {x: 0, y: 0, w: 1920, h: 1080}, viewport, size),
        {x: 1592, y: 1040});
    for (const viewport of [{w: 960, h: 540}, {w: 2560, h: 720}]) {
        const point = placeTextOutsideRegion({x: viewport.w - 1, y: viewport.h - 1},
            {x: 0, y: 0, ...viewport}, viewport, size);
        assert.ok(point.x >= 8 && point.x + size.w <= viewport.w - 8);
        assert.ok(point.y >= 8 && point.y + size.h <= viewport.h - 8);
    }
});

function textFixture(initialPhase = 'recording') {
    const calls = [], paints = [];
    let phase = initialPhase;
    const snapshot = () => ({sessionId: 'one', phase, audio: {}, storage: {}});
    const context = vm.createContext({state:createRecordingSession(), renderRecordingUi() {}, t:key=>key,
        ss: {selCss: {x: 0, y: 0, w: 1920, h: 1080}},
        document: {getElementById: () => null}, performance, setTimeout,
        requestAnimationFrame: fn => paints.push(fn), applyAudioStatus() {},
        showTransientHint() {}, normalizeError: e => e,
        invoke: async (cmd, args) => { calls.push({cmd, args}); if (cmd === 'gif_recorder_state') return snapshot(); },
    });
    const source = readFileSync(new URL('./ss-gif-rec.js', import.meta.url), 'utf8')
        .replace(/^import .*;\r?$/gm, '').replace(/export /g, '');
    vm.runInContext(`${source}\nglobalThis.fixture = {state, openRecordingTextInput};`, context);
    Object.assign(context.fixture.state, {mode:initialPhase, sessionId:'one'});
    return {...context.fixture, calls, paints, snapshot, setPhase: value => {phase = value;}};
}

const flush = () => new Promise(resolve => setImmediate(resolve));
async function flushPaints(f) { await flush(); while (f.paints.length) { f.paints.shift()(); await flush(); } }

test('fullscreen text waits for the real pause and only resumes after the input has disappeared', async () => {
    const f = textFixture();
    const open = f.openRecordingTextInput({x: 100, y: 100}, {w: 800, h: 32});
    await flush();
    assert.equal(f.calls.some(c => c.cmd === 'gif_pause_recording'), true);
    assert.equal(f.calls.some(c => c.cmd === 'gif_overlay_focusable'), false);
    f.setPhase('paused');
    const release = await open;
    assert.equal(f.calls.at(-1).args.focusable, true);
    const close = release();
    await flush();
    assert.equal(f.calls.some(c => c.cmd === 'gif_resume_recording'), false);
    await flushPaints(f); await close;
    assert.equal(f.calls.at(-1).cmd, 'gif_resume_recording');
});

test('text cleanup preserves manual pauses and never resumes a replaced recording session', async () => {
    const f = textFixture('paused');
    const release = await f.openRecordingTextInput({x: 100, y: 100}, {w: 800, h: 32});
    await release();
    assert.equal(f.calls.some(c => c.cmd === 'gif_pause_recording' || c.cmd === 'gif_resume_recording'), false);
    const staleRelease = await f.openRecordingTextInput({x: 100, y: 100}, {w: 800, h: 32});
    f.state.sessionId = 'two';
    const count = f.calls.length;
    await staleRelease();
    assert.equal(f.calls.length, count);
});

test('opening another text input during cleanup postpones the automatic resume', async () => {
    const f = textFixture();
    const open = f.openRecordingTextInput({x: 100, y: 100}, {w: 800, h: 32});
    await flush(); f.setPhase('paused');
    const release = await open;
    const close = release(); await flush();
    const releaseNext = await f.openRecordingTextInput({x: 200, y: 100}, {w: 800, h: 32});
    await flushPaints(f); await close;
    assert.equal(f.calls.some(c => c.cmd === 'gif_resume_recording'), false);
    const closeNext = releaseNext(); await flushPaints(f); await closeNext;
    assert.equal(f.calls.filter(c => c.cmd === 'gif_resume_recording').length, 1);
});

test('录制就绪等待透明层绘制；旧会话 idle 不清理新会话，结束时释放定时器', async () => {
    const classes = () => {
        const values = new Set();
        return {add: (...cs) => cs.forEach(c => values.add(c)),
            remove: (...cs) => cs.forEach(c => values.delete(c)),
            contains: c => values.has(c)};
    };
    const toolbar = {classList: classes(), dataset: {}, setAttribute() {},
        getBoundingClientRect: () => ({x: 0, y: 200, width: 200, height: 40})};
    const body = {classList: classes()};
    const root = {classList: classes()};
    const paints = [], calls = [], timers = new Set();
    let timerId = 0;
    const recordingState=createRecordingSession();
    const context = vm.createContext({state:recordingState, renderRecordingUi() {if(recordingState.active) root.classList.add('gif-recording');else root.classList.remove('gif-recording');},
        ss: {toolbar, selCss: {x: 0, y: 0, w: 100, h: 100}},
        window: {__blinkScreenMeta: {vx: 0, vy: 0, w: 1920, h: 1080}},
        document: {body, documentElement: root, getElementById: () => null, querySelectorAll: () => []},
        annot: {getTool: () => 'pencil'}, positionToolbar() {}, selectTool() {},
        beginAudioRecording: async () => {}, endAudioRecording() {}, applyAudioStatus() {},
        retainToolbarAnchor() {}, closeRecordingPanel() {}, showRecordingMessage() {}, t:key=>key,
        getRenderScale: () => ({scaleX: 1, scaleY: 1}), cssPointToScreen: (x, y) => ({x, y}),
        listen: async () => () => {}, EVENTS: {}, performance: {now: () => 1000},
        setInterval: () => { timers.add(++timerId); return timerId; },
        clearInterval: id => timers.delete(id), clearTimeout() {},
        requestAnimationFrame: fn => paints.push(fn),
        invoke: async (cmd, args) => {
            calls.push({cmd, args});
            if (cmd === 'gif_recorder_state') return {sessionId: 'new', phase: 'recording', elapsedMs: 0};
        },
    });
    const source = readFileSync(new URL('./ss-gif-rec.js', import.meta.url), 'utf8')
        .replace(/^import .*;\r?$/gm, '').replace(/export /g, '');
    vm.runInContext(`${source}\nglobalThis.fixture = {state, enterGifRecordOverlay, applyRecorderState};`, context);
    const f = context.fixture;
    const task = f.enterGifRecordOverlay({x: 0, y: 0, w: 100, h: 100}, 'new');
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(root.classList.contains('gif-recording'), true);
    assert.equal(calls.some(c => c.cmd === 'gif_overlay_ready'), false);
    paints.shift()();
    assert.equal(calls.some(c => c.cmd === 'gif_overlay_ready'), false);
    paints.shift()();
    await task;
    assert.deepEqual(calls.map(c => c.cmd), ['gif_overlay_input', 'gif_overlay_ready', 'gif_recorder_state']);
    f.state.cancelArmed = true;
    f.applyRecorderState({sessionId: 'old', phase: 'idle'});
    assert.equal(f.state.active, true);
    assert.equal(timers.size, 2);
    f.applyRecorderState({sessionId: 'new', phase: 'idle'});
    assert.equal(f.state.active, false);
    assert.equal(f.state.cancelArmed, false);
    assert.equal(timers.size, 0);
    assert.equal(root.classList.contains('gif-recording'), false);
});


function recoveryFixture() {
    let respond;
    const classes = {remove() {}, add() {}, toggle() {}};
    const context = vm.createContext({state:createRecordingSession(), renderRecordingUi() {}, t:key=>key,
        ss:{_loadGen:1}, document:{getElementById:()=>null,body:{classList:classes},documentElement:{classList:classes}},
        annot:{setRenderListener() {}}, performance, clearInterval() {}, clearTimeout() {},
        endAudioRecording() {}, closeRecordingPanel() {}, showRecordingMessage() {}, applyAudioStatus() {},
        invoke:()=>new Promise(resolve=>{respond=resolve;}),
    });
    const source=readFileSync(new URL('./ss-gif-rec.js',import.meta.url),'utf8').replace(/^import .*;\r?$/gm,'').replace(/export /g,'');
    vm.runInContext(`${source}\nglobalThis.fixture={state,restoreRecordingOverlay,applyRecorderState};`,context);
    const f=context.fixture;
    Object.assign(f.state,{mode:'recording',sessionId:'current',snapshotRevision:5});
    return {...f,respond:value=>respond(value)};
}

test('returning to recording preserves a newer pause event than an in-flight state response', async () => {
    const f=recoveryFixture();
    const task=f.restoreRecordingOverlay();
    f.applyRecorderState({sessionId:'current',revision:7,phase:'paused',elapsedMs:2000});
    f.respond({sessionId:'current',revision:6,phase:'recording',elapsedMs:1900,region:{x:0,y:0,w:100,h:100}});
    assert.equal(await task,true);
    assert.equal(f.state.mode,'paused');
    assert.equal(f.state.elapsedBase,2000);
});

test('old restore responses cannot replace a new recording, and missed idle events release stale controls', async () => {
    const f=recoveryFixture();
    const task=f.restoreRecordingOverlay();
    f.state.sessionId='replacement';
    f.respond({sessionId:'current',revision:6,phase:'recording',region:{x:0,y:0,w:100,h:100}});
    assert.equal(await task,true);
    assert.equal(f.state.sessionId,'replacement');
    const cleanup=f.restoreRecordingOverlay();
    f.respond({phase:'idle',revision:9});
    assert.equal(await cleanup,false);
    assert.equal(f.state.active,false);
});

test('recording frame remains entirely outside capture pixels at fractional and mixed DPI', () => {
    const region = {x: -1000, y: 100, w: 800, h: 600};
    for (const dpi of [96, 120, 144, 192]) for (const scale of [1, 1.25, 1.5, 2]) {
        const meta = {vx: -1920, vy: 0, physicalDisplays: [{x:-1920,y:0,w:1920,h:1080,dpi}]};
        const result = recordingFrameGeometry(region, meta, scale, scale);
        const value = key => parseFloat(result[key]) * scale;
        assert.equal(value('left') + value('borderLeftWidth'), region.x - meta.vx);
        assert.equal(value('top') + value('borderTopWidth'), region.y);
        assert.equal(value('left') + value('width') - value('borderRightWidth'), region.x - meta.vx + region.w);
        assert.equal(value('top') + value('height') - value('borderBottomWidth'), region.y + region.h);
    }
});
