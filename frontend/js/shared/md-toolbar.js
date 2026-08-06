/**
 * 共享 MD 工具栏模块（0.18.3）。
 *
 * 便签窗口和内容编辑器窗口共用同一套 Tiptap MD 格式工具栏。
 * 无 bundler，纯 ES module import。
 *
 * 用法：
 *   import { createMdToolbar, bindMdToolbar, updateToolbarStates } from "../shared/md-toolbar.js";
 *   const toolbar = createMdToolbar();        // 创建 DOM
 *   container.appendChild(toolbar);
 *   bindMdToolbar(toolbar, tiptapEditor, { editorEl });  // 绑定事件
 *   // 编辑器 selectionUpdate / transaction 时调用：
 *   updateToolbarStates(toolbar, tiptapEditor);
 */

// ── SVG 图标 ──────────────────────────────────────────

const ICONS = {
  bulletList: `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2"><line x1="8" y1="6" x2="21" y2="6"/><line x1="8" y1="12" x2="21" y2="12"/><line x1="8" y1="18" x2="21" y2="18"/><line x1="3" y1="6" x2="3.01" y2="6"/><line x1="3" y1="12" x2="3.01" y2="12"/><line x1="3" y1="18" x2="3.01" y2="18"/></svg>`,
  orderedList: `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2"><line x1="10" y1="6" x2="21" y2="6"/><line x1="10" y1="12" x2="21" y2="12"/><line x1="10" y1="18" x2="21" y2="18"/><path d="M4 6h1v4"/><path d="M4 10h2"/><path d="M6 18H4c0-1 2-2 2-3s-1-1.5-2-1"/></svg>`,
  taskList: `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2"><polyline points="9 11 12 14 22 4"/><path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11"/></svg>`,
  blockquote: `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2"><path d="M3 21c3 0 5-1 5-5V5c0-1-1-2-2-2H4c-1 0-2 1-2 2v6c0 1 1 2 2 2h2"/><path d="M15 21c3 0 5-1 5-5V5c0-1-1-2-2-2h-2c-1 0-2 1-2 2v6c0 1 1 2 2 2h2"/></svg>`,
  codeBlock: `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2"><polyline points="16 18 22 12 16 6"/><polyline points="8 6 2 12 8 18"/></svg>`,
};

// ── DOM 创建 ──────────────────────────────────────────

/**
 * 创建 MD 工具栏 DOM 元素。
 * @param {string} [id='md-toolbar'] - 工具栏元素 ID
 * @returns {HTMLElement} 工具栏 div 元素（未插入 DOM 树）
 */
export function createMdToolbar(id = "md-toolbar") {
  const toolbar = document.createElement("div");
  toolbar.className = "md-toolbar";
  toolbar.id = id;
  toolbar.innerHTML = `
    <button class="md-tool-btn" data-cmd="h1" title="标题1">H1</button>
    <button class="md-tool-btn" data-cmd="h2" title="标题2">H2</button>
    <button class="md-tool-btn" data-cmd="h3" title="标题3">H3</button>
    <span class="md-tool-sep"></span>
    <button class="md-tool-btn" data-cmd="bold" title="粗体"><b>B</b></button>
    <button class="md-tool-btn" data-cmd="italic" title="斜体"><i>I</i></button>
    <button class="md-tool-btn" data-cmd="strike" title="删除线"><s>S</s></button>
    <button class="md-tool-btn" data-cmd="code" title="行内代码">&lt;/&gt;</button>
    <span class="md-tool-sep"></span>
    <button class="md-tool-btn" data-cmd="bulletList" title="无序列表">${ICONS.bulletList}</button>
    <button class="md-tool-btn" data-cmd="orderedList" title="有序列表">${ICONS.orderedList}</button>
    <button class="md-tool-btn" data-cmd="taskList" title="任务清单">${ICONS.taskList}</button>
    <button class="md-tool-btn" data-cmd="blockquote" title="引用">${ICONS.blockquote}</button>
    <button class="md-tool-btn" data-cmd="codeBlock" title="代码块">${ICONS.codeBlock}</button>
  `;
  return toolbar;
}

// ── 事件绑定 ──────────────────────────────────────────

