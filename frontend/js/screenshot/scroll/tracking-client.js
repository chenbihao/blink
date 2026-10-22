//! 每个会话独占 Worker；终止会话立即拒绝在途请求，不允许迟到回复写入新会话。
export class ScrollTrackingClient {
    constructor(seed = {}, createWorker = () => new Worker(new URL('./tracking-worker.js', import.meta.url), {type: 'module'})) {
        this.pending = new Map();
        this.nextId = 0;
        this.closed = false;
        this.worker = createWorker();
        this.worker.onmessage = ({data}) => {
            const request = this.pending.get(data.id);
            if (!request) return;
            this.pending.delete(data.id);
            clearTimeout(request.timer);
            if (data.error) request.reject(new Error(data.error));
            else request.resolve(data.result);
        };
        this.worker.onerror = () => this.dispose(new Error('长截图追踪线程异常'));
        this.worker.onmessageerror = () => this.dispose(new Error('长截图追踪消息无效'));
        const frames = (seed.frames || []).map(({image, top}) => ({top, width: image.width,
            height: image.height, buffer: image.data.slice().buffer}));
        this.ready = this.request('init', {...seed, frames}, frames.map(item => item.buffer));
        // 初始化失败可早于第一帧调用；保留 rejected ready 供调用方感知，避免悬空 rejection。
        this.ready.catch(() => {});
    }

    request(type, payload, transfer = []) {
        if (this.closed) return Promise.reject(new Error('长截图追踪会话已关闭'));
        if (this.pending.size >= 2) return Promise.reject(new Error('长截图追踪请求积压'));
        return new Promise((resolve, reject) => {
            const id = ++this.nextId;
            const timer = setTimeout(() => this.dispose(new Error('长截图追踪线程响应超时')), 5000);
            this.pending.set(id, {resolve, reject, timer});
            try { this.worker.postMessage({id, type, payload}, transfer); }
            catch (error) { clearTimeout(timer); this.pending.delete(id); reject(error); }
        });
    }

    async process(frame, options) {
        await this.ready;
        return this.request('frame', {width: frame.width, height: frame.height,
            buffer: frame.data.buffer, options}, [frame.data.buffer]);
    }

    async compose() {
        await this.ready;
        const image = await this.request('compose');
        return image && new ImageData(new Uint8ClampedArray(image.buffer), image.width, image.height);
    }

    dispose(error = new Error('长截图追踪会话已关闭')) {
        if (this.closed) return;
        this.closed = true;
        this.worker.terminate();
        for (const request of this.pending.values()) {
            clearTimeout(request.timer);
            request.reject(error);
        }
        this.pending.clear();
    }
}
