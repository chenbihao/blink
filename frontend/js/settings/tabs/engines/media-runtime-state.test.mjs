import assert from 'node:assert/strict';
import {test} from 'node:test';
import {mergeMediaRuntimeSnapshot, mediaRuntimeAction} from './media-runtime-state.js';

const state = (revision, runtime = 'ready', operation = null, epoch = 'first') => ({epoch, revision, runtime: {state: runtime}, operation});
test('late status cannot erase a newer installation operation', () => {
    const active = state(3, 'missing', {id: 'install'});
    assert.equal(mergeMediaRuntimeSnapshot(active, state(2)), active);
    assert.equal(mediaRuntimeAction(active), null);
});
test('same revision can refresh disk state; a restarted service has a new epoch', () => {
    const ready = state(9);
    const changed = state(9, 'corrupt');
    assert.equal(mergeMediaRuntimeSnapshot(ready, changed), changed);
    const restarted = state(0, 'missing', null, 'second');
    assert.equal(mergeMediaRuntimeSnapshot(ready, restarted), restarted);
});
test('actions follow readiness rather than just an executable existing', () => {
    assert.equal(mediaRuntimeAction(null), null);
    assert.equal(mediaRuntimeAction(state(0, 'missing')), 'install');
    assert.equal(mediaRuntimeAction(state(0, 'corrupt')), 'repair');
    assert.equal(mediaRuntimeAction(state(0, 'outdated')), 'repair');
    assert.equal(mediaRuntimeAction(state(0)), 'diagnose');
    assert.equal(mediaRuntimeAction(state(0, 'pending_remove')), null);
});
