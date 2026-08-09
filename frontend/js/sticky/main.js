/**
 * 便签窗口入口（0.16.8 / 0.18.3 IR 编辑）。
 *
 * 从 URL 参数读取 sticky_id，从后端拉取便签数据，初始化编辑/自动保存/颜色/生命周期。
 * 编辑逻辑在前端，后端只做数据持久化和窗口管理。
 *
 * 0.18.3：编辑器从纯文本 textarea 升级为 Tiptap IR（即时渲染）编辑器。
 * 输入 markdown 语法实时渲染（# → H1、**粗体** 等），存储仍为 markdown 文本。
 * 若 Tiptap 加载失败，降级为 textarea（降级方案 B 兜底）。
 *
 * 设计见 phases/0.16-clipboard-polish.md §5.9、phases/0.18-enhancement-chord.md §3.2。
 */

import { applyThemeFromConfig } from "../shared/theme.js";
import { ensureSpriteLoaded } from "../shared/icon.js";
import { getCurrentWindow, confirmDialog, listen, invoke } from "../shared/tauri.js";
import { EVENTS } from "../shared/event-names.js";
import { createMdToolbar, bindMdToolbar, updateToolbarStates } from "../shared/md-toolbar.js";
import {
  getStickyNote,
  updateStickyContent,
  updateStickyAppearance,
  updateStickyGeometry,
  setStickyAlwaysOnTop,
  setStickyVisible,
  deleteStickyNote,
  destroyStickyWindow,
  openContentEditor,
  trashStickyNote,
  showStickyManager,
} from "../shared/api.js";

// ── 状态 ──────────────────────────────────────────────

/** 当前便签 ID */
let stickyId = null;

/** 当前便签数据 */
let stickyNote = null;

/** 内容保存防抖计时器 */
let saveTimer = null;

/** 几何保存防抖计时器 */
let geometryTimer = null;

/** 防抖延迟（ms） */
const SAVE_DEBOUNCE_MS = 500;
const GEOMETRY_DEBOUNCE_MS = 300;

/** Tiptap IR 编辑器实例（null = 未初始化或降级 textarea 模式） */
let tiptapEditor = null;

/** 0.18.3 fix: 程序化设入内容时为 true，抑制 update 事件触发的防抖保存。
 *
 * 根因：Tiptap setContent(json, false) 的 emitUpdate=false 在 Markdown 扩展
 * parse→setContent 路径下仍可能触发 update 事件，导致 loadStickyData →
 * setContent → update → scheduleSave → 500ms 后保存 → emit
 * STICKY_CONTENT_CHANGED → 又触发 loadStickyData 的反馈循环。
 *
 * 通过 isLoading 标志在 setContent 期间屏蔽 scheduleSave，斩断循环。
 */
let isLoading = false;

// ── 0.18.3 fix: 预热窗口未初始化完即被借用的竞态防护 ──
//
// init() 前两个 await（ensureSpriteLoaded / applyThemeFromConfig）是异步 IPC，
// 后端可能在它们完成前就 eval __stickyReload。此时注册一个早期 stub，
// 暂存 pending id，init 完成后再处理。

/** init 是否完成（preheat 或 normal 路径都设为 true） */
let initReady = false;

/** init 完成前收到的 pending reload id */
let pendingStickyId = null;

/** 真正的 __stickyReload 实现（registerStickyReload 注册） */
let realStickyReload = null;

// ── DOM 引用 ──────────────────────────────────────────

const rootEl = document.getElementById("sticky-root");
const editorEl = document.getElementById("sticky-editor");
const fallbackEl = document.getElementById("sticky-fallback");
const textareaEl = document.getElementById("sticky-textarea");
const colorBtn = document.getElementById("btn-color");
const colorPalette = document.getElementById("color-palette");
const pinBtn = document.getElementById("btn-pin");
const moreBtn = document.getElementById("btn-more");
const moreMenu = document.getElementById("more-menu");
const moreOpenEditor = document.getElementById("more-open-editor");
const moreHide = document.getElementById("more-hide");
const moreOpenManager = document.getElementById("more-open-manager");
const closeBtn = document.getElementById("btn-close");
const toolbarBtn = document.getElementById("btn-toolbar");
const mdToolbar = document.getElementById("md-toolbar");

