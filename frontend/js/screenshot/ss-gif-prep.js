//! GIF 录制准备模式（0.28.7 两段式录制第一段）。
//!
//! 点工具栏「录制 GIF」→ `enterGifRecordMode()`：#toolbar 原地隐藏、
//! #gif-prep-bar 复制其 left/top 显示（视觉上「工具栏切换成录制栏」）。
//! 此阶段**选区可拖动移动**（复用既有选区 move 交互，不提供缩放——录制
//! 区域尺寸在开始后固定），可调帧率/光标；点「开始录制」才调
//! `gif_start_recording`（实时选区坐标 + 准备栏位置），overlay 由后端收场。
//! ESC / 取消 → 退回截图标注模式。
//!
//! 后续扩展预留（画笔/鼠标指示器等）：本模块只管模式切换与启动参数，
//! 新功能以 prep-bar 增量控件 + 启动参数透传实现。

import {ss} from './ss-state.js';
import {cssPointToScreen, cssRectToBitmap} from './ss-selection-geometry.js';
import {invoke, normalizeError} from '../shared/tauri.js';
import {showTransientHint} from './ss-ocr.js';

const PREP_BAR_ID = 'gif-prep-bar';

/** 是否处于 GIF 录制准备模式。 */
export function isGifPrepActive() {
    const bar = document.getElementById(PREP_BAR_ID);
    return Boolean(bar && !bar.classList.contains('hidden'));
}

/** 进入准备模式：工具栏原地切换。 */
export function enterGifRecordMode() {
    const toolbar = ss.toolbar || document.getElementById('toolbar');
    const bar = document.getElementById(PREP_BAR_ID);
    if (!toolbar || !bar || isGifPrepActive()) return;
    // 原地切换：准备栏复制工具栏当前位置（含用户拖动过的位置）
    const rect = toolbar.getBoundingClientRect();
    bar.style.left = `${rect.x}px`;
    bar.style.top = `${rect.y}px`;
    toolbar.classList.add('hidden');
    bar.classList.remove('hidden');
}

/** 退出准备模式：回截图标注模式（工具栏还原显示）。 */
export function exitGifRecordMode() {
    const toolbar = ss.toolbar || document.getElementById('toolbar');
    const bar = document.getElementById(PREP_BAR_ID);
    if (bar) bar.classList.add('hidden');
    if (toolbar && !document.body.classList.contains('image-editor-mode')) {
        toolbar.classList.remove('hidden');
    }
}

/** 开始录制：读取当前（可能已拖动过的）选区 + 准备栏配置 → 后端流水线。 */
export async function startGifRecordingFromPrep() {
    if (!ss.selCss || ss.sent) return;
    const fps = parseInt(document.getElementById('gif-prep-fps')?.value || '15', 10);
    const showCursor = document.getElementById('gif-prep-cursor')?.checked ?? true;
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const origin = cssPointToScreen(ss.selCss.x, ss.selCss.y, meta);
    const bmp = cssRectToBitmap(ss.selCss, meta);
    // 准备栏屏幕坐标：控制条定位到这里（工具栏→准备栏→控制条全程原地）
    const bar = document.getElementById(PREP_BAR_ID);
    const barRect = bar?.getBoundingClientRect();
    const controlPos = barRect ? cssPointToScreen(barRect.x, barRect.y, meta) : null;
    ss.sent = true;
    try {
        await invoke('gif_start_recording', {
            x: origin.x,
            y: origin.y,
            w: bmp.w,
            h: bmp.h,
            fps,
            showCursor,
            controlX: controlPos ? controlPos.x : null,
            controlY: controlPos ? controlPos.y : null,
        });
        // overlay 收场由后端在控制条就绪后统一执行（含准备栏所在窗口）
        exitGifRecordMode();
    } catch (err) {
        console.error('[screenshot] GIF 录制启动失败', err);
        showTransientHint(`GIF 录制启动失败：${normalizeError(err).message}`, {isError: true, duration: 3000});
        ss.sent = false;
    }
}

/** 绑定准备栏按钮（index.js 初始化时调用一次）。 */
export function bindGifPrepBar() {
    document.getElementById('gif-prep-start')?.addEventListener('click', () => {
        startGifRecordingFromPrep();
    });
    document.getElementById('gif-prep-cancel')?.addEventListener('click', exitGifRecordMode);
}
