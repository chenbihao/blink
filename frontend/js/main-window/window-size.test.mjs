import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const source = readFileSync(new URL('./window-size.js', import.meta.url), 'utf8')
    .replace(/^import .*;\r?\n/gm, '').replace(/^export /gm, '');

function setup() {
    const frames = new Map(), calls = [];
    let id = 0;
    const appEl = {offsetHeight: 100, style: {}};
    const context = vm.createContext({
        appEl, console,
        requestAnimationFrame: callback => { frames.set(++id, callback); return id; },
        cancelAnimationFrame: key => frames.delete(key),
        resizeWindow: (width, height, layout) => { calls.push({width, height, ...layout}); return Promise.resolve(); },
    });
    vm.runInContext(source, context);
    return {context, appEl, frames, calls,
        flush() { const pending = [...frames.values()]; frames.clear(); pending.forEach(callback => callback()); },
    };
}

{
    const h = setup();
    h.context.syncWindowSize();
    assert.equal(h.frames.size, 0, '启动隐藏态不发 resize');
    h.context.setWindowVisibility(true, 1);
    h.context.syncWindowSize();
    const queued = [...h.frames.values()][0];
    h.context.setWindowVisibility(false, 2);
    queued(); // 即使浏览器已经取出、无法取消的回调也必须自检。
    assert.equal(h.calls.length, 0, '排队后隐藏的 rAF 不发 resize');
}

{
    const h = setup();
    h.context.setWindowVisibility(true, 1);
    h.context.syncWindowSize();
    const stale = [...h.frames.values()][0];
    h.context.setWindowVisibility(false, 2);
    h.context.setWindowVisibility(true, 3);
    h.appEl.offsetHeight = 500;
    h.context.syncWindowSize();
    stale();
    h.flush();
    assert.equal(h.calls.length, 1, '旧会话回调不提交也不清掉新会话的测量');
    assert.equal(h.calls[0].revision, 3);
    assert.equal(h.calls[0].height, 500);
    assert.equal(h.context.setWindowVisibility(false, 2), false, '旧 HIDDEN 被拒绝');
    assert.equal(h.context.acceptsWindowRevision(1), false, '旧模式预告被拒绝');
}

{
    const h = setup();
    h.context.setWindowVisibility(true, 1);
    h.context.syncWindowSize();
    h.context.syncWindowSize();
    h.context.syncWindowSize();
    assert.equal(h.frames.size, 1, '同一帧只测量一次');
    h.flush();
    h.appEl.offsetHeight = 60;
    h.context.syncWindowSize();
    h.flush();
    assert.equal(h.calls[1].height, 100, '同一搜索末页不会收缩');
    h.context.resyncWindowSize();
    h.flush();
    assert.equal(h.calls[2].height, 60, '临时 UI 移除后可显式收缩');
    assert.deepEqual(h.calls.map(call => call.sequence), [1, 2, 3]);
}

{
    const h = setup();
    h.context.setClipboardLayout(true);
    h.context.setWindowVisibility(true, 1);
    h.context.syncWindowSize();
    h.flush();
    assert.equal(h.calls[0].ready, false, 'SHOWN 的空列表测量不代表首搜完成');
    h.appEl.offsetHeight = 600;
    h.context.markClipboardLayoutReady();
    h.flush();
    assert.equal(h.calls[1].ready, true);
    assert.equal(h.calls[1].height, 600);
    h.context.lockWindowPlacement();
    h.flush();
    assert.equal(h.calls[2].interacted, true, '操作锁通过布局提交上报');
    h.context.setClipboardLayout(false);
    h.context.syncWindowSize();
    h.flush();
    assert.equal(h.calls[3].clipboard, false);
}

{
    const h = setup();
    h.context.setWindowVisibility(true, 1);
    h.context.syncWindowSize();
    const stale = [...h.frames.values()][0];
    h.context.resetMaxHeight();
    h.appEl.offsetHeight = 60;
    h.context.syncWindowSize();
    stale();
    h.flush();
    assert.equal(h.calls.length, 1, '新搜索取消旧布局回调');
    assert.equal(h.calls[0].height, 60);
}

console.log('window-size lifecycle and layout ordering passed');