// ── 初始化 ────────────────────────────────────────────

async function init() {
  // 0.18.3 fix: 立即注册早期 stub，在任何 await 之前。
  // 如果后端在 init 的异步步骤（ensureSpriteLoaded / applyThemeFromConfig）完成前
  // 就 eval __stickyReload，stub 会暂存 id，init 完成后处理。
  window.__stickyReload = function (id) {
    if (!initReady) {
      pendingStickyId = id;
      console.log("[sticky] __stickyReload 在 init 完成前被调用，已暂存:", id);
      return;
    }
    // init 完成后，交给真正的 registerStickyReload 注册的实现处理
    if (typeof realStickyReload === "function") {
      realStickyReload(id);
    }
  };

  // 主题 + 图标
  await ensureSpriteLoaded();
  await applyThemeFromConfig();

  // 0.18.3：预热模式——跳过完整初始化，等待 __stickyReload 唤醒
  const params = new URLSearchParams(window.location.search);
  const isPreheat = params.get("preheat") === "1";

  if (isPreheat) {
    // 预创建 Tiptap 编辑器（加载脚本 + 初始化），但不拉取便签数据
    initTiptapEditor();
    console.log("[sticky] 预热模式：Tiptap 已初始化，等待 __stickyReload 唤醒");
    window.__stickyPreheated = true;
    // 预热分支也注册 __stickyReload，否则后端 eval 调用时找不到函数
    registerStickyReload();

    // 0.18.3 fix: 标记 init 完成，处理可能在 await 期间收到的 pending reload
    initReady = true;
    if (pendingStickyId) {
      console.log("[sticky] 预热 init 完成，处理 pending reload:", pendingStickyId);
      const id = pendingStickyId;
      pendingStickyId = null;
      realStickyReload(id);
    }

    // 0.18.3 N+1: 通知后端 spare 已就绪，可被借用
    invoke("sticky_spare_ready").catch((e) =>
      console.error("[sticky] sticky_spare_ready 调用失败:", e)
    );
    return;
  }

  // 从 URL 参数读取 sticky_id
  stickyId = params.get("id");

  if (!stickyId) {
    console.error("[sticky] 未提供 sticky_id");
    return;
  }

  // 初始化 Tiptap IR 编辑器（0.18.3）
  // 必须在 loadStickyData 之前，因为 loadStickyData 会 setContent
  initTiptapEditor();

  // 从后端拉取便签数据
  await loadStickyData();

  // 绑定事件
  bindEditing();
  bindColorPalette();
  bindToolbar();
  bindMoreMenu();
  bindWindowControls();
  bindGeometryTracking();
  bindKeyboard();
  bindContextMenu();

  // 内容变更：只在用户未在编辑时刷新，避免打断输入
  // 外部入口（内容编辑器 / Capability）的变更始终 reload；本窗口防抖写入才避开输入中刷新。
  listen(EVENTS.STICKY_CONTENT_CHANGED, (event) => {
    const payload = event.payload;
    if (payload && payload.stickyId === stickyId) {
      if (payload.source !== "sticky") {
        loadStickyData();
        return;
      }
      // sticky 来源：用户正在本窗口编辑时不 reload，避免反馈循环和光标跳动
      const activeEl = document.activeElement;
      const isEditing = tiptapEditor
        ? activeEl === editorEl || editorEl?.contains(activeEl)
        : activeEl === textareaEl;
      if (!isEditing) {
        loadStickyData();
      }
    }
  });

  // 外观变更：只刷新颜色，不 reload content
  listen(EVENTS.STICKY_APPEARANCE_CHANGED, (event) => {
    const payload = event.payload;
    if (payload && payload.stickyId === stickyId && payload.color) {
      applyColor(payload.color);
    }
  });

  // 0.17.7：便签被移入回收站时隐藏窗口
  listen(EVENTS.STICKY_TRASHED, (event) => {
    const payload = event.payload;
    if (payload && payload.stickyId === stickyId) {
      const win = getCurrentWindow();
      if (win) win.hide();
    }
  });

  // 0.18.0：管理面板设置 visible=false 时，桌面便签窗口同步隐藏
  // 后端 set_sticky_visible 已 emit STICKY_VISIBILITY_CHANGED，此处补监听
  listen(EVENTS.STICKY_VISIBILITY_CHANGED, (event) => {
    const payload = event.payload;
    if (payload && payload.stickyId === stickyId && payload.visible === false) {
      const win = getCurrentWindow();
      if (win) win.hide();
    }
  });

  // 注册窗口复用回调（正常路径也注册，与预热分支共享同一函数）
  registerStickyReload();

  // 0.18.3 fix: 标记 init 完成，处理可能在 await 期间收到的 pending reload
  initReady = true;
  if (pendingStickyId) {
    console.log("[sticky] 正常 init 完成，处理 pending reload:", pendingStickyId);
    const id = pendingStickyId;
    pendingStickyId = null;
    realStickyReload(id);
  }

  console.log("[sticky] init 完成");
}

