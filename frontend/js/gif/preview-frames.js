/** 两个有界二进制解码槽；当前帧优先，播放落后时丢弃尚未开始的旧请求。 */
export class PreviewFrames {
    constructor({state, invoke, onReady, evict, decode = decodeFrame, onError = console.error}) {
        Object.assign(this, {state, invoke, onReady, evict, decode, onError});
        this.jobs = new Map(); this.queue = []; this.active = 0;
    }
    request(indices, priority = false) {
        const s = this.state;
        this.queue = this.queue.filter(job => {
            const keep = job.epoch === s.epoch && job.width === s.previewWidth &&
                (!s.playing || job.index >= (s.current?.srcIdx ?? 0));
            if (!keep) this.jobs.delete(job.key);
            return keep;
        });
        const added = [];
        for (const index of indices) {
            if (!Number.isInteger(index) || index < 0 || (s.playing && index < (s.current?.srcIdx ?? 0))) continue;
            if (s.previewCache.has(index) || index >= (s.view?.frameCount ?? 0)) continue;
            const key = `${s.epoch}:${s.previewWidth}:${index}`;
            const existing = this.jobs.get(key);
            if (existing) {
                const at = this.queue.indexOf(existing);
                if (priority && at >= 0) this.queue.unshift(...this.queue.splice(at, 1));
                continue;
            }
            const job = {key, index, epoch: s.epoch, width: s.previewWidth, sessionId: s.sessionId};
            this.jobs.set(key, job); added.push(job);
        }
        if (priority) this.queue.unshift(...added); else this.queue.push(...added);
        while (this.queue.length > 32) this.jobs.delete(this.queue.pop().key);
        this.pump();
    }
    owns(job) {
        const s = this.state;
        return job.epoch === s.epoch && job.sessionId === s.sessionId && job.width === s.previewWidth;
    }
    pump() {
        while (this.active < 2 && this.queue.length) {
            const job = this.queue.shift();
            if (!this.owns(job) || (this.state.playing && job.index < (this.state.current?.srcIdx ?? 0))) {
                this.jobs.delete(job.key); continue;
            }
            this.active++;
            void this.run(job).finally(() => {
                this.active--; this.jobs.delete(job.key); this.pump();
            });
        }
    }
    async run(job) {
        try {
            const bytes = await this.invoke('gif_get_preview_frame', {
                sessionId: job.sessionId, index: job.index, maxW: job.width,
            });
            if (!this.owns(job)) return;
            const bitmap = await this.decode(bytes);
            if (!this.owns(job)) { bitmap.close(); return; }
            const s = this.state, old = s.previewCache.get(job.index);
            if (old) { s.previewBytes -= old.width * old.height * 4; old.close(); }
            s.previewCache.set(job.index, bitmap);
            s.previewBytes += bitmap.width * bitmap.height * 4;
            this.evict();
            this.onReady(job.index);
        } catch (error) { if (this.owns(job)) this.onError('[gif-editor] 预览帧解码失败', error); }
    }
}

export async function decodeFrame(value) {
    const bytes = value instanceof ArrayBuffer ? new Uint8Array(value) :
        ArrayBuffer.isView(value) ? new Uint8Array(value.buffer, value.byteOffset, value.byteLength) : null;
    if (!bytes || bytes.byteLength < 8) throw new Error('预览帧响应无效');
    const header = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const w = header.getUint32(0, true), h = header.getUint32(4, true);
    if (!w || !h || bytes.byteLength !== 8 + w * h * 4 || bytes.byteLength > (16 << 20) + 8)
        throw new Error('预览帧尺寸或预算无效');
    const pixels = new Uint8ClampedArray(bytes.buffer, bytes.byteOffset + 8, w * h * 4);
    return createImageBitmap(new ImageData(pixels, w, h));
}

/** 解码慢于时钟时展示最新已就绪的过去帧；暂停/精确定位只接受目标帧。 */
export function readyPreview(cache, current, playing) {
    if (cache.has(current)) return cache.get(current);
    if (!playing) return null;
    let nearest = -1;
    for (const index of cache.keys()) if (index <= current && index > nearest) nearest = index;
    return cache.get(nearest) || null;
}
