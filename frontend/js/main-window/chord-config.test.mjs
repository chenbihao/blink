import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const source = readFileSync(new URL('./chord.js', import.meta.url), 'utf8')
    .replace(/^import[\s\S]*?from\s+["'][^"']+["'];\s*/gm, '').replace(/^export /gm, '');
const requests = [];
const context = vm.createContext({
    queryEl: {value: ''}, console,
    listChordActions: () => new Promise((resolve, reject) => requests.push({resolve, reject})),
});
vm.runInContext(source, context);
const old = context.refreshFromConfigData({chord_enabled: true});
const latest = context.refreshFromConfigData({chord_enabled: true});
requests[1].resolve([{id: 'new'}]); await latest;
requests[0].resolve([{id: 'old'}]); await old;
assert.equal(context.getActions()[0].id, 'new', '旧动作读取不能覆盖新配置投影');
const pending = context.refreshFromConfigData({chord_enabled: true});
await context.refreshFromConfigData({chord_enabled: false});
requests[2].reject(new Error('old failure')); await pending;
assert.equal(context.isEnabled(), false);
assert.equal(context.getActions().length, 0, '关闭配置不被旧回调恢复');
console.log('chord config: old action responses and failures ignored');
