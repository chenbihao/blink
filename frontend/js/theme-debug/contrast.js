//! WCAG 对比度计算（0.25.11 主题调试台）。
//! 纯函数、无 DOM 依赖，便于 node 单测；仅支持 rgb()/rgba()/#hex 字面量
//! （CSS 变量的 computed value 都是 rgb/rgba 形态，hex 分支兜底手输场景）。

/**
 * 解析颜色字面量为 [r, g, b, a]（0-255 / 0-1）。
 * @param {string} value
 * @returns {[number, number, number, number] | null}
 */
export function parseColor(value) {
    if (typeof value !== "string") return null;
    const text = value.trim().toLowerCase();

    if (text.startsWith("#")) {
        const hex = text.slice(1);
        if (![3, 4, 6, 8].includes(hex.length)) return null;
        const expand = (part) =>
            part.length === 1 ? part + part : part;
        const r = parseInt(expand(hex.slice(0, hex.length >= 6 ? 2 : 1)), 16);
        const g = parseInt(expand(hex.slice(hex.length >= 6 ? 2 : 1, hex.length >= 6 ? 4 : 2)), 16);
        const b = parseInt(expand(hex.slice(hex.length >= 6 ? 4 : 2, hex.length >= 6 ? 6 : 3)), 16);
        const a = hex.length === 4 || hex.length === 8
            ? parseInt(expand(hex.slice(-2)), 16) / 255
            : 1;
        if ([r, g, b].some((v) => Number.isNaN(v))) return null;
        return [r, g, b, a];
    }

    const match = text.match(/^rgba?\(([^)]+)\)$/);
    if (!match) return null;
    const parts = match[1].split(/[\s,/]+/).filter(Boolean);
    if (parts.length < 3) return null;
    const rgb = parts.slice(0, 3).map((p) => parseFloat(p));
    if (rgb.some((v) => Number.isNaN(v))) return null;
    // 百分比写法（如 rgb(50%, 20%, 10%)）归一到 0-255
    const [r, g, b] = rgb.map((v, i) =>
        parts[i].endsWith("%") ? Math.round((v / 100) * 255) : v,
    );
    const a = parts[3] !== undefined ? parseFloat(parts[3]) : 1;
    if (Number.isNaN(a)) return null;
    return [r, g, b, a];
}

/** 单通道线性化（WCAG 2.x 定义）。 */
function channelLuminance(channel) {
    const c = channel / 255;
    return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
}

/**
 * 相对亮度（WCAG 2.x）。alpha 不参与（调用方自行合成到实底后再传）。
 * @param {string} value 颜色字面量
 * @returns {number | null} 0-1
 */
export function relativeLuminance(value) {
    const parsed = parseColor(value);
    if (!parsed) return null;
    const [r, g, b] = parsed;
    return 0.2126 * channelLuminance(r) + 0.7152 * channelLuminance(g) + 0.0722 * channelLuminance(b);
}

/**
 * 把 alpha 前景合成到不透明背景上（sRGB 逐通道插值）。
 * @param {string} fgValue rgba() 字面量
 * @param {string} bgColor 实底字面量
 * @returns {string} rgb() 字面量；入参非法返回 null
 */
export function compositeOver(fgValue, bgColor) {
    const fg = parseColor(fgValue);
    const bg = parseColor(bgColor);
    if (!fg || !bg) return null;
    const [fr, fgg, fb, fa] = fg;
    const [br, bg2, bb] = bg;
    const mix = (f, b) => Math.round(f * fa + b * (1 - fa));
    return `rgb(${mix(fr, br)}, ${mix(fgg, bg2)}, ${mix(fb, bb)})`;
}

/**
 * WCAG 对比度。
 * @param {string} fgValue 前景
 * @param {string} bgColor 背景
 * @returns {number | null} (L1+0.05)/(L2+0.05)
 */
export function contrastRatio(fgValue, bgColor) {
    const l1 = relativeLuminance(fgValue);
    const l2 = relativeLuminance(bgColor);
    if (l1 === null || l2 === null) return null;
    const [hi, lo] = l1 >= l2 ? [l1, l2] : [l2, l1];
    return (hi + 0.05) / (lo + 0.05);
}

/** WCAG 判级：AA 正文 4.5 / AA 大字与 UI 组件 3.0。 */
export const AA_NORMAL = 4.5;
export const AA_LARGE = 3.0;

/**
 * @param {number | null} ratio
 * @returns {"pass" | "large" | "fail"} pass=正文达标，large=仅大字/UI 达标，fail=不达标
 */
export function verdict(ratio) {
    if (ratio === null) return "fail";
    if (ratio >= AA_NORMAL) return "pass";
    if (ratio >= AA_LARGE) return "large";
    return "fail";
}
