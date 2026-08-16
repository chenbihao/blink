//! 坐标契约与纯函数工具。
//!
//! 统一管理三种坐标系的换算：
//! - 屏幕物理像素（Win32/UIA 返回的虚拟屏幕坐标）
//! - 截图 bitmap 像素（与虚拟桌面物理像素 1:1）
//! - overlay CSS 像素（单一 HWND 的 CSS 坐标）
//!
//! **铁则**：
//! - CSS↔bitmap/screen 的真实比例由 canvas 实测 renderScale 决定，
//!   不由 overlayDpi 或 window.devicePixelRatio 推测。
//! - overlayDpi、GetDpiForWindow、window.devicePixelRatio 只做诊断和 fallback。
//! - 显示器原生 DPI 只用于识别选区位于哪块屏、判断跨 DPI 边界，
//!   不参与坐标乘除。
//! - 矩形右/下边界为排他边界，宽高始终为非负整数物理像素。

// ── render scale 管理 ──────────────────────────────────────────────────────

/**
 * 获取当前 render scale。
 * 优先使用 meta.renderScaleX/renderScaleY（由 syncRenderScale 实测），
 * fallback 到 window.devicePixelRatio，再 fallback 到 1。
 */
export function getRenderScale(meta) {
    let scaleX, scaleY;
    if (meta && typeof meta.renderScaleX === 'number' && meta.renderScaleX > 0) {
        scaleX = meta.renderScaleX;
    } else {
        scaleX = (typeof window !== 'undefined' && window.devicePixelRatio) || 1;
    }
    if (meta && typeof meta.renderScaleY === 'number' && meta.renderScaleY > 0) {
        scaleY = meta.renderScaleY;
    } else {
        scaleY = (typeof window !== 'undefined' && window.devicePixelRatio) || 1;
    }
    return {scaleX, scaleY};
}

/**
 * 手动设置 render scale（测试或特殊情况用）。
 */
export function setRenderScale(meta, scaleX, scaleY) {
    meta.renderScaleX = scaleX;
    meta.renderScaleY = scaleY;
}

/**
 * 从 canvas 实测渲染比例并写入 meta。
 * 必须在 canvas 已有 bitmap 尺寸且 DOM 布局稳定后调用。
 * @returns {boolean} 是否成功（canvas 尺寸为 0 时返回 false）
 */
export function syncRenderScale(canvas, meta) {
    const rect = canvas.getBoundingClientRect();
    if (canvas.width <= 0 || canvas.height <= 0 ||
        rect.width <= 0 || rect.height <= 0) {
        return false;
    }
    const scaleX = canvas.width / rect.width;
    const scaleY = canvas.height / rect.height;
    meta.renderScaleX = scaleX;
    meta.renderScaleY = scaleY;
    meta.viewportCssWidth = rect.width;
    meta.viewportCssHeight = rect.height;
    if (Math.abs(scaleX - scaleY) > 0.01) {
        console.warn('[screenshot] render scale X/Y 不一致', {
            scaleX,
            scaleY,
            canvasWidth: canvas.width,
            canvasHeight: canvas.height,
            rectWidth: rect.width,
            rectHeight: rect.height
        });
    }
    return true;
}

// ── overlayDpr（仅诊断/fallback，不再作为权威坐标比例） ──────────────────────

/**
 * overlay DPR：仅诊断用。优先从 meta.overlayDpi 读取，fallback 到 window.devicePixelRatio。
 * 不参与坐标换算。
 */
export function overlayDpr(meta) {
    if (meta && typeof meta.overlayDpi === 'number' && meta.overlayDpi > 0) {
        return meta.overlayDpi / 96;
    }
    return (typeof window !== 'undefined' && window.devicePixelRatio) || 1;
}

// ── 显示器原生 DPR 识别（不参与坐标乘除） ────────────────────────────────────

/**
 * 按屏幕物理坐标查所在屏的原生 DPR。
 * 直接在 physicalDisplays 中命中物理矩形。
 */
export function monitorDprAtScreen(screenX, screenY, meta) {
    const displays = (meta && Array.isArray(meta.physicalDisplays)) ? meta.physicalDisplays : [];
    for (const d of displays) {
        if (screenX >= d.x && screenX < d.x + d.w && screenY >= d.y && screenY < d.y + d.h) {
            return d.dpi / 96;
        }
    }
    return (typeof window !== 'undefined' && window.devicePixelRatio) || 1;
}

