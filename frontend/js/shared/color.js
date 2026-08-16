//! 0.20.3：颜色字面量契约校验、前端降级识别与 swatch renderer。
//!
//! 与 Rust `domain/color.rs` 共享 fixture（`fixtures/color-literals.json`），
//! Rust/JS 输出必须完全一致。本模块是纯函数，无 DOM/Tauri 依赖（swatch renderer 除外）。
//!
//! ## 支持范围
//! - `#RGB`、`#RGBA`、`#RRGGBB`、`#RRGGBBAA`，alpha 固定在末尾。
//! - `rgb()/rgba()` 常用整数与百分比。
//! - `hsl()/hsla()` 常用角度与百分比。
//! - 不支持 CSS 命名色、渐变、变量引用、长文本中的局部颜色。
//!
//! ## 舍入策略
//! 统一使用 half-away-from-zero（Math.round 的 JS 行为），
//! alpha 保留 3 位小数去尾零。

// ── 类型定义 ───────────────────────────────────────────────────────────────

/**
 * @typedef {{r: number, g: number, b: number, a: number}} Rgba8
 * @typedef {{original: string, rgba: Rgba8, hex: string, rgb: string, hsl: string, alpha: number}} ColorResult
 */

// ── 颜色解析入口 ───────────────────────────────────────────────────────────

/**
 * 尝试将完整 trim 后的字符串解析为颜色字面量。
 *
 * 仅当完整文本可解析时返回 ColorResult，不支持长文本中的局部颜色提取。
 * 空字符串、非法格式返回 null。
 *
 * @param {string} input
 * @returns {ColorResult | null}
 */
export function parse(input) {
    const trimmed = (input ?? "").trim();
    if (!trimmed) return null;

    const first = trimmed.charCodeAt(0);
    let rgba;

    if (first === 0x23 /* '#' */) {
        rgba = parseHex(trimmed);
    } else {
        const lower = trimmed.toLowerCase();
        if (lower.startsWith("rgb")) {
            rgba = parseRgbLike(trimmed);
        } else if (lower.startsWith("hsl")) {
            rgba = parseHslLike(trimmed);
        } else {
            return null;
        }
    }

    if (!rgba) return null;

    return buildColorResult(trimmed, rgba);
}

/**
 * 尝试将多行文本解析为颜色列表（0.20 多行颜色列表）。
 *
 * 按行 split，忽略空行，每个非空行须可解析为颜色字面量。
 * 行数 2~8 之外返回 null（单行走普通 parse，超过 8 行不展示 swatch）。
 *
 * 与 Rust `domain/color.rs::parse_color_list` 输出必须完全一致。
 *
 * @param {string} input
 * @returns {ColorResult[] | null}
 */
export function parseColorList(input) {
    const lines = (input ?? "").split(/\r\n|\r|\n/);
    const results = [];
    for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed) continue;
        const result = parse(trimmed);
        if (!result) return null; // 任一非空行不可解析 → null
        results.push(result);
    }
    if (results.length >= 2 && results.length <= 8) return results;
    return null;
}

/**
 * 从 RGBA8 构建 canonical 输出。
 * @param {string} original
 * @param {Rgba8} rgba
 * @returns {ColorResult}
 */
export function buildColorResult(original, rgba) {
    const alpha = rgba.a / 255;
    return {
        original,
        rgba,
        hex: toHex(rgba),
        rgb: toRgb(rgba),
        hsl: toHsl(rgba),
        alpha,
    };
}

// ── HEX 解析 ───────────────────────────────────────────────────────────────

/**
 * @param {string} s
 * @returns {Rgba8 | null}
 */
function parseHex(s) {
    const hex = s.slice(1); // skip '#'
    switch (hex.length) {
        case 3: {
            const r = hexNibble(hex.charCodeAt(0));
            const g = hexNibble(hex.charCodeAt(1));
            const b = hexNibble(hex.charCodeAt(2));
            if (r == null || g == null || b == null) return null;
            return {r: r * 17, g: g * 17, b: b * 17, a: 255};
        }
        case 4: {
            const r = hexNibble(hex.charCodeAt(0));
            const g = hexNibble(hex.charCodeAt(1));
            const b = hexNibble(hex.charCodeAt(2));
            const a = hexNibble(hex.charCodeAt(3));
            if (r == null || g == null || b == null || a == null) return null;
            return {r: r * 17, g: g * 17, b: b * 17, a: a * 17};
        }
        case 6: {
            const r = hexByte(hex.substring(0, 2));
            const g = hexByte(hex.substring(2, 4));
            const b = hexByte(hex.substring(4, 6));
            if (r == null || g == null || b == null) return null;
            return {r, g, b, a: 255};
        }
        case 8: {
            const r = hexByte(hex.substring(0, 2));
            const g = hexByte(hex.substring(2, 4));
            const b = hexByte(hex.substring(4, 6));
            const a = hexByte(hex.substring(6, 8));
            if (r == null || g == null || b == null || a == null) return null;
            return {r, g, b, a};
        }
        default:
            return null;
    }
}

