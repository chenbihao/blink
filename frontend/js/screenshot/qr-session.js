// Display request generations are independent of OCR and of backend worker admission.
export class QrSession {
    constructor() { this.generation = 0; this.cache = null; this.key = null; }
    begin(key) { this.key = key; return {key, generation: ++this.generation}; }
    current(token, key) { return token.generation === this.generation && token.key === key && this.key === key; }
    close() { ++this.generation; }
    reset() { this.close(); this.key = null; this.cache = null; }
}

// One active metadata probe and one latest waiter. Rapid item changes don't queue IO.
export function createLatestQrResolver(resolveTarget) {
    let running = false, pending = null;
    async function drain() {
        if (running) return;
        running = true;
        try {
            while (pending) {
                const job = pending;
                pending = null;
                if (!job.current()) { job.resolve(null); continue; }
                try { job.resolve(await resolveTarget(job.text)); }
                catch (error) { job.reject(error); }
            }
        } finally { running = false; }
    }
    return (text, current) => new Promise((resolve, reject) => {
        pending?.resolve(null);
        pending = {text, current, resolve, reject};
        void drain();
    });
}

export function qrActionAllowed(target) {
    if (!target || typeof target.target !== 'string') return false;
    if (target.kind === 'web') {
        try {
            const url = new URL(target.target);
            return ['http:', 'https:'].includes(url.protocol) && url.hostname === target.host;
        } catch { return false; }
    }
    return ['directory', 'file'].includes(target.kind)
        && /^[A-Za-z]:[\\/]/.test(target.target)
        && !/[\x00-\x1f"<>|*?]/.test(target.target)
        && !target.target.slice(2).includes(':');
}
