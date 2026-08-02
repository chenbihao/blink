/**
 * 内容编辑器窗口入口（0.16.3）。
 *
 * 装配主题 / i18n / 图标 sprite，从后端拉取 payload，初始化编辑/预览/保存逻辑。
 * 编辑逻辑全在前端，后端只做窗口创建 + 剪贴板读写桥接。
 */

import { applyThemeFromConfig } from "../shared/theme.js";
import { applyI18nFromConfig, t } from "../i18n/index.js";
import { ensureSpriteLoaded } from "../shared/icon.js";
import { initMarkdown, renderMarkdown, highlightCodeBlocks } from "../shared/markdown.js";
import { getCurrentWindow, confirmDialog } from "../shared/tauri.js";
import { getContentEditorPayload, saveContentEditor } from "../shared/api.js";

// ── 状态 ──────────────────────────────────────────────

/** 原始 payload（用于对比是否有未保存改动） */
let originalBody = "";

/** 当前 originRef（保存时传给后端继承 hit_count） */
let originRef = null;

/** 当前模式："edit" | "preview" */
let currentMode = "edit";

/** 防止重复保存 */
let saving = false;

/** 已确认关闭——跳过 onCloseRequested 拦截 */
let allowClose = false;

// ── DOM 引用 ──────────────────────────────────────────

const titleEl = document.getElementById("editor-title");
const textareaEl = document.getElementById("editor-textarea");
const previewEl = document.getElementById("editor-preview");
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

    originalBody = payload.body || "";
    originRef = payload.originRef || null;

    // 填充编辑器
    textareaEl.value = originalBody;

    // 设置标题
    titleEl.textContent = payload.title || t("editor.title.default");

    // 重置状态
    saving = false;
    allowClose = false;
    statusEl.textContent = "";
    saveBtn.disabled = false;

    // 切到编辑模式
    setMode("edit");

    // 聚焦编辑器
    textareaEl.focus();
  } catch (e) {
    console.error("[content-editor] loadPayload 失败:", e);
  }
}

// ── 模式切换 ──────────────────────────────────────────

function setMode(mode) {
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
    const body = textareaEl.value;
    await saveContentEditor(body, originRef);

    // 保存成功
    originalBody = body;
    allowClose = true;
    statusEl.textContent = t("editor.saved");

    // 短暂延迟后关闭窗口
    setTimeout(() => closeWindow(), 500);
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
  return textareaEl.value !== originalBody;
}

/** 关闭窗口 */
function closeWindow() {
  const win = getCurrentWindow();
  if (win) {
    win.close();
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
    maxBtn.addEventListener("click", () => {
      const win = getCurrentWindow();
      if (!win) return;
      // 切换最大化/还原
      if (win.isMaximized()) {
        win.unmaximize();
      } else {
        win.maximize();
      }
    });
  }

  if (closeBtn) {
    closeBtn.addEventListener("click", handleCancel);
  }

  // 系统级关闭请求（Alt+F4 等）——拦截未保存改动
  const win = getCurrentWindow();
  if (win?.onCloseRequested) {
    win.onCloseRequested(async (event) => {
      if (allowClose) return; // 已确认，放行
      if (hasUnsavedChanges()) {
        event.preventDefault();
        handleCancel();
      }
      // 无未保存改动则放行，窗口正常关闭
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