/**
 * @param {number} c - char code
 * @returns {number | null}
 */
function hexNibble(c) {
    if (c >= 0x30 && c <= 0x39) return c - 0x30; // 0-9
    if (c >= 0x61 && c <= 0x66) return c - 0x61 + 10; // a-f
    if (c >= 0x41 && c <= 0x46) return c - 0x41 + 10; // A-F
    return null;
}

/**
 * @param {string} s - 2-char hex string
 * @returns {number | null}
 */
function hexByte(s) {
    if (s.length !== 2) return null;
    const hi = hexNibble(s.charCodeAt(0));
    const lo = hexNibble(s.charCodeAt(1));
    if (hi == null || lo == null) return null;
    return hi * 16 + lo;
}

// ── rgb()/rgba() 解析 ──────────────────────────────────────────────────────

/**
 * @param {string} s
 * @returns {Rgba8 | null}
 */
function parseRgbLike(s) {
    const inner = extractFunctionArgs(s, "rgb");
    if (inner == null) return null;
    const parts = inner.split(",");
    switch (parts.length) {
        case 3: {
            const r = parseChannel(parts[0]);
            const g = parseChannel(parts[1]);
            const b = parseChannel(parts[2]);
            if (r == null || g == null || b == null) return null;
            return {r: clampU8(r), g: clampU8(g), b: clampU8(b), a: 255};
        }
        case 4: {
            const r = parseChannel(parts[0]);
            const g = parseChannel(parts[1]);
            const b = parseChannel(parts[2]);
            const a = parseAlpha(parts[3]);
            if (r == null || g == null || b == null || a == null) return null;
            return {r: clampU8(r), g: clampU8(g), b: clampU8(b), a: alphaToU8(a)};
        }
        default:
            return null;
    }
}

/**
 * @param {string} s
 * @returns {number | null}
 */
function parseChannel(s) {
    s = s.trim();
    if (s.endsWith("%")) {
        const pct = parseFloat(s.slice(0, -1));
        if (isNaN(pct) || pct < 0 || pct > 100) return null;
        return pct / 100 * 255;
    }
    const v = parseFloat(s);
    if (isNaN(v) || v < 0 || v > 255) return null;
    return v;
}

/**
 * @param {string} s
 * @returns {number | null}
 */
function parseAlpha(s) {
    s = s.trim();
    const v = parseFloat(s);
    if (isNaN(v) || v < 0 || v > 1) return null;
    return v;
}

// ── hsl()/hsla() 解析 ──────────────────────────────────────────────────────

/**
 * @param {string} s
 * @returns {Rgba8 | null}
 */
function parseHslLike(s) {
    const inner = extractFunctionArgs(s, "hsl");
    if (inner == null) return null;
    const parts = inner.split(",");
    switch (parts.length) {
        case 3: {
            const h = parseHue(parts[0]);
            const sPct = parsePercent(parts[1]);
            const lPct = parsePercent(parts[2]);
            if (h == null || sPct == null || lPct == null) return null;
            const [r, g, b] = hslToRgb(h, sPct, lPct);
            return {r: clampU8(r), g: clampU8(g), b: clampU8(b), a: 255};
        }
        case 4: {
            const h = parseHue(parts[0]);
            const sPct = parsePercent(parts[1]);
            const lPct = parsePercent(parts[2]);
            const a = parseAlpha(parts[3]);
            if (h == null || sPct == null || lPct == null || a == null) return null;
            const [r, g, b] = hslToRgb(h, sPct, lPct);
            return {r: clampU8(r), g: clampU8(g), b: clampU8(b), a: alphaToU8(a)};
        }
        default:
            return null;
    }
}

/**
 * @param {string} s
 * @returns {number | null}
 */
function parseHue(s) {
    s = s.trim();
    const v = parseFloat(s);
    if (isNaN(v)) return null;
    return v;
}

/**
 * @param {string} s
 * @returns {number | null}
 */
