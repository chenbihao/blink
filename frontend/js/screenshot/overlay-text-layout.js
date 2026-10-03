//! OCR 嵌图文字排版（0.25.7）：参考高度校准 + 原框内逐行宽高适配。
//! 只测绘制文本，不扫描原图；预览与离屏 Pin 合成共用。

export const OVERLAY_FONT_HEIGHT_FACTOR = 0.80;
export const OVERLAY_FONT_FAMILY = 'system-ui, "Microsoft YaHei", "Noto Sans SC", sans-serif';

const MIN_FONT_SIZE = 8;
const GROUP_THRESHOLD = 0.25;
const FONT_STEPS_PER_PIXEL = 10;
const METRIC_EPSILON = 1e-7;
const graphemes = new Intl.Segmenter(undefined, {granularity: 'grapheme'});

function referenceHeight(entry) {
    const h = entry.r.h;
    const fontH = entry.line.fontH;
    return Number.isFinite(fontH) && fontH > 0 ? Math.min(fontH, h) : h;
}

function median(values) {
    const sorted = values.slice().sort((a, b) => a - b);
    const mid = Math.floor(sorted.length / 2);
    return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

function measureBox(ctx, text, size) {
    ctx.font = `${size}px ${OVERLAY_FONT_FAMILY}`;
    const m = ctx.measureText(text);
    const advance = Math.max(0, m.width);
    const left = Number.isFinite(m.actualBoundingBoxLeft) ? Math.max(0, m.actualBoundingBoxLeft) : 0;
    const right = Number.isFinite(m.actualBoundingBoxRight) ? Math.max(advance, m.actualBoundingBoxRight) : advance;
    const hasHeight = Number.isFinite(m.actualBoundingBoxAscent)
        && Number.isFinite(m.actualBoundingBoxDescent)
        && m.actualBoundingBoxAscent + m.actualBoundingBoxDescent > 0;
    const ascent = hasHeight ? m.actualBoundingBoxAscent : size * 0.8;
    const descent = hasHeight ? m.actualBoundingBoxDescent : size * 0.2;
    return {width: left + right, height: ascent + descent, left, ascent, descent};
}

function largestSize(min, max, fits) {
    let lo = Math.floor(min * FONT_STEPS_PER_PIXEL + METRIC_EPSILON);
    let hi = Math.floor(max * FONT_STEPS_PER_PIXEL + METRIC_EPSILON);
    if (hi > 0 && fits(hi / FONT_STEPS_PER_PIXEL)) return hi / FONT_STEPS_PER_PIXEL;
    while (lo < hi) {
        const mid = Math.floor((lo + hi + 1) / 2);
        if (fits(mid / FONT_STEPS_PER_PIXEL)) lo = mid;
        else hi = mid - 1;
    }
    return lo / FONT_STEPS_PER_PIXEL;
}

/**
 * 在参考字号上限内选最大可容纳字号；下限仍超宽才按字素省略。
 * 返回 alphabetic 基线坐标，调用方须使用相同字体、left 对齐绘制。
 * preferredSize 仅为组内参考，不能超过本行 OCR 高度与实际宽高限制。
 */
export function fitOverlayLineText(ctx, entry, fontScale = 1, preferredSize = Infinity) {
    const {r, text} = entry;
    const empty = {size: 0, display: '', x: r.x, y: r.y, truncated: !!text};
    if (!text || ![r.x, r.y, r.w, r.h].every(Number.isFinite) || r.w <= 0 || r.h <= 0) return empty;
    const scale = Number.isFinite(fontScale) && fontScale > 0 ? fontScale : 1;
    const referenceSize = referenceHeight(entry) * OVERLAY_FONT_HEIGHT_FACTOR * scale;
    const maxSize = Math.min(referenceSize, preferredSize);
    const maxHeight = Math.min(referenceSize, r.h);
    const maxWidth = Math.min(r.w * 0.95, r.w - 4);
    if (!(maxSize > 0 && maxWidth > 0)) return empty;

    ctx.save();
    ctx.textAlign = 'left';
    ctx.textBaseline = 'alphabetic';
    try {
        // 高度为硬限制；微小原框不为 8px 下限撑大，滑杆放大也不越出原框。
        const heightSize = largestSize(0, maxSize,
            (size) => measureBox(ctx, text, size).height <= maxHeight + METRIC_EPSILON);
        if (heightSize <= 0) return empty;
        const minSize = Math.min(MIN_FONT_SIZE, heightSize);
        const fits = (value, size) => {
            const box = measureBox(ctx, value, size);
            return box.width <= maxWidth + METRIC_EPSILON && box.height <= maxHeight + METRIC_EPSILON;
        };
        let size, display = text;
        if (fits(text, minSize)) {
            size = largestSize(minSize, heightSize, (candidate) => fits(text, candidate));
        } else {
            size = minSize;
            if (!fits('…', size)) return empty;
            const parts = Array.from(graphemes.segment(text), (part) => part.segment);
            let lo = 0, hi = parts.length;
            while (lo < hi) {
                const mid = Math.floor((lo + hi + 1) / 2);
                if (fits(parts.slice(0, mid).join('') + '…', size)) lo = mid;
                else hi = mid - 1;
            }
            display = parts.slice(0, lo).join('') + '…';
        }
        const box = measureBox(ctx, display, size);
        return {
            size, display,
            x: r.x + 2 + box.left,
            y: r.y + r.h / 2 + (box.ascent - box.descent) / 2,
            truncated: display !== text,
        };
    } finally {
        ctx.restore();
    }
}

/** 相邻行按 OCR 参考高度分组；统一参考字号后逐行适配，长句可独立缩小。 */
export function layoutOverlayText(ctx, entries, fontScale = 1) {
    if (entries.length === 0) return [];
    const scale = Number.isFinite(fontScale) && fontScale > 0 ? fontScale : 1;
    const heights = entries.map(referenceHeight);
    const layouts = new Array(entries.length);
    let start = 0;
    for (let i = 1; i <= entries.length; i++) {
        const anchor = median(heights.slice(start, i));
        if (i < entries.length && Math.abs(heights[i] - anchor) / anchor <= GROUP_THRESHOLD) continue;
        const preferred = anchor * OVERLAY_FONT_HEIGHT_FACTOR * scale;
        for (let j = start; j < i; j++) {
            layouts[j] = fitOverlayLineText(ctx, entries[j], scale, preferred);
        }
        start = i;
    }
    return layouts;
}
