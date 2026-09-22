//! 截图取色器与完整色盘（0.15.4）。
//!
//! 功能：
//! - SV 正方形色盘（HSV 色彩空间，左白下黑标准布局）
//! - 色相条（横向彩虹条）
//! - HEX/RGB/HSL 三种格式切换 + 数值双向联动
//! - 预设色阵列（高频快选）
//! - 吸管取色（从截图主 canvas 像素采样）
//!
//! 色彩空间转换使用标准 HSV/HSL 算法，纯函数无副作用。
//! 色盘交互：mousedown 开始拖拽 → mousemove 实时更新 → mouseup 结束。

import {ss} from './ss-state.js';
import * as annot from './annotation-engine.js';
import {initPalette} from './ss-palette.js';
import {
    enterColorPickerFollowing,
    hidePixelMagnifier,
    isColorPickerActive,
    movePickerPixel,
    resetColorPickerState,
    updatePixelMagnifier
} from './ss-interaction.js';
import {screenshotCursorPosition} from '../shared/api.js';

// ── 色彩空间转换（纯函数）──────────────────────────────────

/** HSV → RGB。h: 0-360, s/v: 0-1 → {r,g,b}: 0-255 */
function hsvToRgb(h, s, v) {
    const c = v * s;
    const x = c * (1 - Math.abs((h / 60) % 2 - 1));
    const m = v - c;
    let r, g, b;
    if (h < 60) {
        r = c;
        g = x;
        b = 0;
    } else if (h < 120) {
        r = x;
        g = c;
        b = 0;
    } else if (h < 180) {
        r = 0;
        g = c;
        b = x;
    } else if (h < 240) {
        r = 0;
        g = x;
        b = c;
    } else if (h < 300) {
        r = x;
        g = 0;
        b = c;
    } else {
        r = c;
        g = 0;
        b = x;
    }
    return {
        r: Math.round((r + m) * 255),
        g: Math.round((g + m) * 255),
        b: Math.round((b + m) * 255),
    };
}

/** RGB → HSV。r/g/b: 0-255 → {h: 0-360, s/v: 0-1} */
function rgbToHsv(r, g, b) {
    r /= 255;
    g /= 255;
    b /= 255;
    const max = Math.max(r, g, b);
    const min = Math.min(r, g, b);
    const d = max - min;
    let h = 0;
    if (d > 0) {
        if (max === r) h = ((g - b) / d) % 6;
        else if (max === g) h = (b - r) / d + 2;
        else h = (r - g) / d + 4;
        h *= 60;
        if (h < 0) h += 360;
    }
    return {h, s: max > 0 ? d / max : 0, v: max};
}

/** RGB → HEX 字符串（#RRGGBB，大写） */
function rgbToHex(r, g, b) {
    return '#' + [r, g, b].map((x) => x.toString(16).padStart(2, '0')).join('').toUpperCase();
}

/** HEX → RGB。非法返回 null */
function hexToRgb(hex) {
    let h = hex.replace('#', '').trim();
    if (h.length === 3) h = h.split('').map((c) => c + c).join('');
    if (h.length !== 6) return null;
    const r = parseInt(h.slice(0, 2), 16);
    const g = parseInt(h.slice(2, 4), 16);
    const b = parseInt(h.slice(4, 6), 16);
    if (isNaN(r) || isNaN(g) || isNaN(b)) return null;
    return {r, g, b};
}

/** RGB → HSL。r/g/b: 0-255 → {h: 0-360, s/l: 0-100} */
function rgbToHsl(r, g, b) {
    r /= 255;
    g /= 255;
    b /= 255;
    const max = Math.max(r, g, b);
    const min = Math.min(r, g, b);
    const l = (max + min) / 2;
    let h = 0, s = 0;
    if (max !== min) {
        const d = max - min;
        s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
        if (max === r) h = ((g - b) / d) % 6;
        else if (max === g) h = (b - r) / d + 2;
        else h = (r - g) / d + 4;
        h *= 60;
        if (h < 0) h += 360;
    }
    return {h: Math.round(h), s: Math.round(s * 100), l: Math.round(l * 100)};
}

