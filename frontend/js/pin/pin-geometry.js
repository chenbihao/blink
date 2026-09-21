//! Pin 图多屏混合 DPI 几何纯函数。
//!
//! 统一模型（禁止在调用方散落 devicePixelRatio 补偿）：
//!
//! ```text
//! sourcePixelW / sourcePixelH  // 图片资源像素，只读
//! baseCssW / baseCssH          // 100% zoom 时的视觉 CSS 尺寸，只读
//! zoom                         // 用户显式缩放，0.1..8
//! imageScreenX / imageScreenY  // 图片左上角屏幕物理坐标
//! monitorDpr                   // 当前窗口目标屏 DPR
//! ```
//!
//! 初始化时：
//! ```text
//! sourceDpr = 图片初始落点显示器 DPI / 96
//! baseCssW  = sourcePixelW / sourceDpr
//! baseCssH  = sourcePixelH / sourceDpr
//! zoom      = 1
//! ```
//!
//! 跨屏后视觉 CSS 尺寸不变，目标屏物理尺寸随 DPI 变化：
//! ```text
//! displayCssW      = baseCssW × zoom
//! displayPhysicalW = round(displayCssW × targetDpr)
//! ```
//!
//! padding 统一为逻辑尺寸：
//! ```text
//! padPhysical = round(PIN_PAD_CSS × targetDpr)
//! ```

// ── 常量 ──────────────────────────────────────────────────────────────────

/** Pin padding 的 CSS（逻辑）尺寸，与后端 PIN_PAD 概念一致但统一为逻辑像素。 */
export const PIN_PAD_CSS = 20;

/** 最小 zoom 值。 */
export const MIN_ZOOM = 0.1;

/** 最大 zoom 值。 */
export const MAX_ZOOM = 8;

// ── 基础计算 ──────────────────────────────────────────────────────────────

/**
 * 计算图片在 100% zoom 时的基础 CSS 尺寸。
 *
 * 初始屏仍满足 `baseCssW × sourceDpr = sourcePixelW`，
 * 即截图像素与来源屏物理像素 1:1。
 *
 * @param {number} sourcePixelW - 图片资源像素宽度
 * @param {number} sourcePixelH - 图片资源像素高度
 * @param {number} sourceDpr - 初始落点显示器的 DPR（= DPI / 96）
 * @returns {{ baseCssW: number, baseCssH: number }}
 */
export function baseCssSize(sourcePixelW, sourcePixelH, sourceDpr) {
    const dpr = Math.max(1, sourceDpr);
    return {
        baseCssW: sourcePixelW / dpr,
        baseCssH: sourcePixelH / dpr,
    };
}

/**
 * 计算图片在指定 DPR 屏上的物理显示尺寸。
 *
 * 视觉 CSS 尺寸 = baseCss × zoom，物理尺寸 = round(CSS × dpr)。
 * 跨屏后 CSS 尺寸不变，物理尺寸随 DPI 变化。
 *
 * @param {number} baseCssW - 100% zoom 时的基础 CSS 宽度
 * @param {number} baseCssH - 100% zoom 时的基础 CSS 高度
 * @param {number} zoom - 用户显式缩放
 * @param {number} targetDpr - 目标屏 DPR
 * @returns {{ physW: number, physH: number }}
 */
export function displayPhysicalSize(baseCssW, baseCssH, zoom, targetDpr) {
    const dpr = Math.max(1, targetDpr);
    const cssW = baseCssW * zoom;
    const cssH = baseCssH * zoom;
    return {
        physW: Math.max(1, Math.round(cssW * dpr)),
        physH: Math.max(1, Math.round(cssH * dpr)),
    };
}

/**
 * 计算 padding 的物理像素值。
 *
 * @param {number} padCss - padding 的 CSS（逻辑）尺寸
 * @param {number} targetDpr - 目标屏 DPR
 * @returns {number}
 */
export function padPhysical(padCss, targetDpr) {
    const dpr = Math.max(1, targetDpr);
    return Math.round(padCss * dpr);
}

/**
 * 计算窗口的物理矩形。
 *
 * ```text
 * winX = imageScreenX - padPhysical
 * winY = imageScreenY - padPhysical
 * winW = displayPhysW + 2 × padPhysical
 * winH = displayPhysH + 2 × padPhysical
 * ```
 *
 * @param {number} imageScreenX - 图片左上角屏幕物理 X
 * @param {number} imageScreenY - 图片左上角屏幕物理 Y
 * @param {number} displayPhysW - 图片物理显示宽度
 * @param {number} displayPhysH - 图片物理显示高度
 * @param {number} padPhys - padding 物理像素
 * @returns {{ winX: number, winY: number, winW: number, winH: number }}
 */
