import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import {annotationOperationShortcut} from './ss-editor-policy.js';

const source = readFileSync(new URL('./index.js', import.meta.url), 'utf8');
const html = readFileSync(new URL('../../chord-screenshot.html', import.meta.url), 'utf8');
const functions = ['focusScreenshotCanvas', 'enterAnnotationMode', 'enterCanvasImageEditor']
    .map(name => {
        const match = source.match(new RegExp(`^function ${name}\\([^]*?^}`, 'm'));
        assert.ok(match, `production function ${name}`);
        return match[0];
    }).join('\n');
const sessionEntries = ['__blinkStartScreenshotSession', '__blinkOpenImageEditor']
    .map(name => source.match(new RegExp(`^window\\.${name} = function[^]*?^};`, 'm'))[0]).join('\n');
const keyStart = source.indexOf("document.addEventListener('keydown', (e) => {");
const keyEnd = source.indexOf('\n});', keyStart) + '\n});'.length;

function harness() {
    let visible = true;
    const listeners = [], focusCalls = [], frames = [];
    const makeNode = (id, tagName = 'BUTTON') => ({
        id, tagName, style: {}, width: 800, height: 600,
        classList: {add() {}, remove() {}, contains() { return false; }},
        closest: () => null,
        getContext: () => ({drawImage() {}, clearRect() {}, putImageData() {}, getImageData: () => ({})}),
        focus(options) {
            focusCalls.push({id, options});
            if (visible) document.activeElement = this;
        },
    });
    const canvas = makeNode('canvas', 'CANVAS'), more = makeNode('btn-more');
    const document = {
        activeElement: more,
        documentElement: {classList: {remove() { visible = true; }}},
        body: {classList: {contains: () => false}},
        createElement: () => makeNode('', 'CANVAS'),
        getElementById: () => null,
        addEventListener: (name, run) => { if (name === 'keydown') listeners.push(run); },
    };
    const ss = {
        canvas, annotCanvas: makeNode('annot-canvas', 'CANVAS'), screenshot: {},
        ctx: canvas.getContext('2d'), toolbar: makeNode('toolbar'),
        editorSession: {beginScreenshotSelection() {}, beginCanvasSource() {}},
        isAnnotating: false,
    };
    let editorLoads = 0;
    const context = {
        isRecordingStarting:()=>false, isGifRecActive: () => false, restoreRecordingOverlay: async () => true,
        ss, document, frames, performance, annotationOperationShortcut,
        console: {debug() {}, info() {}, warn() {}, error() {}},
        IMAGE_SOURCE: {LONG_SCREENSHOT: 'long-screenshot', CLIPBOARD: 'clipboard', PIN: 'pin'},
        annot: {reset() {}, getTool: () => 'select'},
        screenshotSetAnnotationMode: () => Promise.resolve(),
        cssRectToBitmap: rect => rect,
        getRenderScale: () => ({scaleX: 1, scaleY: 1}),
        computeCanvasEditorInitialPosition: () => ({x: 100, y: 100}),
        requestAnimationFrame: run => frames.push(run),
        loadEditorImage: () => editorLoads++,
        // 模拟清理布局编辑器时把焦点还给“更多”；新入口必须随后覆盖它。
        resetState: () => { document.activeElement = more; visible = false; },
    };
    for (const name of ['resetPaletteState', 'hidePixelMagnifier', 'clearHover', 'clearControlHover',
        'updateUndoRedoButtons', 'drawFinalSelection', 'positionToolbar', 'triggerOcrPrewarm',
        'syncInteractionCanvasSize', 'resetLiveSelection']) context[name] = () => {};
    context.window = {innerWidth: 1600, innerHeight: 1000,
        __blinkClearScreenshotVisual: context.resetState};
    vm.createContext(context);
    vm.runInContext(functions + '\n' + sessionEntries + '\n' + source.slice(keyStart, keyEnd), context);
    return {ss, more, canvas, document, context, frames, focusCalls,
        editorLoads: () => editorLoads,
        key(key, extra = {}) {
            const event = {key, target: document.activeElement, preventDefault() { this.defaultPrevented = true; }, ...extra};
            for (const run of listeners) run(event);
            return event;
        },
    };
}

test('repeated screenshot during recording or startup never clears the recording host', async () => {
    for (const phase of ['recording', 'starting']) {
        const h = harness();
        let restored = 0;
        h.context.isGifRecActive = () => phase === 'recording';
        h.context.isRecordingStarting = () => phase === 'starting';
        h.context.restoreRecordingOverlay = async () => {restored++; return true;};
        const previous = h.context.window.__blinkScreenMeta;
        h.context.window.__blinkStartScreenshotSession({vx:100,vy:100,w:800,h:600},0);
        await Promise.resolve();
        assert.equal(restored,1);
        assert.equal(h.context.window.__blinkScreenMeta,previous);
        assert.equal(h.document.activeElement,h.more);
        assert.equal(h.frames.length,0);
    }
});

test('canvas accepts programmatic focus without entering the Tab order', () => {
    assert.match(html, /<canvas\b[^>]*id="canvas"[^>]*tabindex="-1"/);
});

test('an idle backend clears stale recording ownership and opens the requested screenshot immediately', async () => {
    const h=harness();
    let active=true;
    h.context.isGifRecActive=()=>active;
    h.context.restoreRecordingOverlay=async()=>{active=false;return false;};
    const meta={vx:10,vy:20,w:800,h:600};
    h.context.window.__blinkStartScreenshotSession(meta,0);
    await new Promise(setImmediate);
    assert.equal(h.context.window.__blinkScreenMeta,meta);
    assert.equal(h.document.activeElement,h.canvas);
    assert.equal(h.frames.length,1);
});

test('reused screenshot session clears focus restored by old panel cleanup, after becoming visible', () => {
    const h = harness();
    h.context.window.__blinkStartScreenshotSession({}, 0);
    assert.equal(h.document.activeElement, h.canvas);
    assert.equal(h.frames.length, 1, 'focus handoff does not wait for image loading');
    assert.equal(h.focusCalls[0].options.preventScroll, true);
    assert.equal(h.key('a').defaultPrevented, undefined);
    assert.equal(h.document.activeElement, h.canvas);
});

test('new selection takes focus from a stale more button; ordinary keys do not target the toolbar', () => {
    const h = harness();
    h.context.enterAnnotationMode({x: 80, y: 100, w: 200, h: 120});
    assert.equal(h.document.activeElement, h.canvas);
    for (const key of ['a', 'e', 'E', '1', 'Shift']) {
        assert.equal(h.key(key).defaultPrevented, undefined);
        assert.equal(h.document.activeElement, h.canvas);
    }
    h.more.focus();
    h.key('x');
    assert.equal(h.document.activeElement, h.more, 'ordinary keys do not steal intentional control focus');
});

test('image-editor session and canvas-source entry also reset stale toolbar focus', () => {
    const h = harness();
    h.context.window.__blinkOpenImageEditor();
    assert.equal(h.document.activeElement, h.canvas);
    assert.equal(h.editorLoads(), 1);
    h.more.focus();
    h.context.enterCanvasImageEditor({}, 320, 200, 'clipboard');
    assert.equal(h.document.activeElement, h.canvas);
});