/** HSL → RGB。h: 0-360, s/l: 0-100 → {r,g,b}: 0-255 */
function hslToRgb(h, s, l) {
    s /= 100;
    l /= 100;
    const c = (1 - Math.abs(2 * l - 1)) * s;
    const x = c * (1 - Math.abs((h / 60) % 2 - 1));
    const m = l - c / 2;
    let r, g, b;
    if (h < 60) {
        r = c;
        g = x;
        b = 0;
    } else if (h < 120) {
        r = x;
        g = c;
        b = 0;
    } else if (h < 180) {
        r = 0;
        g = c;
        b = x;
    } else if (h < 240) {
        r = 0;
        g = x;
        b = c;
    } else if (h < 300) {
        r = x;
        g = 0;
        b = c;
    } else {
        r = c;
        g = 0;
        b = x;
    }
    return {
        r: Math.round((r + m) * 255),
        g: Math.round((g + m) * 255),
        b: Math.round((b + m) * 255),
    };
}

// ── 色盘状态 ──────────────────────────────────────────────

/** 当前 HSV 状态（内部维护，与 annot.getColor() 同步） */
let hsv = {h: 0, s: 1, v: 1};
/** 0.15.8-fix：当前透明度（0-1，1=不透明） */
let alpha = 1;
/** 当前色彩格式 */
let format = 'hex';
/** 是否正在拖拽 SV 色盘 */
let svDragging = false;
/** 是否正在拖拽色相条 */
let hueDragging = false;
/** 0.15.8-fix：是否正在拖拽透明度条 */
let alphaDragging = false;
/** 是否处于取色器模式 */
let picking = false;

// ── DOM 引用 ──────────────────────────────────────────────

let svCanvas = null;
let svCtx = null;
let hueCanvas = null;
let hueCtx = null;
let alphaCanvas = null;  // 0.15.8-fix：透明度条
let alphaCtx = null;
let colorValueInput = null;
// 0.23.19：色彩格式改自定义下拉（原生 select 弹层由浏览器定位，
// 多屏 + 工具栏 transform scale 下会偏移，无法钳制到目标显示器）
let colorFormatTrigger = null;
let colorFormatLabel = null;
let colorFormatMenu = null;
let colorTriggerDot = null;
let dropdown = null;

// ── 渲染 ──────────────────────────────────────────────────

/** 渲染 SV 色盘 */
function renderSV() {
    if (!svCtx) return;
    const w = svCanvas.width;
    const h = svCanvas.height;
    // 横向渐变：白 → 纯色相
    const pure = hsvToRgb(hsv.h, 1, 1);
    const gradH = svCtx.createLinearGradient(0, 0, w, 0);
    gradH.addColorStop(0, '#ffffff');
    gradH.addColorStop(1, `rgb(${pure.r},${pure.g},${pure.b})`);
    svCtx.fillStyle = gradH;
    svCtx.fillRect(0, 0, w, h);
    // 纵向渐变：透明 → 黑
    const gradV = svCtx.createLinearGradient(0, 0, 0, h);
    gradV.addColorStop(0, 'rgba(0,0,0,0)');
    gradV.addColorStop(1, 'rgba(0,0,0,1)');
    svCtx.fillStyle = gradV;
    svCtx.fillRect(0, 0, w, h);
    // 当前位置标记
    const mx = hsv.s * w;
    const my = (1 - hsv.v) * h;
    svCtx.strokeStyle = '#fff';
    svCtx.lineWidth = 2;
    svCtx.beginPath();
    svCtx.arc(mx, my, 5, 0, Math.PI * 2);
    svCtx.stroke();
    svCtx.strokeStyle = 'rgba(0,0,0,0.5)';
    svCtx.lineWidth = 1;
    svCtx.beginPath();
    svCtx.arc(mx, my, 5, 0, Math.PI * 2);
    svCtx.stroke();
}

