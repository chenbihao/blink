import {test} from 'node:test';
import assert from 'node:assert/strict';
import {bindLayoutDrag} from './screenshot-layout-drag.js';
import {dropLayoutAction} from './screenshot-layout.js';
import {placeActionMenu} from '../screenshot/ss-panel-resize.js';

class Node {
    constructor(classes = '', id = '') {
        this.names = new Set(classes.split(' ')); this.dataset = {id}; this.children = []; this.handlers = {};
        this.style = {setProperty() {}}; this.offsetWidth = 400; this.offsetHeight = 36;
        this.classList = {contains: name => this.names.has(name), add: (...names) => names.forEach(name => this.names.add(name)), remove: (...names) => names.forEach(name => this.names.delete(name))};
    }
    get nextSibling() { return this.parentNode?.children[this.parentNode.children.indexOf(this) + 1]; }
    append(node) { this.insertBefore(node, null); }
    insertBefore(node, before) {
        if (node === before) return;
        node.remove(); node.parentNode = this;
        this.children.splice(before ? this.children.indexOf(before) : this.children.length, 0, node);
    }
    remove() { if (this.parentNode) this.parentNode.children = this.parentNode.children.filter(node => node !== this); this.parentNode = null; }
    cloneNode() { return new Node([...this.names].join(' ')); }
    setAttribute() {}
    addEventListener(name, handler) { this.handlers[name] = handler; }
    closest(selector) { return selector === '.ss-layout-row' && this.names.has('ss-layout-row') ? this : null; }
    getBoundingClientRect() {
        const top = this.dataset.zone === 'more' ? 220 : this.dataset.zone === 'toolbar' ? 0 : (this.parentNode.dataset.zone === 'more' ? 250 : 30) + this.parentNode.children.indexOf(this) * 40;
        return {left: 0, right: 400, top, bottom: top + (this.dataset.zone ? 200 : 36), height: this.dataset.zone ? 200 : 36, width: 400};
    }
}

function harness() {
    const panel = new Node(), lists = new Node(), toolbar = new Node(), more = new Node();
    lists.getBoundingClientRect = () => ({left: 0, right: 400, top: 0, bottom: 420});
    toolbar.dataset.zone = 'toolbar'; more.dataset.zone = 'more'; lists.append(toolbar); lists.append(more);
    const first = new Node('ss-layout-row sortable-item', 'translate'), second = new Node('ss-layout-row sortable-item', 'qr');
    toolbar.append(first); toolbar.append(second);
    globalThis.document = {body: new Node()};
    globalThis.getComputedStyle = () => ({getPropertyValue: () => '', fontSize: '13px'});
    let saving = false; const drops = [];
    const controller = bindLayoutDrag({panel, lists, isSaving: () => saving, onDrop: (...args) => drops.push(args)});
    const send = (name, row, x, y) => lists.handlers[name]({target: row, button: 0, pointerId: 1, clientX: x, clientY: y, preventDefault() {}});
    return {panel, lists, toolbar, more, first, second, controller, send, drops, setSaving: value => {saving = value;}};
}

test('whole-row pointer drag has a floating card, live placeholder and cross-zone commit', () => {
    const h = harness();
    h.send('pointerdown', h.second, 20, 80);
    h.send('pointermove', h.second, 22, 81);
    assert.equal(document.body.children.length, 0);
    h.send('pointermove', h.second, 20, 280);
    assert(h.second.classList.contains('dragging'));
    assert.equal(h.second.parentNode, h.more);
    assert(document.body.children[0].classList.contains('sortable-drag-preview'));
    h.send('pointerup', h.second, 20, 280);
    assert.deepEqual(h.drops, [['qr', 'more', undefined, true]]);
    assert.equal(document.body.children.length, 0);
    assert.deepEqual(h.toolbar.children.map(node => node.dataset.id), ['translate', 'qr']);
});
test('Escape/cancellation and a concurrent save restore the original cards without a drop', () => {
    const h = harness();
    h.send('pointerdown', h.first, 20, 40); h.send('pointermove', h.first, 20, 280);
    assert.equal(h.controller.cancel(), true); assert.equal(h.controller.cancel(), false);
    assert.deepEqual(h.toolbar.children.map(node => node.dataset.id), ['translate', 'qr']);
    h.send('pointerdown', h.first, 20, 40); h.send('pointermove', h.first, 20, 280);
    h.setSaving(true); h.send('pointermove', h.first, 20, 300);
    assert.equal(document.body.children.length, 0); assert.equal(h.drops.length, 0);
});
test('drag preview scales the complete source card while pointer offsets stay in viewport CSS pixels', () => {
    for (const scale of [0.5, 2 / 3, 1.25, 1.5, 2]) {
        const h = harness();
        h.second.getBoundingClientRect = () => ({left: 10, top: 60, width: 400 * scale, height: 36 * scale});
        h.send('pointerdown', h.second, 30, 70);
        h.send('pointermove', h.second, 50, 100);
        const preview = document.body.children[0];
        assert.equal(preview.style.width, '400px');
        assert.equal(preview.style.height, '36px');
        const factors = preview.style.transform.slice(6, -1).split(',').map(Number);
        assert(factors.every(factor => Math.abs(factor - scale) < 1e-9));
        assert.equal(preview.style.left, '30px');
        assert.equal(preview.style.top, '90px');
        h.controller.cancel();
    }
});
test('scroll-clipped rows cannot receive drops over the fixed footer', () => {
    const h = harness();
    h.lists.getBoundingClientRect = () => ({left: 0, right: 400, top: 0, bottom: 180});
    h.send('pointerdown', h.first, 20, 40);
    h.send('pointermove', h.first, 20, 280);
    h.send('pointerup', h.first, 20, 280);
    assert.equal(h.drops.length, 0);
    assert.deepEqual(h.toolbar.children.map(row => row.dataset.id), ['translate', 'qr']);
});
test('drop before/after uses the target identity when moving forward, backward and between zones', () => {
    const initial = {toolbar: ['translate', 'scroll', 'qr'], more: ['ocr']};
    assert.deepEqual(dropLayoutAction(initial, 'translate', 'toolbar', 'qr', true).toolbar, ['scroll', 'qr', 'translate']);
    assert.deepEqual(dropLayoutAction(initial, 'qr', 'toolbar', 'translate').toolbar, ['qr', 'translate', 'scroll']);
    assert.deepEqual(dropLayoutAction(initial, 'ocr', 'toolbar', 'scroll', true).toolbar, ['translate', 'scroll', 'ocr', 'qr']);
    assert.deepEqual(dropLayoutAction(initial, 'qr', 'toolbar', 'qr', true).toolbar, initial.toolbar);
});
test('menu stays right aligned to its trigger, flips above and clamps on negative-origin monitors', () => {
    const monitor = {x: -1000, y: 0, w: 1000, h: 700};
    assert.deepEqual(placeActionMenu({left: -300, right: -268, top: 80, bottom: 112}, 180, 130, monitor), {left: -448, top: 116});
    assert.deepEqual(placeActionMenu({left: -990, right: -958, top: 650, bottom: 682}, 180, 130, monitor), {left: -992, top: 516});
});
