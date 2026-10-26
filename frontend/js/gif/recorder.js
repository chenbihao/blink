//! GIF 录制控制条（0.28.7，gif-control 窗口）。
//!
//! 纯展示 + 指令转发：状态真源是后端 `blink://gif-recorder-state` 事件
//! （250ms 节流）+ ready 时 `gif_recorder_state` 拉取；本页不做任何时钟
//! 推算——倒计时数字、有效时长（暂停剔除）、帧数全部来自后端。
//! 按钮：暂停/继续（同一按钮切换图标）、停止（进编辑器）、取消（丢弃）。

import {ensureSpriteLoaded} from '../shared/icon.js';
import {renderState} from './recorder-view.js';
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

async function command(name) {
    try {
        await invoke(name);
    } catch (err) {
        console.error(`[gif-recorder] ${name} 失败`, err);
    }
}

btnPause.addEventListener('click', () => {
    command(elRecDot.classList.contains('paused') ? 'gif_resume_recording' : 'gif_pause_recording');
});
btnStop.addEventListener('click', () => command('gif_stop_recording'));
btnCancel.addEventListener('click', () => command('gif_cancel_recording'));

// ready：拉一次当前状态（后端 250ms ticker 兜底补齐）
invoke('gif_recorder_state')
    .then(state => {
        if (state && state.phase) renderState(state, dom);
    })
    .catch(err => console.error('[gif-recorder] 状态拉取失败', normalizeError(err)));

listen('blink://gif-recorder-state', event => {
    if (event.payload) renderState(event.payload, dom);
});
