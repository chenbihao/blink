/**
 * 共享 Markdown 渲染模块（0.16.2）。
 *
 * 从 chat/renderer.js 抽取，统一 Markdown 解析、净化与渲染。
 * 对话窗口、内容编辑器（0.16.3）和 0.17 便签窗口使用同一入口。
 *
 * 依赖全局对象（各窗口 HTML 中通过 <script> 加载 vendor 脚本）：
 * - marked：GFM Markdown 解析
 * - DOMPurify：HTML 净化（XSS 防护）
 * - highlight.js（可选）：代码块语法高亮
 *
 * 无 bundler 铁则：不 import vendor 脚本，通过 window.* 访问。
 * 主题适配：只产语义 class（hljs-* / markdown-body），不硬编码颜色。
 */

/** @type {boolean} marked 和 DOMPurify 是否可用 */
let ready = false;

/** @type {boolean} highlight.js 是否可用 */
let hljsReady = false;

/**
 * 初始化渲染器。检查 vendor 全局对象是否存在，配置 marked。
 * 各窗口入口在 DOM ready 后调用一次。
 */
export function initMarkdown() {
  ready = typeof window.marked !== "undefined" && typeof window.DOMPurify !== "undefined";
  if (!ready) {
    console.warn("[shared/markdown] marked 或 DOMPurify 未加载，降级为纯文本渲染");
    return;
  }
  hljsReady = typeof window.hljs !== "undefined";
  if (!hljsReady) {
    console.warn("[shared/markdown] highlight.js 未加载，代码块不语法高亮");
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
 * 检查渲染器是否就绪（marked/DOMPurify 可用）。
 * @returns {boolean}
 */
export function isReady() {
  return ready;
}

/**
 * 安全渲染 Markdown 文本为 HTML。
 *
 * 所有 `<a>` 标签自动添加 `target="_blank" rel="noopener noreferrer"`，
 * 防止 Tauri WebView 内部导航导致应用崩溃。
 *
 * @param {string} text 原始 Markdown 文本
 * @param {{ container?: HTMLElement }} [opts] 可选配置
 *   - container：传入则直接写入 container.innerHTML，省略则返回 HTML 字符串
 * @returns {string|void} 安全的 HTML 字符串（未传 container 时），或 void（传了 container 时）
 */
export function renderMarkdown(text, opts) {
  if (!ready || !text) {
    const html = escapeHtml(text || "");
    if (opts?.container) {
      opts.container.innerHTML = html;
      return;
    }
    return html;
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
    if (opts?.container) {
      opts.container.innerHTML = html;
      return;
    }
    return html;
  } catch (e) {
    console.error("[shared/markdown] Markdown 渲染失败，降级纯文本:", e);
    const html = escapeHtml(text);
    if (opts?.container) {
      opts.container.innerHTML = html;
      return;
    }
    return html;
  }
}

/**
 * 对容器内所有 `pre code` 执行语法高亮。
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
 * 纯文本转义（降级用）。转义 HTML 特殊字符 + 保留换行→<br>。
 * @param {string} text
 * @returns {string}
 */
function escapeHtml(text) {
  const div = document.createElement("div");
  div.textContent = String(text ?? "");
  return div.innerHTML.replace(/\n/g, "<br>");
}