export function physicalWindowRect(imageScreenX, imageScreenY, displayPhysW, displayPhysH, padPhys) {
    return {
        winX: Math.round(imageScreenX - padPhys),
        winY: Math.round(imageScreenY - padPhys),
        winW: Math.max(1, displayPhysW + 2 * padPhys),
        winH: Math.max(1, displayPhysH + 2 * padPhys),
    };
}

/**
 * 一步算出完整窗口物理矩形（组合 displayPhysicalSize + padPhysical + physicalWindowRect）。
 *
 * @param {object} state - `{ baseCssW, baseCssH, zoom, imageScreenX, imageScreenY }`
 * @param {number} targetDpr - 目标屏 DPR
 * @param {number} [padCss] - padding CSS 尺寸，默认 PIN_PAD_CSS
 * @returns {{ winX: number, winY: number, winW: number, winH: number, physW: number, physH: number, padPhys: number }}
 */
export function computeWindowRect(state, targetDpr, padCss = PIN_PAD_CSS) {
    const {physW, physH} = displayPhysicalSize(state.baseCssW, state.baseCssH, state.zoom, targetDpr);
    const padPhys = padPhysical(padCss, targetDpr);
    const rect = physicalWindowRect(state.imageScreenX, state.imageScreenY, physW, physH, padPhys);
    return {...rect, physW, physH, padPhys};
}

// ── 右键菜单临时扩窗 ───────────────────────────────────────────────────────

/**
 * 菜单打开期间的临时扩窗：把窗口物理矩形扩大到菜单完整显示所需的最小尺寸。
 *
 * 背景：窗口尺寸跟随图片缩放（baseCss × zoom），缩到很小的 pin 放不下完整
 * 右键菜单。扩窗只发生在菜单打开期间（关闭即还原），不引入常驻的隐形
 * 挡点击区域，也不参与 mini 动画（动画期间不可能开菜单）。
 *
 * 规则：
 * - 默认只向右/下扩（origin 不动 → 图片物理位置不动）
 * - 需要避让屏幕边缘时由调用方预先算好 `shiftX/shiftY`（物理像素，把窗口
 *   左/上移回工作区内），本函数只负责平移；图片视觉位置由前端用
 *   `pad + shift/dpr` 的 CSS 偏移补偿回来
 *
 * @param {{ winX: number, winY: number, winW: number, winH: number }} rect - 常规窗口矩形
 * @param {{ cssW: number, cssH: number, shiftX?: number, shiftY?: number } | null} expand -
 *        菜单所需最小窗口 CSS 尺寸与避让平移；null = 未扩窗
 * @param {number} targetDpr - 目标屏 DPR
 * @returns {{ winX: number, winY: number, winW: number, winH: number }} 扩窗后的矩形
 */
export function expandRectForMenu(rect, expand, targetDpr) {
    if (!expand) return rect;
    const dpr = Math.max(1, targetDpr);
    const needW = Math.max(1, Math.round(expand.cssW * dpr));
    const needH = Math.max(1, Math.round(expand.cssH * dpr));
    return {
        ...rect,
        winX: rect.winX - (expand.shiftX || 0),
        winY: rect.winY - (expand.shiftY || 0),
        winW: Math.max(rect.winW, needW),
        winH: Math.max(rect.winH, needH),
    };
}

// ── 缩放锚点 ──────────────────────────────────────────────────────────────

/**
 * 鼠标锚点缩放：计算缩放后图片左上角的新屏幕坐标。
 *
 * 原理：缩放前后，鼠标所指的图片内容保持在鼠标下方。
 *
 * ```text
 * anchorX = (pointerScreenX - oldImageX) / oldPhysW
 * anchorY = (pointerScreenY - oldImageY) / oldPhysH
 * // clamp to [0, 1]
 * newImageX = pointerScreenX - anchorX × newPhysW
 * newImageY = pointerScreenY - anchorY × newPhysH
 * ```
 *
 * @param {number} pointerScreenX - 鼠标屏幕物理 X
 * @param {number} pointerScreenY - 鼠标屏幕物理 Y
 * @param {number} oldImageX - 缩放前图片左上角屏幕物理 X
 * @param {number} oldImageY - 缩放前图片左上角屏幕物理 Y
 * @param {number} oldPhysW - 缩放前图片物理宽度
 * @param {number} oldPhysH - 缩放前图片物理高度
 * @param {number} newPhysW - 缩放后图片物理宽度
 * @param {number} newPhysH - 缩放后图片物理高度
 * @returns {{ newImageX: number, newImageY: number }}
 */
