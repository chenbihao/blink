import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const source = readFileSync(new URL('./ss-toolbar.js', import.meta.url), 'utf8');
const body = source.slice(source.indexOf('export async function showTextInput('),
    source.indexOf('/** 工具栏事件绑定')).replace('export ', '');

function fixture() {
    const events = [], nodes = [];
    let ready;
    const wait = new Promise(resolve => {ready = resolve;});
    const hooks = {
        adjustTextPlacement: () => ({x: 8, y: 8}),
        onTextInputOpen: () => wait,
    };
    const ss = {selCss: {x: 0, y: 0, w: 1920, h: 1080}, gifRecHooks: hooks};
    const parent = {
        appendChild(input) {nodes.push(input); input.parentNode = parent; input.isConnected = true; events.push('append');},
        removeChild(input) {input.isConnected = false; events.push('remove');},
    };
    const context = vm.createContext({
        ss, window: {innerWidth: 1920}, console, setTimeout() {},
        document: {body: parent, createElement() {
            return {style: {}, classList: {add() {}}, handlers: {}, textContent: '',
                setAttribute() {}, focus(options) {events.push(['focus', options]);},
                addEventListener(event, fn) {this.handlers[event] = fn;}};
        }},
        getRenderScale: () => ({scaleX: 1, scaleY: 1}),
        annot: {getTextConfig: () => ({fontSize: 24, fontFamily: 'sans-serif'}), getColor: () => '#fff',
            commitText: text => events.push(['commit', text]), cancelText: () => events.push('cancel')},
        redrawAnnotFull() {}, updateUndoRedoButtons() {},
    });
    vm.runInContext(`${body}\nglobalThis.show = showTextInput;`, context);
    return {ss, events, nodes, show: context.show, ready: () => ready(async () => events.push('release'))};
}

test('recording text appears only after capture is paused, and is removed before capture can resume', async () => {
    const f = fixture();
    const opening = f.show(100, 100);
    assert.equal(f.nodes.length, 0);
    f.ready(); await opening;
    const input = f.nodes[0];
    assert.equal(input.style.top, '8px');
    assert.equal(input.style.maxWidth, '800px');
    assert.equal(input.style.maxHeight, '28px');
    assert.equal(f.events[1][1].preventScroll, true);
    input.textContent = '全屏标注';
    input.handlers.keydown({key: 'Enter', preventDefault() {}});
    assert.deepEqual(f.events.slice(-3), [['commit', '全屏标注'], 'remove', 'release']);
    input.handlers.blur();
    assert.equal(f.events.filter(e => e === 'release').length, 1);
});

test('pending text setup cannot append an input into a reused overlay session', async () => {
    const f = fixture();
    const opening = f.show(100, 100);
    f.ss.gifRecHooks = {};
    f.ready(); await opening;
    assert.equal(f.nodes.length, 0);
    assert.deepEqual(f.events, ['release']);
});
