//! 快捷键录制握手：后端 recorder armed 后才通知调用方展示“正在录制”。

import {EVENTS} from "./event-names.js";
import {invoke, listen} from "./tauri.js";

let nextRequestId = 0;

/**
 * 启动一次快捷键录制。
 * @param {() => void} onReady 后端 recorder armed 后调用。
 * @returns {Promise<{modifiers: string[], key: string, display: string}>}
 */
export async function recordHotkey(onReady) {
    const requestId = `${Date.now()}-${++nextRequestId}`;
    const startedAt = performance.now();
    console.info("[hotkey-recorder] requested", {requestId});
    let resolveReady;
    const readyPromise = new Promise((resolve) => {
        resolveReady = resolve;
    });

    const unlisten = await listen(EVENTS.HOTKEY_RECORDING_READY, (event) => {
        if (event.payload?.requestId === requestId) {
            console.info("[hotkey-recorder] ready", {
                requestId,
                sessionId: event.payload?.sessionId,
                elapsedMs: Math.round(performance.now() - startedAt),
            });
            resolveReady(event.payload?.sessionId);
        }
    });

    const recordPromise = invoke("record_hotkey", {requestId});
    try {
        // 正常路径一定先 ready、后录制完成。done 分支覆盖并发拒绝或用户在 ready
        // 事件送达前就完成按键的极端情况，保证不会永远等一个已错过的事件。
        const first = await Promise.race([
            readyPromise.then(() => ({kind: "ready"})),
            recordPromise.then((value) => ({kind: "done", value})),
        ]);
        if (first.kind === "ready") {
            onReady?.();
            const value = await recordPromise;
            console.info("[hotkey-recorder] completed", {
                requestId,
                elapsedMs: Math.round(performance.now() - startedAt),
                display: value?.display,
            });
            return value;
        }
        console.warn("[hotkey-recorder] completed before ready", {
            requestId,
            elapsedMs: Math.round(performance.now() - startedAt),
        });
        return first.value;
    } finally {
        unlisten();
    }
}
