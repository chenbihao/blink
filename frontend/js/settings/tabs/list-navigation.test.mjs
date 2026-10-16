import assert from 'node:assert/strict';
globalThis.window = globalThis;
const {createWheelModeWriter} = await import('./list-navigation.js');
const tick = () => new Promise(resolve => setTimeout(resolve, 0));
let persisted = {wheel_mode: 'step', page_size: 9, theme: 'dark'}, firstSave;
const requests = [], errors = [], saved = [];
globalThis.__TAURI__ = {core: {invoke: async (command, args) => {
    if (command === 'get_config') return {...persisted};
    assert.equal(command, 'set_config'); assert.equal(args.key, 'wheel_mode');
    requests.push(args);
    if (requests.length === 1) await new Promise(resolve => {firstSave = resolve;});
    assert.equal(args.expected, persisted.wheel_mode);
    persisted.wheel_mode = args.value;
}}};
const writer = createWheelModeWriter({onError: e => errors.push(e), onSaved: value => saved.push(value)});
writer.setConfirmed('step');
const first = writer.save('page'); await tick();
const second = writer.save('step'), third = writer.save('page');
assert.equal(requests.length, 1); firstSave();
await Promise.all([first, second, third]);
assert.deepEqual(requests.map(r => r.value), ['page', 'step', 'page']);
assert.deepEqual(saved, ['page', 'step', 'page']);
assert.equal(persisted.page_size, 9); assert.equal(persisted.theme, 'dark');
let failed = true;
globalThis.__TAURI__.core.invoke = async (command, args) => {
    if (command === 'get_config') return {wheel_mode: 'step'};
    if (failed) { failed = false; throw {message: 'config_conflict: changed'}; }
    assert.equal(args.expected, 'step', '失败后重读基线再提交下一意图');
};
assert.equal(await writer.save('step'), false);
assert.match(errors.at(-1).message, /config_conflict/);
assert.equal(await writer.save('page'), true);
console.log('wheel settings: serialized intent, field CAS and conflict recovery passed');
