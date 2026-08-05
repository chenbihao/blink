/**
 * 便签窗口入口（0.16.8）。
 *
 * 从 URL 参数读取 sticky_id，从后端拉取便签数据，初始化编辑/自动保存/颜色/生命周期。
 * 编辑逻辑在前端，后端只做数据持久化和窗口管理。
 *
 * 设计见 phases/0.16-clipboard-polish.md §5.9。
 */

import { applyThemeFromConfig } from "../shared/theme.js";
import { ensureSpriteLoaded } from "../shared/icon.js";
import { getCurrentWindow, confirmDialog, listen } from "../shared/tauri.js";
import { EVENTS } from "../shared/event-names.js";
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

// ── DOM 引用 ──────────────────────────────────────────

const rootEl = document.getElementById("sticky-root");
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

// ── 初始化 ────────────────────────────────────────────

async function init() {
  // 主题 + 图标
  await ensureSpriteLoaded();
  await applyThemeFromConfig();

  // 从 URL 参数读取 sticky_id
  const params = new URLSearchParams(window.location.search);
  stickyId = params.get("id");

  if (!stickyId) {
    console.error("[sticky] 未提供 sticky_id");
    return;
  }

  // 从后端拉取便签数据
  await loadStickyData();

  // 绑定事件
  bindEditing();
  bindColorPalette();
  bindMoreMenu();
  bindWindowControls();
  bindGeometryTracking();
  bindKeyboard();
  bindContextMenu();

  // 内容变更：只在用户未在编辑时刷新，避免打断输入
  listen(EVENTS.STICKY_CONTENT_CHANGED, (event) => {
    const payload = event.payload;
    if (payload && payload.stickyId === stickyId) {
      // 用户正在编辑时不 reload，避免覆盖输入
      const activeEl = document.activeElement;
      const isEditing = activeEl === textareaEl;
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

  // 注册窗口复用回调
  window.__stickyReload = (id) => {
    stickyId = id;
    loadStickyData();
  };

  console.log("[sticky] init 完成");
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
    textareaEl.focus();
  } catch (e) {
    console.error("[sticky] 加载便签数据失败:", e);
  }
}

// ── 内容读写 ───────────────────────────────────────

/** 获取当前便签内容 */
function getContent() {
  return textareaEl.value;
}

/** 设置便签内容 */
function setContent(text) {
  textareaEl.value = text;
}

// ── 编辑与自动保存 ────────────────────────────────────

function bindEditing() {
  textareaEl.addEventListener("input", () => {
    scheduleSave();
  });
}

/** 安排防抖保存 */
function scheduleSave() {
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
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
        format: "plain",
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
        format: "plain",
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

// ── 启动 ──────────────────────────────────────────────

init().catch((e) => console.error("[sticky] init 失败:", e));
