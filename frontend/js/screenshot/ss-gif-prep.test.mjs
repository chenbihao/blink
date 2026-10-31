import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

function harness() {
    const handlers = new Map();
    const nodes = new Map();
    const classes = new Set(['toolbar-mode-gif-prep']);
    const toolbar = {classList: {contains: c => classes.has(c), remove: c => classes.delete(c)}};
    nodes.set('gif-prep-fps', {value: '15'});
    nodes.set('btn-gif', {focus() { this.focused = true; }});
    const ss = {toolbar, selCss: {x: 0, y: 0, w: 100, h: 100}, sent: false};
    const context = vm.createContext({ss, window: {addEventListener: (k, fn) => handlers.set(k, fn)},
        document: {body: {classList: {contains: () => false}}, getElementById: id => nodes.get(id),
            querySelectorAll: () => []}, positionToolbar() {}});
    const source = readFileSync(new URL('./ss-gif-prep.js', import.meta.url), 'utf8')
        .replace(/^import .*;\r?$/gm, '').replace(/export /g, '');
    vm.runInContext(source, context);
    context.bindGifPrepBar();
    return {ss, nodes, classes, escape: handlers.get('keydown')};
}

test('capture Escape returns from recording prep with focused controls, retaining the selection', () => {
    const h = harness();
    let prevented = false;
    let stopped = false;
    h.escape({key: 'Escape', target: {tagName: 'SELECT'}, preventDefault() { prevented = true; },
        stopImmediatePropagation() { stopped = true; }});
    assert.equal(h.classes.has('toolbar-mode-gif-prep'), false);
    assert.equal(h.nodes.get('btn-gif').focused, true);
    assert.ok(h.ss.selCss);
    assert.equal(prevented && stopped, true);
});

test('Escape during recording startup leaves its lifecycle in control', () => {
    const h = harness();
    h.ss.sent = true;
    h.escape({key: 'Escape'});
    assert.equal(h.classes.has('toolbar-mode-gif-prep'), true);
});
