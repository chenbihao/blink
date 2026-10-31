import {test} from 'node:test';
import assert from 'node:assert/strict';
import {DEFAULT_LAYOUT, ACTION_IDS, normalizeLayout, fitLayout, moveLayoutAction} from './screenshot-layout.js';

test('legacy config uses exact default; custom omissions append only to more', () => {
    assert.deepEqual(normalizeLayout(), DEFAULT_LAYOUT);
    assert.deepEqual(normalizeLayout({toolbar:['pin','pin','unknown'],more:['qr','pin']}), {
        toolbar:['pin'], more:['qr', ...ACTION_IDS.filter(id => !['pin','qr'].includes(id))],
    });
});
test('overflow preserves order, is temporary and keeps every configurable action', () => {
    const layout = normalizeLayout();
    const fitted = fitLayout(layout, 2);
    assert.deepEqual(fitted.toolbar, ['translate','scroll']);
    assert.deepEqual(fitted.more, [...layout.toolbar.slice(2), ...layout.more]);
    assert.deepEqual(layout, DEFAULT_LAYOUT);
    assert.deepEqual(fitLayout(layout, 99), DEFAULT_LAYOUT);
});
test('move across zones and within a zone keeps a partition; fixed actions cannot enter it', () => {
    let layout = moveLayoutAction(DEFAULT_LAYOUT,'qr','toolbar',0);
    assert.equal(layout.toolbar[0], 'qr');
    layout = moveLayoutAction(layout,'qr','toolbar',2);
    assert.equal(layout.toolbar[2], 'qr');
    assert.equal(new Set([...layout.toolbar,...layout.more]).size, ACTION_IDS.length);
    assert.deepEqual(moveLayoutAction(layout,'copy','toolbar',0),layout);
});
