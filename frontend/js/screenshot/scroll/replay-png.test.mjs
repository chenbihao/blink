import assert from 'node:assert/strict';
import {deflateSync} from 'node:zlib';
import {decodeReplayPng} from './replay-png.mjs';

function chunk(type, data) {
    const result = Buffer.alloc(data.length + 12);
    result.writeUInt32BE(data.length, 0);
    result.write(type, 4, 4, 'ascii');
    data.copy(result, 8);
    return result;
}

const signature = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(2, 0);
ihdr.writeUInt32BE(1, 4);
ihdr[8] = 8;
ihdr[9] = 6;
const scanline = Buffer.from([0, 255, 0, 128, 255, 10, 20, 30, 40]);
const png = Buffer.concat([
    signature,
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(scanline)),
    chunk('IEND', Buffer.alloc(0)),
]);
const decoded = decodeReplayPng(png);
assert.equal(decoded.width, 2);
assert.equal(decoded.height, 1);
assert.deepEqual([...decoded.data], [255, 0, 128, 255, 10, 20, 30, 40]);

console.log('scroll replay PNG tests passed');
