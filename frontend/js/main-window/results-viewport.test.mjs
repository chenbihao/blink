//! 0.25.29 results.js 视口模型测试：viewStart 泛化（page 模式页对齐 / step 平移）、
//! 流式 appendItems（去重 + 保序 + 旧 seq 丢弃）、removeItems 局部移除、nearTail。
//! 用 clipboard-mode.test.mjs 同款 vm 方案：剥 import，mock DOM/IPC 边界。

import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import * as clipboardEntry from './clipboard-entry.js';

const source = readFileSync(new URL('./results.js', import.meta.url), 'utf8')
    .replace(/^import[\s\S]*?from\s+["'][^"']+["'];\s*/gm, '')
    .replace(/^export /gm, '');

const mkEl = () => {
    const el = {
        children: [], dataset: {}, style: {},
        classList: {_set: new Set(), add(c) {this._set.add(c);}, remove(c) {this._set.delete(c);},
            toggle(c, on) {on ? this._set.add(c) : this._set.delete(c);}, contains(c) {return this._set.has(c);}},
        appendChild(child) {el.children.push(child); return child;},
        replaceChildren(...kids) {el.children = kids;},
        remove() {}, addEventListener() {}, querySelector() {return null;},
        _text: '', set textContent(v) {el._text = v;}, get textContent() {return el._text;},
    };
    return el;
};

function setup() {
    const resultsEl = mkEl();
    const statusCalls = [];
    const pageChanged = [];
    let created = 0, clipActive = false, stepWheel = false;
    const context = vm.createContext({
        ...clipboardEntry,
        resultsEl,
        resetMaxHeight: () => {}, syncWindowSize: () => {},
        bindClipboardPreview: () => {}, bindErrorPreview: () => {}, hideClipboardPreview: () => {},
        activateItem: () => {}, statusbar: {update: (a, p) => statusCalls.push({a, p})},
        invoke: async () => ({}),
        renderIcon: () => mkEl(),
        createSwatch: () => mkEl(), getColorPayload: () => null,
        isColorEntry: () => false, parseColor: () => null,
        clipboardMode: {
            isActive: () => clipActive,
            isStepWheel: () => stepWheel,
            getStreamInfo: () => ({}),
            onPageChanged: () => pageChanged.push(1),
        },
        document: {createElement: () => {created++; return mkEl();}},
        console,
        Promise, Math, Set, Map, JSON, Array, Object, Number, String, isNaN, parseInt,
    });
    vm.runInContext(source, context);
    return {
        context, resultsEl, statusCalls, pageChanged,
        get created() {return created;},
        set clipActive(v) {clipActive = v;},
        set stepWheel(v) {stepWheel = v;},
    };
}

/** 构造剪贴板文本条目（itemKey 走 actions[0].hitId 分支）。 */
const clip = (hitId, score = 0.9) => ({
    name: `item-${hitId}`, source: 'clipboard', score,
    clipboard_ref: {kind: 'text', id: hitId},
    actions: [{kind: 'lazy_copy', hitId}],
});

{
    const h = setup();
    h.context.render([clip('same'), {name: 'picture', source: 'clipboard', actions: [], clipboard_ref: {kind: 'image', id: 'same'}}], 1);
    assert.equal(h.context.getLoadedCount(), 2, '同 ID 不同类型不能误去重');
    h.context.removeItems([{kind: 'image', id: 'same'}]);
    assert.equal(h.context.getLoadedCount(), 1);
    assert.equal(h.context.getItems()[0].clipboard_ref.kind, 'text', '按类型局部删除不误删另一类');
}

