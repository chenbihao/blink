//! GIF 录制控制条（0.28.7，gif-control 窗口）。
//!
//! 纯展示 + 指令转发：状态真源是后端 `blink://gif-recorder-state` 事件
//! （250ms 节流）+ ready 时 `gif_recorder_state` 拉取；本页不做任何时钟
//! 推算——倒计时数字、有效时长（暂停剔除）、帧数全部来自后端。
//! 按钮：暂停/继续（同一按钮切换图标）、停止（进编辑器）、取消（丢弃）。

import {ensureSpriteLoaded} from '../shared/icon.js';
import {formatElapsed, renderState} from './recorder-view.js';
import {invoke, listen, normalizeError} from '../shared/tauri.js';

ensureSpriteLoaded();

const elCountdown = document.getElementById('countdown');
const elRecLine = document.getElementById('rec-line');
const elRecDot = document.getElementById('rec-dot');
const elRecTime = document.getElementById('rec-time');
const elRecFrames = document.getElementById('rec-frames');
const btnPause = document.getElementById('btn-pause');
const btnStop = document.getElementById('btn-stop');
const btnCancel = document.getElementById('btn-cancel');

const dom = {
    countdown: elCountdown,
    recLine: elRecLine,
    recDot: elRecDot,
    recTime: elRecTime,
    recFrames: elRecFrames,
    pauseBtn: btnPause,
};

// 最近一次后端 phase（暂停/恢复按它切换，不读 DOM class——250ms 状态
// 事件延迟内连点 DOM class 还是旧值，会误发反向指令）。
let lastPhase = '';

// 计时插值：后端 250ms 节流是 IPC 真源，但十分位 4Hz 跳变观感卡顿——
// recording 态本地按到达时刻线性外推，50ms 刷新（≈20Hz）；paused / 后端
// 校正值到达时立即收敛。
let elapsedBase = 0;
let elapsedAt = 0;
// 倒计时插值（毫秒递减；显示用 Math.ceil 秒）
let countdownBase = 0;
let countdownAt = 0;

function applyState(state) {
    if (state && state.phase) {
        lastPhase = state.phase;
        if (typeof state.elapsedMs === 'number') {
            elapsedBase = state.elapsedMs;
            elapsedAt = performance.now();
        }
        if (state.phase === 'countdown' && typeof state.countdownMs === 'number') {
            countdownBase = state.countdownMs;
            countdownAt = performance.now();
        }
        renderState(state, dom);
    }
}

setInterval(() => {
    if (lastPhase === 'recording') {
        elRecTime.textContent = formatElapsed(elapsedBase + (performance.now() - elapsedAt));
    } else if (lastPhase === 'countdown' && countdownBase > 0) {
        const remain = countdownBase - (performance.now() - countdownAt);
        elCountdown.textContent = String(Math.max(1, Math.ceil(Math.max(0, remain) / 1000)));
    }
}, 50);

async function command(name) {
    try {
        await invoke(name);
    } catch (err) {
        console.error(`[gif-recorder] ${name} 失败`, err);
    }
}

btnPause.addEventListener('click', () => {
    if (lastPhase === 'countdown') return; // 倒计时未开录，无暂停语义
    command(lastPhase === 'paused' ? 'gif_resume_recording' : 'gif_pause_recording');
});
btnStop.addEventListener('click', () => command('gif_stop_recording'));
btnCancel.addEventListener('click', () => command('gif_cancel_recording'));

// ready：拉一次当前状态（后端 250ms ticker 兜底补齐）
invoke('gif_recorder_state')
    .then(applyState)
    .catch(err => console.error('[gif-recorder] 状态拉取失败', normalizeError(err)));

listen('blink://gif-recorder-state', event => {
    applyState(event.payload);
});
