//! 内容高度测量与布局会话：合并同一帧的测量，拒绝隐藏/旧会话的回调。
//! 同一搜索内高度只增不减；新搜索或临时 UI 收回时允许重新基准。

import {appEl} from "./dom.js";
import {resizeWindow} from "../shared/api.js";

const WIDTH = 700;
let maxHeight = 0;
let windowVisible = false;
let revision = 0;
let sequence = 0;
let generation = 0;
let pendingFrame = null;
let pendingRebase = false;
let clipboard = false;
let clipboardReady = false;
let interacted = false;

function invalidateMeasurement() {
    generation++;
    if (pendingFrame !== null) cancelAnimationFrame(pendingFrame);
    pendingFrame = null;
    pendingRebase = false;
}

/** 旧的模式预告/显隐事件不得覆盖较新的唤起。 */
export function acceptsWindowRevision(nextRevision) {
    return Number.isSafeInteger(nextRevision) && nextRevision >= revision;
}

/** 后端是可见性真源；返回 false 表示旧事件，调用方须连同 UI 复位一起丢弃。 */
export function setWindowVisibility(visible, nextRevision) {
    if (!acceptsWindowRevision(nextRevision)) return false;
    if (nextRevision !== revision || visible !== windowVisible) {
        invalidateMeasurement();
        sequence = 0;
        interacted = false;
    }
    revision = nextRevision;
    windowVisible = visible;
    return true;
}

/** 模式开始时首搜尚未完成；布局 ready 只能由当前请求成功/失败的结果确认。 */
export function setClipboardLayout(active, ready = false) {
    clipboard = active;
    clipboardReady = active && ready;
}

export function markClipboardLayoutReady() {
    if (!clipboard) return;
    clipboardReady = true;
    syncWindowSize();
}

/** 用户开始操作后及时上报，后台光标结果不能再把列表移走。 */
export function lockWindowPlacement() {
    if (!windowVisible || interacted) return;
    interacted = true;
    syncWindowSize();
}

function measure(rebase = false) {
    if (!windowVisible) return;
    pendingRebase ||= rebase;
    if (pendingFrame !== null) return;
    const myGeneration = generation;
    const myRevision = revision;
    pendingFrame = requestAnimationFrame(() => {
        if (!windowVisible || myGeneration !== generation || myRevision !== revision) return;
        pendingFrame = null;
        const height = appEl.offsetHeight;
        if (height <= 0) { pendingRebase = false; return; }
        maxHeight = pendingRebase ? height : Math.max(maxHeight, height);
        pendingRebase = false;
        appEl.style.minHeight = maxHeight + "px";
        resizeWindow(WIDTH, maxHeight, {
            revision: myRevision, sequence: ++sequence,
            clipboard, ready: !clipboard || clipboardReady, interacted,
        }).catch((error) => console.warn("主窗口布局提交失败", error));
    });
}

/** DOM 改动后在下一帧测量；多个来源的同步调用只发一次 IPC。 */
export function syncWindowSize() {
    measure();
}

export function resetMaxHeight() {
    invalidateMeasurement();
    maxHeight = 0;
    appEl.style.minHeight = "";
}

export function resyncWindowSize() {
    if (!windowVisible) return;
    appEl.style.minHeight = "";
    measure(true);
}
