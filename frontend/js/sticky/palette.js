/**
 * 便签共享色板模块（0.20.0）。
 *
 * 顶部菜单和右键菜单只消费颜色 id 与 CSS class，不保存第二份 hex。
 * 色板元素复用已有 `.color-swatch` + `.color-{id}` CSS class（sticky.css 定义），
 * 额外的 `.ctx-color-swatch` 仅控制右键菜单中的尺寸/定位，不重复定义颜色。
 *
 * 颜色 id 与后端 StickyColor 枚举、sticky.html 的 data-color 完全一致。
 *
 * 用法：
 *   import { STICKY_COLORS, createSwatch, createSwatchRow } from "../sticky/palette.js";
 *   const row = createSwatchRow({ selectedColor: note.color, onSelect: (id) => applyColor(id) });
 *   menu.appendChild(row);
 */

/**
 * 便签颜色 id 列表——与后端 StickyColor 枚举及 sticky.html 保持一致。
 * @type {string[]}
 */
export const STICKY_COLORS = [
  "theme",
  "yellow",
  "pink",
  "purple",
  "blue",
  "green",
  "gray",
];

/**
 * 创建色板按钮元素。
 *
 * 复用 `.color-swatch` + `.color-{id}` CSS class 获取颜色（不保存第二份 hex），
 * 通过 extraClass 参数追加场景特定 class（如 `.ctx-color-swatch` 用于右键菜单尺寸）。
 *
 * @param {string} colorId - 颜色 id（STICKY_COLORS 中的值）
 * @param {object} [opts]
 * @param {string} [opts.extraClass] - 额外 CSS class（如 "ctx-color-swatch"）
 * @param {boolean} [opts.selected=false] - 是否选中态
 * @param {string} [opts.title] - 鼠标提示（默认用颜色 id）
 * @param {(color: string) => void} [opts.onSelect] - 点击回调
 * @returns {HTMLButtonElement}
 */
export function createSwatch(colorId, opts = {}) {
  const sw = document.createElement("button");
  // 复用已有 color-swatch + color-{id} class，不重新定义颜色
  const classes = ["color-swatch", `color-${colorId}`];
  if (opts.extraClass) classes.push(opts.extraClass);
  sw.className = classes.join(" ");
  sw.dataset.color = colorId;
  sw.title = opts.title ?? colorId;
  if (opts.selected) {
    sw.classList.add("selected");
  }
  if (opts.onSelect) {
    sw.addEventListener("click", (e) => {
      e.stopPropagation();
      opts.onSelect(colorId);
    });
  }
  return sw;
}

/**
 * 创建包含所有色板的行容器。
 *
 * @param {object} [opts]
 * @param {string} [opts.rowClass="ctx-color-row"] - 行容器 CSS class
 * @param {string} [opts.swatchExtraClass="ctx-color-swatch"] - 色板额外 CSS class
 * @param {string} [opts.selectedColor] - 当前选中的颜色 id
 * @param {(color: string) => void} [opts.onSelect] - 色板点击回调
 * @returns {HTMLDivElement}
 */
export function createSwatchRow(opts = {}) {
  const rowClass = opts.rowClass ?? "ctx-color-row";
  const swatchExtraClass = opts.swatchExtraClass ?? "ctx-color-swatch";
  const selectedColor = opts.selectedColor ?? null;

  const row = document.createElement("div");
  row.className = rowClass;

  for (const colorId of STICKY_COLORS) {
    const sw = createSwatch(colorId, {
      extraClass: swatchExtraClass,
      selected: colorId === selectedColor,
      onSelect: opts.onSelect,
    });
    row.appendChild(sw);
  }

  return row;
}
