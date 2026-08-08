//! 0.15.8 选区体验增强：坐标契约与纯函数工具。
//!
//! 统一管理三种坐标系的换算：
//! - 屏幕物理像素（Rust 返回的虚拟屏幕坐标）
//! - 截图 bitmap 像素（与屏幕物理像素 1:1）
//! - overlay CSS 像素（受 devicePixelRatio 影响）
//!
//! **铁则**（0.18.8 修订）：
//! - 所有换算必须通过本模块，禁止在各处直接用 devicePixelRatio 拼公式
//! - 矩形右/下边界为排他边界，宽高始终为非负整数物理像素
//! - 跨出虚拟屏幕的窗口矩形先与截图 bitmap 求交
//! - **A/B 类**（屏幕坐标换算 / 输出裁剪尺寸）走本模块按屏查 dpr（per-monitor）
//! - **C 类**（主 canvas bitmap 映射：蒙版绘制 / cropData 提取 / compositeSelection 裁剪）
//!   保留 `window.devicePixelRatio`（overlay dpr），因主 canvas backing store = 全屏 bitmap
//!   物理像素、CSS width:100% 铺满 overlay 视口，bitmap↔CSS 映射比全局固定，无法按屏分段
//! - **D 类**（canvas 绘制粗细 / 手柄 / 线宽 / 取色像素）保留直读 `window.devicePixelRatio`

/**
 * 屏幕物理坐标 → bitmap 坐标
 * bitmap 左上角固定对应 (meta.vx, meta.vy)，与屏幕物理像素 1:1
 */
export function screenToBitmap(screenX, screenY, meta) {
  return {
    x: Math.round(screenX - meta.vx),
    y: Math.round(screenY - meta.vy)
  };
}

/**
 * bitmap 坐标 → 屏幕物理坐标
 */
export function bitmapToScreen(bitmapX, bitmapY, meta) {
  return {
    x: Math.round(meta.vx + bitmapX),
    y: Math.round(meta.vy + bitmapY)
  };
}

/**
 * 按屏幕物理坐标查所在屏的 dpr。
 * displays 每屏 CSS 矩形是用各自 dpr 折算的，需先把 CSS 矩形还原为物理矩形来命中检测。
 * 找不到屏时 fallback `window.devicePixelRatio`（兼容单屏/降级）。
 */
export function dprAtScreen(screenX, screenY, meta) {
  const displays = (meta && Array.isArray(meta.displays)) ? meta.displays : [];
  for (const d of displays) {
    const dpr = d.dpi / 96;
    // CSS 矩形还原为物理矩形：css * dpr + meta.vx
    const physX = d.x * dpr + (meta.vx || 0);
    const physY = d.y * dpr + (meta.vy || 0);
    const physW = d.w * dpr;
    const physH = d.h * dpr;
    if (screenX >= physX && screenX < physX + physW &&
        screenY >= physY && screenY < physY + physH) {
      return dpr;
    }
  }
  console.warn('[screenshot] dprAtScreen: 坐标未命中任何屏，fallback overlay dpr', { screenX, screenY });
  return window.devicePixelRatio || 1;
}

/**
 * 按 overlay CSS 坐标查所在屏的 dpr。
 * displays 每屏 CSS 矩形用各自 dpr 折算，CSS 坐标落在哪块屏就取该屏 dpr。
 * 边界点归属规则与 `findDisplayCssAt` 一致：取 displays 数组中靠后的命中项
 * （x < d.x + d.w 是右开边界，点落在屏 A 右边界的同时也落在屏 B 左边界，取后者）。
 * 找不到屏时 fallback `window.devicePixelRatio`（兼容单屏/降级）。
 */
export function dprAtCss(cssX, cssY, meta) {
  const displays = (meta && Array.isArray(meta.displays)) ? meta.displays : [];
  let found = null;
  for (const d of displays) {
    if (cssX >= d.x && cssX < d.x + d.w && cssY >= d.y && cssY < d.y + d.h) {
      found = d; // 取最后一个命中项（靠后的屏）
    }
  }
  if (found) return found.dpi / 96;
  console.warn('[screenshot] dprAtCss: 坐标未命中任何屏，fallback overlay dpr', { cssX, cssY });
  return window.devicePixelRatio || 1;
}