/**
 * 注册 __stickyReload 回调——预热和正常路径共享。
 * 预热分支也调用此函数，确保后端 eval 能找到 __stickyReload。
 */
function registerStickyReload() {
  // 真正的实现存入 realStickyReload，供早期 stub 转发
  realStickyReload = (id) => {
    // 窗口复用前先销毁旧编辑器实例，避免 DOM 争抢
    if (window.__stickyDestroyEditor) {
      window.__stickyDestroyEditor();
    }
    // 重新初始化 Tiptap（editorEl 已被 destroy 清理）
    initTiptapEditor();

    // 0.18.3：首次从预热唤醒需绑定全部 DOM 事件；
    // recycled spare（非首次）只需重建 Tiptap 实例级监听
    const isFirstBorrow = window.__stickyPreheated;
    if (isFirstBorrow) {
      window.__stickyPreheated = false;
      bindColorPalette();
      bindToolbar();
      bindMoreMenu();
      bindWindowControls();
      bindGeometryTracking();
      bindKeyboard();
      bindContextMenu();

      // 内容变更监听
      // 外部入口（内容编辑器 / Capability）的变更始终 reload
      listen(EVENTS.STICKY_CONTENT_CHANGED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === stickyId) {
          if (payload.source !== "sticky") {
            loadStickyData();
            return;
          }
          const activeEl = document.activeElement;
          const isEditing = tiptapEditor
            ? activeEl === editorEl || editorEl?.contains(activeEl)
            : activeEl === textareaEl;
          if (!isEditing) {
            loadStickyData();
          }
        }
      });

      listen(EVENTS.STICKY_APPEARANCE_CHANGED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === stickyId && payload.color) {
          applyColor(payload.color);
        }
      });

      listen(EVENTS.STICKY_TRASHED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === stickyId) {
          const win = getCurrentWindow();
          if (win) win.hide();
        }
      });

      listen(EVENTS.STICKY_VISIBILITY_CHANGED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === stickyId && payload.visible === false) {
          const win = getCurrentWindow();
          if (win) win.hide();
        }
      });

      // bindEditing 单独调用（在 setContent 之前绑定 update 事件）
      bindEditing();
    } else {
      // 0.18.3：recycled spare — 编辑器已重建，需重新绑定 Tiptap 实例级事件
      bindEditing();
      // 重建 MD 工具栏（bindMdToolbar 闭包捕获旧 editor 引用，必须重建）
      if (mdToolbar) {
        mdToolbar.innerHTML = "";
        const toolbar = createMdToolbar("md-toolbar-inner");
        mdToolbar.appendChild(toolbar);
        bindMdToolbar(toolbar, tiptapEditor, { editorEl, autoHide: true, toggleBtn: toolbarBtn });
      }
      if (tiptapEditor) {
        tiptapEditor.on("selectionUpdate", () => updateToolbarStates(mdToolbar, tiptapEditor));
        tiptapEditor.on("transaction", () => updateToolbarStates(mdToolbar, tiptapEditor));
      }
    }

    stickyId = id;
    loadStickyData();
  };
}