/**
 * 绑定 MD 工具栏事件。
 *
 * @param {HTMLElement} toolbarEl - 工具栏 DOM 元素
 * @param {Object} editor - Tiptap 编辑器实例
 * @param {Object} [opts] - 可选配置
 * @param {HTMLElement} [opts.editorEl] - 编辑器容器元素（用于 autoHide 排除点击）
 * @param {boolean} [opts.autoHide=false] - 是否点击编辑器外部时自动收起
 * @param {HTMLElement} [opts.toggleBtn] - 切换按钮（autoHide 模式下使用）
 * @returns {Function} cleanup 函数，调用以移除所有监听器
 */
export function bindMdToolbar(toolbarEl, editor, opts = {}) {
  const { editorEl, autoHide = false, toggleBtn = null } = opts;

  // 工具栏内部点击不冒泡（防止触发外部关闭逻辑）
  toolbarEl.addEventListener("click", (e) => {
    e.stopPropagation();
  });

  // 绑定工具按钮
  const btns = toolbarEl.querySelectorAll(".md-tool-btn");
  btns.forEach((btn) => {
    btn.addEventListener("click", () => {
      const cmd = btn.dataset.cmd;
      if (!cmd || !editor) return;
      executeMdCommand(cmd, editor);
    });
  });

  // autoHide：点击编辑器外部时收起工具栏
  let docClickHandler = null;
  if (autoHide) {
    docClickHandler = (e) => {
      if (toolbarEl.hidden) return;
      // 点击编辑器/降级 textarea 时不收起
      if (editorEl && (editorEl.contains(e.target) || editorEl === e.target)) {
        return;
      }
      // 点击切换按钮本身不收起（由切换按钮逻辑处理）
      if (toggleBtn && (toggleBtn.contains(e.target) || toggleBtn === e.target)) {
        return;
      }
      toolbarEl.hidden = true;
      if (toggleBtn) toggleBtn.classList.remove("active");
    };
    document.addEventListener("click", docClickHandler);
  }

  // 返回 cleanup 函数
  return () => {
    if (docClickHandler) {
      document.removeEventListener("click", docClickHandler);
    }
  };
}

// ── 命令执行 ──────────────────────────────────────────

/**
 * 执行 MD 格式命令。
 * @param {string} cmd - 命令名（bold/italic/h1/bulletList 等）
 * @param {Object} editor - Tiptap 编辑器实例
 */
export function executeMdCommand(cmd, editor) {
  if (!editor) return;
  const cmds = editor.commands;
  switch (cmd) {
    case "bold":
      cmds.toggleBold();
      break;
    case "italic":
      cmds.toggleItalic();
      break;
    case "strike":
      cmds.toggleStrike();
      break;
    case "code":
      cmds.toggleCode();
      break;
    case "h1":
      cmds.toggleHeading({ level: 1 });
      break;
    case "h2":
      cmds.toggleHeading({ level: 2 });
      break;
    case "h3":
      cmds.toggleHeading({ level: 3 });
      break;
    case "bulletList":
      cmds.toggleBulletList();
      break;
    case "orderedList":
      cmds.toggleOrderedList();
      break;
    case "blockquote":
      cmds.toggleBlockquote();
      break;
    case "codeBlock":
      cmds.toggleCodeBlock();
      break;
    case "taskList":
      cmds.toggleTaskList();
      break;
  }
  editor.commands.focus();
}

// ── 状态刷新 ──────────────────────────────────────────

/**
 * 刷新工具栏按钮激活态。
 * @param {HTMLElement} toolbarEl - 工具栏 DOM 元素
 * @param {Object} editor - Tiptap 编辑器实例
 */
export function updateToolbarStates(toolbarEl, editor) {
  if (!editor) return;
  const activeStates = {
    bold: editor.isActive("bold"),
    italic: editor.isActive("italic"),
    strike: editor.isActive("strike"),
    code: editor.isActive("code"),
    h1: editor.isActive("heading", { level: 1 }),
    h2: editor.isActive("heading", { level: 2 }),
    h3: editor.isActive("heading", { level: 3 }),
    bulletList: editor.isActive("bulletList"),
    orderedList: editor.isActive("orderedList"),
    blockquote: editor.isActive("blockquote"),
    codeBlock: editor.isActive("codeBlock"),
    taskList: editor.isActive("taskList"),
  };
  toolbarEl.querySelectorAll(".md-tool-btn").forEach((btn) => {
    const cmd = btn.dataset.cmd;
    if (activeStates[cmd] !== undefined) {
      btn.classList.toggle("active", activeStates[cmd]);
    }
  });
}