/**
 * 按 CSS 坐标所在屏的 dpr，把 CSS 尺寸转为物理尺寸（B 类：输出裁剪尺寸用）。
 */
export function cssSizeToPhysical(cssSize, cssX, cssY, meta) {
  return Math.round(cssSize * dprAtCss(cssX, cssY, meta));
}

/**
 * 屏幕物理坐标 → CSS 坐标（按坐标所在屏的 dpr 分段换算）
 */
export function screenToCss(screenX, screenY, meta) {
  const dpr = dprAtScreen(screenX, screenY, meta);
  return {
    x: (screenX - (meta.vx || 0)) / dpr,
    y: (screenY - (meta.vy || 0)) / dpr
  };
}

/**
 * overlay CSS 坐标 → 屏幕物理坐标（按 CSS 坐标所在屏的 dpr 分段换算）
 */
export function cssToScreen(cssX, cssY, meta) {
  const dpr = dprAtCss(cssX, cssY, meta);
  return {
    x: Math.round((meta.vx || 0) + cssX * dpr),
    y: Math.round((meta.vy || 0) + cssY * dpr)
  };
}

/**
 * bitmap 坐标 → overlay CSS 坐标
 */
export function bitmapToCss(bitmapX, bitmapY, meta) {
  const screen = bitmapToScreen(bitmapX, bitmapY, meta);
  return screenToCss(screen.x, screen.y, meta);
}

/**
 * overlay CSS 坐标 → bitmap 坐标
 */
export function cssToBitmap(cssX, cssY, meta) {
  const screen = cssToScreen(cssX, cssY, meta);
  return screenToBitmap(screen.x, screen.y, meta);
}

/**
 * 转换矩形（屏幕物理坐标 → bitmap 坐标）
 */
export function rectScreenToBitmap(rect, meta) {
  const topLeft = screenToBitmap(rect.x, rect.y, meta);
  return {
    x: topLeft.x,
    y: topLeft.y,
    w: Math.max(0, Math.round(rect.w)),
    h: Math.max(0, Math.round(rect.h))
  };
}

/**
 * 转换矩形（bitmap 坐标 → 屏幕物理坐标）
 */
export function rectBitmapToScreen(rect, meta) {
  const topLeft = bitmapToScreen(rect.x, rect.y, meta);
  return {
    x: topLeft.x,
    y: topLeft.y,
    w: Math.max(0, Math.round(rect.w)),
    h: Math.max(0, Math.round(rect.h))
  };
}

/**
 * 转换矩形（屏幕物理坐标 → overlay CSS 坐标）
 */
export function rectScreenToCss(rect, meta) {
  const topLeft = screenToCss(rect.x, rect.y, meta);
  const dpr = dprAtScreen(rect.x, rect.y, meta);
  return {
    x: topLeft.x,
    y: topLeft.y,
    w: rect.w / dpr,
    h: rect.h / dpr
  };
}

/**
 * 转换矩形（overlay CSS 坐标 → 屏幕物理坐标）
 */
export function rectCssToScreen(rect, meta) {
  const topLeft = cssToScreen(rect.x, rect.y, meta);
  const dpr = dprAtCss(rect.x, rect.y, meta);
  return {
    x: topLeft.x,
    y: topLeft.y,
    w: Math.round(rect.w * dpr),
    h: Math.round(rect.h * dpr)
  };
}

/**
 * 矩形 clamp：确保矩形在 bitmap 范围内
 */
export function clampRectToBitmap(rect, bitmapWidth, bitmapHeight) {
  const x = Math.max(0, Math.min(rect.x, bitmapWidth));
  const y = Math.max(0, Math.min(rect.y, bitmapHeight));
  const right = Math.max(x, Math.min(rect.x + rect.w, bitmapWidth));
  const bottom = Math.max(y, Math.min(rect.y + rect.h, bitmapHeight));
  return {
    x,
    y,
    w: right - x,
    h: bottom - y
  };
}