/**
 * 从后端拉取便签数据并填充 UI。
 */
async function loadStickyData() {
  try {
    stickyNote = await getStickyNote(stickyId);
    if (!stickyNote) {
      console.error("[sticky] 便签不存在:", stickyId);
      return;
    }

    // 填充编辑器
    setContent(stickyNote.content || "");

    // 应用颜色
    applyColor(stickyNote.color || "theme");

    // 应用置顶状态
    updatePinButton(stickyNote.alwaysOnTop);

    // 聚焦编辑器
    focusEditor();
  } catch (e) {
    console.error("[sticky] 加载便签数据失败:", e);
  }
}

// ── Tiptap IR 编辑器初始化（0.18.3）──────────────────────────

/**
 * 初始化 Tiptap IR 编辑器。
 * 若 window.BlinkTiptap 不存在或初始化失败，降级为 textarea。
 */
function initTiptapEditor() {
  if (!window.BlinkTiptap) {
    console.warn("[sticky] BlinkTiptap 未加载，降级为 textarea");
    enableFallback();
    return;
  }

  try {
    const { Editor, StarterKit, Markdown, TaskList, TaskItem } = window.BlinkTiptap;
    tiptapEditor = new Editor({
      element: editorEl,
      extensions: [
        StarterKit,
        Markdown,
        TaskList,
        TaskItem,
      ],
      content: "",
      contentType: "markdown",
      editorProps: {
        attributes: {
          class: "sticky-tiptap",
          spellcheck: "false",
        },
        handlePaste: (view, event) => {
          // 0.18.3：粘贴纯文本时走 Markdown 解析，使 - [ ] / **bold** 等语法实时生效
          const text = event.clipboardData?.getData("text/plain");
          if (!text || !tiptapEditor) return false;
          try {
            const json = tiptapEditor.markdown.parse(text);
            tiptapEditor.commands.insertContent(json);
            event.preventDefault();
            return true;
          } catch (e) {
            console.warn("[sticky] paste markdown parse failed, fallback to default:", e);
            return false;
          }
        },
      },
    });
    console.log("[sticky] Tiptap IR 编辑器初始化成功");
  } catch (e) {
    console.error("[sticky] Tiptap 初始化失败，降级为 textarea:", e);
    tiptapEditor = null;
    enableFallback();
  }
}

/** 启用 textarea 降级模式 */
function enableFallback() {
  tiptapEditor = null;
  editorEl.hidden = true;
  if (fallbackEl) fallbackEl.hidden = false;
}

// ── 内容读写 ───────────────────────────────────────

/** 获取当前便签内容（markdown 文本） */
function getContent() {
  if (tiptapEditor) {
    return tiptapEditor.getMarkdown();
  }
  return textareaEl.value;
}

/** 设置便签内容（markdown 文本） */
function setContent(text) {
  if (tiptapEditor) {
    // 0.18.3 fix: isLoading 标志确保 setContent 期间不会触发防抖保存
    isLoading = true;
    try {
      // 通过 markdown.parse 将 markdown 文本转为 JSON，再 setContent
      const json = tiptapEditor.markdown.parse(text);
      tiptapEditor.commands.setContent(json, false);
    } catch (e) {
      console.error("[sticky] setContent 解析失败:", e);
      // 降级：直接设为纯文本 HTML
      tiptapEditor.commands.setContent(`<p>${escapeHtml(text)}</p>`, false);
    } finally {
      isLoading = false;
    }
  } else {
    textareaEl.value = text;
  }
}

