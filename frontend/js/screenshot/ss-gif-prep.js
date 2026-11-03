//! GIF 录制准备模式（0.28.7 两段式录制第一段；0.28.23 统一浮栏版）。
//!
//! 架构：#toolbar 是**唯一**的浮栏宿主——选区跟随（positionToolbar）、
//! 用户拖动位置（#toolbar-drag + data-user-moved）、屏内 clamp、uiScale
//! 缩放全部作用于宿主。准备面只是宿主上的一个 class
//! （toolbar-mode-gif-prep）：CSS 隐藏标注工具组、显示 #gif-prep-bar，
//! 抓手保留。本模块只负责切 class 与启动参数，不碰定位。
//!
//! 0.28.23：录制开始后 overlay 不再隐藏/另起控制条——准备态 → 录制叠层
//! （ss-gif-rec.js）是同一宿主的相邻两面，位置/拖动/工具天然连续。开录前
//! 检查已有标注可迁移性（模糊/马赛克/放大镜/水印等依赖截图底图的效果阻止
//! 开始，引导用户返回清除）。

import {ss} from './ss-state.js';
import {cssPointToScreen, cssRectToBitmap} from './ss-selection-geometry.js';
import {positionToolbar} from './ss-display.js';
import {invoke, normalizeError} from '../shared/tauri.js';
import {showTransientHint} from './ss-ocr.js';
import {annotationsMigratable, enterGifRecordOverlay, handleOverlayHidden} from './ss-gif-rec.js';

/** 宿主上的录制准备态 class（对应 CSS 的面切换规则）。 */
const PREP_MODE_CLASS = 'toolbar-mode-gif-prep';

/** 区域尺寸上限（与后端 gif_service 校验一致；物理像素）。 */
const REGION_MIN_W = 64, REGION_MAX_W = 2560;
const REGION_MIN_H = 64, REGION_MAX_H = 1600;

/** 工具栏宿主（#toolbar）。 */
function toolbarEl() {
    return ss.toolbar || document.getElementById('toolbar');
}

/** 是否处于 GIF 录制准备模式。 */
export function isGifPrepActive() {
    const toolbar = toolbarEl();
    return Boolean(toolbar && toolbar.classList.contains(PREP_MODE_CLASS));
}

/** 选区是否满足录制区域尺寸约束（物理像素）。 */
function regionSizeValid() {
    if (!ss.selCss) return false;
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const bmp = cssRectToBitmap(ss.selCss, meta);
    return bmp.w >= REGION_MIN_W && bmp.w <= REGION_MAX_W
        && bmp.h >= REGION_MIN_H && bmp.h <= REGION_MAX_H;
}

/** 进入准备模式：宿主原地切面（位置/跟随/拖动全部继承宿主机制）。
 *  选区尺寸超限时不进入并提示。 */
export function enterGifRecordMode() {
    const toolbar = toolbarEl();
    if (!toolbar || isGifPrepActive()) return;
    if (!regionSizeValid()) {
        showTransientHint(
            `录制区域超限：宽 ${REGION_MIN_W}–${REGION_MAX_W} / 高 ${REGION_MIN_H}–${REGION_MAX_H}（物理像素），请调整选区`,
            {isError: true, duration: 3000},
        );
        return;
    }
    toolbar.classList.add(PREP_MODE_CLASS);
    // 面宽不同（准备面比标注面窄）：按新宽度立即右对齐到选区——否则
    // 首次进入停在旧左边界，拖一次选区才归位。用户拖过宿主则保留位置
    // 仅 clamp（positionToolbar 的 userMoved 语义）。
    positionToolbar({...ss.selCss}, true);
}

/** 退出准备模式：宿主切回标注面（工具组还原显示）。 */
export function exitGifRecordMode() {
    const toolbar = toolbarEl();
    if (toolbar) toolbar.classList.remove(PREP_MODE_CLASS);
    // 回标注模式：宿主应可见（ESC/取消路径本来可见；清场路径由
    // cleanupCanvasVisuals 先行隐藏，这里 remove hidden 不复活它——顺序上
    // cleanup 不会调本函数，防御性保留原语义）
    if (toolbar && !document.body.classList.contains('image-editor-mode')) {
        toolbar.classList.remove('hidden');
    }
    if (!ss.sent) {
        if (ss.selCss) positionToolbar({...ss.selCss}, true);
        document.getElementById('btn-gif')?.focus({preventScroll: true});
    }
}

/** 开始录制：读取当前（可能已拖动过的）选区 + 准备栏配置 → 后端流水线。
 *  成功后 overlay 转入录制叠层（统一浮栏 + 实时涂画，ss-gif-rec.js）。 */
export async function startGifRecordingFromPrep() {
    if (ss.sent) return;
    // 准备模式下选区可能已被清掉（点选区外退出标注等）——有反馈地退出，
    // 不做静默 no-op
    if (!ss.selCss) {
        showTransientHint('选区已失效，已退出录制准备', {duration: 2000});
        exitGifRecordMode();
        return;
    }
    if (!regionSizeValid()) {
        showTransientHint(
            `录制区域超限：宽 ${REGION_MIN_W}–${REGION_MAX_W} / 高 ${REGION_MIN_H}–${REGION_MAX_H}（物理像素）`,
            {isError: true, duration: 3000},
        );
        return;
    }
    // 0.28.23 §5.7.2：模糊/马赛克/放大镜/水印等依赖截图底图的标注无法迁移
    // 进录制——明确列出并阻止，用户须返回截图主动清除或替换。
    if (!annotationsMigratable()) {
        return;
    }
    const fps = parseInt(document.getElementById('gif-prep-fps')?.value || '15', 10);
    const showCursor = document.getElementById('gif-prep-cursor')?.checked ?? true;
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const origin = cssPointToScreen(ss.selCss.x, ss.selCss.y, meta);
    const bmp = cssRectToBitmap(ss.selCss, meta);
    ss.sent = true;
    ss.gifWindowHold = true;
    let sessionId;
    try {
        const info = await invoke('gif_start_recording', {
            x: origin.x,
            y: origin.y,
            w: bmp.w,
            h: bmp.h,
            fps,
            showCursor,
        });
        sessionId = info.sessionId;
        // overlay 不隐藏：同一宿主切到录制面，涂画层就位
        await enterGifRecordOverlay({x: origin.x, y: origin.y, w: bmp.w, h: bmp.h}, sessionId);
    } catch (err) {
        if (sessionId) {
            await invoke('gif_cancel_recording').catch(() => {});
            handleOverlayHidden();
        }
        console.error('[screenshot] GIF 录制启动失败', err);
        showTransientHint(`GIF 录制启动失败：${normalizeError(err).message}`, {isError: true, duration: 3000});
        ss.sent = false;
    } finally {
        ss.gifWindowHold = false;
    }
}

/** 绑定准备栏按钮（index.js 初始化时调用一次）。 */
export function bindGifPrepBar() {
    // 在窗口捕获阶段消费准备态 Esc，避免控件/浮层先吞掉；开始录制后由
    // 录制叠层的捕获层接管（录制中 Esc 不退出）。
    window.addEventListener('keydown', (event) => {
        if (event.key !== 'Escape' || event.isComposing || !isGifPrepActive() || ss.sent) return;
        event.preventDefault();
        event.stopImmediatePropagation();
        exitGifRecordMode();
    }, true);
    document.getElementById('gif-prep-start')?.addEventListener('click', () => {
        startGifRecordingFromPrep();
    });
    document.getElementById('gif-prep-cancel')?.addEventListener('click', exitGifRecordMode);
}
