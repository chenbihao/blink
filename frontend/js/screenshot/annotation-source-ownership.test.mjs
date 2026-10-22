import assert from 'node:assert/strict';
globalThis.window = {};
const annot = await import('./annotation-engine.js');
const created = [];
function canvas(w = 0, h = 0) {
    return {width: w, height: h, getContext: () => ({clearRect() {}, putImageData(data) { this.data = data; }})};
}
globalThis.document = {createElement: () => { const c = canvas(); created.push(c); return c; }};
const layer = canvas(); annot.init(layer);
const pixels = {width: 930, height: 17935};
const base = canvas(pixels.width, pixels.height);
annot.reset(pixels.width, pixels.height, pixels, base);
assert.equal(annot.getCropSourceCanvas(), base);
assert.equal(created.length, 0, '只读编辑底图无需再分配整张长图缓存');

// 选区更新需要自有副本，不能 resize/覆盖共享编辑底图。
annot.updateCropData({width: 20, height: 10}, 20, 10);
const owned = annot.getCropSourceCanvas();
assert.notEqual(owned, base);
assert.deepEqual([base.width, base.height], [930, 17935]);
assert.deepEqual([owned.width, owned.height], [20, 10]);

annot.reset(930, 17935, pixels, base);
assert.equal(owned.width * owned.height, 0, '替换自有缓存时立即释放像素');
annot.reset(2, 3, {width: 2, height: 3});
assert.deepEqual([base.width, base.height], [930, 17935], '会话切换不能销毁借用底图');
assert.notEqual(annot.getCropSourceCanvas(), base);
assert.deepEqual([annot.getCropSourceCanvas().width, annot.getCropSourceCanvas().height], [2, 3]);
console.log('annotation source ownership tests passed');
