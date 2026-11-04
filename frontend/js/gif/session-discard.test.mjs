import test from 'node:test';
import assert from 'node:assert/strict';
import {discardRecording} from './session-discard.js';

test('deleting waits for export cancellation to finish and binds explicit consent to the session', async () => {
    const calls = [], stages = ['encoding', 'cancelling', 'cancelled'];
    await discardRecording({sessionId: 'one', isCurrent: () => true, now: () => 0, wait: async () => {},
        invoke: async (command, args) => {
            calls.push([command, args]);
            if (command === 'gif_export_status') return {stage: stages.shift()};
        }});
    assert.deepEqual(calls.map(c => c[0]), ['gif_export_status','gif_export_cancel','gif_export_status','gif_export_status','gif_discard_session']);
    assert.deepEqual(calls.at(-1)[1], {sessionId:'one', discardCurrent:true});
});

test('a replaced session or unresponsive export cannot be deleted by a late confirmation', async () => {
    let current = true;
    const calls = [];
    await assert.rejects(discardRecording({sessionId:'old', isCurrent:()=>current, now:()=>0,
        invoke:async command => {calls.push(command); current=false; return null;}}), e=>e.code==='gif_stale_session');
    assert.deepEqual(calls, ['gif_export_status']);
    let time=0;
    await assert.rejects(discardRecording({sessionId:'one', isCurrent:()=>true, now:()=>time, timeout:150,
        wait:async ms=>{time+=ms;}, invoke:async command=>{ assert.notEqual(command,'gif_discard_session'); return {stage:'cancelling'}; }}),
        e=>e.code==='gif_discard_timeout');
});
