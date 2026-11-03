import test from 'node:test';
import assert from 'node:assert/strict';
import {
    TimelinePages
} from './timeline-pages.js';

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