/**
 * 矩形 clamp：确保矩形在 CSS overlay 范围内
 */
export function clampRectToCss(rect, overlayWidth, overlayHeight) {
  const x = Math.max(0, Math.min(rect.x, overlayWidth));
  const y = Math.max(0, Math.min(rect.y, overlayHeight));
  const right = Math.max(x, Math.min(rect.x + rect.w, overlayWidth));
  const bottom = Math.max(y, Math.min(rect.y + rect.h, overlayHeight));
  return {
    x,
    y,
    w: right - x,
    h: bottom - y
  };
}

/**
 * 点是否在矩形内
 */
export function pointInRect(px, py, rect) {
  return px >= rect.x && px < rect.x + rect.w && py >= rect.y && py < rect.y + rect.h;
}

/**
 * 两点距离（CSS 像素）
 */
export function distanceCss(x1, y1, x2, y2) {
  const dx = x2 - x1;
  const dy = y2 - y1;
  return Math.sqrt(dx * dx + dy * dy);
}

/**
 * 格式化选区信息显示
 * 返回格式: (x, y) width × height px
 * 坐标和尺寸均为物理像素
 */
export function formatSelectionInfo(screenX, screenY, width, height) {
  return `(${Math.round(screenX)}, ${Math.round(screenY)}) ${Math.round(width)} × ${Math.round(height)} px`;
}

/**
 * 格式化色值信息
 * fmt: 0=HEX, 1=RGB, 2=HSL
 */
export function formatColor(r, g, b, fmt) {
  if (fmt === 1) {
    return `RGB(${r}, ${g}, ${b})`;
  }
  if (fmt === 2) {
    const [h, s, l] = rgbToHsl(r, g, b);
    return `HSL(${h}, ${s}%, ${l}%)`;
  }
  return `#${r.toString(16).padStart(2, '0')}${g.toString(16).padStart(2, '0')}${b.toString(16).padStart(2, '0')}`.toUpperCase();
}

/**
 * RGB → HSL 转换
 */
export function rgbToHsl(r, g, b) {
  r /= 255; g /= 255; b /= 255;
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const l = (max + min) / 2;
  if (max === min) return [0, 0, Math.round(l * 100)];
  const d = max - min;
  const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
  let h;
  if (max === r) h = ((g - b) / d + (g < b ? 6 : 0)) / 6;
  else if (max === g) h = ((b - r) / d + 2) / 6;
  else h = ((r - g) / d + 4) / 6;
  return [Math.round(h * 360), Math.round(s * 100), Math.round(l * 100)];
}

/** pending-snap 是否已达到自由框选阈值。 */
export function shouldStartFreeSelection(startX, startY, currentX, currentY, threshold = 3) {
  return distanceCss(startX, startY, currentX, currentY) >= threshold;
}

/**
 * 计算像素放大镜在 bitmap 边缘处的安全读取区域及网格偏移。
 * 返回值可直接传给 getImageData，中心格始终对应目标像素。
 */
export function magnifierSampleRegion(px, py, canvasWidth, canvasHeight, cols = 16, rows = 9) {
  const halfRows = Math.floor(rows / 2);
  const halfCols = Math.floor(cols / 2);
  const desiredX = px - halfCols;
  const desiredY = py - halfRows;
  const readX = Math.max(0, desiredX);
  const readY = Math.max(0, desiredY);
  const gridOffsetX = readX - desiredX;
  const gridOffsetY = readY - desiredY;
  return {
    readX,
    readY,
    gridOffsetX,
    gridOffsetY,
    width: Math.max(0, Math.min(cols - gridOffsetX, canvasWidth - readX)),
    height: Math.max(0, Math.min(rows - gridOffsetY, canvasHeight - readY)),
  };
}
