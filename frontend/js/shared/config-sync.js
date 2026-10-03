/** 单飞配置读取；旧响应（含错误）不能提交，写入或编辑期间延后回填。 */
export function createConfigRefresher({read, apply, onError = console.error}) {
    let generation = 0, dirty = false, running = null, holds = 0, latest;
    let committed = false, initialFailed = false, initialError;
    let readyWaiters = [];
    async function drain() {
        while (dirty && holds === 0) {
            dirty = false;
            const revision = generation;
            try {
                const value = await read();
                if (revision !== generation || holds > 0) { dirty = true; continue; }
                apply(value);
                latest = value;
                committed = true;
                initialFailed = false;
                initialError = undefined;
                for (const waiter of readyWaiters) waiter.resolve(value);
                readyWaiters = [];
            } catch (error) {
                if (revision !== generation) { dirty = true; continue; }
                if (!committed) {
                    initialFailed = true;
                    initialError = error;
                    for (const waiter of readyWaiters) waiter.reject(error);
                    readyWaiters = [];
                }
                onError(error);
            }
        }
        return latest;
    }
    function schedule() {
        if (running) return running;
        if (holds > 0 || !dirty) return Promise.resolve(latest);
        // 首个 await 前先建立 single-flight，避免 apply/read 触发重入。
        running = Promise.resolve().then(drain).finally(() => { running = null; if (dirty && holds === 0) schedule(); });
        return running;
    }
    return {
        ready() {
            if (committed) return Promise.resolve(latest);
            if (initialFailed) return Promise.reject(initialError);
            return new Promise((resolve, reject) => readyWaiters.push({resolve, reject}));
        },
        refresh() { initialFailed = false; initialError = undefined; generation++; dirty = true; return schedule(); },
        hold() {
            generation++; holds++; dirty = true;
            let released = false;
            return () => {
                if (released) return;
                released = true;
                holds--; generation++; dirty = true;
                schedule();
            };
        },
    };
}

export function affectsSharedConfig(payload) {
    const key = payload?.key;
    return !key || key.startsWith("app.") || ["auto_start", "language", "hotkey", "chord_toggles", "chord_bindings", "disabled_chord_actions"].includes(key);
}

let activityId = 0;
const ACTIVITY_EVENT = "blink:config-activity";
function notify(detail) {
    if (globalThis.document?.dispatchEvent && typeof CustomEvent === "function") {
        document.dispatchEvent(new CustomEvent(ACTIVITY_EVENT, {detail}));
    }
}

/** 释放延后到调用方更新 confirmed / 回滚完成之后，随后权威重读。 */
export async function withConfigActivity(key, operation) {
    const id = ++activityId;
    notify({id, key, pending: true});
    try { return await operation(); }
    finally { setTimeout(() => notify({id, key, pending: false}), 0); }
}

/** 每个窗口仅安装一次；保留尚未提交的输入及滑块编辑。 */
export function connectConfigActivity(refresher, target = document) {
    const writes = new Map(), edits = new Map();
    target.addEventListener(ACTIVITY_EVENT, ({detail}) => {
        if (detail.pending) writes.set(detail.id, refresher.hold());
        else { writes.get(detail.id)?.(); writes.delete(detail.id); }
    });
    target.addEventListener("input", ({target: control}) => {
        if (!control?.matches?.('input:not([type="checkbox"]), textarea') || edits.has(control)) return;
        edits.set(control, refresher.hold());
    }, true);
    const finishEdit = ({target: control}) => {
        const release = edits.get(control);
        if (!release) return;
        edits.delete(control);
        setTimeout(release, 0);
    };
    target.addEventListener("change", finishEdit, true);
    target.addEventListener("focusout", finishEdit, true);
}
