//! 0.20.7：配色格式化纯函数模块（单一真源）。
//!
//! 从 ss-palette.js 提取的纯函数，供 ss-palette.js 和测试共同 import，
//! 消除测试中复制代码的维护风险。
//!
//! **不含**：色彩运算算法（OKLCH/HSL/聚类等），所有配色计算在后端 Rust 完成。
//! **只含**：格式化输出函数（HEX/RGB/HSL 字符串、CSS 变量声明等）。

/**
 * RGB → HEX 字符串（#RRGGBB，大写）。
 * @param {number} r
 * @param {number} g
 * @param {number} b
 * @returns {string}
 */
export function rgbToHex(r, g, b) {
  const h = (v) => Math.max(0, Math.min(255, Math.round(v))).toString(16).padStart(2, '0').toUpperCase();
  return `#${h(r)}${h(g)}${h(b)}`;
}

/**
 * 将 HEX 颜色转为 HSL 展示字符串（仅用于输出格式化，不参与配色运算）。
 *
 * @param {string} hex - #RRGGBB
 * @returns {string} "hsl(H, S%, L%)"
 */
export function hexToHslString(hex) {
  const r = parseInt(hex.slice(1, 3), 16) / 255;
  const g = parseInt(hex.slice(3, 5), 16) / 255;
  const b = parseInt(hex.slice(5, 7), 16) / 255;
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const delta = max - min;
  const l = (max + min) / 2;
  let s = 0, h = 0;
  if (delta !== 0) {
    s = l > 0.5 ? delta / (2 - max - min) : delta / (max + min);
    if (max === r) h = ((g - b) / delta) % 6;
    else if (max === g) h = (b - r) / delta + 2;
    else h = (r - g) / delta + 4;
    h *= 60;
    if (h < 0) h += 360;
  }
  return `hsl(${Math.round(h)}, ${Math.round(s * 100)}%, ${Math.round(l * 100)}%)`;
}

/**
 * 将选中的颜色列表（HEX 数组）格式化为指定输出格式。
 *
 * @param {string[]} hexColors
 * @param {'hex'|'rgb'|'hsl'|'list'} format
 * @returns {string}
 */
export function formatOutput(hexColors, format) {
  if (format === 'list' || format === 'hex') {
    return hexColors.join('\n');
  }
  if (format === 'rgb') {
    return hexColors.map((hex) => {
      const r = parseInt(hex.slice(1, 3), 16);
      const g = parseInt(hex.slice(3, 5), 16);
      const b = parseInt(hex.slice(5, 7), 16);
      return `rgb(${r}, ${g}, ${b})`;
    }).join('\n');
  }
  if (format === 'hsl') {
    return hexColors.map((hex) => hexToHslString(hex)).join('\n');
  }
  return hexColors.join('\n');
}

/**
 * 将角色色列表输出为 CSS variables。
 * 变量名按角色稳定生成，如 --blink-color-background。
 *
 * @param {Array} roles
 * @returns {string} CSS variable 声明
 */
export function formatAsCssVariables(roles) {
  const roleCounts = new Map();
  return roles.map((r) => {
    const count = (roleCounts.get(r.role) || 0) + 1;
    roleCounts.set(r.role, count);
    const suffix = count === 1 ? '' : `-${count}`;
    return `  --blink-color-${r.role}${suffix}: ${r.hex};`;
  }).join('\n');
}

/**
 * 格式化选色输出。支持 hex/rgb/hsl/list/css 模式。
 *
 * @param {string[]} hexColors
 * @param {'auto'|'list'|'css'} mode - 'auto' = 复用顶部格式选择
 * @param {Function} getColorFormat - 返回当前共享格式 ('hex'|'rgb'|'hsl')
 * @param {Array} [paletteRoles] - 角色色数组（css 模式用）
 * @returns {string}
 */
export function formatPaletteColors(hexColors, mode, getColorFormat, paletteRoles) {
  const fmt = mode || 'auto';
  if (fmt === 'list') return hexColors.join('\n');
  if (fmt === 'css') {
    const roleByHex = new Map((paletteRoles || []).map((role) => [role.hex, role]));
    const cssRoles = hexColors.map((hex, index) => (
      roleByHex.get(hex) || { hex, role: `selected-${index + 1}` }
    ));
    return formatAsCssVariables(cssRoles);
  }
  const actualFormat = fmt === 'auto' ? (getColorFormat ? getColorFormat() : 'hex') : fmt;
  return formatOutput(hexColors, actualFormat);
}
