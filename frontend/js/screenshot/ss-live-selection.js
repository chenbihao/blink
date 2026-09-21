//! 截图实线选区的实时预览层（0.23.15）。
//!
//! 拖动（新建拖选 / 选区移动 / 八向缩放）期间的选区预览由本模块**独家**负责：
//! 四块遮罩 + 唯一一个实线边框，几何全部是 CSS 像素，直接消费
//! `norm(startX, startY, endX, endY)` 或 `ss.selCss`。
//! 拖动期间 `#interaction-canvas` 不参与任何绘制——它只在拖动开始时清空一次，
//! 以及在提交时一次性画回成品（`ss-draw.drawFinalSelection`）。
//!
//! 为什么不是 canvas：`#interaction-canvas` 的 backing store 与虚拟桌面物理像素
//! 同尺寸（双 4K 约 16.6M 像素），每帧 `clearRect` + 4 次全屏 `fillRect` 的成本
//! 随截图总面积线性增长，在 4K / 多屏 / 高刷下突破帧预算，表现为实线持续落后
//! 鼠标与旧帧边框残影。DOM 层的每帧成本是 5 个盒子的几何写入 + 合成，与像素
//! 面积无关。
//!
//! 调度铁则：
//! - 同一时刻最多一个 rAF（单飞）；事件侧只写「最新矩形」，不直接落 DOM。
//! - rAF 执行时只消费最新一帧的矩形，丢弃中间过期采样。
//! - 首次更新**同步**落地首帧：canvas 清空与 DOM 覆盖必须落在同一帧，
//!   否则会出现「选区消失一帧」的闪白。
//! - 隐藏/复位后代际（epoch）递增，在途旧 rAF 不得重新显示本层。
//! - 尺寸提示与实时几何共用同一个 rAF，每帧最多更新一次。

import {ss} from './ss-state.js';
import {
    cssPointToScreen,
    cssRectToBitmap,
    formatSelectionInfo,
    uiScaleAtCss,
} from './ss-selection-geometry.js';
import {applyFloatingUiScaleAt} from './ss-display.js';

/** 遮罩色：与 `drawFinalSelection` 的 canvas 遮罩保持一致，避免提交时跳色 */
const MASK_ALPHA = 0.45;
/** 实线边框色：与 `drawFinalSelection` 一致 */
const BORDER_COLOR = '#4a9eff';
/** 1× 视觉下的边框线宽（CSS px）；实际线宽按目标屏做跨屏补偿 */
const BORDER_CSS_WIDTH = 2;

/** 实时层是否处于激活态（激活 = canvas 已清空、DOM 层承担预览） */
let liveActive = false;
/** 单飞 rAF id（null = 无待执行帧） */
let liveRaf = null;
/** 待渲染的最新矩形（事件侧只写这里） */
let pendingRect = null;
/** 代际：隐藏/复位后递增，使在途旧 rAF 失效 */
let liveEpoch = 0;
/** 最近一次写入的边框线宽（避免逐帧重复写同一值） */
let lastBorderWidth = -1;
/** 最近一次写入的尺寸提示文本（避免逐帧重复写 textContent） */
let lastHintText = null;
/**
 * DOM 层缺失的告警只打一次。
 * 该缺失只可能来自 HTML 与 JS 版本不一致（部署事故），不是运行期状态；
 * 而在线路径每帧都会尝试激活，逐帧 warn 会以 100+ Hz 刷爆控制台。
 */
let domMissingWarned = false;

/** 实时层是否已激活（供交互层判断是否需要「开始拖动」） */
export function isLiveSelectionActive() {
    return liveActive;
}

/** 当前待渲染矩形（诊断与测试用） */
export function getPendingLiveRect() {
    return pendingRect;
}

/** 取实时层 DOM 引用；任一缺失说明 HTML/CSS 契约被破坏 */
function liveElements() {
    const {
        liveSelectionEl, liveMaskTop, liveMaskBottom,
        liveMaskLeft, liveMaskRight, liveBorderEl,
    } = ss;
    if (!liveSelectionEl || !liveMaskTop || !liveMaskBottom
        || !liveMaskLeft || !liveMaskRight || !liveBorderEl) {
        return null;
    }
    return {liveSelectionEl, liveMaskTop, liveMaskBottom, liveMaskLeft, liveMaskRight, liveBorderEl};
}

/** 非负数化：负值写进遮罩的宽高会被浏览器判为非法声明直接丢弃 */
function px(value) {
    return Math.max(0, Math.round(value)) + 'px';
}

/**
 * 把矩形写进四块遮罩与唯一边框（CSS 像素）。
 *
 * 每块遮罩只写「变化的那一个维度」：上遮罩只写 height、下遮罩只写 top，
 * 左右遮罩各写 left/top/height（或 width）。因此全程不读 viewport 尺寸，
 * 不会在写样式后触发强制同步布局。
 *
 * 遮罩的宽高必须非负（负值声明会被丢弃）；边框的 left/top 允许为负，
 * 与改造前 canvas `strokeRect` 支持负坐标的语义一致——快速拖出屏幕边缘时
 * 边框仍贴在正确位置，而不会被钳到 0。
 */
