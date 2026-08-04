/**
 * 共享 Markdown 渲染模块（0.17.6）。
 *
 * 从 chat/renderer.js 抽取，统一 Markdown 解析、净化与渲染。
 * 对话窗口、内容编辑器（0.16.3）和 0.16.8 便签窗口使用同一入口。
 *
 * 0.17.6: marked + DOMPurify + highlight.js 替换为 Cherry Markdown Stream。
 *
 * 依赖全局对象（各窗口 HTML 中通过 <script> 加载 vendor 脚本）：
 * - Cherry Markdown Stream 版：`window.Cherry`（renderMarkdown / createStreamRenderer / createMarkdownEditor）
 * - Cherry CSS：cherry-markdown.min.css（样式）
 *
 * 无 bundler 铁则：不 import vendor 脚本，通过 window.* 访问。
 */

/** @type {boolean} Cherry Markdown 是否可用 */
let ready = false;

/**
 * 初始化渲染器。检查 Cherry vendor 全局对象是否存在。
 * 各窗口入口在 DOM ready 后调用一次。
 */
export function initMarkdown() {
  ready = typeof window.Cherry !== "undefined";
  if (!ready) {
    console.warn("[shared/markdown] Cherry Markdown 未加载，降级为纯文本渲染");
    return;
  }
}

/**
 * 检查渲染器是否就绪（Cherry 可用）。
 * @returns {boolean}
 */
export function isReady() {
  return ready;
}

/**
 * 安全渲染 Markdown 文本为 HTML。
 *
 * 使用 Cherry Markdown Stream 版的 `renderMarkdown` 方法。
 * Cherry 已内置 XSS 防护 + 代码高亮，无需额外处理。
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
    const html = window.Cherry.renderMarkdown(text);
    // Cherry 已内置 link target="_blank" 处理，但为确保兼容性，仍添加
    const withBlankTarget = html.replace(/<a\s+href=/g, '<a target="_blank" href=');
    if (opts?.container) {
      opts.container.innerHTML = withBlankTarget;
      return;
    }
    return withBlankTarget;
  } catch (e) {
    console.error("[shared/markdown] Cherry Markdown 渲染失败，降级纯文本:", e);
    const html = escapeHtml(text);
    if (opts?.container) {
      opts.container.innerHTML = html;
      return;
    }
    return html;
  }
}

/**
 * 创建流式渲染实例（0.17.6）。
 *
 * Cherry Stream 版的 `createStreamRenderer` 返回带 `write(text)` 方法的实例，
 * 自动补全未闭合 MD 片段，解决 marked 全量解析导致的闪烁问题。
 *
 * @param {HTMLElement} container 容器元素
 * @returns {{ write(text: string): void }} 流式渲染实例
 */
export function renderMarkdownStream(container) {
  if (!ready || !container) {
    console.warn("[shared/markdown] Cherry Markdown 未就绪，流式渲染降级");
    // 返回一个简单的流式渲染器（回退到纯文本）
    let accumulated = "";
    return {
      write(text) {
        accumulated += text;
        container.innerHTML = escapeHtml(accumulated);
      },
    };
  }
  try {
    return window.Cherry.createStreamRenderer(container);
  } catch (e) {
    console.error("[shared/markdown] Cherry 流式渲染器创建失败:", e);
    return {
      write(text) {
        // 降级：直接显示为纯文本
        const textNode = document.createTextNode(text);
        container.appendChild(textNode);
        container.appendChild(document.createElement("br"));
      },
    };
  }
}

/**
 * 创建 Markdown 编辑器（Live Preview）（0.17.7a 预留）。
 *
 * Cherry 提供 `edit&preview` 模式：左侧编辑 MD 源文本 + 右侧实时渲染预览。
 * 此方法为 0.17.7a Live Preview 编辑器预留，0.17.6 暂不使用。
 *
 * @param {HTMLElement} element 编辑器容器元素
 * @param {{ theme?: string, defaultText?: string }} [opts] 可选配置
 * @returns {{ getMarkdown(): string, destroy(): void }} 编辑器实例
 */
export function createMarkdownEditor(element, opts = {}) {
  if (!ready) {
    console.warn("[shared/markdown] Cherry Markdown 未就绪，无法创建编辑器");
    return null;
  }
  try {
    return window.Cherry.createMarkdownEditor(element, opts);
  } catch (e) {
    console.error("[shared/markdown] Cherry 编辑器创建失败:", e);
    return null;
  }
}

/**
 * 对容器内所有 `pre code` 执行语法高亮（0.17.6：Cherry 内置高亮，此函数废弃）。
 *
 * Cherry Markdown 内置代码高亮，无需手动调用。
 * 保留此函数仅为兼容性（现有调用点不会崩溃）。
 *
 * @param {HTMLElement} container 包含 pre code 的容器元素
 * @deprecated Cherry Markdown 内置代码高亮，无需手动调用
 */
export function highlightCodeBlocks(container) {
  // Cherry Markdown 已内置代码高亮，此函数废弃但保留以兼容
  console.debug("[shared/markdown] highlightCodeBlocks: Cherry Markdown 内置高亮，无需手动调用");
}

/**
 * 纯文本转义（降级用）。转义 HTML 特殊字符 + 保留换行 + <br>。
 * @param {string} text
 * @returns {string}
 */
function escapeHtml(text) {
  const div = document.createElement("div");
  div.textContent = String(text ?? "");
  return div.innerHTML.replace(/\n/g, "<br>");
}