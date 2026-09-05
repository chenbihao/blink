//! OCR 面板缩放几何纯函数（0.22.7 新增）。
//!
//! 提供独立可测的几何函数，供截图 OCR 面板和钉图 OCR 面板共用。
//! 不依赖 DOM / Tauri，全部用纯数值计算。
//!
//! 设计目标：
//! - 最小/最大尺寸钳制
//! - 显示器边界钳制（面板右下角不越出屏幕）
//! - DPI 视觉缩放补偿（面板用 `transform: scale(uiScale)` 时，
//!   offsetWidth/Height 是未缩放值，视觉尺寸 = offset × uiScale）

/**
 * 面板缩放的最小尺寸约束（CSS 像素，未缩放值）。
 * 视觉尺寸 = 这些值 × uiScale。
 * 0.22.10：去掉固定最大上限——放大空间由调用侧 clampPanelToMonitor 的
 * 显示器边界钳制承担（面板右下角不越屏即天然上限）。
 */
export const PANEL_MIN_W = 280;
export const PANEL_MIN_H = 240;

/**
 * 钳制面板新尺寸到最小范围。
 *
 * @param {number} rawW - 拖动产生的原始宽度（CSS px，未缩放）
 * @param {number} rawH - 拖动产生的原始高度（CSS px，未缩放）
 * @returns {{w: number, h: number}} 钳制后的尺寸
 */
export function clampPanelSize(rawW, rawH) {
    return {
        w: Math.max(PANEL_MIN_W, rawW),
        h: Math.max(PANEL_MIN_H, rawH),
    };
}

/**
 * 钳制面板位置+尺寸，使其不越出显示器边界。
 *
 * 面板用 `transform: scale(uiScale)` 做视觉缩放，所以：
 * - offsetWidth/offsetHeight 是未缩放的布局尺寸
 * - 视觉宽度 = w × uiScale，视觉高度 = h × uiScale
 *
 * 右下角拖动缩放时，左上角固定，只需确保右下角不越出显示器：
 *   left + w × uiScale ≤ mon.x + mon.w - margin
 *   top  + h × uiScale ≤ mon.y + mon.h - margin
 *
 * 如果越界，收束尺寸（不动位置，因为右下角拖动时左上角固定）。
 *
 * @param {number} left - 面板 left（CSS px）
 * @param {number} top - 面板 top（CSS px）
 * @param {number} w - 面板宽度（CSS px，未缩放）
 * @param {number} h - 面板高度（CSS px，未缩放）
 * @param {number} uiScale - 视觉缩放比
 * @param {{x: number, y: number, w: number, h: number}} mon - 显示器 CSS 矩形
 * @param {number} [margin=8] - 边界留白
 * @returns {{w: number, h: number}} 钳制后的尺寸（可能缩小以满足边界）
 */
export function clampPanelToMonitor(left, top, w, h, uiScale, mon, margin = 8) {
    const visW = w * uiScale;
    const visH = h * uiScale;
    const maxVisW = mon.x + mon.w - margin - left;
    const maxVisH = mon.y + mon.h - margin - top;
    // 如果空间不够（面板左上角太靠右下），至少留最小尺寸
    const allowedVisW = Math.max(PANEL_MIN_W * uiScale, maxVisW);
    const allowedVisH = Math.max(PANEL_MIN_H * uiScale, maxVisH);
    const clampedVisW = Math.min(visW, allowedVisW);
    const clampedVisH = Math.min(visH, allowedVisH);
    // 转回未缩放值，再走一次最小钳制
    const newW = clampedVisW / uiScale;
    const newH = clampedVisH / uiScale;
    return clampPanelSize(newW, newH);
}

/**
 * 计算右下角拖动缩放后的面板尺寸。
 *
 * @param {number} startW - 拖动开始时面板宽度（CSS px，未缩放）
 * @param {number} startH - 拖动开始时面板高度（CSS px，未缩放）
 * @param {number} deltaW - 鼠标 X 位移（CSS px，正值=向右拉大）
 * @param {number} deltaH - 鼠标 Y 位移（CSS px，正值=向下拉大）
 * @param {number} uiScale - 视觉缩放比（鼠标位移是视觉像素，需除以 uiScale 得到布局像素）
 * @returns {{w: number, h: number}} 钳制后的尺寸
 */
export function computeResizedPanel(startW, startH, deltaW, deltaH, uiScale) {
    // 鼠标位移是视觉像素；面板 width/height 是未缩放布局值
    // delta_layout = delta_visual / uiScale
    const dw = uiScale > 0 ? deltaW / uiScale : deltaW;
    const dh = uiScale > 0 ? deltaH / uiScale : deltaH;
    return clampPanelSize(startW + dw, startH + dh);
}
