//! 原生 ES module Worker；匹配、固定层处理、提交和最终合成都在此线程执行。
import {ScrollTrackingEngine} from './tracking-engine.js';
import {createScrollThumbnail} from './thumbnail.js';

let engine;
self.onmessage = ({data: {id, type, payload}}) => {
    try {
        let result;
        const transfer = [];
        if (type === 'init') {
            const seed = payload || {};
            seed.frames = (seed.frames || []).map(item => ({top: item.top,
                image: new ImageData(new Uint8ClampedArray(item.buffer), item.width, item.height)}));
            engine = new ScrollTrackingEngine(seed, seed.limits);
            result = null;
        } else if (type === 'frame') {
            if (!engine) throw new Error('长截图追踪尚未初始化');
            const frame = new ImageData(new Uint8ClampedArray(payload.buffer), payload.width, payload.height);
            result = engine.process(frame, payload.options);
            if (result.decision.accepted) {
                result.segments = engine.takeSegments();
                for (const item of result.segments) if (item.buffer) {
                    const thumbnail = createScrollThumbnail(new ImageData(
                        new Uint8ClampedArray(item.buffer), item.width, item.height));
                    item.preview = {width: thumbnail.width, height: thumbnail.height, buffer: thumbnail.data.buffer};
                    transfer.push(item.buffer, item.preview.buffer);
                }
            }
        } else if (type === 'compose') {
            if (!engine) throw new Error('长截图追踪尚未初始化');
            const image = engine.compose();
            result = image && {width: image.width, height: image.height, buffer: image.data.buffer};
            if (result) transfer.push(result.buffer);
        } else throw new Error('未知长截图 Worker 请求');
        self.postMessage({id, result}, transfer);
    } catch (error) {
        self.postMessage({id, error: error?.message || String(error)});
    }
};
