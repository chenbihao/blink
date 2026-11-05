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
    nodes.set('gif-recording-recent', {addEventListener: (k, fn) => handlers.set('recent:' + k, fn)});
    const ss = {toolbar, selCss: {x: 0, y: 0, w: 100, h: 100}, sent: false, _loadGen: 1};
    const context = vm.createContext({ss,state,renderRecordingUi() {},handleOverlayHidden(){state.reset();}, window: {addEventListener: (k, fn) => handlers.set(k, fn)},
        console:{error() {}}, t:key=>key, normalizeError:error=>error, showTransientHint() {},
        leaveRecordingPreparation() {}, commitRecordingSelection() {}, drawStaticBase() {}, drawFinalSelection() {}, restorePreparationAnnotations() {}, waitForRecordingPreparation:async()=>{}, document: {body: {classList: {contains: () => false}}, getElementById: id => nodes.get(id),
            querySelectorAll: () => []}, positionToolbar() {}, bindAudioControls() {}, bindRecordingControls() {}, retainToolbarAnchor() {}, closeRecordingPanel() {}, showRecordingMessage() {}, stopAudioMonitor: async () => {}});
    const source = readFileSync(new URL('./ss-gif-prep.js', import.meta.url), 'utf8')
        .replace(/^import .*;\r?$/gm, '').replace(/export /g, '');
    vm.runInContext(source, context);
    context.bindGifPrepBar();
    return {ss, state, nodes, classes, context, escape: handlers.get('keydown')};
}

test('capture Escape returns from recording prep with focused controls, retaining the selection', () => {
    const h = harness();
    const restored=[];
    h.context.drawStaticBase=()=>restored.push('base');h.context.drawFinalSelection=()=>restored.push('selection');
    let prevented = false;
    let stopped = false;
    h.escape({key: 'Escape', target: {tagName: 'SELECT'}, preventDefault() { prevented = true; },
        stopImmediatePropagation() { stopped = true; }});
    assert.equal(h.state.mode,'screenshot');
    assert.equal(h.nodes.get('btn-gif').focused, true);
    assert.ok(h.ss.selCss);
    assert.equal(prevented && stopped, true);
    assert.deepEqual(restored,['base','selection']);
});

const flush = () => new Promise(setImmediate);
function deferred() {
    let resolve, reject;
    const promise = new Promise((yes, no) => {resolve = yes; reject = no;});
    return {promise, resolve, reject};
}

test('recent recordings waits for native preparation release and screenshot hiding, opening only once', async () => {
    const h = harness(), closing = deferred(), hiding = deferred(), calls = [];
    h.context.handleOverlayHidden = () => {h.state.reset(); return closing.promise;};
    h.ss._doCancel = () => {calls.push('hide'); return hiding.promise;};
    h.context.invoke = async command => calls.push(command);
    const opening = h.context.openRecentRecordings();
    await h.context.openRecentRecordings();
    await flush();
    assert.deepEqual(calls, []);
    closing.resolve(); await flush();
    assert.deepEqual(calls, ['hide']);
    hiding.resolve(true); await opening;
    assert.deepEqual(calls, ['hide', 'gif_open_editor_home']);
});

test('a replacement screenshot or preparation invalidates the pending recent entry before cancel', async () => {
    for (const replace of [h => h.ss._loadGen++, h => h.state.reset('ready')]) {
        const h = harness(), closing = deferred();
        h.context.handleOverlayHidden = () => {h.state.reset(); return closing.promise;};
        h.ss._doCancel = () => assert.fail('must not hide a replacement session');
        h.context.invoke = () => assert.fail('stale navigation must not open the editor');
        const opening = h.context.openRecentRecordings();
        replace(h); closing.resolve(); await opening;
    }
});

test('a screenshot replaced while hiding prevents late editor activation', async () => {
    const h = harness(), hiding = deferred();
    h.ss._doCancel = () => hiding.promise;
    h.context.invoke = () => assert.fail('late activation must not steal focus');
    const opening = h.context.openRecentRecordings();
    await flush(); h.ss._loadGen++; hiding.resolve(true); await opening;
});

test('a failed preparation release or failed screenshot hide does not proceed to the editor', async () => {
    const h = harness(), closing = deferred(), hints = [];
    h.context.handleOverlayHidden = () => {h.state.reset(); return closing.promise;};
    h.context.showTransientHint = text => hints.push(text);
    h.ss._doCancel = () => assert.fail('lease release failed');
    h.context.invoke = () => assert.fail('must not open with an active overlay');
    const opening = h.context.openRecentRecordings();
    closing.reject(new Error('release failed')); await opening;
    assert.equal(hints.length, 1);

    h.state.reset('ready');
    h.context.handleOverlayHidden = () => {h.state.reset();};
    h.ss._doCancel = async () => false;
    await h.context.openRecentRecordings();
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