function parsePercent(s) {
    s = s.trim();
    if (s.endsWith("%")) {
        const pct = parseFloat(s.slice(0, -1));
        if (isNaN(pct) || pct < 0 || pct > 100) return null;
        return pct;
    }
    return null;
}

// ── 函数参数提取 ───────────────────────────────────────────────────────────

/**
 * 从 `rgb(...)` / `rgba(...)` / `hsl(...)` / `hsla(...)` 中提取括号内参数。
 * @param {string} s
 * @param {string} fnName - "rgb" 或 "hsl"（不含 a 后缀）
 * @returns {string | null}
 */
function extractFunctionArgs(s, fnName) {
    const lower = s.toLowerCase();
    const prefixA = fnName + "a(";
    const prefix = fnName + "(";

    let start;
    if (lower.startsWith(prefixA)) {
        start = prefixA.length;
    } else if (lower.startsWith(prefix)) {
        start = prefix.length;
    } else {
        return null;
    }

    const rest = s.substring(start);
    const end = rest.lastIndexOf(")");
    if (end < 0 || end !== rest.length - 1) return null;

    return rest.substring(0, end);
}

// ── 颜色转换 ───────────────────────────────────────────────────────────────

/**
 * half-away-from-zero 舍入（JS Math.round 的行为）
 * @param {number} v
 * @returns {number}
 */
function roundHalfAway(v) {
    return Math.round(v);
}

/**
 * @param {number} v
 * @returns {number} 0-255 整数
 */
function clampU8(v) {
    return Math.max(0, Math.min(255, roundHalfAway(v)));
}

/**
 * @param {number} a - 0.0-1.0
 * @returns {number} 0-255 整数
 */
function alphaToU8(a) {
    return Math.max(0, Math.min(255, roundHalfAway(a * 255)));
}

/**
 * HSL → RGB 转换。h 为角度，s/l 为 0-100。
 * @param {number} hDeg
 * @param {number} sPct
 * @param {number} lPct
 * @returns {[number, number, number]} - r, g, b as 0-255 float
 */
function hslToRgb(hDeg, sPct, lPct) {
    const h = ((hDeg % 360) + 360) % 360 / 360;
    const s = sPct / 100;
    const l = lPct / 100;

    const q = l < 0.5 ? l * (1 + s) : l + s - l * s;
    const p = 2 * l - q;

    const r = hueToRgb(p, q, h + 1 / 3);
    const g = hueToRgb(p, q, h);
    const b = hueToRgb(p, q, h - 1 / 3);

    return [r * 255, g * 255, b * 255];
}

/**
 * @param {number} p
 * @param {number} q
 * @param {number} t
 * @returns {number}
 */
function hueToRgb(p, q, t) {
    if (t < 0) t += 1;
    if (t > 1) t -= 1;
    if (t < 1 / 6) return p + (q - p) * 6 * t;
    if (t < 0.5) return q;
    if (t < 2 / 3) return p + (q - p) * (2 / 3 - t) * 6;
    return p;
}

// ── Canonical 输出 ─────────────────────────────────────────────────────────

/**
 * @param {Rgba8} rgba
 * @returns {string}
 */
export function toHex(rgba) {
    const h = (v) => v.toString(16).padStart(2, "0").toUpperCase();
    if (rgba.a < 255) {
        return `#${h(rgba.r)}${h(rgba.g)}${h(rgba.b)}${h(rgba.a)}`;
    }
    return `#${h(rgba.r)}${h(rgba.g)}${h(rgba.b)}`;
}

/**
 * @param {Rgba8} rgba
 * @returns {string}
 */
export function toRgb(rgba) {
    const alpha = rgba.a / 255;
    if (alpha < 1) {
        return `rgb(${rgba.r}, ${rgba.g}, ${rgba.b}, ${formatAlpha(alpha)})`;
    }
    return `rgb(${rgba.r}, ${rgba.g}, ${rgba.b})`;
}

/**
 * @param {Rgba8} rgba
 * @returns {string}
 */
export function toHsl(rgba) {
    const [h, s, l] = rgbToHsl(rgba.r, rgba.g, rgba.b);
    const alpha = rgba.a / 255;
    if (alpha < 1) {
        return `hsl(${h}, ${s}%, ${l}%, ${formatAlpha(alpha)})`;
    }
    return `hsl(${h}, ${s}%, ${l}%)`;
}

/**
 * RGB → HSL 转换，返回 [h: int, s: number (1 decimal), l: number (1 decimal)]
 * @param {number} r - 0-255
 * @param {number} g
 * @param {number} b
 * @returns {[number, number, number]}
 */