function writeGeometry(rect, els) {
    const w = Math.max(0, rect.w);
    const h = Math.max(0, rect.h);

    // 先把矩形四角对齐到唯一整数格点，再推导所有边——禁止 round(y)、round(h)、
    // round(y+h) 三个表达式独立取整：混合 DPI 下指针 CSS 坐标（物理像素 ÷
    // renderScale）是小数，独立取整时 round(y)+round(h) 与 round(y+h) 约半数
    // 位置差 1px，左右遮罩与下遮罩之间会出现整屏宽的 1px 空缝（暗桌面上的亮
    // 横线，新建拖选时正好跟随下边缘移动）或 1px 叠色（α 0.45 → 0.70 暗线段）。
    // 宽高由两个格点相减得出，与提交路径 drawFinalSelection「一份取整矩形推导
    // 四块 fillRect」的纪律一致。
    const bx = Math.round(rect.x);
    const by = Math.round(rect.y);
    const bw = Math.max(0, Math.round(rect.x + w) - bx);
    const bh = Math.max(0, Math.round(rect.y + h) - by);

    // 遮罩是屏幕覆盖：left/top 钳到可视区 0（与 canvas fillRect 负坐标被视口
    // 自然裁剪的行为对齐），右/下边界保持原格点，四块遮罩拼满全屏无重叠无缝隙。
    const left = Math.max(0, bx);
    const top = Math.max(0, by);
    const right = Math.max(left, bx + bw);
    const bottom = Math.max(top, by + bh);

    // 选区外暗：上/下/左/右四块，组合出选区内的透明豁口
    els.liveMaskTop.style.height = px(top);
    els.liveMaskBottom.style.top = px(bottom);
    els.liveMaskLeft.style.top = px(top);
    els.liveMaskLeft.style.height = px(bottom - top);
    els.liveMaskLeft.style.width = px(left);
    els.liveMaskRight.style.top = px(top);
    els.liveMaskRight.style.height = px(bottom - top);
    els.liveMaskRight.style.left = px(right);

    // 唯一边框：真实取整矩形。left/top 允许为负，与 canvas strokeRect 支持负
    // 坐标的语义一致——快速拖出屏幕边缘时边框仍贴在正确位置；宽高与遮罩同一
    // 份格点，右/下边缘与遮罩边界天然对齐（旧实现 round(w) 与 round(x+w) 的
    // ±1px 竖向错位一并消除）。
    els.liveBorderEl.style.left = bx + 'px';
    els.liveBorderEl.style.top = by + 'px';
    els.liveBorderEl.style.width = px(bw);
    els.liveBorderEl.style.height = px(bh);

    // 跨屏视觉补偿：与改造前 canvas 的 lineWidth = 2 × monitorDpr 等价
    // （canvas 位图宽 2×monitorDpr ÷ renderScale = 2×uiScale CSS 宽）。
    // 权威比例仍是 canvas 实测 renderScale；devicePixelRatio 只做兜底。
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const borderWidth = Math.max(1, BORDER_CSS_WIDTH * uiScaleAtCss(left, top, meta));
    if (borderWidth !== lastBorderWidth) {
        lastBorderWidth = borderWidth;
        els.liveBorderEl.style.borderWidth = borderWidth + 'px';
    }
}

/**
 * 尺寸提示（物理像素尺寸 + 屏幕坐标）与实时几何共用同一个 rAF。
 * 每帧最多更新一次 textContent 与 left/top；文本未变化时不重复写。
 */
function updateSizeHint(rect) {
    const {sizeHint} = ss;
    if (!sizeHint) return;
    // canvas-backed 来源（长截图 / 剪贴板图片编辑）不显示截图坐标提示
    if (ss.editorSession?.canvasBacked) return;
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const bmp = cssRectToBitmap(rect, meta);
    const screenPos = cssPointToScreen(rect.x, rect.y, meta);
    const text = formatSelectionInfo(screenPos.x, screenPos.y, bmp.w, bmp.h);
    if (text !== lastHintText) {
        lastHintText = text;
        sizeHint.textContent = text;
    }
    sizeHint.classList.remove('hidden');
    applyFloatingUiScaleAt(sizeHint, rect.x, rect.y);
    sizeHint.style.left = (rect.x + 4) + 'px';
    sizeHint.style.top = (rect.y > 24 ? rect.y - 22 : rect.y + 4) + 'px';
}

/** 清空一次动态交互层——进入 DOM 实时预览前的唯一一次 canvas 操作 */
function clearInteractionCanvasOnce() {
    const {interactionCanvas, interactionCtx} = ss;
    if (!interactionCanvas || !interactionCtx || interactionCanvas.width <= 0) return;
    interactionCtx.clearRect(0, 0, interactionCanvas.width, interactionCanvas.height);
}

