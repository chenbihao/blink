import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const sdk = readFileSync(new URL('../../../resources/plugin-host/sdk.js', import.meta.url), 'utf8');

test('plugin payloads use a private channel and preserve the latest payload during ready', async () => {
    let resolveReady;
    let channel;
    const window = {__TAURI__: {core: {
        Channel: class { constructor() { channel = this; } },
        invoke(command, args) {
            assert.equal(command, 'plugin_page_ready');
            assert.equal(args.onPayload, channel);
            return new Promise(resolve => { resolveReady = resolve; });
        },
    }}};
    vm.runInNewContext(sdk, {window, document: {head: null}});
    const received = [];
    const unsubscribe = await window.Blink.onPayload(value => received.push(value));
    const ready = window.Blink.init('builtin.translate');
    channel.onmessage({text: 'latest'});
    resolveReady({payload: {text: 'initial'}, context: {}});
    assert.equal((await ready).payload.text, 'latest');
    assert.deepEqual(received, []);
    channel.onmessage({text: 'next'});
    assert.deepEqual(received, [{text: 'next'}]);
    unsubscribe();
    channel.onmessage({text: 'ignored'});
    assert.equal(received.length, 1);
});