function rgbToHsl(r, g, b) {
    const rN = r / 255;
    const gN = g / 255;
    const bN = b / 255;

    const max = Math.max(rN, gN, bN);
    const min = Math.min(rN, gN, bN);
    const delta = max - min;

    const l = (max + min) / 2;

    let s;
    if (delta === 0) {
        s = 0;
    } else if (l < 0.5) {
        s = delta / (max + min);
    } else {
        s = delta / (2 - max - min);
    }

    let h;
    if (delta === 0) {
        h = 0;
    } else {
        let hRaw;
        if (rN === max) {
            hRaw = (gN - bN) / delta;
        } else if (gN === max) {
            hRaw = 2 + (bN - rN) / delta;
        } else {
            hRaw = 4 + (rN - gN) / delta;
        }
        h = hRaw * 60;
    }

    const hMod = ((h % 360) + 360) % 360;
    const hInt = Math.round(hMod) % 360;

    // s 和 l 保留 1 位小数
    const sPct = Math.round(s * 1000) / 10;
    const lPct = Math.round(l * 1000) / 10;

    return [hInt, sPct, lPct];
}

/**
 * 格式化 alpha 浮点为字符串：最多 3 位小数，去掉尾部 0。
 * @param {number} a
 * @returns {string}
 */
function formatAlpha(a) {
    if (a === 0) return "0";
    if (a === 1) return "1";
    const s = a.toFixed(3);
    return s.replace(/0+$/, "").replace(/\.$/, "");
}

// ── Swatch Renderer ────────────────────────────────────────────────────────

/**
 * 创建颜色 swatch DOM 元素（棋盘底 + 色块 + 浅色边界）。
 * 用于主窗口结果列表和剪贴板颜色项的统一渲染。
 *
 * @param {Rgba8 | {r: number, g: number, b: number, a: number}} rgba
 * @param {object} [opts]
 * @param {string} [opts.className] - 额外 CSS class
 * @param {number} [opts.size=36] - 色块边长 px
 * @returns {HTMLElement}
 */
export function createSwatch(rgba, opts = {}) {
    const {className = "", size = 36} = opts;

    const swatch = document.createElement("span");
    swatch.className = `color-swatch ${className}`.trim();
    swatch.style.width = `${size}px`;
    swatch.style.height = `${size}px`;
    swatch.style.flexShrink = "0";
    swatch.style.display = "inline-block";
    swatch.style.borderRadius = "var(--radius-sm, 4px)";
    swatch.style.position = "relative";
    swatch.style.overflow = "hidden";
    swatch.style.border = "1px solid var(--border, rgba(128,128,128,0.3))";

    // 棋盘底（透明/半透明色显示棋盘格背景）
    if (rgba.a < 255) {
        swatch.style.backgroundImage = `
      linear-gradient(45deg, var(--text-weak, #ccc) 25%, transparent 25%),
      linear-gradient(-45deg, var(--text-weak, #ccc) 25%, transparent 25%),
      linear-gradient(45deg, transparent 75%, var(--text-weak, #ccc) 75%),
      linear-gradient(-45deg, transparent 75%, var(--text-weak, #ccc) 75%)
    `.replace(/\s+/g, " ").trim();
        swatch.style.backgroundSize = `${Math.max(4, size / 3)}px ${Math.max(4, size / 3)}px`;
        swatch.style.backgroundPosition = `0 0, 0 ${Math.max(4, size / 3) / 2}px, ${Math.max(4, size / 3) / 2}px ${-Math.max(4, size / 3) / 2}px`;
    }

    // 色块层
    const colorLayer = document.createElement("span");
    colorLayer.style.position = "absolute";
    colorLayer.style.inset = "0";
    colorLayer.style.backgroundColor = `rgba(${rgba.r}, ${rgba.g}, ${rgba.b}, ${(rgba.a / 255).toFixed(4)})`;
    swatch.appendChild(colorLayer);

    return swatch;
}

/**
 * 判断一个 AppEntry 是否为颜色结果项（source === "color"）。
 * @param {{source?: string}} entry
 * @returns {boolean}
 */
export function isColorEntry(entry) {
    return entry?.source === "color";
}

/**
 * 从 AppEntry 的 actions 中提取颜色 payload（canonical hex）。
 * @param {{actions?: Array<{kind: string, payload?: string}>}} entry
 * @returns {string | null}
 */
export function getColorPayload(entry) {
    const action = entry?.actions?.[0];
    if (!action || action.kind !== "copy") return null;
    return action.payload ?? null;
}