/** 渲染色相条 */
function renderHue() {
    if (!hueCtx) return;
    const w = hueCanvas.width;
    const h = hueCanvas.height;
    const grad = hueCtx.createLinearGradient(0, 0, w, 0);
    for (let i = 0; i <= 6; i++) {
        const rgb = hsvToRgb(i * 60, 1, 1);
        grad.addColorStop(i / 6, `rgb(${rgb.r},${rgb.g},${rgb.b})`);
    }
    hueCtx.fillStyle = grad;
    hueCtx.fillRect(0, 0, w, h);
    // 当前色相标记
    const mx = (hsv.h / 360) * w;
    hueCtx.strokeStyle = '#fff';
    hueCtx.lineWidth = 2;
    hueCtx.beginPath();
    hueCtx.moveTo(mx, 0);
    hueCtx.lineTo(mx, h);
    hueCtx.stroke();
}

/** P1-3：渲染透明度条——标准方形棋盘，棋盘色/边框/指示线走主题变量 */
function renderAlpha() {
    if (!alphaCtx) return;
    const w = alphaCanvas.width;
    const h = alphaCanvas.height;
    // P1-3：棋盘格背景色从主题变量读取（--surface 为深色格，--surface-2 为浅色格）
    const style = getComputedStyle(alphaCanvas);
    const checkerLight = style.getPropertyValue('--surface-2').trim() || '#ccc';
    const checkerDark = style.getPropertyValue('--surface').trim() || '#999';
    const cellSize = 4;
    for (let y = 0; y < h; y += cellSize) {
        for (let x = 0; x < w; x += cellSize) {
            alphaCtx.fillStyle = ((Math.floor(x / cellSize) + Math.floor(y / cellSize)) % 2 === 0) ? checkerLight : checkerDark;
            alphaCtx.fillRect(x, y, cellSize, cellSize);
        }
    }
    // 当前色 + 透明度渐变
    const rgb = hsvToRgb(hsv.h, hsv.s, hsv.v);
    const grad = alphaCtx.createLinearGradient(0, 0, w, 0);
    grad.addColorStop(0, `rgba(${rgb.r},${rgb.g},${rgb.b},0)`);
    grad.addColorStop(1, `rgba(${rgb.r},${rgb.g},${rgb.b},1)`);
    alphaCtx.fillStyle = grad;
    alphaCtx.fillRect(0, 0, w, h);
    // P1-3：指示线用主题文字色，确保在明/暗主题下均可见
    const mx = alpha * w;
    alphaCtx.strokeStyle = style.getPropertyValue('--text').trim() || '#fff';
    alphaCtx.lineWidth = 2;
    alphaCtx.beginPath();
    alphaCtx.moveTo(mx, 0);
    alphaCtx.lineTo(mx, h);
    alphaCtx.stroke();
}

/** 更新数值输入框 */
function updateValueInput() {
    if (!colorValueInput) return;
    const rgb = hsvToRgb(hsv.h, hsv.s, hsv.v);
    if (format === 'hex') {
        colorValueInput.value = rgbToHex(rgb.r, rgb.g, rgb.b);
    } else if (format === 'rgb') {
        colorValueInput.value = `${rgb.r}, ${rgb.g}, ${rgb.b}`;
    } else {
        const hsl = rgbToHsl(rgb.r, rgb.g, rgb.b);
        colorValueInput.value = `${hsl.h}, ${hsl.s}%, ${hsl.l}%`;
    }
}

/** 0.15.8-fix：生成带透明度的颜色字符串 */
function getColorString() {
    const rgb = hsvToRgb(hsv.h, hsv.s, hsv.v);
    if (alpha < 1) {
        return `rgba(${rgb.r},${rgb.g},${rgb.b},${alpha.toFixed(2)})`;
    }
    return rgbToHex(rgb.r, rgb.g, rgb.b);
}

