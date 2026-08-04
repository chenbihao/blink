/**
 * 共享 Markdown 渲染模块（0.17.6 / 0.17.7a）。
 *
 * 从 chat/renderer.js 抽取，统一 Markdown 解析、净化与渲染。
 * 对话窗口、内容编辑器（0.16.3）和便签窗口使用同一入口。
 *
 * 0.17.6: marked + DOMPurify + highlight.js 替换为 Cherry Markdown Stream。
 * 0.17.7a: createMarkdownEditor 使用自定义 textarea + Cherry Stream 预览方案
 *          （Stream 版无 Editor/Toolbar 组件，用自定义工具栏 + textarea + 实时预览替代）。
 *
 * 依赖全局对象（各窗口 HTML 中通过 <script> 加载 vendor 脚本）：
 * - Cherry Markdown Stream 版：`window.Cherry`（CherryStream 构造函数）
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
  // Cherry Stream 版 UMD 导出：window.Cherry = CherryStream 构造函数
  ready = typeof window.Cherry === "function";
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
 * 使用 Cherry Markdown Stream 版实例的 `setValue` + `getHtml` 方法。
 * Cherry 已内置 XSS 防护 + 代码高亮，无需额外处理。
 *
 * @param {string} text 原始 Markdown 文本
 * @param {{ container?: HTMLElement }} [opts] 可选配置
 *   - container：传入则直接渲染到 container，省略则返回 HTML 字符串
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
    if (opts?.container) {
      // 容器模式：创建 CherryStream 实例挂载到容器，渲染后销毁
      const tempId = `cherry-render-${Date.now()}`;
      opts.container.id = opts.container.id || tempId;
      const instance = new window.Cherry({
        id: opts.container.id,
        value: text,
      });
      // CherryStream 渲染后内容已在容器中，销毁实例释放资源
      // 但销毁会清空容器，所以这里不销毁，让内容保留
      // 实际上 getHtml 返回的是 HTML 字符串，我们直接用 innerHTML
      const html = instance.getHtml(false);
      instance.destroy();
      opts.container.innerHTML = addBlankTarget(html);
      return;
    }
    // 字符串模式：创建临时容器渲染，取 HTML 后清理
    const tempDiv = document.createElement("div");
    tempDiv.id = `cherry-temp-${Date.now()}`;
    tempDiv.style.display = "none";
    document.body.appendChild(tempDiv);
    const instance = new window.Cherry({
      id: tempDiv.id,
      value: text,
    });
    const html = instance.getHtml(false);
    instance.destroy();
    tempDiv.remove();
    return addBlankTarget(html);
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
 * Cherry Stream 版实例的 `setValue` 方法做增量渲染，
 * 自动补全未闭合 MD 片段，解决 marked 全量解析导致的闪烁问题。
 *
 * @param {HTMLElement} container 容器元素
 * @returns {{ write(text: string): void, destroy(): void }} 流式渲染实例
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
      destroy() {},
    };
  }
  try {
    // 确保容器有 id
    if (!container.id) {
      container.id = `cherry-stream-${Date.now()}`;
    }
    let instance = null;
    let lastText = "";

    // 创建 CherryStream 实例（延迟创建，首次 write 时初始化）
    function ensureInstance() {
      if (!instance) {
        instance = new window.Cherry({
          id: container.id,
          value: "",
        });
      }
    }

    return {
      write(text) {
        try {
          ensureInstance();
          lastText = text;
          instance.setValue(text);
        } catch (e) {
          console.error("[shared/markdown] Cherry 流式渲染 write 失败:", e);
          container.innerHTML = escapeHtml(text);
        }
      },
      destroy() {
        if (instance) {
          try {
            instance.destroy();
          } catch {
            // ignore
          }
          instance = null;
        }
      },
    };
  } catch (e) {
    console.error("[shared/markdown] Cherry 流式渲染器创建失败:", e);
    return {
      write(text) {
        container.innerHTML = escapeHtml(text);
      },
      destroy() {},
    };
  }
}

// ── Markdown 编辑器（自定义 Live Preview）──────────────────────
//
// Cherry Stream 版不含 Editor/Toolbar 组件，因此用自定义方案实现 edit&preview：
// - 左侧 <textarea> 编辑 Markdown 源文本
// - 右侧 Cherry Stream 实例实时渲染预览
// - 自定义工具栏按钮（compact / full）在 textarea 光标处插入 MD 语法
// - 存储仍为 Markdown 文本（textarea.value）

/** 工具栏按钮定义 */
const TOOLBAR_DEFS = {
  bold: { title: "粗体", icon: "B", wrap: ["**", "**"] },
  italic: { title: "斜体", icon: "I", wrap: ["*", "*"] },
  strikethrough: { title: "删除线", icon: "S", wrap: ["~~", "~~"] },
  h1: { title: "标题1", icon: "H1", prefix: "# " },
  h2: { title: "标题2", icon: "H2", prefix: "## " },
  ul: { title: "无序列表", icon: "•", prefix: "- " },
  ol: { title: "有序列表", icon: "1.", prefix: "1. " },
  checklist: { title: "任务列表", icon: "☐", prefix: "- [ ] " },
  code: { title: "代码块", icon: "</>", wrap: ["\n```\n", "\n```\n"] },
  insertcode: { title: "行内代码", icon: "`", wrap: ["`", "`"] },
  link: { title: "链接", icon: "🔗", wrap: ["[", "](url)"] },
  quote: { title: "引用", icon: "❝", prefix: "> " },
  table: { title: "表格", icon: "▦", insert: "\n| 列1 | 列2 | 列3 |\n|---|---|---|\n| | | |\n" },
};

