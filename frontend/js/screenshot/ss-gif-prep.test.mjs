import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import {createRecordingSession} from './recording-session.js';

function harness() {
    const handlers = new Map();
    const nodes = new Map();
    const state=createRecordingSession();state.reset('ready');
    const classes = new Set();
    const toolbar = {classList: {contains: c => classes.has(c), remove: c => classes.delete(c)}};
    nodes.set('gif-prep-fps', {value: '15'});
    nodes.set('btn-gif', {focus() { this.focused = true; }});
    const ss = {toolbar, selCss: {x: 0, y: 0, w: 100, h: 100}, sent: false};
    const context = vm.createContext({ss,state,renderRecordingUi() {},handleOverlayHidden(){state.reset();}, window: {addEventListener: (k, fn) => handlers.set(k, fn)},
        leaveRecordingPreparation() {}, document: {body: {classList: {contains: () => false}}, getElementById: id => nodes.get(id),
            querySelectorAll: () => []}, positionToolbar() {}, bindAudioControls() {}, bindRecordingControls() {}, retainToolbarAnchor() {}, closeRecordingPanel() {}, showRecordingMessage() {}, stopAudioMonitor: async () => {}});
    const source = readFileSync(new URL('./ss-gif-prep.js', import.meta.url), 'utf8')
        .replace(/^import .*;\r?$/gm, '').replace(/export /g, '');
    vm.runInContext(source, context);
    context.bindGifPrepBar();
    return {ss, state, nodes, classes, context, escape: handlers.get('keydown')};
}

test('capture Escape returns from recording prep with focused controls, retaining the selection', () => {
    const h = harness();
    let prevented = false;
    let stopped = false;
    h.escape({key: 'Escape', target: {tagName: 'SELECT'}, preventDefault() { prevented = true; },
        stopImmediatePropagation() { stopped = true; }});
    assert.equal(h.state.mode,'screenshot');
    assert.equal(h.nodes.get('btn-gif').focused, true);
    assert.ok(h.ss.selCss);
    assert.equal(prevented && stopped, true);
});

test('Escape during recording startup leaves its lifecycle in control', () => {
    const h = harness();
    h.ss.sent = true; h.state.setMode('starting');
    h.escape({key: 'Escape'});
    assert.equal(h.state.mode,'starting');
});

test('a late startup result cancels only its original backend session after the UI has reset', async () => {
    const h=harness(),calls=[];
    let resolveStart;
    Object.assign(h.context,{
        annotationsMigratable:()=>true,
        cssRectToBitmap:r=>r,
        cssPointToScreen:(x,y)=>({x,y}),
        audioOptions:()=>({mode:'silent'}),
        invoke:async(command,args)=>{
            calls.push({command,args});
            if(command==='gif_start_recording') return new Promise(resolve=>{resolveStart=resolve;});
        },
        enterGifRecordOverlay:()=>assert.fail('stale startup must not enter another session'),
    });
    const pending=h.context.startGifRecordingFromPrep();
    await new Promise(setImmediate);
    assert.equal(h.state.mode,'starting');
    h.state.reset('ready');h.state.sessionId='new';h.state.setMode('recording');
    resolveStart({sessionId:'old'});
    await pending;
    assert.equal(h.state.sessionId,'new');
    assert.equal(h.state.mode,'recording');
    assert.equal(calls[1].command,'gif_cancel_recording');
    assert.equal(calls[1].args.sessionId,'old');
});