/** 全量更新：色盘 + 色相条 + 透明度条 + 数值 + annot 颜色 + 触发器圆点 */
function updateAll() {
    renderSV();
    renderHue();
    renderAlpha();
    updateValueInput();
    const colorStr = getColorString();
    annot.setColor(colorStr);
    // 0.23.19：颜色写 --swatch-overlay（CSS 里叠在棋盘格上方的颜色层）。
    // 不能写 background-color——它垫在 background-image 之下，会被不透明
    // 棋盘格完全盖住（表现为"只有棋盘格没有颜色"）。
    if (colorTriggerDot) colorTriggerDot.style.setProperty('--swatch-overlay', colorStr);
    // 更新预设 swatch active 状态
    if (dropdown) {
        const hex = rgbToHex(hsvToRgb(hsv.h, hsv.s, hsv.v).r, hsvToRgb(hsv.h, hsv.s, hsv.v).g, hsvToRgb(hsv.h, hsv.s, hsv.v).b);
        dropdown.querySelectorAll('.color-swatch').forEach((s) => {
            s.classList.toggle('active', s.dataset.color?.toLowerCase() === hex.toLowerCase() && alpha >= 1);
        });
    }
}

// ── 交互 ──────────────────────────────────────────────────

/** 从鼠标事件计算 SV 坐标 */
function svFromEvent(e) {
    const rect = svCanvas.getBoundingClientRect();
    let x = (e.clientX - rect.left) / rect.width;
    let y = (e.clientY - rect.top) / rect.height;
    x = Math.max(0, Math.min(1, x));
    y = Math.max(0, Math.min(1, y));
    hsv.s = x;
    hsv.v = 1 - y;
    updateAll();
}

/** 从鼠标事件计算色相 */
function hueFromEvent(e) {
    const rect = hueCanvas.getBoundingClientRect();
    let x = (e.clientX - rect.left) / rect.width;
    x = Math.max(0, Math.min(1, x));
    hsv.h = x * 360;
    updateAll();
}

/** 0.15.8-fix：从鼠标事件计算透明度 */
function alphaFromEvent(e) {
    const rect = alphaCanvas.getBoundingClientRect();
    let x = (e.clientX - rect.left) / rect.width;
    x = Math.max(0, Math.min(1, x));
    alpha = x;
    updateAll();
}

/** 解析数值输入框 */
function parseValueInput() {
    if (!colorValueInput) return;
    const val = colorValueInput.value.trim();
    let rgb = null;
    if (format === 'hex') {
        rgb = hexToRgb(val);
    } else if (format === 'rgb') {
        const m = val.match(/(\d+)\s*,\s*(\d+)\s*,\s*(\d+)/);
        if (m) rgb = {r: +m[1], g: +m[2], b: +m[3]};
    } else {
        const m = val.match(/(\d+)\s*,\s*(\d+)%?\s*,\s*(\d+)%?/);
        if (m) rgb = hslToRgb(+m[1], +m[2], +m[3]);
    }
    if (!rgb) return;
    rgb.r = Math.max(0, Math.min(255, rgb.r));
    rgb.g = Math.max(0, Math.min(255, rgb.g));
    rgb.b = Math.max(0, Math.min(255, rgb.b));
    hsv = rgbToHsv(rgb.r, rgb.g, rgb.b);
    updateAll();
}

/** 0.23.19：统一格式切换入口——同步内部 format、触发器文案与菜单 active 态 */
function setFormat(f) {
    if (f !== 'hex' && f !== 'rgb' && f !== 'hsl') return;
    format = f;
    if (colorFormatLabel) colorFormatLabel.textContent = format.toUpperCase();
    if (colorFormatMenu) {
        colorFormatMenu.querySelectorAll('.dropdown-item').forEach((b) => {
            b.classList.toggle('active', b.dataset.format === format);
        });
    }
    updateValueInput();
}

// ── 取色器（吸管）模式 ────────────────────────────────────

