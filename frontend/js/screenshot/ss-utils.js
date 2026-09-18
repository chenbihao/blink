//! 截图 overlay 工具函数（0.14.6 §4 拆分）。
//!
//! 从 chord-screenshot.js 提取的纯函数，无状态依赖。

/** 标准化矩形坐标：返回 {x, y, w, h}，w/h 非负 */
export function norm(x1, y1, x2, y2) {
    return {
        x: Math.min(x1, x2), y: Math.min(y1, y2),
        w: Math.abs(x2 - x1), h: Math.abs(y2 - y1),
    };
}

/** 点是否在矩形内 */
export function pointInRect(px, py, rect) {
    return px >= rect.x && px <= rect.x + rect.w && py >= rect.y && py <= rect.y + rect.h;
}

/**
 * 拖拽产生的选区矩形（0.23.15 纯函数化）。
 *
 * 起点→当前点；`shiftKey` 按住时强制 1:1 正方形——边长取两轴较大位移，
 * 符号各自保持拖拽方向（因此反向拖选同样成立）。
 *
 * 新建拖选、pending-snap 达到 3px 阈值后转自由框选，这两条路径在 pointermove
 * 与 release 时都调用本函数。共用同一份数学，是「松手用 release 坐标重算最终
 * 矩形」不会与拖动过程中的采样出现分歧的前提。
 *
 * @param {number} startX 起点 CSS X
 * @param {number} startY 起点 CSS Y
 * @param {number} curX 当前点 CSS X
 * @param {number} curY 当前点 CSS Y
 * @param {boolean} shiftKey 是否强制 1:1
 * @returns {{x:number,y:number,w:number,h:number}} CSS 像素矩形（w/h 非负）
 */
export function computeDragRect(startX, startY, curX, curY, shiftKey = false) {
    if (!shiftKey) return norm(startX, startY, curX, curY);
    const dx = curX - startX;
    const dy = curY - startY;
    const side = Math.max(Math.abs(dx), Math.abs(dy));
    return norm(
        startX, startY,
        startX + (dx >= 0 ? side : -side),
        startY + (dy >= 0 ? side : -side),
    );
}

/**
 * 矩形/椭圆按住 Shift 约束长宽等比（0.11.8-e）：
 * 从起点 (sx,sy) 到当前 (ex,ey)，取 max(|dx|,|dy|) 作等边，符号保持原方向。
 * 只对 rect/ellipse 生效——箭头/铅笔等自由笔画不约束。
 * 返回修正后的 {x, y}，或 null 表示不需要约束。
 *
 * @param {string} tool — 当前标注工具（由调用方传入 annot.getTool()）
 */
export function applySquareConstraint(sx, sy, ex, ey, tool) {
    if (tool !== 'rect' && tool !== 'ellipse') return null;
    const dx = ex - sx;
    const dy = ey - sy;
    const side = Math.max(Math.abs(dx), Math.abs(dy));
    return {
        x: sx + (dx >= 0 ? side : -side),
        y: sy + (dy >= 0 ? side : -side),
    };
}

/**
 * 计算单轴平移边界（0.19.16）。
 *
 * 图片小于视口：保持完整位于视口内，min=origin, max=origin+viewport-image。
 * 图片大于视口：允许大部分处于屏幕外，但始终保留 minVisible 像素可抓回。
 *   min = origin + minVisible - imageSize（左/上最多拖到只剩尾部 minVisible）
 *   max = origin + viewportSize - minVisible（右/下最多拖到只剩头部 minVisible）
 *
 * @param {number} imageSize - 图片在该轴的 CSS 像素尺寸
 * @param {number} viewportSize - 视口在该轴的 CSS 像素尺寸
 * @param {number} origin - 视口在该轴的 CSS 原点（默认 0，副屏可能为负）
 * @param {number} minVisible - 最少保留可见的 CSS 像素（默认 48）
 * @returns {{ min: number, max: number }}
 */
export function computePanAxisBounds(imageSize, viewportSize, origin = 0, minVisible = 48) {
    if (!Number.isFinite(imageSize) || !Number.isFinite(viewportSize) || imageSize <= 0 || viewportSize <= 0) {
        return {min: origin, max: origin};
    }
    if (imageSize <= viewportSize) {
        return {
            min: origin,
            max: origin + viewportSize - imageSize,
        };
    }
    return {
        min: origin + minVisible - imageSize,
        max: origin + viewportSize - minVisible,
    };
}

/**
 * 浮动 UI 元素定位纯函数（0.19.16）。
 *
 * 根据锚选区矩形、视觉宽高和目标显示器矩形，
 * 计算 floating UI 的 {left, top} 坐标。
 *
 * @param {{ x, y, w, h }} anchorRect - 锚选区 CSS 矩形
 * @param {number} visualWidth - 元素缩放后视觉宽度
 * @param {number} visualHeight - 元素缩放后视觉高度
 * @param {{ x, y, w, h }} monitorRect - 目标显示器 CSS 矩形
 * @param {number} margin - 边距
 * @param {string} preferred - 首选方向 'below-center' | 'above-center'
 * @returns {{ left: number, top: number }}
 */
export function computeFloatingPlacement({
                                             anchorRect, visualWidth, visualHeight, monitorRect, margin = 8,
                                             preferred = 'below-center',
                                         }) {
    const centerX = anchorRect.x + anchorRect.w / 2;
    const belowTop = anchorRect.y + anchorRect.h + margin;
    const aboveTop = anchorRect.y - visualHeight - margin;

    let top;
    if (belowTop + visualHeight <= monitorRect.y + monitorRect.h - margin) {
        top = belowTop;
    } else if (aboveTop >= monitorRect.y + margin) {
        top = aboveTop;
    } else {
        top = Math.max(
            monitorRect.y + margin,
            Math.min(anchorRect.y, monitorRect.y + monitorRect.h - visualHeight - margin),
        );
    }

    let left = centerX - visualWidth / 2;
    left = Math.max(
        monitorRect.x + margin,
        Math.min(left, monitorRect.x + monitorRect.w - visualWidth - margin),
    );
    top = Math.max(
        monitorRect.y + margin,
        Math.min(top, monitorRect.y + monitorRect.h - visualHeight - margin),
    );

    return {left, top};
}

/**
 * 计算 canvas-backed 编辑器的初始定位（0.19.16 多 DPI 适配）。
 *
 * 图片小于来源显示器：居中到该屏幕。
 * 图片超出来源显示器：左/上保留 12px。
 *
 * @param {number} cssW - 图片 CSS 宽度
 * @param {number} cssH - 图片 CSS 高度
 * @param {{x,y,w,h}} monitorRect - 来源显示器 CSS 矩形
 * @returns {{x:number, y:number}}
 */
export function computeCanvasEditorInitialPosition(cssW, cssH, monitorRect) {
    const x = cssW <= monitorRect.w
        ? Math.round(monitorRect.x + (monitorRect.w - cssW) / 2)
        : monitorRect.x + 12;
    const y = cssH <= monitorRect.h
        ? Math.round(monitorRect.y + (monitorRect.h - cssH) / 2)
        : monitorRect.y + 12;
    return {x, y};
}