/** 工具栏预设 */
const TOOLBAR_PRESETS = {
  compact: ["bold", "italic", "strikethrough", "|", "ul", "ol", "checklist", "|", "code"],
  full: [
    "bold", "italic", "strikethrough", "|",
    "h1", "h2", "|",
    "ul", "ol", "checklist", "|",
    "code", "insertcode", "link", "quote", "table",
  ],
};

/**
 * 创建 Markdown 编辑器（Live Preview）（0.17.7a）。
 *
 * 自定义 edit&preview 方案：左侧 textarea + 右侧 Cherry Stream 预览 + 自定义工具栏。
 * 存储仍为 Markdown 文本（不做 HTML↔MD 转换）。
 *
 * @param {HTMLElement} element 编辑器容器元素
 * @param {{ defaultText?: string, toolbar?: string, onChange?: (md: string) => void }} [opts]
 *   - defaultText：初始 Markdown 文本
 *   - toolbar：'compact' | 'full'（默认 compact）
 *   - onChange：内容变更回调
 * @returns {{ getMarkdown(): string, setMarkdown(text: string): void, destroy(): void } | null} 编辑器实例
 */
export function createMarkdownEditor(element, opts = {}) {
  if (!ready) {
    console.warn("[shared/markdown] Cherry Markdown 未就绪，无法创建编辑器");
    return null;
  }
  if (!element) {
    console.warn("[shared/markdown] createMarkdownEditor: element 为 null");
    return null;
  }

  try {
    const preset = typeof opts.toolbar === "string"
      ? (TOOLBAR_PRESETS[opts.toolbar] || TOOLBAR_PRESETS.compact)
      : (opts.toolbar || TOOLBAR_PRESETS.compact);

    // 构建 DOM 结构
    element.innerHTML = "";
    element.classList.add("md-editor");

    // 工具栏
    const toolbarEl = document.createElement("div");
    toolbarEl.className = "md-editor-toolbar";
    for (const key of preset) {
      if (key === "|") {
        const sep = document.createElement("span");
        sep.className = "md-editor-toolbar-sep";
        toolbarEl.appendChild(sep);
        continue;
      }
      const def = TOOLBAR_DEFS[key];
      if (!def) continue;
      const btn = document.createElement("button");
      btn.className = "md-editor-toolbar-btn";
      btn.type = "button";
      btn.title = def.title;
      btn.textContent = def.icon;
      btn.addEventListener("mousedown", (e) => e.preventDefault()); // 不抢焦点
      btn.addEventListener("click", () => applyToolbarAction(def));
      toolbarEl.appendChild(btn);
    }

    // 编辑区 + 预览区
    const bodyEl = document.createElement("div");
    bodyEl.className = "md-editor-body";

    const textareaEl = document.createElement("textarea");
    textareaEl.className = "md-editor-textarea";
    textareaEl.spellcheck = false;
    textareaEl.placeholder = "输入 Markdown 内容...";
    textareaEl.value = opts.defaultText || "";

    const previewEl = document.createElement("div");
    previewEl.className = "md-editor-preview cherry-markdown";
    if (!previewEl.id) {
      previewEl.id = `cherry-preview-${Date.now()}`;
    }

    bodyEl.appendChild(textareaEl);
    bodyEl.appendChild(previewEl);

    element.appendChild(toolbarEl);
    element.appendChild(bodyEl);

    // 创建 Cherry Stream 预览实例
    let cherryInstance = null;
    try {
      cherryInstance = new window.Cherry({
        id: previewEl.id,
        value: textareaEl.value,
      });
    } catch (e) {
      console.error("[shared/markdown] Cherry 预览实例创建失败，预览降级:", e);
    }

    // 防抖更新预览
    let previewTimer = null;
    function updatePreview() {
      if (previewTimer) clearTimeout(previewTimer);
      previewTimer = setTimeout(() => {
        if (cherryInstance) {
          try {
            cherryInstance.setValue(textareaEl.value);
          } catch (e) {
            console.error("[shared/markdown] 预览更新失败:", e);
          }
        }
      }, 200);
    }

    // textarea 输入事件
    textareaEl.addEventListener("input", () => {
      updatePreview();
      if (opts.onChange) opts.onChange(textareaEl.value);
    });

    // 工具栏操作：在 textarea 光标处插入/包裹 Markdown 语法
    function applyToolbarAction(def) {
      const start = textareaEl.selectionStart;
      const end = textareaEl.selectionEnd;
      const value = textareaEl.value;
      const selected = value.substring(start, end);

      if (def.insert) {
        // 插入固定文本（表格）
        const newValue = value.substring(0, end) + def.insert + value.substring(end);
        textareaEl.value = newValue;
        const newCursor = end + def.insert.length;
        textareaEl.setSelectionRange(newCursor, newCursor);
      } else if (def.wrap) {
        // 包裹选中文本
        const newValue = value.substring(0, start) + def.wrap[0] + selected + def.wrap[1] + value.substring(end);
        textareaEl.value = newValue;
        const newStart = start + def.wrap[0].length;
        const newEnd = end + def.wrap[0].length;
        textareaEl.setSelectionRange(newStart, newEnd);
      } else if (def.prefix) {
        // 行前缀（列表/标题/引用）：在每行行首添加前缀
        const lineStart = value.lastIndexOf("\n", start - 1) + 1;
        const beforeLine = value.substring(0, lineStart);
        const afterLine = value.substring(lineStart);
        const newValue = beforeLine + def.prefix + afterLine;
        textareaEl.value = newValue;
        const newCursor = start + def.prefix.length;
        textareaEl.setSelectionRange(newCursor, newCursor);
      }

      textareaEl.focus();
      updatePreview();
      if (opts.onChange) opts.onChange(textareaEl.value);
    }

    // 初始预览
    updatePreview();

    // 返回统一接口
    return {
      /** 获取当前 Markdown 文本 */
      getMarkdown() {
        return textareaEl.value;
      },
      /** 设置 Markdown 文本 */
      setMarkdown(text) {
        textareaEl.value = text || "";
        updatePreview();
        if (opts.onChange) opts.onChange(textareaEl.value);
      },
      /** 聚焦编辑器 */
      focus() {
        textareaEl.focus();
      },
      /** 销毁编辑器实例 */
      destroy() {
        if (previewTimer) {
          clearTimeout(previewTimer);
          previewTimer = null;
        }
        if (cherryInstance) {
          try {
            cherryInstance.destroy();
          } catch {
            // ignore
          }
          cherryInstance = null;
        }
        element.innerHTML = "";
        element.classList.remove("md-editor");
      },
      /** 获取底层 textarea 元素（高级操作） */
      _textarea: textareaEl,
      _preview: previewEl,
    };
  } catch (e) {
    console.error("[shared/markdown] Markdown 编辑器创建失败:", e);
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
}

/**
 * 给 HTML 中的 <a> 标签添加 target="_blank"。
 * @param {string} html
 * @returns {string}
 */
function addBlankTarget(html) {
  return html.replace(/<a\s+href=/g, '<a target="_blank" href=');
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