/** 进入取色模式 */
function enterPickMode() {
    picking = true;
    ss.eyedropperActive = true;  // 0.15.10：让 mousemove 显示像素放大镜
    // 0.20.6：进入取色跟随模式（方向键精调）
    enterColorPickerFollowing();
    if (dropdown) dropdown.setAttribute('data-open', 'false');
    ss.canvas.style.cursor = 'crosshair';
    // 0.23.19：划词层 #ocr-hit-canvas 是主 canvas 的兄弟层且层级更高——此前
    // 监听挂在 ss.canvas 上，划词激活时点击全被划词层截走，取色表现为
    // 「被 OCR 划词识别覆盖」。改挂 document 捕获阶段并阻断传播：无论指针
    // 落在划词层、标注层还是工具栏上，取色都优先接管；点击选区外也不会再
    // 触发主 canvas 的「退出标注模式」分支（取消选取）。
    document.documentElement.classList.add('eyedropper-picking');

    // 统一清理：移除监听器，复位 cursor
    let cleaned = false;
    const cleanup = () => {
        if (cleaned) return;
        cleaned = true;
        picking = false;
        ss.eyedropperActive = false;  // 0.15.10：清除标志
        // 0.20.6：重置取色状态机
        resetColorPickerState();
        ss.canvas.style.cursor = '';
        document.documentElement.classList.remove('eyedropper-picking');
        document.removeEventListener('pointerdown', onPick, true);
        document.removeEventListener('pointermove', onPickMove, true);
        document.removeEventListener('keydown', onPickKey, true);
        // 0.15.10：隐藏像素放大镜
        hidePixelMagnifier();
    };

    // 捕获阶段 pointerdown：从原始截图像素采样。
    // 使用 Win32 GetCursorPos 获取物理屏幕坐标，直接映射到 bitmap 坐标，
    // 不通过 CSS 坐标插值，确保在高 DPI 屏幕上逐物理像素精确采样。
    const onPick = async (e) => {
        e.preventDefault();
        e.stopPropagation();
        if (!picking) return;
        const meta = window.__blinkScreenMeta || {vx: 0, vy: 0};
        try {
            const pos = await screenshotCursorPosition();
            const bitmapX = Math.round(pos.x - meta.vx);
            const bitmapY = Math.round(pos.y - meta.vy);
            await samplePixelAt(bitmapX, bitmapY);
        } catch (err) {
            console.warn('[color-picker] 取色失败', err);
        }
        cleanup();
    };
    document.addEventListener('pointerdown', onPick, true);

    // 0.23.19：指针可能悬浮在划词层/放大镜等非 canvas 元素上，主 canvas 的
    // pointermove 收不到——同样挂 document 捕获阶段驱动放大镜预览。
    const onPickMove = (e) => {
        if (!picking) return;
        updatePixelMagnifier(e.clientX, e.clientY);
        ss._lastMagnifierPos = {x: e.clientX, y: e.clientY};
    };
    document.addEventListener('pointermove', onPickMove, true);

    // 0.20.6：取色器键盘事件——方向键移动 1 物理像素、Esc 退出
    const onPickKey = (e) => {
        // IME composition 优先
        if (e.isComposing) return;
        if (e.key === 'Escape') {
            // 精调提示承诺「Esc 取消」——只取消取色，不冒泡给主 ESC 级联
            // （否则取色中按 Esc 会把整个截图会话一起取消）。
            e.preventDefault();
            e.stopPropagation();
            cleanup();
            return;
        }
        // 方向键移动 1 物理像素（取色器激活时）
        if (isColorPickerActive()) {
            let dx = 0, dy = 0;
            if (e.key === 'ArrowLeft') {
                dx = -1;
            } else if (e.key === 'ArrowRight') {
                dx = 1;
            } else if (e.key === 'ArrowUp') {
                dy = -1;
            } else if (e.key === 'ArrowDown') {
                dy = 1;
            }
            if (dx !== 0 || dy !== 0) {
                e.preventDefault();
                e.stopPropagation();
                movePickerPixel(dx, dy);
            }
        }
    };
    document.addEventListener('keydown', onPickKey, true);
}

/** 0.20.6：从指定 bitmap 坐标采样像素并更新色盘 */
async function samplePixelAt(bitmapX, bitmapY) {
    const source = ss.screenshotOffscreen || ss.canvas;
    const sourceCtx = source.getContext('2d');
    const pixel = sourceCtx.getImageData(bitmapX, bitmapY, 1, 1).data;
    hsv = rgbToHsv(pixel[0], pixel[1], pixel[2]);
    alpha = 1;
    updateAll();
}

// ── 初始化与绑定 ──────────────────────────────────────────

