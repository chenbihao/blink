//! GIF 录制准备模式（0.28.7 两段式录制第一段；单宿主双面重构版）。
//!
//! 架构：#toolbar 是**唯一**的浮栏宿主——选区跟随（positionToolbar）、
//! 用户拖动位置（#toolbar-drag + data-user-moved）、屏内 clamp、uiScale
//! 缩放全部作用于宿主。录制准备态只是宿主上的一个 class
//! （toolbar-mode-gif-prep）：CSS 隐藏标注工具组、显示 #gif-prep-bar
//! （录制面），抓手保留。本模块只负责切 class 与启动参数，不碰定位——
//! 「双栏并行 / 准备栏不跟随 / 不可拖动」这类同步问题在结构上不存在。
//!
//! 此阶段选区可拖动移动（缩放在 index.js pointerdown 中禁用——录制区域
//! 尺寸在开始后固定），可调帧率/光标；点「开始录制」才调
//! `gif_start_recording`（实时选区坐标 + 宿主位置），overlay 由后端收场。
//! ESC / 取消 / 选区失效 → 退回截图标注模式。

import {ss} from './ss-state.js';
import {cssPointToScreen, cssRectToBitmap} from './ss-selection-geometry.js';
import {positionToolbar} from './ss-display.js';
import {invoke, normalizeError} from '../shared/tauri.js';
import {showTransientHint} from './ss-ocr.js';

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
    // 预热录制辅助窗口（隐藏态建好）——用户调帧率/光标的间隙完成 WebView2
    // 初始化，点「开始录制」走复用路径毫秒级出边框。失败静默（正式路径
    // 会再建）。
    // gifWindowHold：建窗会把窗口级焦点瞬移给新窗口（WebView2 初始化），
    // 豁免期内的 blur 不视为退出意图（index.js blur 处理器）；真切走异进程
    // 仍由后端看门狗兜底。hold 覆盖建窗全程 + 尾巴。
    ss.gifWindowHold = true;
    invoke('gif_prewarm_windows')
        .catch(() => {})
        .finally(() => {
            setTimeout(() => {
                ss.gifWindowHold = false;
            }, 500);
        });
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

/** 开始录制：读取当前（可能已拖动过的）选区 + 准备栏配置 → 后端流水线。 */
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
    const fps = parseInt(document.getElementById('gif-prep-fps')?.value || '15', 10);
    const showCursor = document.getElementById('gif-prep-cursor')?.checked ?? true;
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const origin = cssPointToScreen(ss.selCss.x, ss.selCss.y, meta);
    const bmp = cssRectToBitmap(ss.selCss, meta);
    // 宿主屏幕坐标：控制条定位到这里（工具栏→准备面→控制条全程原地）。
    // getBoundingClientRect 已含 uiScale transform，映射回屏幕物理坐标。
    const barRect = toolbarEl()?.getBoundingClientRect();
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
    // 在窗口捕获阶段消费准备态 Esc，避免控件/浮层先吞掉；开始录制后由控制条接管。
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
