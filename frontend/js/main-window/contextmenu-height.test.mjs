import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const html = readFileSync(new URL('../../contextmenu-popup.html', import.meta.url), 'utf8');
const source = html.slice(html.indexOf('    let heightFrame'), html.indexOf('    // Lucide sprite'));
const settle = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
function setup() {
    const frames = new Map(), calls = [], pending = [];
    let serial = 0, height = 327;
    const window = {innerHeight: 330, visualViewport: {height: 330}, addEventListener() {}};
    const context = vm.createContext({window, document: {getElementById: () => ({getBoundingClientRect: () => ({height})})},
        requestAnimationFrame(fn) {frames.set(++serial, fn); return serial;},
        cancelAnimationFrame(id) {frames.delete(id);},
        invoke(command, args) {calls.push({command, args}); return new Promise(resolve => pending.push(resolve));},
        console,
    });
    vm.runInContext("let displayedMenuId = 'menu'; let displayedGeneration = 2;\n" + source, context);
    return {calls, pending, frames, context, window,
        schedule: () => context.scheduleHeightFit(),
        frame() {const current = [...frames.values()]; frames.clear(); current.forEach(fn => fn());},
        set height(value) {height = value;},
    };
}
test('下一帧回传实际 border-box 高度，不重复追加 padding 或固定余量', () => {
    const h = setup();
    h.schedule();
    assert.equal(h.calls.length, 0);
    h.frame();
    assert.equal(h.calls[0].command, 'fit_context_menu_height');
    assert.deepEqual(JSON.parse(JSON.stringify(h.calls[0].args)), {
        menuId: 'menu', generation: 2, contentHeight: 327, viewportHeight: 330,
    });
});
test('并发测量串行收敛，原生 resize 完成后重新测 viewport，避免重复缩小', async () => {
    const h = setup();
    h.schedule(); h.frame();
    h.schedule(); h.frame();
    assert.equal(h.calls.length, 1);
    h.window.visualViewport.height = 327;
    h.pending[0]();
    await settle();
    h.frame();
    assert.equal(h.calls.length, 2);
    assert.equal(h.calls[1].args.viewportHeight, 327);
});
test('菜单切换丢弃旧帧，在途请求结束后只提交新菜单会话与代次', async () => {
    const h = setup();
    h.schedule();
    const oldFrame = [...h.frames.values()][0];
    vm.runInContext("displayedMenuId = 'latest'; displayedGeneration = 3;", h.context);
    h.schedule();
    oldFrame();
    assert.equal(h.calls.length, 0);
    h.frame();
    assert.equal(h.calls[0].args.menuId, 'latest');
    vm.runInContext("displayedMenuId = 'next'; displayedGeneration = 4;", h.context);
    h.schedule(); h.frame();
    h.pending[0](); await settle(); h.frame();
    assert.equal(h.calls[1].args.menuId, 'next');
    assert.equal(h.calls[1].args.generation, 4);
});
test('预热空菜单和无尺寸布局不提交窗口校正', () => {
    const h = setup();
    h.height = 0;
    h.schedule(); h.frame();
    assert.equal(h.calls.length, 0);
    h.height = 327;
    vm.runInContext("displayedMenuId = '';", h.context);
    h.schedule(); h.frame();
    assert.equal(h.calls.length, 0);
});
