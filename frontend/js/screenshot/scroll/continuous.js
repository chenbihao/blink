//! 采样与单提交者解耦。FIFO 保留中间重叠帧，按实际保留字节/帧数限流。
export class ContinuousScrollCapture {
    constructor(options) {
        this.options = options;
        this.queue = [];
        this.heldBytes = 0;
        this.heldFrames = 0;
        this.closed = false;
        this.paused = true;
        this.pauseRevision = 0;
        this.timer = 0;
        this.captureTask = null;
        this.consumerTask = null;
        this.controlTail = Promise.resolve();
        this.controlCount = 0;
        this.motionUntil = 0;
        this.discontinuity = false;
        this.pressured = false;
        this.maxFrames = options.maxFrames ?? 6;
        this.maxBytes = options.maxBytes ?? 96 * 1024 * 1024;
        this.peakBufferedBytes = 0;
        this.peakBufferedFrames = 0;
        this.now = options.now || (() => performance.now());
        this.setTimer = options.setTimer || ((callback, ms) => setTimeout(callback, ms));
        this.clearTimer = options.clearTimer || (id => clearTimeout(id));
    }

    start() { if (!this.closed) { this.paused = false; this.wake(); } }
    wake() {
        this.motionUntil = this.now() + 450;
        if (!this.closed && !this.paused) this.schedule(0);
    }
    schedule(ms) {
        if (this.timer) this.clearTimer(this.timer);
        if (this.closed || this.paused) return;
        this.timer = this.setTimer(() => {
            this.timer = 0;
            void this.capture().catch(error => this.options.onError?.(error)).finally(() => {
                this.schedule(this.now() < this.motionUntil ? 33 : 250);
            });
        }, ms);
    }
    stats() { return {frames: this.heldFrames, bytes: this.heldBytes, pressured: this.pressured}; }
    diagnostics() {
        return {mode: 'continuous-gdi-worker', activeIntervalMs: 33, idleIntervalMs: 250,
            maxBufferedFrames: this.maxFrames, maxBufferedBytes: this.maxBytes,
            peakBufferedFrames: this.peakBufferedFrames, peakBufferedBytes: this.peakBufferedBytes};
    }

    capture(metadata = {}, required = false) {
        if (this.closed) return Promise.resolve({reason: 'aborted'});
        if (this.captureTask) return this.captureTask;
        const bytes = this.options.frameBytes;
        if (bytes <= 0 || bytes > this.maxBytes) return Promise.reject(new Error('采集区域超过连续帧缓冲上限'));
        if (this.heldFrames >= this.maxFrames || this.heldBytes + bytes > this.maxBytes) {
            // 不能用最新帧覆盖旧帧；暂停采样后下一帧必须重新验证绝对位置。
            this.discontinuity = true;
            if (!this.pressured) this.options.onPressure?.(this.stats());
            this.pressured = true;
            return Promise.resolve({reason: 'backpressure'});
        }
        this.heldBytes += bytes;
        this.heldFrames++;
        this.peakBufferedBytes = Math.max(this.peakBufferedBytes, this.heldBytes);
        this.peakBufferedFrames = Math.max(this.peakBufferedFrames, this.heldFrames);
        const task = (async () => {
            let queued = false;
            try {
                const captureStartedAt = this.now();
                const inputMetadata = this.options.metadata?.() || {};
                const captured = await this.options.capture();
                if (this.closed) return {reason: 'aborted'};
                if (!captured.frame) throw new Error(captured.reason || '完整帧采集失败');
                const details = {...inputMetadata, ...metadata, captureStartedAt, capturedAt: this.now(),
                    discontinuity: this.discontinuity};
                this.discontinuity = false;
                let resolve, reject;
                const result = new Promise((done, fail) => { resolve = done; reject = fail; });
                // 后台采样不等待算法，但显式 sample 等待其自己的处理结果。
                result.catch(() => {});
                this.queue.push({frame: captured.frame, metadata: details, bytes, resolve, reject});
                queued = true;
                this.consume();
                return required ? {completion: result} : null;
            } finally {
                if (!queued) { this.heldBytes -= bytes; this.heldFrames--; }
            }
        })().finally(() => { if (this.captureTask === task) this.captureTask = null; });
        this.captureTask = task;
        return task;
    }

    consume() {
        if (this.consumerTask || this.closed) return;
        const task = (async () => {
            while (this.queue.length && !this.closed) {
                const entry = this.queue.shift();
                try {
                    const result = await this.options.process(entry.frame, entry.metadata);
                    if (!this.closed) {
                        if (result.moved) this.motionUntil = this.now() + 450;
                        this.options.onResult?.(result, entry.metadata);
                        entry.resolve(result);
                    } else entry.resolve({reason: 'aborted'});
                } catch (error) {
                    entry.reject(error);
                    if (!this.closed) this.options.onError?.(error);
                } finally {
                    this.heldBytes -= entry.bytes;
                    this.heldFrames--;
                    if (this.heldFrames <= 1 && this.heldBytes + this.options.frameBytes <= this.maxBytes) {
                        this.pressured = false;
                    }
                }
            }
        })().finally(() => {
            if (this.consumerTask === task) this.consumerTask = null;
            if (this.queue.length && !this.closed) this.consume();
        });
        this.consumerTask = task;
    }

    async drain() {
        while (this.captureTask || this.consumerTask) {
            await Promise.all([this.captureTask, this.consumerTask]);
        }
    }
    pause() {
        this.paused = true;
        this.pauseRevision++;
        if (this.timer) this.clearTimer(this.timer);
        this.timer = 0;
    }
    async pauseAndDrain() {
        this.pause();
        await this.controlTail;
        await this.drain();
    }

    /** 自动输入与收尾的强制新采样也通过相同 FIFO；不绕过中间帧。 */
    sample(metadata = {}) {
        if (this.controlCount >= 2) return Promise.reject(new Error('长截图采样请求积压'));
        this.controlCount++;
        const task = this.controlTail.catch(() => {}).then(async () => {
            const wasPaused = this.paused, revision = this.pauseRevision;
            this.paused = true;
            if (this.timer) this.clearTimer(this.timer);
            this.timer = 0;
            try {
                await this.drain();
                const captured = await this.capture(metadata, true);
                return captured?.completion ? await captured.completion : captured || {reason: 'aborted'};
            } finally {
                if (!wasPaused && revision === this.pauseRevision && !this.closed) {
                    this.paused = false;
                    this.schedule(33);
                }
            }
        }).finally(() => { this.controlCount--; });
        this.controlTail = task;
        return task;
    }

    dispose() {
        this.closed = true;
        this.paused = true;
        this.pauseRevision++;
        if (this.timer) this.clearTimer(this.timer);
        this.timer = 0;
        for (const entry of this.queue.splice(0)) {
            this.heldBytes -= entry.bytes;
            this.heldFrames--;
            entry.resolve({reason: 'aborted'});
        }
    }
}
