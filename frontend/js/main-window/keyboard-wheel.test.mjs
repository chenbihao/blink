import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import {createListNavigation} from './list-navigation.js';
const source = readFileSync(new URL('./keyboard.js', import.meta.url), 'utf8')
    .replace(/^import[\s\S]*?from\s+["'][^"']+["'];\s*/gm, '').replace(/^export /gm, '');
function setup(clipboard, mode) {
    const listNavigation = createListNavigation();
    listNavigation.refreshFromConfigData({wheel_mode: mode});
    let wheel, ai = false, command = false, hasItems = true;
    const moves = [], queryEl = {value: 'query'};
    const noop = () => {};
    const context = vm.createContext({
        listNavigation, queryEl,
        document: {addEventListener: noop}, appEl: {addEventListener: (name, cb, options) => {
            if (name === 'wheel') { wheel = cb; assert.equal(options.passive, false); }
        }},
        listen: noop, EVENTS: {}, aiMode: {isActive: () => ai}, cmdMode: {isActive: () => command},
        clipboardMode: {isActive: () => clipboard},
        results: {hasItems: () => hasItems, scrollItems: n => moves.push(['step', n]),
            pageUp: n => moves.push(['page', -n]), pageDown: n => moves.push(['page', n])},
    });
    vm.runInContext(source, context); context.init();
    return {moves, queryEl, listNavigation,
        set clipboard(v) {clipboard = v;}, set ai(v) {ai = v;},
        set command(v) {command = v;}, set hasItems(v) {hasItems = v;},
        scroll(deltaY, deltaMode = 0) { let blocked = false;
            wheel({deltaY, deltaMode, preventDefault: () => {blocked = true;}}); return blocked; },
    };
}
for (const mode of ['step', 'page']) {
    const histories = [];
    for (const clipboard of [false, true]) {
        const h = setup(clipboard, mode);
        assert.equal(h.scroll(1), true); assert.deepEqual(h.moves, []);
        h.scroll(59); h.scroll(40); h.scroll(200); h.scroll(-3, 1); h.scroll(1, 2);
        assert.deepEqual(h.moves, [[mode, 1], [mode, 2], [mode, -1], [mode, 1]]);
        histories.push(h.moves);
    }
    assert.deepEqual(histories[0], histories[1]);
}
{
    const h = setup(false, 'step');
    h.scroll(60); h.queryEl.value = 'new'; h.scroll(40); assert.equal(h.moves.length, 0);
    h.scroll(60); assert.deepEqual(h.moves, [['step', 1]]);
    h.scroll(60); h.clipboard = true; h.scroll(40); assert.equal(h.moves.length, 1);
    h.listNavigation.refreshFromConfigData({wheel_mode: 'page'});
    h.scroll(60); assert.equal(h.moves.length, 1, '热切模式清空余量');
    h.scroll(40); assert.deepEqual(h.moves.at(-1), ['page', 1]);
    h.scroll(60); h.listNavigation.reset(); h.scroll(40); assert.equal(h.moves.length, 2, '会话重置不继承余量');
    h.scroll(-60); assert.equal(h.moves.length, 2, '反向先清旧余量');
    h.scroll(-40); assert.deepEqual(h.moves.at(-1), ['page', -1]);
    for (const invalid of [0, NaN, Infinity, -Infinity]) h.scroll(invalid);
    assert.equal(h.moves.length, 3);
}
for (const surface of ['ai', 'command', 'hasItems']) {
    const h = setup(false, 'step'); h.scroll(60);
    h[surface] = surface !== 'hasItems';
    assert.equal(h.scroll(100), surface === 'hasItems');
    assert.deepEqual(h.moves, []);
    h[surface] = surface === 'hasItems'; h.scroll(40);
    assert.deepEqual(h.moves, [], '被绕过的视图不保留旧列表余量');
}
console.log('wheel: search/clipboard parity, units, accumulation, context and native scrolling passed');