/**
 * 按 overlay CSS 坐标查所在屏的原生 DPR。
 * CSS → screen (via renderScale) → 命中 physicalDisplays → 返回 dpi/96
 */
export function monitorDprAtCss(cssX, cssY, meta) {
    const screen = cssPointToScreen(cssX, cssY, meta);
    return monitorDprAtScreen(screen.x, screen.y, meta);
}

/**
 * 浮动 UI 在 overlay CSS 坐标系中的视觉缩放比。
 *
 * 单一跨屏 HWND 的 DOM 只能按一个 renderScale 渲染，因此当工具栏落在
 * 不同 DPI 显示器时物理尺寸不一致。uiScaleAtCss 补偿这个差异：
 *
 *   uiScale = monitorDpr / renderScale
 *
 * - renderScale=1.5, 目标屏 100% → 1/1.5 ≈ 0.667（UI 需缩小）
 * - renderScale=1.5, 目标屏 150% → 1.5/1.5 = 1（无缩放）
 * - renderScale=1,   目标屏 200% → 2/1 = 2（UI 需放大）
 *
 * 调用方用 `transform: scale(uiScale)` + `transform-origin: top left` 应用。
 * position/clamp 逻辑必须使用变换后的视觉宽高（= offsetWidth * uiScale）。
 */
export function uiScaleAtCss(cssX, cssY, meta) {
    const monitorDpr = monitorDprAtCss(cssX, cssY, meta);
    const {scaleX} = getRenderScale(meta);
    return monitorDpr / scaleX;
}

// 兼容导出
export {monitorDprAtCss as dprAtCss, monitorDprAtScreen as dprAtScreen};

// ── 坐标换算（统一使用 renderScale） ──────────────────────────────────────────

/**
 * 屏幕物理坐标 → bitmap 坐标
 * bitmap 左上角固定对应 (meta.vx, meta.vy)，与屏幕物理像素 1:1
 */
export function screenToBitmap(screenX, screenY, meta) {
    return {
        x: Math.round(screenX - (meta?.vx || 0)),
        y: Math.round(screenY - (meta?.vy || 0))
    };
}

/**
 * bitmap 坐标 → 屏幕物理坐标
 */
export function bitmapToScreen(bitmapX, bitmapY, meta) {
    return {
        x: Math.round((meta?.vx || 0) + bitmapX),
        y: Math.round((meta?.vy || 0) + bitmapY)
    };
}

/**
 * 屏幕物理坐标 → CSS 坐标（使用 renderScale）
 */
export function screenPointToCss(screenX, screenY, meta) {
    const {scaleX, scaleY} = getRenderScale(meta);
    return {
        x: (screenX - (meta?.vx || 0)) / scaleX,
        y: (screenY - (meta?.vy || 0)) / scaleY
    };
}

/**
 * overlay CSS 坐标 → 屏幕物理坐标（使用 renderScale）
 */
export function cssPointToScreen(cssX, cssY, meta) {
    const {scaleX, scaleY} = getRenderScale(meta);
    return {
        x: Math.round((meta?.vx || 0) + cssX * scaleX),
        y: Math.round((meta?.vy || 0) + cssY * scaleY)
    };
}

/**
 * 虚拟屏幕物理坐标 → 截图 bitmap 坐标。
 *
 * 全虚拟桌面截图与 Win32 屏幕物理像素 1:1；这里只需减去虚拟屏原点，
 * 不经过 renderScale，避免高 DPI 屏幕上把光标量化到 CSS 像素网格。
 */
export function screenPointToBitmap(screenX, screenY, meta) {
    return {
        x: Math.round(screenX - (meta?.vx || 0)),
        y: Math.round(screenY - (meta?.vy || 0)),
    };
}

/**
 * bitmap 坐标 → overlay CSS 坐标
 * bitmap 与虚拟桌面物理像素 1:1，CSS = bitmap / renderScale
 */
export function bitmapPointToCss(bitmapX, bitmapY, meta) {
    const {scaleX, scaleY} = getRenderScale(meta);
    return {x: bitmapX / scaleX, y: bitmapY / scaleY};
}

/**
 * overlay CSS 坐标 → bitmap 坐标
 * bitmap = CSS * renderScale
 */
export function cssPointToBitmap(cssX, cssY, meta) {
    const {scaleX, scaleY} = getRenderScale(meta);
    return {x: Math.round(cssX * scaleX), y: Math.round(cssY * scaleY)};
}

/**
 * overlay CSS 矩形 → bitmap 矩形
 */