/** 转义 HTML 特殊字符（降级路径用） */
function escapeHtml(s) {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

/** 聚焦当前编辑器（Tiptap 或 textarea） */
function focusEditor() {
  if (tiptapEditor) {
    tiptapEditor.commands.focus();
  } else {
    textareaEl.focus();
  }
}

// ── 编辑与自动保存 ────────────────────────────────────

function bindEditing() {
  if (tiptapEditor) {
    // Tiptap 的 update 事件替代 textarea 的 input 事件
    tiptapEditor.on("update", () => {
      scheduleSave();
    });
  } else {
    textareaEl.addEventListener("input", () => {
      scheduleSave();
    });
  }
}

/** 安排防抖保存 */
function scheduleSave() {
  // 0.18.3 fix: 程序化设入内容时跳过，避免反馈循环
  if (isLoading) return;
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    saveTimer = null; // 0.18.3 fix: 触发后清空，避免 flushSave 误判
    saveContent();
  }, SAVE_DEBOUNCE_MS);
}

/** 立即保存内容 */
async function saveContent() {
  if (!stickyId) return;
  const content = getContent();
  try {
    await updateStickyContent(stickyId, content);
  } catch (e) {
    console.error("[sticky] 保存内容失败:", e);
  }
}

/** 强制 flush（失焦/隐藏前调用） */
async function flushSave() {
  if (saveTimer) {
    clearTimeout(saveTimer);
    saveTimer = null;
    await saveContent();
  }
}

// ── 颜色面板 ──────────────────────────────────────────

function bindColorPalette() {
  // 切换面板显示
  colorBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    moreMenu.hidden = true;
    colorPalette.hidden = !colorPalette.hidden;
  });

  // 选择颜色
  colorPalette.querySelectorAll(".color-swatch").forEach((swatch) => {
    swatch.addEventListener("click", async () => {
      const color = swatch.dataset.color;
      colorPalette.hidden = true;
      applyColor(color);
      try {
        await updateStickyAppearance(stickyId, color);
      } catch (e) {
        console.error("[sticky] 更新颜色失败:", e);
      }
    });
  });

  // 点击外部关闭面板
  document.addEventListener("click", () => {
    colorPalette.hidden = true;
    moreMenu.hidden = true;
  });
}

/** 应用颜色 class + 更新色板选中态 */
function applyColor(color) {
  rootEl.className = "sticky-root";
  rootEl.classList.add(`color-${color}`);
  // 更新色板选中指示
  colorPalette.querySelectorAll(".color-swatch").forEach((swatch) => {
    swatch.classList.toggle("selected", swatch.dataset.color === color);
  });
}

// ── MD 工具栏（0.18.3）──────────────────────────────────

function bindToolbar() {
  // 切换工具栏显示
  toolbarBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    colorPalette.hidden = true;
    moreMenu.hidden = true;
    mdToolbar.hidden = !mdToolbar.hidden;
    // 工具栏激活态
    toolbarBtn.classList.toggle("active", !mdToolbar.hidden);
  });

  // 0.18.3：工具栏按钮事件 + autoHide 逻辑由共享模块 bindMdToolbar 处理
  bindMdToolbar(mdToolbar, tiptapEditor, {
    editorEl,
    autoHide: true,
    toggleBtn: toolbarBtn,
  });

  // 编辑器更新时刷新工具栏按钮激活态
  if (tiptapEditor) {
    tiptapEditor.on("selectionUpdate", () => updateToolbarStates(mdToolbar, tiptapEditor));
    tiptapEditor.on("transaction", () => updateToolbarStates(mdToolbar, tiptapEditor));
  }

  // Ctrl+滚轮缩放字体 + Ctrl+中键还原
  bindFontZoom();
}

// ── Ctrl+滚轮缩放字体（0.18.3）─────────────────────────

