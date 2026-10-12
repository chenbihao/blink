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

// ── mini 落位可见性钳制 ────────────────────────────────────────────────────

/**
 * 把窗口矩形原点钳制进工作区（物理像素），返回钳制后的原点（尺寸不变）。
 *
 * 背景：超高长图 pin 允许 JS 拖动把窗口顶边推到屏幕外（0.24.10 设计行为，
 * 否则长图下方内容不可达），此时以图片左上角锚定的 mini 小窗会整体落在
 * 工作区外——表现为"双击后 pin 消失"。
 *
 * 规则（逐轴独立）：
 * - 窗口边长 ≤ 工作区边长：四向 clamp（左/上越界右/下拉回，右/下越界左/上拉回）
 * - 窗口边长 > 工作区边长：该轴居中（极端小工作区，最大化可见面积）
 * - 工作区边长 ≤ 0：该轴不钳制（防御异常输入）
 *
 * 调用方在钳制发生后必须同步改写锚点状态（imageScreen = winOrigin + pad），
 * 否则 reconcile 安全网会用旧锚点把窗口拉回屏外。
 *
 * @param {{ winX: number, winY: number, winW: number, winH: number }} rect - 窗口物理矩形
 * @param {{ x: number, y: number, w: number, h: number }} work - 工作区物理矩形
 *   （rcWork，虚拟屏幕坐标系，副屏在左/上时 x/y 为负）
 * @returns {{ winX: number, winY: number }}
 */
export function clampWinOriginToWorkArea(rect, work) {
    const clampAxis = (origin, side, waStart, waLen) => {
        if (!(waLen > 0)) return origin;
        if (side > waLen) return Math.round(waStart + (waLen - side) / 2);
        return Math.min(Math.max(origin, waStart), waStart + waLen - side);
    };
    return {
        winX: clampAxis(rect.winX, rect.winW, work.x, work.w),
        winY: clampAxis(rect.winY, rect.winH, work.y, work.h),
    };
}

// ── JS 拖动 DPI 换算 ──────────────────────────────────────────────────────

/**
 * 长图 JS 拖动的屏幕位移换算：WebView2 的 `MouseEvent.screenX/Y` 是 CSS 像素
 * （高 DPI 屏当物理像素用会偏 1/dpr，main-window/contextmenu.js 同一实测
 * 结论），按当前屏 DPR 换算为物理位移后累加到图片锚点。
 *
 * 跨 DPI 屏时 WebView 坐标空间随所在屏整体重标：DPR 翻转瞬间先把已积累
 * 位移按**旧尺度**落账（`startImage += (lastScreen - startScreen) × 旧 dpr`）、
 * 重定基准，再以新尺度继续——分段换算，避免用新尺度重算全程产生假跳变；
 * 翻转事件本身的跨界微差由拖动结束的 reconcileFromBackend 收敛。
 *
 * 本函数原地推进会话状态（reducer 风格）。
 *
 * @param {{ startScreenX: number, startScreenY: number, startImageX: number,
 *           startImageY: number, lastScreenX: number, lastScreenY: number,
 *           dpr: number }} state - 拖动会话状态（原地修改）
 * @param {number} screenX - 当前事件 screenX（CSS px）
 * @param {number} screenY - 当前事件 screenY（CSS px）
 * @param {number} dpr - 当前窗口屏 DPR（拖动期取 window.devicePixelRatio）
 * @returns {{ imageScreenX: number, imageScreenY: number }} 本次事件的图片左上角物理坐标
 */
export function applyJsDragMove(state, screenX, screenY, dpr) {
    if (dpr !== state.dpr) {
        state.startImageX += Math.round((state.lastScreenX - state.startScreenX) * state.dpr);
        state.startImageY += Math.round((state.lastScreenY - state.startScreenY) * state.dpr);
        state.startScreenX = state.lastScreenX;
        state.startScreenY = state.lastScreenY;
        state.dpr = dpr;
    }
    state.lastScreenX = screenX;
    state.lastScreenY = screenY;
    return {
        imageScreenX: state.startImageX + Math.round((screenX - state.startScreenX) * state.dpr),
        imageScreenY: state.startImageY + Math.round((screenY - state.startScreenY) * state.dpr),
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
 * 锚点工具函数。注：mini mode 自 0.25.26 起为左上角锚定 + 工作区钳制
 * （见 clampWinOriginToWorkArea），本函数当前无调用方，保留作锚点数学参考。
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
