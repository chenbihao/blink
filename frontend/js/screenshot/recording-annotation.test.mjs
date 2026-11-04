import test from 'node:test';
import assert from 'node:assert/strict';
import {createAnnotationUploader} from './recording-annotation.js';
const tick = () => new Promise(resolve => setImmediate(resolve));

test('drawing snapshots coalesce behind one binary upload and preserve session revisions', async () => {
    const encodes=[], sends=[];
    let release;
    const uploader=createAnnotationUploader({canvas:{toBlob:fn=>encodes.push(fn)}, sessionId:'one', interval:0,
        send:async(bytes,options)=>{sends.push({bytes,options}); await new Promise(resolve=>{release=resolve;});}});
    const first=uploader.flush();
    encodes.shift()(new Blob([new Uint8Array([1,2,3])]));
    await tick();
    for(let i=0;i<20;i++)uploader.notify();
    assert.equal(encodes.length,0);
    release(); await tick();
    assert.equal(encodes.length,1);
    encodes.shift()(new Blob([new Uint8Array([4])])); await tick();
    release(); await first;
    assert.equal(sends.length,2);
    assert.deepEqual([...sends[1].bytes],[4]);
    assert.equal(sends[1].options.headers['x-annotation-revision'],'2');
    assert.equal(sends[1].options.headers['x-recording-session'],'one');
    uploader.stop();
});

test('a canvas encoding that finishes after session cleanup never uploads into a new recording', async () => {
    let complete;
    const uploader=createAnnotationUploader({canvas:{toBlob:fn=>{complete=fn;}}, sessionId:'old',send:async()=>assert.fail('stale upload')});
    const task=uploader.flush(); uploader.stop(); complete(new Blob(['png'])); await task;
});
