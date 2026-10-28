import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const source = readFileSync(new URL('../../../plugins/dictionary/web/page.js', import.meta.url), 'utf8');

function harness(ops = []) {
    const calls = [];
    const timers = [];
    const status = {textContent: ''};
    const node = {addEventListener() {}, focus() {}};
    const context = vm.createContext({
        document: {
            getElementById: () => node,
            addEventListener() {},
            querySelector: selector => selector === '.dict-prepare-status' ? status : null,
        },
        window: {addEventListener() {}, __TAURI__: {core: {invoke: async (command, args) => {
            calls.push({command, args});
            return ops;
        }}}},
        Blink: {init: async () => ({}), onPayload() {}},
        setTimeout: callback => {timers.push(callback); return timers.length;},
        clearTimeout() {}, console,
    });
    vm.runInContext(source, context);
    return {context, calls, timers, status};
}

test('dictionary prepare bridge includes its plugin identity for every command', async () => {
    const {context, calls} = harness();
    await vm.runInContext('hostInvoke("plugin_page_prepare_dataset", {datasetId: "dictionary-basic"})', context);
    await vm.runInContext('hostInvoke("plugin_page_prepare_status", {})', context);
    assert.equal(calls[0].args.datasetId, 'dictionary-basic');
    assert.ok(calls.every(call => call.args.pluginId === 'builtin.dictionary'));
});

test('optional etymology failure does not mask a ready basic dictionary', async () => {
    const {context, timers, status} = harness([
        {dataset_id: 'dictionary-etymology', phase: {kind: 'failed', message: 'offline'}},
        {dataset_id: 'dictionary-basic', phase: {kind: 'succeeded'}},
    ]);
    await vm.runInContext('pollPrepareStatus()', context);
    await timers.shift()();
    assert.match(status.textContent, /已就绪/);
    assert.equal(timers.length, 0);
});
