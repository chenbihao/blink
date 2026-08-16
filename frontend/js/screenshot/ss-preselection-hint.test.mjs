import assert from 'node:assert/strict';
import test from 'node:test';

import {hidePreselectionHint, resetPreselectionHint, showPreselectionHint,} from './ss-preselection-hint.js';

class FakeClassList {
    constructor() {
        this.values = new Set();
    }

    toggle(name, enabled) {
        if (enabled) this.values.add(name);
        else this.values.delete(name);
    }

    contains(name) {
        return this.values.has(name);
    }
}

function createFakeDocument() {
    const children = [];
    return {
        children,
        body: {
            appendChild(element) {
                children.push(element);
            },
        },
        createElement() {
            return {
                classList: new FakeClassList(),
                style: {},
                title: '',
                offsetHeight: 0,
            };
        },
    };
}

test('窗口与控件预选复用同一元素并连续交接', () => {
    const fakeDocument = createFakeDocument();
    globalThis.document = fakeDocument;

    showPreselectionHint({x: 10, y: 20, w: 800, h: 600}, 'window', 'Editor');
    const hint = fakeDocument.children[0];
    assert.equal(fakeDocument.children.length, 1);
    assert.equal(hint.style.left, '10px');
    assert.equal(hint.style.opacity, '1');

    showPreselectionHint({x: 40, y: 60, w: 200, h: 80}, 'control');
    assert.equal(fakeDocument.children.length, 1, '层级切换不应创建第二个预选框');
    assert.equal(hint.style.left, '40px');
    assert.equal(hint.style.width, '200px');
    assert.equal(hint.style.opacity, '1');
    assert.ok(hint.classList.contains('preselection-hint--control'));

    hidePreselectionHint('window');
    assert.equal(hint.style.opacity, '1', '旧窗口层级不能隐藏已接管的控件预选框');

    hidePreselectionHint('control');
    assert.equal(hint.style.opacity, '0');
    showPreselectionHint({x: 10, y: 20, w: 800, h: 600}, 'window', 'Editor');
    assert.equal(hint.style.opacity, '1', '淡出完成前切回窗口应直接延续当前元素');
    assert.equal(hint.style.width, '800px');
    assert.ok(hint.classList.contains('preselection-hint--window'));

    resetPreselectionHint();
    assert.equal(hint.style.visibility, 'hidden');
    delete globalThis.document;
});
