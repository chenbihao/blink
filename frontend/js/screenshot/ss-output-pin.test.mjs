import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
const sent = [];
globalThis.window = {__blinkScreenMeta: {}, __TAURI__: {core: {invoke: async (...args) => {
    sent.push(args); return 'pin-spare-test';
}}}};
const {screenshotPin} = await import('../shared/api.js');
const bytes = new Uint8Array([137, 80, 78, 71]);
await screenshotPin(bytes, -200, 0, false);
assert.equal(sent[0][1], bytes, 'PNG 必须保持 raw body，不进入 JSON 数组');
const layer = () => ({width: 2, height: 3, classList: {add() {}},
    getContext: () => ({drawImage() {}, clearRect() {}})});
const base = layer(), canvas = layer(), annotation = layer();
const state = globalThis.pinOutputHarness = {
    ss: {selCss: {x: 0, y: 0, w: 2, h: 3}, sent: false,
        canvas, ctx: canvas.getContext('2d'), annotCanvas: annotation, annotCtx: annotation.getContext('2d'),
        editorSession: {epoch: 1, source: 'long', canvasBacked: true, baseCanvas: base, screenX: -200},
        scrollBandX: -200, scrollBandY: 0, scrollSession: {active: false}},
    annot: {hasAnnotations: () => true, getOverlay: () => null, clearAll() {}, clearOverlay() {}},
    IMAGE_SOURCE: {LONG_SCREENSHOT: 'long', SCREENSHOT: 'screenshot'},
};
globalThis.document = {documentElement: {classList: {add() {}}},
    createElement: () => ({...layer(), toBlob: done => done(new Blob([bytes]))})};
const source = (await readFile(new URL('./ss-output.js', import.meta.url), 'utf8'))
    .replace(/^import[\s\S]*?from ['"][^'"]+['"];\r?$/gm, '');
const prefix = `import {screenshotPin} from ${JSON.stringify(new URL('../shared/api.js', import.meta.url).href)};
const {ss,annot,IMAGE_SOURCE} = globalThis.pinOutputHarness;`;
const {doPinSelection} = await import('data:text/javascript;base64,' + Buffer.from(prefix + source).toString('base64'));
await doPinSelection();
for (let i = 0; i < 10; i++) await Promise.resolve();
const pin = sent.at(-1);
assert.equal(pin[0], 'screenshot_pin');
assert.deepEqual(pin[1], bytes);
assert.equal(pin[2].headers['screen-x'], '-200');
assert.equal(pin[2].headers['screen-y'], '0');
assert.equal(pin[2].headers['show-translating'], 'false');
assert.equal(pin[2].headers['pin-timing'], undefined);
console.log('pin raw output adapter tests passed');
