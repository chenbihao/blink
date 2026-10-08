//! 截图预选框视觉层。
//!
//! 窗口与控件命中共用同一个 DOM 元素，因此跨层级切换时可以继承上一帧的
//! 几何位置并连续形变；kind 只负责语义配色，不再用两个元素交叉淡入淡出。
//!
//! 0.25.14：本模块同时驱动**预选挖洞遮罩**（#preselection-mask 四块 DOM）——
//! 预选阶段的「框外暗、框内透」与预选框边框同生命周期：show 时激活（首次清
//! interaction-canvas 整屏暗罩 + 同帧写几何），几何变化走与边框一致的 0.13s
//! 形变过渡；hide 延迟到边框 120ms 淡出确认（期间被新 show 取消则连续形变）。
//! 遮罩消隐**不负责恢复整屏暗罩**：hide 的全部调用路径（拖动开始/吸附确认/
//! ESC/reset）都会跟进 canvas 提交，且 `drawDimmed` / `drawFinalSelection`
//! 开头会调用 `hideCutoutMask` 兜底，与二者调用 `hideLiveSelection` 同构。

import {uiScaleAtCss} from './ss-selection-geometry.js';
import {ss} from './ss-state.js';

const HIDE_DELAY_MS = 120;
const BORDER_CSS_WIDTH = 3; // 与 chord-screenshot.css .preselection-hint 的 border 同值

let hintEl = null;
let hintOwner = null;
let hintPresented = false;
let hintHideTimer = 0;

// ── 0.25.14：预选挖洞遮罩状态 ────────────────────────────────────────────
/** 遮罩是否处于激活态（激活 = interaction-canvas 整屏暗罩已清、DOM 层承担遮罩） */
let maskActive = false;
/** 最近一次写入的洞矩形（几何未变时跳过重复写样式） */
let lastMaskRect = null;

function ensureHint() {
    if (hintEl) return hintEl;
    hintEl = document.createElement('div');
    hintEl.id = 'preselection-hint';
    hintEl.className = 'preselection-hint preselection-hint--window';
    document.body.appendChild(hintEl);
    return hintEl;
}

/** 非负数化：负值写进遮罩的宽高会被浏览器判为非法声明直接丢弃 */
function px(value) {
    return Math.max(0, Math.round(value)) + 'px';
}

/** 取挖洞遮罩 DOM 引用；任一缺失说明 HTML/CSS 契约被破坏 */
function maskElements() {
    const {preMaskEl, preMaskTop, preMaskBottom, preMaskLeft, preMaskRight} = ss;
    if (!preMaskEl || !preMaskTop || !preMaskBottom || !preMaskLeft || !preMaskRight) {
        return null;
    }
    return {preMaskEl, preMaskTop, preMaskBottom, preMaskLeft, preMaskRight};
}

/** 挖洞遮罩是否可用：用户开关开（默认开）且非 canvas-backed 来源（图片编辑
 *  的屏幕坐标吸附无意义，该路径本就不加载窗口列表） */
function cutoutEnabled() {
    return ss.screenshotConfig.preselectionCutout !== false
        && !ss.editorSession.canvasBacked;
}

/**
 * 把洞矩形写进四块遮罩（CSS 像素）。
 *
 * 整数格点纪律与 ss-live-selection 的 writeGeometry 一致：先对齐四角格点再推导
 * 所有边，禁止独立取整——混合 DPI 下指针 CSS 坐标是小数，独立取整会在左右遮罩与
 * 下遮罩之间产生整屏宽的 1px 空缝或叠色（0.23.15 已踩过的坑）。遮罩 left/top 钳
 * 到可视区 0，四块拼满全屏无重叠无缝隙；洞为整屏（桌面回退预选）时四块宽高全 0，
 * 视觉即全屏提亮，无需特判。
 */
function writeMaskGeometry(rect, els) {
    const bx = Math.round(rect.x);
    const by = Math.round(rect.y);
    const bw = Math.max(0, Math.round(rect.x + Math.max(0, rect.w)) - bx);
    const bh = Math.max(0, Math.round(rect.y + Math.max(0, rect.h)) - by);
    const left = Math.max(0, bx);
    const top = Math.max(0, by);
    const right = Math.max(left, bx + bw);
    const bottom = Math.max(top, by + bh);

    els.preMaskTop.style.height = px(top);
    els.preMaskBottom.style.top = px(bottom);
    els.preMaskLeft.style.top = px(top);
    els.preMaskLeft.style.height = px(bottom - top);
    els.preMaskLeft.style.width = px(left);
    els.preMaskRight.style.top = px(top);
    els.preMaskRight.style.height = px(bottom - top);
    els.preMaskRight.style.left = px(right);
}

/**
 * 激活/更新挖洞遮罩（与预选框同一 rect）。
 *
 * 首次激活时「清 interaction-canvas 整屏暗罩 + DOM 遮罩覆盖」必须落在同一帧
 * （与 activateLiveSelection 同款纪律），否则出现一帧全亮或双暗；且首帧几何
 * **不播形变动画**（与 hint 首次出现的处理一致）——display:none 期间四块保留
 * 上次的几何，恢复显示后直接写目标会从旧位置扫过屏幕，因此禁 transition →
 * 写首帧 → 强制 reflow 锁定起始值 → 恢复。此后几何变化走 CSS 过渡（0.13s，
 * 与边框同曲线），洞边界与虚线框每帧重合、连续形变。
 */