{
    // page 模式行为不变：pageDown 页对齐、末页 stop、getPage 换算
    const h = setup();
    const items = Array.from({length: 25}, (_, i) => clip(`c${i}`));
    h.context.render(items, 1);
    assert.equal(h.context.getPage(), 0, '初始第一页');
    h.context.pageDown();
    assert.equal(h.context.getPage(), 1, 'pageDown 翻一页');
    assert.equal(h.context.getNth(1).actions[0].hitId, 'c9', '第二页第一条是全局第 10 条');
    h.context.pageDown(); h.context.pageDown();
    assert.equal(h.context.getPage(), 2, '末页（25 条 = 3 页）');
    h.context.pageDown();
    assert.equal(h.context.getPage(), 2, '末页后不再翻');
    h.context.pageUp();
    assert.equal(h.context.getPage(), 1, 'pageUp 回一页');
}
{
    // step 模式 scrollItems：逐条平移 + 边界 clamp + Alt+1~9 跟随视口
    const h = setup();
    const items = Array.from({length: 12}, (_, i) => clip(`s${i}`));
    h.context.render(items, 1);
    h.context.scrollItems(1);
    assert.equal(h.context.getNth(1).actions[0].hitId, 's1', '平移 1 条后视口从 s1 开始');
    h.context.scrollItems(-1);
    assert.equal(h.context.getNth(1).actions[0].hitId, 's0', '向上平移回顶');
    h.context.scrollItems(-5);
    assert.equal(h.context.getNth(1).actions[0].hitId, 's0', '顶部 clamp');
    h.context.scrollItems(50);
    assert.equal(h.context.getNth(1).actions[0].hitId, 's3', '底部 clamp（12-9=3）');
    h.context.scrollItems(1);
    assert.equal(h.context.getNth(1).actions[0].hitId, 's3', '已在底不再平移');
}
{
    // appendItems：去重 + 保序（不重排）+ 旧 seq 丢弃 + nearTail 联动
    const h = setup();
    const first = [clip('a'), clip('b')];
    h.context.render(first, 7);
    assert.equal(h.context.nearTail(), true, '2 条 < 一屏余量，距底');
    const appended = h.context.appendItems([clip('b'), clip('c'), clip('d')], 7);
    assert.equal(appended, true, '有新增');
    assert.equal(h.context.getLoadedCount(), 4, 'b 去重后 4 条');
    h.context.pageDown(); // 4 条不足第二页 → 仍在第一页，末条应为追加的 d（保序，不在前插）
    assert.equal(h.context.getNth(4)?.actions?.[0]?.hitId, 'd', '追加在尾部（不前插）');
    assert.equal(h.context.appendItems([clip('b')], 7), false, '全重复批次返回 false');
    // 旧 seq 的追加：丢弃
    assert.equal(h.context.appendItems([clip('z')], 6), false, '旧 seq 丢弃');
    assert.equal(h.context.getLoadedCount(), 4, '旧 seq 不生效');
    // 新 seq 重置后 nearTail 重算
    h.context.render([clip('only')], 8);
    assert.equal(h.context.getLoadedCount(), 1);
}
{
    // step 模式 ArrowUp/Down 越界：平移 1 条（光标跟视口），不整页跳
    const h = setup();
    h.clipActive = true; h.stepWheel = true;
    const items = Array.from({length: 12}, (_, i) => clip(`s${i}`));
    h.context.render(items, 1);
    for (let i = 0; i < 8; i++) h.context.move(1); // 光标走到视口末行 s8
    h.context.move(1); // 越下界 → 平移 1 条
    assert.equal(h.context.getNth(9)?.actions?.[0]?.hitId, 's9', 'step 越下界平移 1 条（s9 滚入）');
    assert.equal(h.context.getNth(1).actions[0].hitId, 's1', '视口起点 s1');
    for (let i = 0; i < 8; i++) h.context.move(-1); // 光标回到视口首行
    h.context.move(-1); // 越上界 → 平移回 1 条
    assert.equal(h.context.getNth(1).actions[0].hitId, 's0', 'step 越上界平移回顶');
}
{
    // removeItems：局部移除 + viewStart 夹紧
    const h = setup();
    const items = Array.from({length: 20}, (_, i) => clip(`r${i}`));
    h.context.render(items, 1);
    h.context.pageDown(); // viewStart = 9
    h.context.removeItems(['r9', 'r10', 'r11'].map(id => ({kind: 'text', id})));
    assert.equal(h.context.getLoadedCount(), 17);
    assert.equal(h.context.getNth(1).actions[0].hitId, 'r12', '视口起点越过已删条目，指向 r12（页对齐保持）');
    // 图片项按 lnkPath 匹配
    h.context.appendItems([{name: 'img', source: 'clipboard', clipboard_ref: {kind: 'image', id: 'clipimg_x'}, actions: []}], 1);
    h.context.removeItems([{kind: 'image', id: 'clipimg_x'}]);
    assert.equal(h.context.getLoadedCount(), 17, 'lnkPath 匹配的图片项被移除');
    // 全删光
    h.context.removeItems(items.map(x => x.clipboard_ref));
    assert.equal(h.context.getLoadedCount(), 0, '全部移除后清零');
}
{
    const h = setup();
    h.context.render(Array.from({length: 20}, (_, i) => clip(`config${i}`)), 1);
    h.context.scrollItems(3);
    h.context.refreshMaxResultsFromConfigData({page_size: 7});
    assert.equal(h.resultsEl.children.length, 7, '页大小热更新同步重绘 DOM');
    assert.equal(h.context.getNth(1).actions[0].hitId, 'config3', '热更新保留浏览位置');
    assert.equal(h.context.getLoadedCount(), 20, '已加载数据不重查');
}
console.log('results 视口模型：page/step、流式追加、局部移除与页大小热更新通过');