/** 当前字体缩放倍率（1.0 = 默认） */
let fontScale = 1.0;
const FONT_SCALE_MIN = 0.7;
const FONT_SCALE_MAX = 2.0;
const FONT_SCALE_STEP = 0.1;

function bindFontZoom() {
  editorEl.addEventListener("wheel", (e) => {
    if (!e.ctrlKey) return;
    e.preventDefault();
    if (e.deltaY < 0) {
      fontScale = Math.min(FONT_SCALE_MAX, fontScale + FONT_SCALE_STEP);
    } else {
      fontScale = Math.max(FONT_SCALE_MIN, fontScale - FONT_SCALE_STEP);
    }
    applyFontScale();
  });

  // Ctrl+中键还原
  editorEl.addEventListener("auxclick", (e) => {
    if (e.button === 1 && e.ctrlKey) {
      e.preventDefault();
      fontScale = 1.0;
      applyFontScale();
    }
  });
}

function applyFontScale() {
  const pm = editorEl.querySelector(".ProseMirror");
  if (pm) {
    pm.style.fontSize = `calc(var(--text-sm) * ${fontScale})`;
  }
}

// ── 更多菜单 ──────────────────────────────────────────

function bindMoreMenu() {
  moreBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    colorPalette.hidden = true;
    moreMenu.hidden = !moreMenu.hidden;
  });

  // 在编辑器中打开
  moreOpenEditor.addEventListener("click", async () => {
    moreMenu.hidden = true;
    await flushSave();
    try {
      await openContentEditor({
        body: getContent(),
        format: "markdown",
        title: "编辑便签内容",
        origin: "sticky",
        originRef: stickyId,
        savePolicy: "sticky_update",
      });
    } catch (e) {
      console.error("[sticky] 打开编辑器失败:", e);
    }
  });

  // 隐藏（仅隐藏窗口，不进回收站）
  moreHide.addEventListener("click", async () => {
    moreMenu.hidden = true;
    await flushSave();
    try {
      await setStickyVisible(stickyId, false);
    } catch (e) {
      console.error("[sticky] 设置可见性失败:", e);
    }
    const win = getCurrentWindow();
    if (win) win.hide();
  });

  // 便签管理（0.17.7：从便签页唤起管理窗口）
  if (moreOpenManager) {
    moreOpenManager.addEventListener("click", async () => {
      moreMenu.hidden = true;
      try {
        await showStickyManager();
      } catch (e) {
        console.error("[sticky] 打开便签管理失败:", e);
      }
    });
  }
}

// ── 窗口控制 ──────────────────────────────────────────

function bindWindowControls() {
  // 0.17.7：关闭 = 移入回收站（后端 CloseRequested handler 负责 trash + hide）
  // 前端只需 flush 保存，然后触发 close（后端 prevent_close + trash + hide）
  closeBtn.addEventListener("click", async () => {
    await flushSave();
    const win = getCurrentWindow();
    if (win) win.close();
  });

  // 置顶切换
  pinBtn.addEventListener("click", async () => {
    const newState = !pinBtn.classList.contains("active");
    updatePinButton(newState);
    try {
      await setStickyAlwaysOnTop(stickyId, newState);
      const win = getCurrentWindow();
      if (win) win.setAlwaysOnTop(newState);
    } catch (e) {
      console.error("[sticky] 切换置顶失败:", e);
      updatePinButton(!newState);
    }
  });
}

/** 更新置顶按钮状态 */
function updatePinButton(active) {
  if (active) {
    pinBtn.classList.add("active");
  } else {
    pinBtn.classList.remove("active");
  }
}

// ── 键盘快捷键（0.16.11）──────────────────────────────

function bindKeyboard() {
  document.addEventListener("keydown", (e) => {
    // ESC：内容为空时关闭（隐藏）便签窗口
    if (e.key === "Escape") {
      e.preventDefault();
      if (!getContent().trim()) {
        closeSticky();
      }
      return;
    }
  });
}

/** 关闭（隐藏）便签窗口 */
async function closeSticky() {
  await flushSave();
  const win = getCurrentWindow();
  if (win) win.close();
}

