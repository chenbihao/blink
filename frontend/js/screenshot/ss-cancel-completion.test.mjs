import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFileSync} from 'node:fs';

const source = readFileSync(new URL('./ss-output.js', import.meta.url), 'utf8');
const body = source.slice(source.indexOf('export function doCancel()'), source.indexOf('/** 判断是否有主动召唤出的交互面板')).replace('export ', '');

test('cancel reports completion only after the selected native hide operation has finished', async () => {
    for (const route of ['image', 'screenshot', 'overlay']) {
        let finish, cleared = 0;
        const pending = new Promise(resolve => {finish = resolve;});
        const hide = () => pending;
        const context = vm.createContext({
            ss: {editorSession: {source: 'screenshot'}, selectionRevision: 0, translationRevision: 0, isAnnotating: route === 'screenshot'},
            document: {body: {classList: {contains: () => false}}}, console: {info() {}, warn() {}, error() {}}, setTimeout() {},
            cancelActiveOcr() {}, hideSelLoading() {}, hideSsTooltip() {}, cleanupCanvasVisuals() {cleared++;},
            IMAGE_SOURCE: {LONG_SCREENSHOT: 'long'}, cancelIsImageEditorSession: () => route === 'image',
            imageEditorCancel: hide, screenshotCancel: hide, hideScreenshotOverlay: hide,
        });
        vm.runInContext(body, context);
        let completed = false;
        const cancelled = context.doCancel().then(result => {completed = true; return result;});
        await Promise.resolve();
        assert.equal(completed, false);
        assert.equal(cleared, 1);
        assert.equal(await context.doCancel(), false, 'duplicate cancel must not report successful hiding');
        finish(); assert.equal(await cancelled, true);
    }
});
