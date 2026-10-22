import {readFile} from 'node:fs/promises';
import {Worker as NodeWorker} from 'node:worker_threads';
import {deflateSync} from 'node:zlib';

export const dataUrl = source => 'data:text/javascript;base64,' + Buffer.from(source).toString('base64');
export const loadSource = name => readFile(new URL(name, import.meta.url), 'utf8');
export const imageDataPolyfill = `globalThis.ImageData = class ImageData {
    constructor(data, w, h) {
        this.width = typeof data === 'number' ? data : w;
        this.height = typeof data === 'number' ? w : h;
        this.data = typeof data === 'number' ? new Uint8ClampedArray(data * w * 4) : data;
    }
};`;
if (!globalThis.ImageData) new Function(imageDataPolyfill)();

export function documentFrame(top, width = 36, height = 90) {
    const frame = new ImageData(width, height);
    for (let y = 0; y < height; y++) for (let x = 0; x < width; x++) {
        const i = (y * width + x) * 4, yy = top + y;
        frame.data[i] = (yy * 17 + Math.floor(yy / 7) * 29 + x * 3) & 255;
        frame.data[i + 1] = (yy * 7 + Math.floor(yy / 11) * 43 + x * 11) & 255;
        frame.data[i + 2] = (yy * 13 + Math.floor(yy / 17) * 61 + x * 5) & 255;
        frame.data[i + 3] = 255;
    }
    return frame;
}

export function encodeTestPng(frame) {
    const chunk = (type, data) => {
        const result = Buffer.alloc(data.length + 12);
        result.writeUInt32BE(data.length); result.write(type, 4); data.copy(result, 8);
        let crc = 0xffffffff;
        for (const byte of result.subarray(4, result.length - 4)) {
            crc ^= byte;
            for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0);
        }
        result.writeUInt32BE((crc ^ 0xffffffff) >>> 0, result.length - 4);
        return result;
    };
    const header = Buffer.alloc(13);
    header.writeUInt32BE(frame.width); header.writeUInt32BE(frame.height, 4);
    header[8] = 8; header[9] = 6;
    const scanlines = Buffer.alloc((frame.width * 4 + 1) * frame.height);
    for (let y = 0; y < frame.height; y++) scanlines.set(
        frame.data.subarray(y * frame.width * 4, (y + 1) * frame.width * 4),
        y * (frame.width * 4 + 1) + 1);
    return Buffer.concat([Buffer.from([137,80,78,71,13,10,26,10]), chunk('IHDR', header),
        chunk('IDAT', deflateSync(scanlines)), chunk('IEND', Buffer.alloc(0))]);
}

const stitch = dataUrl(await loadSource('./stitch.js'));
const tracker = dataUrl((await loadSource('./tracker.js')).replace("'./stitch.js'", JSON.stringify(stitch)));
export const engineUrl = dataUrl((await loadSource('./tracking-engine.js'))
    .replace("'./stitch.js'", JSON.stringify(stitch)).replace("'./tracker.js'", JSON.stringify(tracker)));
export const {ScrollTrackingEngine} = await import(engineUrl);
export const {createScrollThumbnail} = await import(dataUrl(await loadSource('./thumbnail.js')));
const workerSource = (await loadSource('./tracking-worker.js'))
    .replace("'./thumbnail.js'", JSON.stringify(dataUrl(await loadSource('./thumbnail.js'))));
export const realWorkerUrl = new URL(dataUrl(`
import {parentPort} from 'node:worker_threads';
${imageDataPolyfill}
globalThis.self = {postMessage: (data, transfer) => parentPort.postMessage(data, transfer)};
parentPort.on('message', data => self.onmessage({data}));
${workerSource.replace("'./tracking-engine.js'", JSON.stringify(engineUrl))}
`));

export function createRealWorker() {
    const worker = new NodeWorker(realWorkerUrl, {type: 'module'});
    const adapter = {postMessage: (...args) => worker.postMessage(...args), terminate: () => worker.terminate()};
    worker.on('message', data => adapter.onmessage?.({data}));
    worker.on('error', error => adapter.onerror?.(error));
    worker.on('messageerror', error => adapter.onmessageerror?.(error));
    return adapter;
}

/** 可控时钟的生命周期测试使用同一 Worker 处理体，并真实执行 transferable 语义。 */
export class ClockedWorker {
    constructor() {
        this.closed = false;
        this.scope = {postMessage: (data, transfer) => {
            const message = structuredClone(data, {transfer});
            queueMicrotask(() => { if (!this.closed) this.onmessage?.({data: message}); });
        }};
        new Function('self', 'ScrollTrackingEngine', 'createScrollThumbnail', workerSource.replace(/^import .*;$/gm, ''))(
            this.scope, ScrollTrackingEngine, createScrollThumbnail);
    }
    postMessage(data, transfer) {
        const message = structuredClone(data, {transfer});
        queueMicrotask(() => { if (!this.closed) this.scope.onmessage({data: message}); });
    }
    terminate() { this.closed = true; }
}
