/**
 * 内容编辑器窗口入口（0.16.3）。
 *
 * 装配主题 / i18n / 图标 sprite，从后端拉取 payload，初始化编辑/预览/保存逻辑。
 * 编辑逻辑全在前端，后端只做窗口创建 + 剪贴板读写桥接。
 */

import { applyThemeFromConfig } from "../shared/theme.js";
import { applyI18nFromConfig, t } from "../i18n/index.js";
import { ensureSpriteLoaded } from "../shared/icon.js";
import { initMarkdown, renderMarkdown, createMarkdownEditor, highlightCodeBlocks } from "../shared/markdown.js";
import { getCurrentWindow, confirmDialog } from "../shared/tauri.js";
import { getContentEditorPayload, saveContentEditor } from "../shared/api.js";

// ── 状态 ──────────────────────────────────────────────

/** 原始 payload（用于对比是否有未保存改动） */
let originalBody = "";

/** 当前 originRef（保存时传给后端继承 hit_count） */
let originRef = null;

/** 当前 savePolicy（0.16.9：clipboard_new | sticky_update） */
let savePolicy = "clipboard_new";

/** 当前内容格式：plain | md（0.17.7a） */
let currentFormat = "plain";

/** Cherry Markdown 编辑器实例（format=md 时使用，null = 纯文本模式） */
let mdEditor = null;

/** 当前模式："edit" | "preview" */
let currentMode = "edit";

/** 防止重复保存 */
let saving = false;

/** 已确认关闭——跳过 onCloseRequested 拦截 */
let allowClose = false;

/** 保存成功后延迟关闭的计时器——loadPayload 时清除，防止复用窗口被误关 */
let closeTimeout = null;

// ── DOM 引用 ──────────────────────────────────────────

const titleEl = document.getElementById("editor-title");
const textareaEl = document.getElementById("editor-textarea");
const previewEl = document.getElementById("editor-preview");
const mdContainerEl = document.getElementById("editor-md-container");
const modeEditBtn = document.getElementById("mode-edit");
const modePreviewBtn = document.getElementById("mode-preview");
const saveBtn = document.getElementById("btn-save");
const cancelBtn = document.getElementById("btn-cancel");
const statusEl = document.getElementById("editor-status");

// ── 初始化 ────────────────────────────────────────────

async function init() {
  // 主题 + i18n + 图标
  await ensureSpriteLoaded();
  await applyThemeFromConfig();
  await applyI18nFromConfig();

  // Markdown 渲染器初始化
  initMarkdown();

  // 从后端拉取 payload
  await loadPayload();

  // 绑定事件
  bindToolbar();
  bindWindowControls();
  bindKeyboard();

  // 注册窗口复用回调（后端 eval 调用）
  window.__contentEditorReload = loadPayload;

  // 0.16.13：后端已改为 visible(true) + background_color 创建窗口，不再依赖前端 show。
  // 此处 win.show()/setFocus() 保留为 harmless no-op（窗口已由后端 show）。
  const win = getCurrentWindow();
  if (win) {
    try {
      await win.show();
      await win.setFocus();
    } catch (e) {
      console.error("[content-editor] show window 失败:", e);
    }
  }

  tracing("editor window: init 完成");
}

/**
 * 从后端拉取 payload 并填充编辑器。
 * 窗口首次打开和复用时都会调用。
 */
async function loadPayload() {
  try {
    const payload = await getContentEditorPayload();
    if (!payload) {
      tracing("loadPayload: 无 payload，可能是窗口已关闭后再次打开");
      return;
    }

    // 0.16.13: 归一化 \r\n → \n。<textarea>.value 会把 \r\n 规范化为 \n，
    // 若 originalBody 保留 \r\n，hasUnsavedChanges() 会误判为"已改动"，
    // 导致未编辑也弹"放弃更改"确认框。来源为剪贴板/便签时尤为常见（Windows 文本含 \r\n）。
    originalBody = (payload.body || "").replace(/\r\n/g, "\n");
    originRef = payload.originRef || null;
    savePolicy = payload.savePolicy || "clipboard_new";
    currentFormat = payload.format || "plain";

    // 0.17.7a：format=markdown 时使用 Cherry Markdown 编辑器（edit&preview + full 工具栏）
    if (currentFormat === "markdown" && mdContainerEl) {
      // 隐藏纯文本编辑区/预览区
      textareaEl.hidden = true;
      previewEl.hidden = true;
      mdContainerEl.hidden = false;

      // 隐藏编辑/预览模式切换按钮（split view 同时显示）
      modeEditBtn.style.display = "none";
      modePreviewBtn.style.display = "none";

      // 创建或复用 Cherry 编辑器
      if (mdEditor) {
        mdEditor.setMarkdown(originalBody);
      } else {
        mdEditor = createMarkdownEditor(mdContainerEl, {
          toolbar: "full",
          defaultText: originalBody,
        });
      }
    } else {
      // 纯文本模式：显示 textarea，隐藏 Cherry 编辑器
      textareaEl.hidden = false;
      mdContainerEl.hidden = true;
      modeEditBtn.style.display = "";
      modePreviewBtn.style.display = "";

      // 销毁 Cherry 编辑器（如果之前创建过）
      if (mdEditor) {
        mdEditor.destroy();
        mdEditor = null;
      }

      // 填充编辑器
      textareaEl.value = originalBody;
    }

    // 设置标题
    titleEl.textContent = payload.title || t("editor.title.default");

    // 重置状态——清除可能残留的 close 计时器，防止复用时误关
    if (closeTimeout) {
      clearTimeout(closeTimeout);
      closeTimeout = null;
    }
    saving = false;
    allowClose = false;
    statusEl.textContent = "";
    saveBtn.disabled = false;

    // 切到编辑模式
    if (currentFormat === "markdown" && mdEditor) {
      mdEditor.focus();
    } else {
      setMode("edit");
      textareaEl.focus();
    }
  } catch (e) {
    console.error("[content-editor] loadPayload 失败:", e);
  }
}