/** rAF 回调：只消费最新矩形，落 DOM 几何 + 尺寸提示 */
function flushLiveSelection() {
    liveRaf = null;
    if (!liveActive) return;
    const rect = pendingRect;
    pendingRect = null;
    if (!rect) return;
    const els = liveElements();
    if (!els) return;
    writeGeometry(rect, els);
    updateSizeHint(rect);
}

/**
 * 激活实时层：清空一次 interaction-canvas + 显示 DOM 层 + **同步**落地首帧。
 *
 * 「同步落地首帧」是必需的：清空 canvas 的同一帧必须有 DOM 覆盖，否则用户会
 * 看到一帧无蒙版/无选区的画面。也因此首帧不走 rAF（后续帧才走单飞 rAF），
 * 保证「一帧最多渲染一次」。
 *
 * @returns {boolean} 是否成功激活（DOM 缺失时返回 false，且不清空 canvas）
 */
function activateLiveSelection(rect) {
    const els = liveElements();
    if (!els) {
        // 只警告一次：本路径每帧都会被尝试，逐帧 warn 会刷爆控制台；
        // 且该缺陷只可能来自 HTML/JS 版本不一致，重复提示无增量信息。
        if (!domMissingWarned) {
            domMissingWarned = true;
            console.warn('[screenshot] live-selection: DOM 层缺失（HTML 与 JS 版本不一致？），'
                + '本次会话不做选区实时预览');
        }
        return false;
    }
    liveActive = true;
    liveEpoch++;
    clearInteractionCanvasOnce();
    els.liveSelectionEl.classList.remove('hidden');
    writeGeometry(rect, els);
    updateSizeHint(rect);
    return true;
}

/**
 * 更新实时选区几何——**本模块唯一的事件侧入口**。
 *
 * pointer 事件只负责把「最新矩形」交到这里；同一时刻最多一个 rAF；
 * rAF 执行时只消费最新矩形，中间采样自然被丢弃。
 * 首次调用（未激活）时同步激活并落地首帧。
 */
export function updateLiveSelection(rect) {
    if (!rect) return;
    if (!liveActive) {
        activateLiveSelection(rect);
        return;
    }
    pendingRect = rect;
    if (liveRaf !== null) return;
    const epoch = liveEpoch;
    liveRaf = requestAnimationFrame(() => {
        if (epoch !== liveEpoch) {
            // 隐藏/复位已发生，本帧作废（不写 liveRaf，避免清掉新一帧的 id）
            return;
        }
        flushLiveSelection();
    });
}

/**
 * 隐藏实时层（幂等）：取消未落地的 rAF、清 pending、隐藏 DOM。
 *
 * 由 `drawFinalSelection` / `drawDimmed` 内部调用，因此「canvas 提交 + DOM 隐藏」
 * 必然落在同一个 JS task、同一帧内——这是「松手无闪白、无双边框」的实现方式，
 * 且自动覆盖所有退出路径（正常提交 / 选区过小 / ESC / reset / blur / 会话清场）。
 */
export function hideLiveSelection() {
    if (liveRaf !== null) {
        cancelAnimationFrame(liveRaf);
        liveRaf = null;
    }
    pendingRect = null;
    if (!liveActive) return;
    liveActive = false;
    liveEpoch++;
    const els = liveElements();
    if (els) els.liveSelectionEl.classList.add('hidden');
}

/**
 * 复位实时层：隐藏 + 几何清零 + 代际递增。
 * 用于 ESC / reset / 会话切换 / 窗口隐藏 / pointercancel——保证旧会话的迟到
 * 回调无法重新显示它，也不会在下次激活前露出上一轮的残留几何。
 */
export function resetLiveSelection() {
    hideLiveSelection();
    lastHintText = null;
    lastBorderWidth = -1;
    const els = liveElements();
    if (!els) return;
    els.liveMaskTop.style.height = '0px';
    els.liveMaskBottom.style.top = '0px';
    els.liveMaskLeft.style.top = '0px';
    els.liveMaskLeft.style.height = '0px';
    els.liveMaskLeft.style.width = '0px';
    els.liveMaskRight.style.top = '0px';
    els.liveMaskRight.style.height = '0px';
    els.liveMaskRight.style.left = '0px';
    els.liveBorderEl.style.left = '0px';
    els.liveBorderEl.style.top = '0px';
    els.liveBorderEl.style.width = '0px';
    els.liveBorderEl.style.height = '0px';
}

// 遮罩色/边框色的真源在 CSS（.live-selection-mask / .live-selection-border）；
// 这里导出的常量只用于测试对拍，避免 CSS 与 JS 两处静默漂移。
export const LIVE_SELECTION_STYLE = Object.freeze({
    maskColor: `rgba(0, 0, 0, ${MASK_ALPHA})`,
    borderColor: BORDER_COLOR,
    borderCssWidth: BORDER_CSS_WIDTH,
});