/** 从 annot 当前颜色同步到色盘 */
export function syncFromAnnot() {
    const color = annot.getColor();
    // 0.15.8-fix：解析 rgba 获取透明度
    const rgbaMatch = color.match(/rgba?\(([^)]+)\)/i);
    if (rgbaMatch) {
        const parts = rgbaMatch[1].split(',').map((p) => p.trim());
        if (parts.length >= 3) {
            hsv = rgbToHsv(parseInt(parts[0]), parseInt(parts[1]), parseInt(parts[2]));
            alpha = parts[3] !== undefined ? parseFloat(parts[3]) : 1;
            updateAll();
            return;
        }
    }
    const rgb = hexToRgb(color);
    if (rgb) {
        hsv = rgbToHsv(rgb.r, rgb.g, rgb.b);
        alpha = 1;
        updateAll();
    }
}

/**
 * 0.23.19：新截图会话把标注色重置为默认红（#FF0000，与预设第一格一致）。
 * 复用窗口时上一会话选择的颜色不应跨会话残留——色盘每次会话首次打开默认红色。
 */
export function resetColorToDefault() {
    hsv = {h: 0, s: 1, v: 1};
    alpha = 1;
    updateAll();
}

/** 初始化色盘模块（幂等，在 bindToolbar 中调用） */
export function initColorPicker() {
    dropdown = document.getElementById('color-dropdown');
    svCanvas = dropdown ? dropdown.querySelector('.sv-canvas') : null;
    hueCanvas = dropdown ? dropdown.querySelector('.hue-bar') : null;
    alphaCanvas = dropdown ? dropdown.querySelector('.alpha-bar') : null;
    colorValueInput = dropdown ? dropdown.querySelector('.color-value') : null;
    colorFormatTrigger = dropdown ? dropdown.querySelector('.color-format-trigger') : null;
    colorFormatLabel = colorFormatTrigger ? colorFormatTrigger.querySelector('.color-format-label') : null;
    colorFormatMenu = dropdown ? dropdown.querySelector('.color-format-menu') : null;
    colorTriggerDot = document.getElementById('color-trigger-dot');

    if (svCanvas) svCtx = svCanvas.getContext('2d');
    if (hueCanvas) hueCtx = hueCanvas.getContext('2d');
    if (alphaCanvas) alphaCtx = alphaCanvas.getContext('2d');

    // SV 色盘交互
    if (svCanvas) {
        svCanvas.addEventListener('mousedown', (e) => {
            e.stopPropagation();
            svDragging = true;
            svFromEvent(e);
        });
        document.addEventListener('mousemove', (e) => {
            if (svDragging) svFromEvent(e);
        });
        document.addEventListener('mouseup', () => {
            svDragging = false;
        });
    }

    // 色相条交互
    if (hueCanvas) {
        hueCanvas.addEventListener('mousedown', (e) => {
            e.stopPropagation();
            hueDragging = true;
            hueFromEvent(e);
        });
        document.addEventListener('mousemove', (e) => {
            if (hueDragging) hueFromEvent(e);
        });
        document.addEventListener('mouseup', () => {
            hueDragging = false;
        });
    }

    // 0.15.8-fix：透明度条交互
    if (alphaCanvas) {
        alphaCanvas.addEventListener('mousedown', (e) => {
            e.stopPropagation();
            alphaDragging = true;
            alphaFromEvent(e);
        });
        document.addEventListener('mousemove', (e) => {
            if (alphaDragging) alphaFromEvent(e);
        });
        document.addEventListener('mouseup', () => {
            alphaDragging = false;
        });
    }

    // 0.23.19：色彩格式自定义下拉（原生 select 弹层多屏定位偏移，见模块头注释）
    if (colorFormatTrigger && colorFormatMenu) {
        const toggleFormatMenu = (open) => {
            colorFormatMenu.setAttribute('data-open', open ? 'true' : 'false');
        };
        colorFormatTrigger.addEventListener('click', (e) => {
            e.stopPropagation();
            toggleFormatMenu(colorFormatMenu.getAttribute('data-open') !== 'true');
        });
        colorFormatTrigger.addEventListener('mousedown', (e) => e.stopPropagation());
        // 0.15.10：滚轮切换色彩格式（HEX→RGB→HSL→HEX）
        colorFormatTrigger.addEventListener('wheel', (e) => {
            e.preventDefault();
            e.stopPropagation();
            const formats = ['hex', 'rgb', 'hsl'];
            let idx = formats.indexOf(format);
            if (idx < 0) idx = 0;
            idx += e.deltaY > 0 ? 1 : -1;
            idx = (idx + formats.length) % formats.length;
            setFormat(formats[idx]);
        }, {passive: false});
        colorFormatMenu.querySelectorAll('.dropdown-item').forEach((item) => {
            item.addEventListener('mousedown', (e) => e.stopPropagation());
            item.addEventListener('click', (e) => {
                e.stopPropagation();
                setFormat(item.dataset.format);
                toggleFormatMenu(false);
            });
        });
        // 点击格式选择器外部时收起菜单（色盘面板内部的其余区域也算外部）
        document.addEventListener('mousedown', (e) => {
            if (colorFormatMenu.getAttribute('data-open') === 'true'
                && !e.target.closest('.color-format-picker')) {
                toggleFormatMenu(false);
            }
        });
    }

    // 数值输入
    if (colorValueInput) {
        // 0.15.8-fix：用 input 事件替代 change，实现即时联动
        colorValueInput.addEventListener('input', (e) => {
            e.stopPropagation();
            parseValueInput();
        });
        colorValueInput.addEventListener('mousedown', (e) => e.stopPropagation());
        colorValueInput.addEventListener('keydown', (e) => {
            e.stopPropagation();
            if (e.key === 'Enter') {
                parseValueInput();
                colorValueInput.blur();
            }
        });
    }

    // 取色器按钮
    const eyedropperBtn = dropdown ? dropdown.querySelector('.eyedropper-btn') : null;
    if (eyedropperBtn) {
        eyedropperBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            enterPickMode();
        });
    }

    // 色相条滚轮（§2.3 滚轮优先铁则）
    if (hueCanvas) {
        hueCanvas.addEventListener('wheel', (e) => {
            e.preventDefault();
            e.stopPropagation();
            hsv.h = (hsv.h + (e.deltaY > 0 ? 2 : -2) + 360) % 360;
            updateAll();
        }, {passive: false});
    }

    // 0.15.8-fix：透明度条滚轮（与色相条方向一致： deltaY > 0 = 增大）
    if (alphaCanvas) {
        alphaCanvas.addEventListener('wheel', (e) => {
            e.preventDefault();
            e.stopPropagation();
            alpha = Math.max(0, Math.min(1, alpha + (e.deltaY > 0 ? 0.05 : -0.05)));
            updateAll();
        }, {passive: false});
    }

    // 色彩预设滚轮切换
    const presetsContainer = dropdown ? dropdown.querySelector('.color-presets') : null;
    if (presetsContainer) {
        presetsContainer.addEventListener('wheel', (e) => {
            e.preventDefault();
            e.stopPropagation();
            const swatches = Array.from(presetsContainer.querySelectorAll('.color-swatch'));
            if (swatches.length < 2) return;
            let curIdx = swatches.findIndex((s) => s.classList.contains('active'));
            if (curIdx < 0) curIdx = 0;
            const newIdx = Math.max(0, Math.min(swatches.length - 1, curIdx + (e.deltaY > 0 ? 1 : -1)));
            if (newIdx !== curIdx) swatches[newIdx].click();
        }, {passive: false});
    }

    // 0.15.6：配色提取模块
    initPalette();

    // 首次渲染
    syncFromAnnot();
}

// 导出转换工具函数（供 0.15.6 配色提取复用）
export {hsvToRgb, rgbToHsv, rgbToHex, hexToRgb, rgbToHsl, hslToRgb};

// P1-3：导出共享色彩格式 getter/setter，配色区复用顶部 .color-format 的格式状态
export function getColorFormat() {
    return format;
}

export function setColorFormat(f) {
    setFormat(f);
}