// ── 窗口几何追踪与持久化 ──────────────────────────────

function bindGeometryTracking() {
  const win = getCurrentWindow();
  if (!win) return;

  // 监听窗口移动/缩放（Tauri 的 onResized / onMoved）
  if (win.onResized) {
    win.onResized(() => scheduleGeometrySave());
  }
  if (win.onMoved) {
    win.onMoved(() => scheduleGeometrySave());
  }
}

/** 安排几何保存防抖 */
function scheduleGeometrySave() {
  if (geometryTimer) clearTimeout(geometryTimer);
  geometryTimer = setTimeout(() => {
    saveGeometry();
  }, GEOMETRY_DEBOUNCE_MS);
}

/** 保存窗口几何 */
async function saveGeometry() {
  if (!stickyId) return;
  const win = getCurrentWindow();
  if (!win) return;
  try {
    const pos = await win.outerPosition();
    const size = await win.outerSize();
    await updateStickyGeometry(
      stickyId,
      pos.x,
      pos.y,
      size.width,
      size.height,
    );
  } catch (e) {
    console.error("[sticky] 保存几何失败:", e);
  }
}

// ── 右键菜单（0.17.7）──────────────────────────────

/** 当前右键菜单元素（null = 未显示） */
let contextMenuEl = null;

function bindContextMenu() {
  // 屏蔽原生右键菜单
  document.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    showContextMenu(e.clientX, e.clientY);
  });

  // 点击外部 / ESC 关闭右键菜单
  document.addEventListener("click", (e) => {
    if (contextMenuEl && !contextMenuEl.contains(e.target)) {
      hideContextMenu();
    }
  });
}

function showContextMenu(x, y) {
  hideContextMenu();

  const menu = document.createElement("div");
  menu.className = "sticky-context-menu";

  // 在编辑器中打开
  const itemEditor = document.createElement("button");
  itemEditor.className = "ctx-item";
  itemEditor.textContent = "在编辑器中打开";
  itemEditor.addEventListener("click", async () => {
    hideContextMenu();
    await flushSave();
    try {
      await openContentEditor({
        body: getContent(),
        format: "markdown",
        title: "编辑便签内容",
        origin: "sticky",
        originRef: stickyId,
        savePolicy: "sticky_update",
      });
    } catch (e) {
      console.error("[sticky] 打开编辑器失败:", e);
    }
  });
  menu.appendChild(itemEditor);

  // 分割线
  menu.appendChild(makeSeparator());

  // 改颜色（内联色板）
  const colorLabel = document.createElement("div");
  colorLabel.className = "ctx-submenu-label";
  colorLabel.textContent = "改颜色";
  menu.appendChild(colorLabel);

  const colorRow = document.createElement("div");
  colorRow.className = "ctx-color-row";
  for (const color of ["theme", "yellow", "pink", "purple", "blue", "green", "gray"]) {
    const swatch = document.createElement("button");
    swatch.className = `ctx-color-swatch`;
    swatch.style.background = colorSwatchBg(color);
    swatch.title = color;
    swatch.addEventListener("click", async () => {
      hideContextMenu();
      applyColor(color);
      try {
        await updateStickyAppearance(stickyId, color);
      } catch (e) {
        console.error("[sticky] 更新颜色失败:", e);
      }
    });
    colorRow.appendChild(swatch);
  }
  menu.appendChild(colorRow);

  // 分割线
  menu.appendChild(makeSeparator());

  // 隐藏
  const itemHide = document.createElement("button");
  itemHide.className = "ctx-item";
  itemHide.textContent = "隐藏";
  itemHide.addEventListener("click", async () => {
    hideContextMenu();
    await flushSave();
    try {
      await setStickyVisible(stickyId, false);
    } catch (e) {
      console.error("[sticky] 设置可见性失败:", e);
    }
    const win = getCurrentWindow();
    if (win) win.hide();
  });
  menu.appendChild(itemHide);

  // 删除（=移入回收站）
  const itemDelete = document.createElement("button");
  itemDelete.className = "ctx-item ctx-danger";
  itemDelete.textContent = "删除";
  itemDelete.addEventListener("click", async () => {
    hideContextMenu();
    await flushSave();
    try {
      await trashStickyNote(stickyId);
      const win = getCurrentWindow();
      if (win) win.hide();
    } catch (e) {
      console.error("[sticky] 删除便签失败:", e);
    }
  });
  menu.appendChild(itemDelete);

  // 边缘检测定位
  rootEl.appendChild(menu);
  const menuRect = menu.getBoundingClientRect();
  const winRect = rootEl.getBoundingClientRect();
  let mx = x;
  let my = y;
  if (mx + menuRect.width > winRect.width) {
    mx = winRect.width - menuRect.width - 2;
  }
  if (my + menuRect.height > winRect.height) {
    my = winRect.height - menuRect.height - 2;
  }
  mx = Math.max(0, mx);
  my = Math.max(0, my);
  menu.style.left = `${mx}px`;
  menu.style.top = `${my}px`;

  contextMenuEl = menu;
}

