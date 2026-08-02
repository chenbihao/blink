//! 长截图缩略预览的纯布局计算。

function clamp(value, minimum, maximum) {
  return Math.max(minimum, Math.min(Math.max(minimum, maximum), value));
}

export function computePreviewPosition(options) {
  const {
    rect, direction, viewportWidth, viewportHeight,
    previewWidth, previewHeight, gap,
  } = options;
  if (direction === 'vertical') {
    const right = rect.x + rect.w + gap;
    const left = rect.x - gap - previewWidth;
    const preferredLeft = right + previewWidth <= viewportWidth - gap
      ? right
      : left >= gap ? left : right;
    return {
      left: clamp(preferredLeft, gap, viewportWidth - previewWidth - gap),
      top: clamp(rect.y, gap, viewportHeight - previewHeight - gap),
    };
  }

  const below = rect.y + rect.h + gap;
  const above = rect.y - gap - previewHeight;
  const preferredTop = below + previewHeight <= viewportHeight - gap
    ? below
    : above >= gap ? above : below;
  return {
    left: clamp(rect.x, gap, viewportWidth - previewWidth - gap),
    top: clamp(preferredTop, gap, viewportHeight - previewHeight - gap),
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
