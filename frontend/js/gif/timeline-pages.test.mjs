import test from 'node:test';
import assert from 'node:assert/strict';
import {
    TimelinePages
} from './timeline-pages.js';

test('mix-only revisions retain valid frames and selections while old pending results are ignored',async()=>{
    let resolve;
    const pages=new TimelinePages(async (_command,request)=>{
        if(request.selection)return {revision:request.revision,hasHold:true,durationMs:250};
        if(request.play===999)return new Promise(r=>{resolve=r;});
        return {revision:request.revision,frames:[{playIdx:1,srcIdx:1,startMs:100,durMs:100}]};
    });
    pages.reset('one',1);
    const first=await pages.locate({play:1});
    await pages.selection([[1,1]]);
    const pending=pages.locate({play:999});
    pages.reset('one',2,true);
    assert.equal(await pages.locate({play:1}),first);
    assert.equal((await pages.selection([[1,1]])).revision,2);
    resolve({revision:1,frames:[{playIdx:999}]});assert.equal(await pending,null);
    pages.reset('one',3);
    assert.equal(pages.cached({play:1}),null);
});

test('a long timeline caches pages and reuses playback/seek metadata without expanding frames', async () => {
    let calls = 0;
    const pages = new TimelinePages(async (_command, request) => {
        calls++;
        const first = request.play ?? Math.floor(request.timeMs / 20);
        return {
            revision: 7,
            frames: Array.from({
                length: 128
            }, (_, n) => ({
                playIdx: first + n,
                srcIdx: first + n,
                startMs: (first + n) * 20,
                durMs: 20
            }))
        };
    });
    pages.reset('long', 7);
    assert.equal((await pages.locate({
        play: 100000
    })).srcIdx, 100000);
    assert.equal((await pages.locate({
        timeMs: 2000025
    })).srcIdx, 100001);
    assert.equal(calls, 1);
    for (let n = 0; n < 20; n++) await pages.locate({
        play: n * 1000
    });
    assert.equal(pages.playPages.size, 8);
    assert.ok([...pages.playPages.values()].reduce((sum, p) => sum + p.frames.length, 0) <= 1024);
});

test('old session/revision responses are discarded and identical pending queries coalesce', async () => {
    let resolve, calls = 0;
    const pages = new TimelinePages(() => {
        calls++;
        return new Promise(r => {
            resolve = r;
        });
    });
    pages.reset('old', 1);
    const a = pages.locate({
            play: 1
        }),
        b = pages.locate({
            play: 1
        });
    assert.equal(calls, 1);
    pages.reset('new', 2);
    resolve({
        revision: 1,
        frames: [{
            playIdx: 1,
            srcIdx: 99
        }]
    });
    assert.equal(await a, null);
    assert.equal(await b, null);
    assert.equal(pages.playPages.size, 0);
    assert.equal(pages.pending.size, 0);
});

test('source metadata is bounded and includes deleted frames in source space', async () => {
    const requests = [];
    const pages = new TimelinePages(async (_cmd, request) => {
        requests.push(request);
        return {
            revision: 4,
            frames: [{
                srcIdx: request.sourceOffset,
                deleted: true
            }]
        };
    });
    pages.reset('a', 4);
    assert.equal((await pages.sources(1001))[0].srcIdx, 768);
    assert.equal((await pages.sources(1002))[0].deleted, true);
    assert.equal(requests.length, 1);
    assert.equal(requests[0].revision, 4);
});

test('playback prefetch reuses timeline pages and crosses page boundaries with a bounded window', async()=>{
    let calls=0;
    const pages=new TimelinePages(async(_command,{play})=>({revision:1,frames:Array.from({length:128},(_,i)=>({playIdx:play+i,srcIdx:(play+i)*2}))}));
    const original=pages.invoke;pages.invoke=(...args)=>{calls++;return original(...args);};
    pages.reset('one',1);
    await pages.locate({play:0});
    assert.equal((await pages.ahead(1,16)).length,16);
    assert.equal(calls,1);
    const window=await pages.ahead(120,16);
    assert.equal(window.length,16);assert.equal(window.at(-1).srcIdx,270);
    assert.equal(calls,2);
    assert.equal((await pages.ahead(120,1000)).length,32);
});
