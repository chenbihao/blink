import assert from 'node:assert/strict';

globalThis.window = { localStorage: null };
const { readScrollDebugSetting, writeScrollDebugSetting } = await import(
  new URL('./scroll-debug-setting.js', import.meta.url)
);

const values = new Map();
const storage = {
  getItem: (key) => values.get(key) ?? null,
  setItem: (key, value) => values.set(key, value),
  removeItem: (key) => values.delete(key),
};

assert.equal(readScrollDebugSetting(storage), false);
writeScrollDebugSetting(true, storage);
assert.equal(values.get('blink.scrollDebug'), '1');
assert.equal(readScrollDebugSetting(storage), true);
writeScrollDebugSetting(false, storage);
assert.equal(values.has('blink.scrollDebug'), false);
assert.equal(readScrollDebugSetting({ getItem: () => { throw new Error('denied'); } }), false);

console.log('scroll debug setting tests passed');
