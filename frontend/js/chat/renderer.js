/**
 * Markdown 渲染器（0.12.1 Phase 5）。
 *
 * 使用 marked（GFM）+ DOMPurify（sanitizer）。
 * 模型输出视为不可信内容：禁 raw HTML/script/img/on*；link 协议白名单。
 * renderer 初始化或解析失败时降级到 textContent + pre-wrap。
 */

/* global marked, DOMPurify */

import { escapeText } from "./utils.js";
import { invoke } from "../shared/tauri.js";

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
 *
 * 所有 `<a>` 标签自动添加 `target="_blank" rel="noopener noreferrer"`，
 * 并通过全局点击委托在外部浏览器打开（Tauri WebView 内部导航会导致应用崩溃）。
 *
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
    let html = window.DOMPurify.sanitize(rawHtml, {
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
    // 确保所有 <a> 在外部打开（Tauri WebView 内部导航会崩溃）
    html = html.replace(/<a\s/g, '<a target="_blank" rel="noopener noreferrer" ');
    return html;
  } catch (e) {
    console.error("[chat/renderer] Markdown 渲染失败，降级纯文本:", e);
    return escapeHtml(text);
  }
}

/**
 * 绑定全局链接点击委托——拦截 chat-messages 内的 <a> 点击，
 * 通过后端 `open_url` command 在外部浏览器打开，防止 WebView 内部导航导致应用崩溃。
 *
 * 在 main.js init() 中调用一次。
 */
export function bindLinkOpener() {
  const messagesEl = document.getElementById("chat-messages");
  if (!messagesEl) return;
  messagesEl.addEventListener("click", (e) => {
    const link = e.target.closest("a[href]");
    if (!link) return;
    const href = link.getAttribute("href") || "";
    // 只拦截 http/https/mailto（与 DOMPurify 白名单一致）
    if (!/^(?:https?|mailto):/i.test(href)) return;
    e.preventDefault();
    // 用后端 open_url command（与设置页 openExternalUrl 一致）
    if (invoke) {
      invoke("open_url", { url: href }).catch((err) => {
        console.error("[chat] open_url 失败:", err);
        // 降级：window.open（在 Tauri 中可能无效，但不会崩溃）
        window.open(href, "_blank");
      });
    } else {
      window.open(href, "_blank");
    }
  });
}

/**
 * 纯文本转义（降级用）。复用 utils.escapeText + 保留换行→<br>。
 * @param {string} text
 * @returns {string}
 */
function escapeHtml(text) {
  return escapeText(text).replace(/\n/g, "<br>");
}
