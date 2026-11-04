import test from 'node:test';
import assert from 'node:assert/strict';
import {createRecordingSession} from './recording-session.js';

test('all recording exits invalidate pending work and restore a clean preparation session', () => {
    const session=createRecordingSession();
    for(const mode of ['ready','starting','recording','paused','stopping']) {
        session.reset('ready');
        const epoch=session.epoch;
        session.setMode(mode);
        if(!['ready','starting'].includes(mode)) session.sessionId='old';
        const id=session.sessionId;
        session.drawing=true;session.restoredControlsOnly=true;session.audioError='device lost';
        session.textInputs.add({});session.cancelArmed=true;
        session.reset();
        assert.equal(session.mode,'screenshot');
        assert.equal(session.active,false);
        assert.equal(session.owns(epoch,id),false);
        session.reset('ready');
        assert.equal(session.preparing,true);
        assert.equal(session.busy,false);
        assert.equal(session.drawing,false);
        assert.equal(session.restoredControlsOnly,false);
        assert.equal(session.audioError,null);
        assert.equal(session.textInputs.size,0);
        assert.equal(session.cancelArmed,false);
    }
});

test('pause and resume retain the same capture identity while mode governs every operation', () => {
    const session=createRecordingSession();session.reset('ready');
    const epoch=session.epoch;
    session.setMode('starting');assert.equal(session.busy,true);assert.equal(session.active,false);
    session.sessionId='capture';session.setMode('recording');
    session.setMode('paused');assert.equal(session.active,true);assert.equal(session.busy,false);
    assert.equal(session.owns(epoch,'capture'),true);
    session.setMode('recording');assert.equal(session.owns(epoch,'capture'),true);
    session.setMode('stopping');assert.equal(session.busy,true);
    assert.throws(()=>session.setMode('recording-ish'),TypeError);
});