function syncCutoutMask(rect) {
    if (!cutoutEnabled() || !rect) return;
    const els = maskElements();
    if (!els) return;
    if (!maskActive) {
        maskActive = true;
        lastMaskRect = null;
        const {interactionCanvas, interactionCtx} = ss;
        if (interactionCanvas && interactionCtx && interactionCanvas.width > 0) {
            interactionCtx.clearRect(0, 0, interactionCanvas.width, interactionCanvas.height);
        }
        els.preMaskEl.classList.remove('hidden');
        const blocks = [els.preMaskTop, els.preMaskBottom, els.preMaskLeft, els.preMaskRight];
        for (const b of blocks) b.style.transition = 'none';
        writeMaskGeometry(rect, els);
        void els.preMaskEl.offsetHeight;
        for (const b of blocks) b.style.transition = '';
        lastMaskRect = {x: rect.x, y: rect.y, w: rect.w, h: rect.h};
        return;
    }
    const r = {x: rect.x, y: rect.y, w: rect.w, h: rect.h};
    if (lastMaskRect
        && lastMaskRect.x === r.x && lastMaskRect.y === r.y
        && lastMaskRect.w === r.w && lastMaskRect.h === r.h) return;
    lastMaskRect = r;
    writeMaskGeometry(rect, els);
}

/**
 * 隐藏挖洞遮罩（幂等、立即——display 切换不受 transition 影响）。
 *
 * 调用方：① hidePreselectionHint 的淡出确认回调（120ms 内被取消则不隐藏，
 * 保持几何连续形变）；② drawDimmed / drawFinalSelection 开头（canvas 提交
 * 前同帧让位，封死「DOM 遮罩 + canvas 遮罩」双暗）；③ resetPreselectionHint
 * （会话清场）与 index.js resetState（防旧几何跨会话残留）。
 * 不恢复整屏暗罩：hide 的调用路径都会跟进 canvas 提交；纯 hide 无后续提交
 * 的场景（如 pointerleave）到下次 mousemove 重新 show，间隔内无感知影响。
 */
export function hideCutoutMask() {
    if (!maskActive) return;
    maskActive = false;
    lastMaskRect = null;
    const els = maskElements();
    if (els) els.preMaskEl.classList.add('hidden');
}

/**
 * 显示或移动统一预选框。
 *
 * 真正隐藏前收到新的 show 时会取消淡出，并从当前屏幕位置继续形变，避免
 * window -> control -> window 在同一 mousemove 内发生瞬移。
 */
export function showPreselectionHint(rect, kind, title = '') {
    const el = ensureHint();
    if (hintHideTimer) {
        clearTimeout(hintHideTimer);
        hintHideTimer = 0;
    }

    const wasHidden = !hintPresented;
    if (wasHidden) el.style.transition = 'none';

    el.classList.toggle('preselection-hint--window', kind === 'window');
    el.classList.toggle('preselection-hint--control', kind === 'control');
    el.style.left = `${rect.x}px`;
    el.style.top = `${rect.y}px`;
    el.style.width = `${rect.w}px`;
    el.style.height = `${rect.h}px`;
    // 0.23.18：框体必须精确对齐窗口矩形（几何层不缩放），但边框线宽是视觉
    // 重量——按预选框中心所在屏 uiScale 补偿（与实时选区边框 BORDER_CSS_WIDTH
    // × uiScale 同一契约，spec-frontend §5.6），物理线宽跨屏一致。
    const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
    const uiScale = uiScaleAtCss(rect.x + rect.w / 2, rect.y + rect.h / 2, meta);
    const border = Math.max(1, Math.round(BORDER_CSS_WIDTH * uiScale));
    el.style.borderWidth = border + 'px';
    el.style.borderRadius = border + 'px';
    el.style.visibility = 'visible';
    el.style.opacity = '1';
    el.title = title;
    hintOwner = kind;
    // 0.25.14：挖洞遮罩与边框同 rect 同生命周期（开关关闭/图片编辑来源时不激活）
    syncCutoutMask(rect);

    if (wasHidden) {
        // 首次出现只淡入，不从默认 (0,0) 滑入；后续层级切换沿用同一几何轨迹。
        el.offsetHeight;
        el.style.transition = '';
        hintPresented = true;
    }
}

/** 仅当调用方仍拥有预选框时淡出，防止旧层级隐藏掉刚切换的新层级。
 *  0.25.14-fix：挖洞遮罩的隐藏**延迟到淡出确认**（timer 回调）——120ms 内被新
 *  show 取消时遮罩保持显示，几何过渡连续形变到新层级矩形（与边框行为一致，
 *  跨层级切换不错位）；拖动/吸附/取消路径由 drawDimmed / drawFinalSelection
 *  开头的强制隐藏兜底，不受此延迟影响。 */
export function hidePreselectionHint(owner) {
    if (!hintEl || !hintPresented || (owner && hintOwner !== owner)) return;

    hintEl.style.opacity = '0';
    if (hintHideTimer) clearTimeout(hintHideTimer);
    hintHideTimer = setTimeout(() => {
        hintHideTimer = 0;
        if (!hintEl || hintEl.style.opacity !== '0') return;
        hideCutoutMask();
        hintEl.style.visibility = 'hidden';
        hintPresented = false;
        hintOwner = null;
    }, HIDE_DELAY_MS);
}

/** overlay 关闭时立即复位，不播放退场动画。 */
export function resetPreselectionHint() {
    if (hintHideTimer) {
        clearTimeout(hintHideTimer);
        hintHideTimer = 0;
    }
    hideCutoutMask();
    hintOwner = null;
    hintPresented = false;
    if (!hintEl) return;
    hintEl.style.transition = 'none';
    hintEl.style.opacity = '0';
    hintEl.style.visibility = 'hidden';
    hintEl.style.transition = '';
}
