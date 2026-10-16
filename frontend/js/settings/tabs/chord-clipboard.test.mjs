import assert from 'node:assert/strict';

globalThis.window = globalThis;
const {createClipboardConfigWriter} = await import('./chord-clipboard.js');
const tick = () => new Promise(resolve => setTimeout(resolve, 0));
let persisted = {max_items: 10000, search_enabled: true, enabled: false};
const requests = [];
let firstSave;
globalThis.__TAURI__ = {core: {invoke: async (command, args) => {
    if (command === 'get_config') return {clipboard: structuredClone(persisted)};
    assert.equal(command, 'set_config');
    requests.push(args);
    if (requests.length === 1) await new Promise(resolve => { firstSave = resolve; });
    for (const field of Object.keys(args.value)) assert.deepEqual(args.expected[field], persisted[field]);
    Object.assign(persisted, args.value);
}}};
const errors = [];
const writer = createClipboardConfigWriter({onError: message => errors.push(message)});
writer.setConfirmed(persisted);
const limit = {dataset: {field: 'max_items'}, value: '20000', checkValidity: () => true};
const first = writer.save(limit);
await tick();
limit.value = '10000';
const second = writer.save(limit);
limit.value = '30000';
const third = writer.save(limit);
limit.value = '10000'; // 事件捕获后 DOM 变化不能更改已排队的意图。
await tick();
assert.equal(requests.length, 1, '快速切换串行保存');
firstSave();
await Promise.all([first, second, third]);
assert.deepEqual(requests.map(r => r.value), [{max_items: 20000}, {max_items: 10000}, {max_items: 30000}]);
assert.equal(persisted.max_items, 30000);
assert.equal(persisted.enabled, false, '仅修改目标字段，不覆盖独立监听开关');

globalThis.__TAURI__.core.invoke = async (command) => {
    if (command === 'get_config') return {clipboard: {...persisted, max_items: 20000}};
    throw {message: 'config_conflict: changed'};
};
assert.equal(await writer.save(limit), false, '保存失败返回明确结果');
assert.match(errors.at(-1), /config_conflict/);
const invalid = {dataset: {field: 'max_items'}, value: '', reportValidity() { this.reported = true; }};
assert.equal(await writer.save(invalid), false);
assert.equal(invalid.reported, true, '空值不静默回写默认上限');
console.log('clipboard settings: captured field values, serialized intent, CAS and failure recovery passed');
