/** Bounded timeline metadata cache. Every reply belongs to one session/revision. */
export class TimelinePages {
    constructor(invoke) {
        this.invoke = invoke;
        this.inflight = new Set();
        this.reset(null, 0);
    }
    reset(sessionId, revision) {
        this.generation = (this.generation || 0) + 1;
        this.sessionId = sessionId;
        this.revision = revision;
        this.playPages = new Map();
        this.sourcePages = new Map();
        this.pending = new Map();
    }
    async query(key, request, cache) {
        if (cache.has(key)) return cache.get(key);
        if (this.pending.has(key)) return this.pending.get(key);
        const generation = this.generation;
        if (this.pending.size >= 4) return null;
        if (this.inflight.size >= 4) {
            await Promise.race([...this.inflight].map(p => p.catch(() => null)));
            if (generation !== this.generation) return null;
            return this.query(key, request, cache);
        }
        const task = this.invoke('gif_timeline_window', {
                sessionId: this.sessionId,
                revision: this.revision,
                ...request
            })
            .then(value => {
                if (generation !== this.generation || value.revision !== this.revision) return null;
                cache.set(key, value);
                while (cache.size > 8) cache.delete(cache.keys().next().value);
                return value;
            }).finally(() => {
                this.inflight.delete(task);
                if (generation === this.generation) this.pending.delete(key);
            });
        this.pending.set(key, task);
        this.inflight.add(task);
        return task;
    }
    async locate(request) {
        for (const page of this.playPages.values()) {
            const frame = page.frames.find(f => request.play != null ? f.playIdx === request.play :
                request.source != null ? f.srcIdx === request.source :
                f.startMs <= request.timeMs && request.timeMs < f.startMs + f.durMs);
            if (frame) return frame;
        }
        const key = `play:${JSON.stringify(request)}`;
        const page = await this.query(key, request, this.playPages);
        return page?.frames[0] || null;
    }
    async sources(offset) {
        const first = Math.floor(Math.max(0, offset) / 256) * 256;
        return (await this.query(`source:${first}`, {
            sourceOffset: first
        }, this.sourcePages))?.frames || [];
    }
}