function hideContextMenu() {
  if (contextMenuEl) {
    contextMenuEl.remove();
    contextMenuEl = null;
  }
}

function makeSeparator() {
  const sep = document.createElement("div");
  sep.className = "ctx-separator";
  return sep;
}

function colorSwatchBg(color) {
  const map = {
    theme: "var(--accent)",
    yellow: "#fdd835",
    pink: "#f48fb1",
    purple: "#ce93d8",
    blue: "#90caf9",
    green: "#a5d6a7",
    gray: "#bdbdbd",
  };
  return map[color] || "#fdd835";
}

// ── 退出前 flush（0.16.11）────────────────────────────

/**
 * 应用退出时后端 eval 调用此函数，立即保存未写入的内容和几何。
 * 防抖计时器内的内容最多 500ms 未保存，退出时强制 flush。
 */
window.__stickyFlush = async function () {
  // 清除防抖计时器，直接保存
  if (saveTimer) {
    clearTimeout(saveTimer);
    saveTimer = null;
  }
  if (geometryTimer) {
    clearTimeout(geometryTimer);
    geometryTimer = null;
  }
  // 并行保存内容和几何
  await Promise.allSettled([saveContent(), saveGeometry()]);
};

// ── Tiptap 编辑器销毁（窗口复用前清理）────────────────────

/**
 * 窗口复用（__stickyReload）前先销毁旧编辑器实例，
 * 避免多个 ProseMirror 实例争抢同一 DOM 节点。
 */
window.__stickyDestroyEditor = function () {
  if (tiptapEditor) {
    try {
      tiptapEditor.destroy();
    } catch (e) {
      console.error("[sticky] destroy editor failed:", e);
    }
    tiptapEditor = null;
  }
};

/**
 * 0.18.3 N+1：spare 回收时后端 eval 调用此函数，重置到预热态。
 * 销毁编辑器、清除计时器、重置 stickyId，然后通知后端已就绪。
 * initReady 保持 true——realStickyReload 已注册，下次 __stickyReload 可直接执行。
 */
window.__stickyReset = function () {
  if (window.__stickyDestroyEditor) {
    window.__stickyDestroyEditor();
  }
  stickyId = null;
  stickyNote = null;
  if (saveTimer) {
    clearTimeout(saveTimer);
    saveTimer = null;
  }
  if (geometryTimer) {
    clearTimeout(geometryTimer);
    geometryTimer = null;
  }
  console.log("[sticky] spare 已回收，通知后端就绪");
  // 通知后端：回收完成，可被借用
  invoke("sticky_spare_ready").catch((e) =>
    console.error("[sticky] sticky_spare_ready (recycle) 调用失败:", e)
  );
};

// ── 启动 ──────────────────────────────────────────────

init().catch((e) => console.error("[sticky] init 失败:", e));
