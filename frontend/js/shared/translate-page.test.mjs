import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const source = readFileSync(new URL('../../../plugins/builtin/translate/web/page.js', import.meta.url), 'utf8');
const settle = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
function setup({settings = {}, payload = null, settingsPromise} = {}) {
    const elements = new Map(), calls = [], pending = [];
    const element = () => ({value: '', textContent: '', children: [], events: {},
        classList: {add() {}, remove() {}}, appendChild(item) {this.children.push(item);},
        addEventListener(type, fn) {this.events[type] = fn;}, focus() {}, setSelectionRange() {},
    });
    let onPayload;
    vm.runInNewContext(source, {
        document: {getElementById(id) {if (!elements.has(id)) elements.set(id, element()); return elements.get(id);},
            createElement: element, addEventListener() {}},
        window: {Blink: {init: async () => ({context: {language: 'zh'}, payload}),
            settings: () => settingsPromise || Promise.resolve(settings),
            onPayload(fn) {onPayload = fn;},
            invoke(_, args) {calls.push(args); return new Promise(resolve => pending.push(resolve));},
        }}, setTimeout, clearTimeout,
    });
    return {elements, calls, pending, payload: value => onPayload(value)};
}
test('翻译首次/空唤起/带文本复用默认自动，手动方向仅影响本次会话', async () => {
    const h = setup({payload: {text: '你好'}});
    await settle();
    assert.equal(h.elements.get('target').value, 'auto');
    assert.equal(h.calls[0].target_lang, 'auto');
    h.elements.get('target').value = 'ja';
    h.elements.get('target').events.change();
    assert.equal(h.calls.at(-1).target_lang, 'ja');
    h.payload({text: 'hello'});
    assert.equal(h.calls.at(-1).target_lang, 'auto');
    h.pending[0]({ok: true, content: '旧结果'});
    await settle();
    assert.equal(h.elements.get('output').value, '', '新唤起使旧请求失效');
    h.pending.at(-1)({ok: true, content: '你好'});
    await settle();
    assert.equal(h.elements.get('output').value, '你好');
    h.payload(null);
    assert.equal(h.elements.get('input').value, '');
    assert.equal(h.elements.get('output').value, '');
    assert.equal(h.elements.get('target').value, 'auto');
});
test('已保存固定目标及人工载荷覆盖得到保留，不回写设置', async () => {
    const h = setup({settings: {target_lang: 'ko'}, payload: {text: 'hello'}});
    await settle();
    assert.equal(h.calls[0].target_lang, 'ko');
    h.payload({text: '你好', target_lang: 'en'});
    assert.equal(h.calls.at(-1).target_lang, 'en');
    h.payload({text: 'next'});
    assert.equal(h.calls.at(-1).target_lang, 'ko');
});
test('设置加载期间的新唤起优先于初始载荷', async () => {
    let resolve;
    const h = setup({payload: {text: 'old'}, settingsPromise: new Promise(done => {resolve = done;})});
    await settle();
    h.payload({text: 'latest'});
    resolve({});
    await settle();
    assert.equal(h.calls.length, 1);
    assert.equal(h.calls[0].text, 'latest');
});
