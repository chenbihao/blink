import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const source = readFileSync(new URL('./keyboard.js', import.meta.url), 'utf8')
    .replace(/^import[\s\S]*?from\s+["'][^"']+["'];\s*/gm, '').replace(/^export /gm, '');
let wheel, step = true, active = true, revision = 0;
const moves = [];
const noop = () => {};
const context = vm.createContext({
    document: {addEventListener: noop}, appEl: {addEventListener: (name, cb) => { if (name === 'wheel') wheel = cb; }},
    listen: noop, EVENTS: {}, aiMode: {isActive: () => false}, cmdMode: {isActive: () => false},
    clipboardMode: {isActive: () => active, isStepWheel: () => step, getWheelRevision: () => revision},
    results: {hasItems: () => true, scrollItems: n => moves.push(n), pageUp: () => moves.push('up'), pageDown: () => moves.push('down')},
});
vm.runInContext(source, context);
context.init();
const scroll = (deltaY, deltaMode = 0) => wheel({deltaY, deltaMode, preventDefault: noop});
scroll(100); assert.deepEqual(moves, [1], 'step 一格移动一条');
step = false; revision++;
scroll(100); assert.equal(moves.at(-1), 'down', '切换 page 后下一格立即整页');
step = true; revision++;
scroll(60); assert.equal(moves.length, 2);
revision++; // 新查询/新会话作废余量
scroll(60); assert.equal(moves.length, 2);
scroll(40); assert.equal(moves.at(-1), 1);
scroll(-3, 1); assert.equal(moves.at(-1), -1, 'DOM_DELTA_LINE 统一换算');
scroll(1, 2); assert.equal(moves.at(-1), 1, 'DOM_DELTA_PAGE 统一换算');
active = false; scroll(0); assert.equal(moves.length, 5, '零 delta 不翻页');
console.log('wheel: live mode switches, session remainder and delta units passed');
