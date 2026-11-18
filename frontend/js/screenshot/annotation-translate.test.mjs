//! 0.29.2 选区移动/缩放的标注保留：translateCommands 平移全部命令坐标，
//! rebase 保留命令/撤销栈/水印并重放（"钉住屏幕内容"语义的数据层验证）。
import assert from 'node:assert/strict';
globalThis.window = {};
const annot = await import('./annotation-engine.js');

const noop = () => {};
function makeCtx(layer) {
    return {
        canvas: layer,
        save: noop, restore: noop, beginPath: noop, closePath: noop,
        moveTo: noop, lineTo: noop, stroke: noop, fill: noop,
        clearRect: noop, fillRect: noop, strokeRect: noop, rect: noop, clip: noop,
        arc: noop, ellipse: noop,
        fillText: noop, measureText: () => ({width: 10}),
        drawImage: noop, setLineDash: noop, putImageData: noop,
        getImageData: () => ({width: 0, height: 0, data: []}),
    };
}
function makeCanvas(w = 0, h = 0) {
    const c = {width: w, height: h};
    c.getContext = () => makeCtx(c);
    return c;
}
globalThis.document = {createElement: () => makeCanvas()};

const layer = makeCanvas(300, 200);
annot.init(layer);
annot.reset(300, 200, {width: 300, height: 200});

// ── 生成多类型命令：两点工具 / 轨迹工具 / 文本 / 数字 / 聚光灯 / 放大镜 ──
annot.setTool('rect');
annot.startDraw(10, 10); annot.moveDraw(50, 40); annot.endDraw(50, 40);
annot.setTool('pencil');
annot.startDraw(5, 5); annot.moveDraw(15, 8); annot.moveDraw(25, 12); annot.endDraw(25, 12);
annot.setTool('text');
annot.startDraw(30, 30);
const textResult = annot.endDraw(30, 30);
assert.equal(textResult.needsText, true);
annot.commitText('你好');
annot.setTool('number');
annot.startDraw(60, 60); annot.endDraw(60, 60);
annot.setTool('spotlight');
annot.startDraw(100, 100); annot.moveDraw(160, 150); annot.endDraw(160, 150);
annot.setTool('magnifier');
annot.startDraw(200, 120); annot.moveDraw(240, 160); annot.endDraw(240, 160);

const before = annot.getCommands();
assert.equal(before.length, 6);
// 形状审计：平移覆盖全部类型的前提——所有命令统一以 points 承载坐标
for (const cmd of before) {
    assert.ok(Array.isArray(cmd.points) && cmd.points.length >= 1, `${cmd.type} 必须是 points 模型`);
}
const snapshot = before.map((cmd) => ({type: cmd.type, points: cmd.points.map((p) => ({...p}))}));

// ── translateCommands：全部命令（含撤销栈外的重做分支）平移，非坐标字段不动 ──
annot.translateCommands(20, -10);
const shifted = annot.getCommands();
for (const [i, cmd] of shifted.entries()) {
    for (const [j, p] of cmd.points.entries()) {
        assert.equal(p.x, snapshot[i].points[j].x + 20, `${cmd.type}[${j}].x`);
        assert.equal(p.y, snapshot[i].points[j].y - 10, `${cmd.type}[${j}].y`);
    }
}
assert.equal(shifted.find((c) => c.type === 'text').text, '你好', '非坐标字段不受平移影响');

// 撤销一次后再次平移——重做分支（undoIndex 之后）的命令也必须被平移
annot.undo();
assert.equal(annot.getCommands().length, 5);
annot.translateCommands(5, 5);
annot.redo();
const magnifier = annot.getCommands().find((c) => c.type === 'magnifier');
assert.deepEqual(magnifier.points.map((p) => ({x: p.x, y: p.y})), [
    {x: 200 + 20 + 5, y: 120 - 10 + 5},
    {x: 240 + 20 + 5, y: 160 - 10 + 5},
], '重做分支命令历经两次平移');

// 幂等：零位移不产生任何变化
const stable = annot.getCommands().map((c) => c.points.map((p) => ({...p})));
annot.translateCommands(0, 0);
assert.deepEqual(annot.getCommands().map((c) => c.points.map((p) => ({...p}))), stable);

// ── rebase：保留命令/撤销栈/水印，重设画布几何并重放 ──
annot.commitWatermark({text: 'blink', layout: 'br', color: '#ffffff', opacity: 0.5, density: 1});
annot.rebase(400, 260, {width: 400, height: 260});
assert.equal(layer.width, 400, 'rebase 更新画布 backing 宽');
assert.equal(layer.height, 260, 'rebase 更新画布 backing 高');
assert.equal(annot.getCommands().length, 6, 'rebase 保留全部命令');
assert.equal(annot.canUndo(), true, 'rebase 保留撤销栈');

for (let i = 0; i < 6; i++) annot.undo();
assert.equal(annot.getCommands().length, 0);
assert.equal(annot.hasAnnotations(), true, 'rebase 后水印配置仍保留');

// 对照：reset 仍是清空语义（新选区）
annot.reset(100, 80);
assert.equal(annot.getCommands().length, 0);
assert.equal(annot.hasAnnotations(), false, 'reset 清空命令与水印');

console.log('annotation translate/rebase tests passed');
