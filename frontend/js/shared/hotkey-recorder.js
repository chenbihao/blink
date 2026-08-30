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
    let readySessionId;

    const unlisten = await listen(EVENTS.HOTKEY_RECORDING_READY, (event) => {
        if (event.payload?.requestId === requestId) {
            readySessionId = event.payload?.sessionId;
            console.info("[hotkey-recorder] ready", {
                requestId,
                sessionId: readySessionId,
                elapsedMs: Math.round(performance.now() - startedAt),
            });
            resolveReady(readySessionId);
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
            ackReady(requestId, readySessionId);
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

/**
 * 一次性诊断：回传 ready ACK（前端已显示“正在录制”）。
 * 失败只落 console，不影响录制流程。
 */
function ackReady(requestId, sessionId) {
    if (!sessionId) {
        return;
    }
    invoke("ack_hotkey_recording_ready", {requestId, sessionId}).catch((error) => {
        console.warn("[hotkey-recorder] ready ack failed", {requestId, sessionId, error});
    });
}
