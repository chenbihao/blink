/**
 * Markdown 渲染器（0.12.1 Phase 5）。
 *
 * 使用 marked（GFM）+ DOMPurify（sanitizer）。
 * 模型输出视为不可信内容：禁 raw HTML/script/img/on*；link 协议白名单。
 * renderer 初始化或解析失败时降级到 textContent + pre-wrap。
 */

/* global marked, DOMPurify */

/** @type {boolean} marked 和 DOMPurify 是否可用 */
let ready = false;

/**
 * 初始化渲染器。检查 vendor 全局对象是否存在。
 */
export function initRenderer() {
  ready = typeof window.marked !== "undefined" && typeof window.DOMPurify !== "undefined";
  if (!ready) {
    console.warn("[chat/renderer] marked 或 DOMPurify 未加载，降级为纯文本渲染");
    return;
  }
  // 配置 marked：启用 GFM，禁用 header IDs
  window.marked.setOptions({
    gfm: true,
    breaks: true,
    headerIds: false,
    mangle: false,
  });
}

/**
 * 安全渲染 Markdown 文本为 HTML。
 * @param {string} text 原始 Markdown 文本
 * @returns {string} 安全的 HTML 字符串
 */
export function renderMarkdown(text) {
  if (!ready || !text) {
    return escapeHtml(text || "");
  }
  try {
    const rawHtml = window.marked.parse(text);
    // DOMPurify sanitize：禁 script/img/on*，link 协议白名单
    return window.DOMPurify.sanitize(rawHtml, {
      ALLOWED_TAGS: [
        "p", "br", "strong", "em", "del", "code", "pre", "blockquote",
        "ul", "ol", "li", "a", "table", "thead", "tbody", "tr", "th", "td",
        "hr", "h1", "h2", "h3", "h4", "h5", "h6", "details", "summary",
      ],
      ALLOWED_ATTR: ["href", "title", "target", "rel"],
      ALLOW_DATA_ATTR: false,
      // 协议白名单
      ALLOWED_URI_REGEXP: /^(?:(?:https?|mailto):)/i,
    });
  } catch (e) {
    console.error("[chat/renderer] Markdown 渲染失败，降级纯文本:", e);
    return escapeHtml(text);
  }
}

/**
 * 纯文本转义（降级用）。
 * @param {string} text
 * @returns {string}
 */
function escapeHtml(text) {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML.replace(/\n/g, "<br>");
}