export function cssRectToBitmap(rect, meta) {
    const {scaleX, scaleY} = getRenderScale(meta);
    return {
        x: Math.round(rect.x * scaleX),
        y: Math.round(rect.y * scaleY),
        w: Math.round(rect.w * scaleX),
        h: Math.round(rect.h * scaleY)
    };
}

/**
 * bitmap 矩形 → overlay CSS 矩形
 */
export function bitmapRectToCss(rect, meta) {
    const {scaleX, scaleY} = getRenderScale(meta);
    return {x: rect.x / scaleX, y: rect.y / scaleY, w: rect.w / scaleX, h: rect.h / scaleY};
}

/**
 * 屏幕物理矩形 → overlay CSS 矩形
 */
export function screenRectToCss(rect, meta) {
    const {scaleX, scaleY} = getRenderScale(meta);
    const topLeft = screenPointToCss(rect.x, rect.y, meta);
    return {
        x: topLeft.x,
        y: topLeft.y,
        w: rect.w / scaleX,
        h: rect.h / scaleY
    };
}

/**
 * overlay CSS 矩形 → 屏幕物理矩形
 */
export function cssRectToScreen(rect, meta) {
    const {scaleX, scaleY} = getRenderScale(meta);
    const topLeft = cssPointToScreen(rect.x, rect.y, meta);
    return {
        x: topLeft.x,
        y: topLeft.y,
        w: Math.round(rect.w * scaleX),
        h: Math.round(rect.h * scaleY)
    };
}

// ── 兼容导出（旧名称，逐步迁移调用方） ──────────────────────────────────────
export {
    screenPointToCss as screenToCss,
    cssPointToScreen as cssToScreen,
    bitmapPointToCss as bitmapToCss,
    cssPointToBitmap as cssToBitmap,
    screenRectToCss as rectScreenToCss,
    cssRectToScreen as rectCssToScreen,
};

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
 * @deprecated 使用 cssRectToBitmap 代替。保留兼容，内部委托。
 */
export function cssSizeToPhysical(cssSize, cssX, cssY, meta) {
    const {scaleX, scaleY} = getRenderScale(meta);
    // 按旧签名：只返回一个值，用 X scale（与旧行为一致）
    return Math.round(cssSize * scaleX);
}

/**
 * 矩形 clamp：确保矩形在 bitmap 范围内
 */
export function clampRectToBitmap(rect, bitmapWidth, bitmapHeight) {
    const x = Math.max(0, Math.min(rect.x, bitmapWidth));
    const y = Math.max(0, Math.min(rect.y, bitmapHeight));
    const right = Math.max(x, Math.min(rect.x + rect.w, bitmapWidth));
    const bottom = Math.max(y, Math.min(rect.y + rect.h, bitmapHeight));
    return {x, y, w: right - x, h: bottom - y};
}

/**
 * 矩形 clamp：确保矩形在 CSS overlay 范围内
 */
export function clampRectToCss(rect, overlayWidth, overlayHeight) {
    const x = Math.max(0, Math.min(rect.x, overlayWidth));
    const y = Math.max(0, Math.min(rect.y, overlayHeight));
    const right = Math.max(x, Math.min(rect.x + rect.w, overlayWidth));
    const bottom = Math.max(y, Math.min(rect.y + rect.h, overlayHeight));
    return {x, y, w: right - x, h: bottom - y};
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
        return `rgb(${r}, ${g}, ${b})`;
    }
    if (fmt === 2) {
        const [h, s, l] = rgbToHsl(r, g, b);
        return `hsl(${h}, ${s}%, ${l}%)`;
    }
    return `#${r.toString(16).padStart(2, '0')}${g.toString(16).padStart(2, '0')}${b.toString(16).padStart(2, '0')}`.toUpperCase();
}

/**
 * RGB → HSL 转换
 */
