/**
 * chat 模块共享工具函数（0.12.8）。
 *
 * 消除 escapeText / escapeAttr 在 main.js / sidebar.js / components.js 中的三处重复。
 */

/** HTML 文本转义（防 XSS）。 */
export function escapeText(text) {
  const div = document.createElement("div");
  div.textContent = String(text ?? "");
  return div.innerHTML;
}

/** 属性转义（用于 data-* 和 title 属性，防 XSS）。 */
export function escapeAttr(text) {
  return String(text ?? "").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
