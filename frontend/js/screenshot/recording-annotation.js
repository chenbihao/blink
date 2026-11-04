//! 标注变化才上传 PNG 二进制快照；至多一个在途请求，连续笔画合并为最新状态。
export function createAnnotationUploader({canvas, sessionId, send, onError = () => {}, interval = 33}) {
    let active = true, dirty = true, revision = 0, timer = null, task = null;
    const encode = () => new Promise((resolve, reject) => {
        canvas.toBlob(blob => blob ? resolve(blob) : reject(new Error('标注画布编码失败')), 'image/png');
    });
    async function flush() {
        clearTimeout(timer); timer = null;
        if (task) { await task; if (active && dirty) return flush(); return; }
        if (!active || !dirty) return;
        task = (async () => {
            while (active && dirty) {
                dirty = false;
                const next = ++revision;
                const blob = await encode();
                if (!active) return;
                if (blob.size > 64 * 1024 * 1024) throw new Error('标注快照超过传输预算');
                const bytes = new Uint8Array(await blob.arrayBuffer());
                if (!active) return;
                await send(bytes, {headers: {'x-recording-session': sessionId, 'x-annotation-revision': String(next)}});
                if (active && dirty && interval > 0) await new Promise(resolve => setTimeout(resolve, interval));
            }
        })();
        try { await task; }
        catch (error) { dirty = true; throw error; }
        finally { task = null; }
    }
    return {
        notify() {
            if (!active) return;
            dirty = true;
            if (task || timer !== null) return;
            timer = setTimeout(() => { void flush().catch(error => { if (active) onError(error); }); }, interval);
        },
        flush,
        stop() { active = false; clearTimeout(timer); timer = null; },
    };
}