export function rgbToHsl(r, g, b) {
    r /= 255;
    g /= 255;
    b /= 255;
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

// ── 0.20.6：像素精调纯函数 ──────────────────────────────────────────────────
//
// 所有函数在 bitmap 物理像素坐标空间操作，不涉及 CSS/DOM。
// 调用方负责把 CSS 坐标通过 cssRectToBitmap / cssPointToBitmap 转入 bitmap 空间后再调用。

/**
 * 选区整体移动 1 bitmap 物理像素，并钳制到 bitmap 边界。
 * @param {{x,y,w,h}} rect - bitmap 矩形
 * @param {number} dx - X 方向位移（-1 / 0 / +1）
 * @param {number} dy - Y 方向位移（-1 / 0 / +1）
 * @param {number} bitmapWidth
 * @param {number} bitmapHeight
 * @returns {{x,y,w,h}} 钳制后的新矩形（w/h 不变）
 */
export function moveRect1px(rect, dx, dy, bitmapWidth, bitmapHeight) {
    const newX = rect.x + dx;
    const newY = rect.y + dy;
    // 钳制：选区不超出 bitmap 范围 [0, bitmapWidth - rect.w]
    const clampedX = Math.max(0, Math.min(newX, Math.max(0, bitmapWidth - rect.w)));
    const clampedY = Math.max(0, Math.min(newY, Math.max(0, bitmapHeight - rect.h)));
    return {x: clampedX, y: clampedY, w: rect.w, h: rect.h};
}

/**
 * Resize 选区时应用 1:1 等边约束（Shift 按下）。
 * 以 anchor 边为基准，让新矩形宽=高=max(新宽,新高)。
 * @param {{x,y,w,h}} original - 原始 bitmap 矩形
 * @param {string} handle - 'nw'|'ne'|'sw'|'se'|'n'|'s'|'e'|'w'
 * @param {number} newW - resize 后的新宽
 * @param {number} newH - resize 后的新新高
 * @param {number} bitmapWidth
 * @param {number} bitmapHeight
 * @returns {{x,y,w,h}} 约束+钳制后的矩形
 */
export function applySquareResize(original, handle, newW, newH, bitmapWidth, bitmapHeight) {
    const side = Math.max(Math.abs(newW), Math.abs(newH));
    const s = Math.max(MIN_RESIZE_SIDE, side);
    let x = original.x, y = original.y, w = s, h = s;
    // 按 handle 方向确定锚点
    // 锚点 = 被拖拽边的对侧边
    if (handle.includes('w')) x = original.x + original.w - s;   // 拖西边 → 锚东
    if (handle.includes('n')) y = original.y + original.h - s;  // 拖北边 → 锚南
    // 'e' / 's' 保持 original.x / original.y 不变
    // 钳制到 bitmap
    if (x < 0) {
        const d = -x;
        x = 0;
        w = Math.max(MIN_RESIZE_SIDE, s - d);
        h = w;
    }
    if (y < 0) {
        const d = -y;
        y = 0;
        h = Math.max(MIN_RESIZE_SIDE, s - d);
        w = h;
    }
    if (x + w > bitmapWidth) {
        w = Math.max(MIN_RESIZE_SIDE, bitmapWidth - x);
        h = w;
    }
    if (y + h > bitmapHeight) {
        h = Math.max(MIN_RESIZE_SIDE, bitmapHeight - y);
        w = h;
    }
    return {x, y, w, h};
}

/** resize 最小边长常量 */
const MIN_RESIZE_SIDE = 2;

/**
 * 准星（取色十字）移动 1 bitmap px 并钳制到选区 bitmap 范围。
 * @param {{x,y}} pos - 当前准星 bitmap 坐标
 * @param {number} dx
 * @param {number} dy
 * @param {{x,y,w,h}} rect - 选区 bitmap 矩形（准星被限制在此范围内）
 * @returns {{x,y}} 钳制后的新坐标
 */
export function moveCrosshair1px(pos, dx, dy, rect) {
    const newX = pos.x + dx;
    const newY = pos.y + dy;
    // 准星限制在选区内 [rect.x, rect.x + rect.w - 1]
    const clampedX = Math.max(rect.x, Math.min(newX, rect.x + Math.max(0, rect.w - 1)));
    const clampedY = Math.max(rect.y, Math.min(newY, rect.y + Math.max(0, rect.h - 1)));
    return {x: clampedX, y: clampedY};
}

/**
 * 钳制选区矩形到 bitmap 物理边界。
 * 与 clampRectToBitmap 类似，但确保 w/h 不为负且不越界。
 */
export function clampRectToBitmapBounds(rect, bitmapWidth, bitmapHeight) {
    const x = Math.max(0, Math.min(rect.x, Math.max(0, bitmapWidth - 1)));
    const y = Math.max(0, Math.min(rect.y, Math.max(0, bitmapHeight - 1)));
    const right = Math.max(x + MIN_RESIZE_SIDE, Math.min(rect.x + rect.w, bitmapWidth));
    const bottom = Math.max(y + MIN_RESIZE_SIDE, Math.min(rect.y + rect.h, bitmapHeight));
    return {x, y, w: right - x, h: bottom - y};
}

/**
 * 计算像素放大镜在 bitmap 边缘处的安全读取区域及网格偏移。
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
