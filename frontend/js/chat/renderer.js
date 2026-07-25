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

/** @type {boolean} highlight.js 是否可用（0.12.5 §5.7） */
let hljsReady = false;

/**
 * 初始化渲染器。检查 vendor 全局对象是否存在。
 */
export function initRenderer() {
  ready = typeof window.marked !== "undefined" && typeof window.DOMPurify !== "undefined";
  if (!ready) {
    console.warn("[chat/renderer] marked 或 DOMPurify 未加载，降级为纯文本渲染");
    return;
  }
  hljsReady = typeof window.hljs !== "undefined";
  if (!hljsReady) {
    console.warn("[chat/renderer] highlight.js 未加载，代码块不语法高亮");
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
 * 对容器内所有 `pre code` 执行语法高亮（0.12.5 §5.7）。
 *
 * 在 finalizeAssistantMessage 后调用——流式渲染中不高亮（代码不完整，性能开销大）。
 * highlight.js 直接操作 DOM（添加 span.hljs-* class），不经过 DOMPurify——
 * hljs 只在已 sanitize 的 DOM 上添加语义 class，安全。
 *
 * @param {HTMLElement} container 包含 pre code 的容器元素
 */
export function highlightCodeBlocks(container) {
  if (!hljsReady || !container) return;
  container.querySelectorAll("pre code").forEach((code) => {
    // 避免重复高亮（流式 finalize + 历史消息复用同一 DOM 可能多次调）
    if (code.dataset.highlighted) return;
    try {
      window.hljs.highlightElement(code);
      code.dataset.highlighted = "yes";
    } catch {
      // 忽略单块高亮失败（语言未注册等）
    }
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
