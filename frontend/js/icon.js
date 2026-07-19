//! 图标渲染工具（0.10.8 §11.2 方案 3）。
//!
//! **来源**：Lucide 图标包，通过 `scripts/fetch-lucide-icons.py` 一次性打包成
//! `frontend/assets/icons/sprite.svg`，运行期零网络依赖。
//!
//! **消费**：
//!   `import { renderIcon, iconHTML } from "./icon.js";`
//!   `container.appendChild(renderIcon("settings"));`
//!   或模板拼接：`el.innerHTML = `<div>${iconHTML("folder")} 打开路径</div>`;`
//!
//! **样式**：`.icon { width/height: 1em; stroke: currentColor; fill: none; ... }`，
//! 尺寸由容器 font-size 控制，颜色跟随 CSS `color`——暗/亮主题自动适配。
//! 详见 `css/components/icon.css`。
//!
//! **初始化**：`ensureSpriteLoaded()` 由 main.js / settings-main.js 启动时调用一次，
//! 拉 sprite.svg 并注入 `<body>` 首元素（display:none 但 `<use>` 可引用）。
//! 幂等：多窗口 / 多入口重复调用只加载一次（Promise 缓存）。

const SPRITE_URL = "assets/icons/sprite.svg";
let spritePromise = null;

/**
 * 确保 SVG sprite 已注入 DOM。返回 Promise 用于 `await` 保证首屏无 FOUC。
 *
 * **调用时机**：main.js / settings-main.js 首行 `await ensureSpriteLoaded();`
 * 后再挂载视图。sprite 加载失败降级为无图标（`<use>` 引用不到 symbol 时 <svg> 显示为空
 * 方框，不影响 layout）。
 */
export function ensureSpriteLoaded() {
  if (spritePromise) return spritePromise;
  spritePromise = fetch(SPRITE_URL)
    .then((r) => {
      if (!r.ok) throw new Error(`icon sprite fetch failed: HTTP ${r.status}`);
      return r.text();
    })
    .then((text) => {
      // 首元素注入：<use xlink:href="#icon-xxx"> 在同 document 内解析，
      // sprite 出现在 <body> 首位即可。display:none 已在 sprite 属性内。
      document.body.insertAdjacentHTML("afterbegin", text);
    })
    .catch((e) => {
      console.warn("[icon] sprite 加载失败，图标将为空：", e);
      // 不 rethrow —— 图标缺失不该阻塞主流程
    });
  return spritePromise;
}

/**
 * 生成图标 DOM 节点。
 * @param {string} name  Lucide 图标名（如 "settings" / "folder"），与 sprite 内 symbol id 对应
 * @param {{ariaLabel?: string, extraClass?: string}} [opts]
 * @returns {SVGElement}
 */
export function renderIcon(name, opts = {}) {
  const NS = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(NS, "svg");
  svg.setAttribute("class", opts.extraClass ? `icon ${opts.extraClass}` : "icon");
  // aria-hidden 是无障碍缺省态：图标是装饰性 / 标签仍走文本节点。
  // 需要独立含义时用 ariaLabel 明确覆盖。
  if (opts.ariaLabel) {
    svg.setAttribute("role", "img");
    svg.setAttribute("aria-label", opts.ariaLabel);
  } else {
    svg.setAttribute("aria-hidden", "true");
  }
  const use = document.createElementNS(NS, "use");
  // href 是 SVG2 标准；旧引擎（WebView2 一直 Chromium，可放心用）
  use.setAttribute("href", `#icon-${name}`);
  svg.appendChild(use);
  return svg;
}

/**
 * 图标 HTML 字符串（模板拼接用）。
 * 注意：模板拼接场景下调用方需自行确保 sprite 已注入（`await ensureSpriteLoaded()`）。
 * @param {string} name
 * @param {{ariaLabel?: string, extraClass?: string}} [opts]
 * @returns {string}
 */
export function iconHTML(name, opts = {}) {
  const cls = opts.extraClass ? `icon ${opts.extraClass}` : "icon";
  const a11y = opts.ariaLabel
    ? ` role="img" aria-label="${escapeAttr(opts.ariaLabel)}"`
    : ' aria-hidden="true"';
  return `<svg class="${cls}"${a11y}><use href="#icon-${escapeAttr(name)}"/></svg>`;
}

function escapeAttr(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  }[c]));
}