export function zoomAroundPointer(
    pointerScreenX, pointerScreenY,
    oldImageX, oldImageY,
    oldPhysW, oldPhysH,
    newPhysW, newPhysH,
) {
    const safeOldW = Math.max(1, oldPhysW);
    const safeOldH = Math.max(1, oldPhysH);
    const anchorX = clamp01((pointerScreenX - oldImageX) / safeOldW);
    const anchorY = clamp01((pointerScreenY - oldImageY) / safeOldH);
    return {
        newImageX: Math.round(pointerScreenX - anchorX * newPhysW),
        newImageY: Math.round(pointerScreenY - anchorY * newPhysH),
    };
}

/**
 * 图片中心锚点缩放：保持中心位置不变。
 *
 * 用于双击 mini mode / restore，中心保持在原位。
 *
 * @param {number} imageCenterX - 图片中心屏幕物理 X
 * @param {number} imageCenterY - 图片中心屏幕物理 Y
 * @param {number} newPhysW - 缩放后图片物理宽度
 * @param {number} newPhysH - 缩放后图片物理高度
 * @returns {{ newImageX: number, newImageY: number }}
 */
export function zoomAroundCenter(imageCenterX, imageCenterY, newPhysW, newPhysH) {
    return {
        newImageX: Math.round(imageCenterX - newPhysW / 2),
        newImageY: Math.round(imageCenterY - newPhysH / 2),
    };
}

// ── DPI reconcile ─────────────────────────────────────────────────────────

/**
 * DPI 变化后的 reconcile：保持视觉 CSS 尺寸不变，重算物理矩形。
 *
 * 调用时机：`onScaleChanged` 回调，或拖动跨 DPI 边界后回读窗口实际物理矩形。
 *
 * 步骤：
 * 1. 用新 DPR 重算 padding 和图片物理尺寸（视觉 CSS 尺寸 = baseCss × zoom 不变）
 * 2. 从实际窗口物理矩形反推图片左上角：`imageScreenX = actualWinX + newPad`
 * 3. 用新图片位置 + 新物理尺寸算出最终窗口矩形
 *
 * reconcile 应幂等，窗口边缘在两块屏之间往返时允许多次调用。
 *
 * @param {object} state - `{ baseCssW, baseCssH, zoom }`
 * @param {number} newDpr - 新目标屏 DPR
 * @param {{ winX: number, winY: number, winW: number, winH: number }} actualWinRect - 窗口当前实际物理矩形
 * @param {number} [padCss] - padding CSS 尺寸，默认 PIN_PAD_CSS
 * @returns {{ imageScreenX: number, imageScreenY: number, winX: number, winY: number, winW: number, winH: number, physW: number, physH: number, padPhys: number }}
 */
export function reconcileDpi(state, newDpr, actualWinRect, padCss = PIN_PAD_CSS) {
    const padPhys = padPhysical(padCss, newDpr);
    // 从实际窗口位置反推图片位置
    const imageScreenX = actualWinRect.winX + padPhys;
    const imageScreenY = actualWinRect.winY + padPhys;
    // 用新 DPR 重算物理尺寸（CSS 尺寸不变）
    const {physW, physH} = displayPhysicalSize(state.baseCssW, state.baseCssH, state.zoom, newDpr);
    // 算最终窗口矩形
    const rect = physicalWindowRect(imageScreenX, imageScreenY, physW, physH, padPhys);
    return {
        imageScreenX,
        imageScreenY,
        ...rect,
        physW,
        physH,
        padPhys,
    };
}

/**
 * 从窗口物理矩形和 DPR 反推图片屏幕坐标。
 *
 * @param {{ winX: number, winY: number }} winRect - 窗口物理矩形
 * @param {number} targetDpr - 目标屏 DPR
 * @param {number} [padCss] - padding CSS 尺寸
 * @returns {{ imageScreenX: number, imageScreenY: number }}
 */
export function imageScreenFromWinRect(winRect, targetDpr, padCss = PIN_PAD_CSS) {
    const padPhys = padPhysical(padCss, targetDpr);
    return {
        imageScreenX: winRect.winX + padPhys,
        imageScreenY: winRect.winY + padPhys,
    };
}

// ── 工具函数 ──────────────────────────────────────────────────────────────

/** clamp 到 [0, 1]。 */
function clamp01(v) {
    return Math.max(0, Math.min(1, v));
}

/** clamp zoom 到合法范围。 */
export function clampZoom(zoom) {
    return Math.max(MIN_ZOOM, Math.min(MAX_ZOOM, zoom));
}

/**
 * 计算图片中心屏幕物理坐标。
 *
 * @param {number} imageScreenX - 图片左上角屏幕物理 X
 * @param {number} imageScreenY - 图片左上角屏幕物理 Y
 * @param {number} physW - 图片物理宽度
 * @param {number} physH - 图片物理高度
 * @returns {{ cx: number, cy: number }}
 */
export function imageCenter(imageScreenX, imageScreenY, physW, physH) {
    return {
        cx: imageScreenX + physW / 2,
        cy: imageScreenY + physH / 2,
    };
}
