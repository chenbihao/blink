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
