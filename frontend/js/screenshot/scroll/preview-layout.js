//! 长截图缩略预览的纯布局计算。
//!
//! 所有定位均基于 **来源显示器矩形**（monitorRect），不使用全局 viewport。
//! monitorRect 可有非零 origin（副屏在主屏左侧/上方时 x/y 为负）。

function clamp(value, minimum, maximum) {
    return Math.max(minimum, Math.min(Math.max(minimum, maximum), value));
}

/**
 * 计算缩略预览在来源显示器矩形内的位置。
 *
 * @param {object} options
 * @param {{x,y,w,h}} options.rect - 选区 CSS 矩形
 * @param {string} options.direction - 'vertical' | 'horizontal'
 * @param {{x,y,w,h}} options.monitorRect - 来源显示器 CSS 矩形（可有非零 origin）
 * @param {number} options.previewWidth - 预览视觉宽度（含 uiScale）
 * @param {number} options.previewHeight - 预览视觉高度（含 uiScale）
 * @param {number} options.gap - 间距
 * @returns {{left:number, top:number}}
 */
export function computePreviewPosition(options) {
    const {
        rect, direction, monitorRect,
        previewWidth, previewHeight, gap,
    } = options;
    const mx = monitorRect.x;
    const my = monitorRect.y;
    const mw = monitorRect.w;
    const mh = monitorRect.h;
    if (direction === 'vertical') {
        const right = rect.x + rect.w + gap;
        const left = rect.x - gap - previewWidth;
        const preferredLeft = right + previewWidth <= mx + mw - gap
            ? right
            : left >= mx + gap ? left : right;
        return {
            left: clamp(preferredLeft, mx + gap, mx + mw - previewWidth - gap),
            top: clamp(rect.y, my + gap, my + mh - previewHeight - gap),
        };
    }

    const below = rect.y + rect.h + gap;
    const above = rect.y - gap - previewHeight;
    const preferredTop = below + previewHeight <= my + mh - gap
        ? below
        : above >= my + gap ? above : below;
    return {
        left: clamp(rect.x, mx + gap, mx + mw - previewWidth - gap),
        top: clamp(preferredTop, my + gap, my + mh - previewHeight - gap),
    };
}

/** 滚轮发生后、真实定位完成前，仅供预览框使用的有界预测坐标。 */
export function computePredictedLocatorTop(options) {
    const {
        currentTop, pendingTop, direction, lastAcceptedShift, viewportHeight,
    } = options;
    const normalizedDirection = Math.sign(direction || 0);
    if (!normalizedDirection || !Number.isFinite(currentTop) || viewportHeight <= 0) {
        return currentTop;
    }
    const previousMagnitude = Math.abs(lastAcceptedShift || 0);
    const magnitude = clamp(
        previousMagnitude || viewportHeight * 0.35,
        viewportHeight * 0.15,
        viewportHeight * 0.65,
    );
    const base = Number.isFinite(pendingTop) ? pendingTop : currentTop;
    return Math.round(clamp(
        base + normalizedDirection * magnitude,
        currentTop - viewportHeight * 0.8,
        currentTop + viewportHeight * 0.8,
    ));
}