// ── 模式切换 ──────────────────────────────────────────

function setMode(mode) {
  // 0.17.7a：format=markdown 时不支持模式切换（split view 同时显示）
  if (currentFormat === "markdown" && mdEditor) return;
  currentMode = mode;

  if (mode === "edit") {
    modeEditBtn.classList.add("active");
    modePreviewBtn.classList.remove("active");
    textareaEl.hidden = false;
    previewEl.hidden = true;
    textareaEl.focus();
  } else {
    modeEditBtn.classList.remove("active");
    modePreviewBtn.classList.add("active");
    textareaEl.hidden = true;
    previewEl.hidden = false;
    renderPreview();
  }
}

/** 渲染 Markdown 预览 */
function renderPreview() {
  // 0.17.7a：format=markdown 时预览由 Cherry 编辑器实时处理
  if (currentFormat === "markdown" && mdEditor) return;
  const text = textareaEl.value;
  renderMarkdown(text, { container: previewEl });
  highlightCodeBlocks(previewEl);
}

// ── 工具栏 ────────────────────────────────────────────

function bindToolbar() {
  modeEditBtn.addEventListener("click", () => setMode("edit"));
  modePreviewBtn.addEventListener("click", () => setMode("preview"));
  saveBtn.addEventListener("click", handleSave);
  cancelBtn.addEventListener("click", handleCancel);
}

// ── 保存 ──────────────────────────────────────────────

async function handleSave() {
  if (saving) return;
  saving = true;
  saveBtn.disabled = true;
  statusEl.textContent = t("editor.saving");

  try {
    // 0.17.7a：format=md 时从 Cherry 编辑器获取内容
    const body = mdEditor ? mdEditor.getMarkdown() : textareaEl.value;
    await saveContentEditor(body, originRef, savePolicy);

    // 保存成功
    originalBody = body;
    allowClose = true;
    statusEl.textContent = t("editor.saved");

    // 短暂延迟后关闭窗口
    closeTimeout = setTimeout(() => {
      closeWindow();
      closeTimeout = null;
    }, 500);
  } catch (e) {
    console.error("[content-editor] 保存失败:", e);
    const msg = typeof e === "string" ? e : String(e?.message || e);
    statusEl.textContent = t("editor.saveFailed", { message: msg });
    saving = false;
    saveBtn.disabled = false;
  }
}

// ── 关闭 / 取消 ───────────────────────────────────────

async function handleCancel() {
  if (hasUnsavedChanges() && !allowClose) {
    const confirmed = await confirmDialog(t("editor.unsavedWarning"), {
      kind: "warning",
      okLabel: t("editor.discard"),
      cancelLabel: t("editor.continueEdit"),
    });
    if (!confirmed) return;
  }
  allowClose = true;
  closeWindow();
}

/** 检查是否有未保存的改动 */
function hasUnsavedChanges() {
  const current = mdEditor ? mdEditor.getMarkdown() : textareaEl.value;
  return current !== originalBody;
}

/** 关闭窗口（改为 hide 复用模式，不再销毁窗口） */
function closeWindow() {
  const win = getCurrentWindow();
  if (win) {
    win.hide();
  }
}

// ── 窗口控制 ──────────────────────────────────────────

function bindWindowControls() {
  const minBtn = document.getElementById("titlebar-minimize");
  const maxBtn = document.getElementById("titlebar-maximize");
  const closeBtn = document.getElementById("titlebar-close");

  if (minBtn) {
    minBtn.addEventListener("click", () => {
      getCurrentWindow()?.minimize();
    });
  }

  if (maxBtn) {
    maxBtn.addEventListener("click", async () => {
      const win = getCurrentWindow();
      if (!win) return;
      const isMax = await win.isMaximized();
      if (isMax) {
        await win.unmaximize();
      } else {
        await win.maximize();
      }
    });
  }

  if (closeBtn) {
    closeBtn.addEventListener("click", handleCancel);
  }

  // 系统级关闭请求（Alt+F4 等）
  // hide-on-close 模式——始终 prevent_close + hide，
  // 窗口复用而非销毁重建，消除"每两次有1次点不开"的竞态。
  const win = getCurrentWindow();
  if (win?.onCloseRequested) {
    win.onCloseRequested(async (event) => {
      event.preventDefault(); // 始终阻止销毁
      if (allowClose || !hasUnsavedChanges()) {
        closeWindow(); // win.hide()
      } else {
        handleCancel(); // 显示未保存确认对话框
      }
    });
  }
}

// ── 键盘快捷键 ────────────────────────────────────────

function bindKeyboard() {
  document.addEventListener("keydown", (e) => {
    // Ctrl+S：保存
    if ((e.ctrlKey || e.metaKey) && e.key === "s") {
      e.preventDefault();
      handleSave();
      return;
    }

    // Esc：关闭（有未保存改动时提示）
    if (e.key === "Escape") {
      e.preventDefault();
      handleCancel();
      return;
    }
  });
}

// ── 工具 ──────────────────────────────────────────────

/** 简易日志（绕过 frontendLog，直接 console） */
function tracing(msg) {
  console.log(`[content-editor] ${msg}`);
}

// ── 启动 ──────────────────────────────────────────────

init().catch((e) => console.error("[content-editor] init 失败:", e));
