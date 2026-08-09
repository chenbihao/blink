/**
 * 便签管理窗口入口（0.16.10 / 0.17.7 回收站增强）。
 *
 * 双 tab 设计：活跃便签（trashed=false）+ 回收站（trashed=true）。
 * 活跃 tab：聚焦/恢复、编辑、改色、隐藏、删除(→回收站)
 * 回收站 tab：恢复、彻底删除、清空回收站
 *
 * 关闭管理窗口不影响桌面便签。
 */

import { applyThemeFromConfig } from "../shared/theme.js";
import { ensureSpriteLoaded, iconHTML } from "../shared/icon.js";
import { getCurrentWindow, confirmDialog, listen } from "../shared/tauri.js";
import {
  listStickyNotes,
  listTrashedStickyNotes,
  createStickyNote,
  showStickyWindow,
  setStickyVisible,
  deleteStickyNote,
  destroyStickyWindow,
  updateStickyAppearance,
  trashStickyNote,
  restoreStickyNote,
  clearTrashedStickyNotes,
  openContentEditor,
} from "../shared/api.js";
import { EVENTS } from "../shared/event-names.js";

// ── DOM 引用 ──────────────────────────────────────────

const listEl = document.getElementById("sticky-list");
const trashListEl = document.getElementById("trash-list");
const emptyEl = document.getElementById("empty-state");
const emptyTextEl = document.getElementById("empty-text");
const tabActive = document.getElementById("tab-active");
const tabTrash = document.getElementById("tab-trash");
const btnNewSticky = document.getElementById("btn-new-sticky");
const btnEmptyNew = document.getElementById("btn-empty-new");
const btnClearTrash = document.getElementById("btn-clear-trash");

// ── 状态 ──────────────────────────────────────────────

let currentTab = "active";

// ── 初始化 ────────────────────────────────────────────

async function init() {
  await ensureSpriteLoaded();
  await applyThemeFromConfig();

  bindWindowControls();
  bindButtons();
  bindTabs();
  bindEvents();

  await loadList();

  // 注册窗口复用回调
  window.__stickyManagerReload = loadList;

  const win = getCurrentWindow();
  if (win) {
    try {
      await win.show();
      await win.setFocus();
    } catch (e) {
      console.error("[sticky-manager] show window 失败:", e);
    }
  }

  console.log("[sticky-manager] init 完成");
}

// ── Tab 切换 ──────────────────────────────────────────

function bindTabs() {
  tabActive.addEventListener("click", () => switchTab("active"));
  tabTrash.addEventListener("click", () => switchTab("trash"));
}

function switchTab(tab) {
  currentTab = tab;
  tabActive.classList.toggle("active", tab === "active");
  tabTrash.classList.toggle("active", tab === "trash");
  btnNewSticky.hidden = tab === "trash";
  btnClearTrash.hidden = tab === "active";
  loadList();
}

// ── 数据加载 ──────────────────────────────────────────

async function loadList() {
  try {
    if (currentTab === "active") {
      const notes = await listStickyNotes();
      renderList(notes, false);
    } else {
      const notes = await listTrashedStickyNotes();
      renderList(notes, true);
    }
  } catch (e) {
    console.error("[sticky-manager] 加载便签列表失败:", e);
  }
}

function renderList(notes, isTrash) {
  const container = isTrash ? trashListEl : listEl;
  const otherContainer = isTrash ? listEl : trashListEl;
  otherContainer.hidden = true;
  container.hidden = false;

  container.replaceChildren();

  if (!notes.length) {
    emptyEl.hidden = false;
    emptyTextEl.textContent = isTrash ? "回收站为空" : "还没有便签";
    btnEmptyNew.hidden = isTrash;
    return;
  }

  emptyEl.hidden = true;

  for (const note of notes) {
    container.appendChild(createItem(note, isTrash));
  }
}

function createItem(note, isTrash) {
  const item = document.createElement("div");
  item.className = `sticky-item${!isTrash && !note.visible ? " is-hidden" : ""}`;
  item.dataset.id = note.id;

  // 颜色条
  const colorBar = document.createElement("div");
  colorBar.className = `sticky-color-bar ${note.color || "theme"}`;
  item.appendChild(colorBar);

  // 内容区
  const content = document.createElement("div");
  content.className = "sticky-content";

  const summary = document.createElement("div");
  summary.className = "sticky-summary";
  summary.textContent = makeSummary(note.content);
  // 0.17.7: font-style: normal 防御（中文不斜体铁则）
  summary.style.fontStyle = "normal";

  const meta = document.createElement("div");
  meta.className = "sticky-meta";
  meta.style.fontStyle = "normal";

  if (!isTrash) {
    // 活跃 tab：显示可见性 badge
    if (!note.visible) {
      const badge = document.createElement("span");
      badge.className = "sticky-badge hidden";
      badge.innerHTML = `${iconHTML("eye-off")}<span>桌面已隐藏</span>`;
      meta.appendChild(badge);
    }
  } else {
    // 回收站 tab：显示删除时间
    if (note.deletedAt) {
      const badge = document.createElement("span");
      badge.className = "sticky-badge trashed";
      badge.textContent = formatTime(note.deletedAt);
      meta.appendChild(badge);
    }
  }

  const timeText = formatTime(note.updatedAt);
  if (timeText) {
    const timeEl = document.createElement("span");
    timeEl.textContent = timeText;
    meta.appendChild(timeEl);
  }

  content.appendChild(summary);
  content.appendChild(meta);
  item.appendChild(content);

  // 操作按钮
  const actions = document.createElement("div");
  actions.className = "sticky-actions";

  if (!isTrash) {
    // 活跃 tab 操作
    if (note.visible) {
      actions.appendChild(makeActionBtn("聚焦", "move-up-right", () => {
        showStickyWindow(note.id).catch((e) => console.error("showStickyWindow failed:", e));
      }));
    } else {
      actions.appendChild(makeActionBtn("显示到桌面", "eye", () => {
        setStickyVisible(note.id, true)
          .catch((e) => console.error("show sticky failed:", e));
      }));
    }

    actions.appendChild(makeActionBtn("编辑", "pencil", async () => {
      await openContentEditor({
        body: note.content || "",
        format: "markdown",
        title: "编辑便签内容",
        origin: "sticky",
        originRef: note.id,
        savePolicy: "sticky_update",
      }).catch((e) => console.error("openContentEditor failed:", e));
    }));

    actions.appendChild(makeColorBtn(note));

    if (note.visible) {
      actions.appendChild(makeActionBtn("隐藏", "eye-off", () => {
        setStickyVisible(note.id, false).catch((e) => console.error("hide failed:", e));
      }));
    }

    // 0.17.7：删除=移入回收站（不再是永久删除）
    actions.appendChild(makeActionBtn("删除", "trash-2", () => {
      handleTrash(note);
    }, true));
  } else {
    // 回收站 tab 操作
    actions.appendChild(makeActionBtn("恢复", "rotate-ccw", () => {
      handleRestore(note);
    }));

    // 彻底删除（永久）
    actions.appendChild(makeActionBtn("彻底删除", "eraser", () => {
      handlePurge(note);
    }, true));
  }

  item.appendChild(actions);
  return item;
}

// ── 操作 ──────────────────────────────────────────────

/** 活跃 tab：移入回收站 */
async function handleTrash(note) {
  try {
    await trashStickyNote(note.id);
    await destroyStickyWindow(note.id);
  } catch (e) {
    console.error("[sticky-manager] 移入回收站失败:", e);
  }
}

/** 回收站 tab：恢复 */
async function handleRestore(note) {
  try {
    await restoreStickyNote(note.id);
    await showStickyWindow(note.id);
  } catch (e) {
    console.error("[sticky-manager] 恢复便签失败:", e);
  }
}

/** 回收站 tab：彻底删除（永久） */
async function handlePurge(note) {
  const confirmed = await confirmDialog("确定彻底删除此便签？此操作不可恢复。", {
    kind: "warning",
    okLabel: "彻底删除",
    cancelLabel: "取消",
  });
  if (!confirmed) return;

  try {
    await deleteStickyNote(note.id);
    await destroyStickyWindow(note.id);
  } catch (e) {
    console.error("[sticky-manager] 彻底删除失败:", e);
  }
}

/** 回收站 tab：清空回收站 */
async function handleClearTrash() {
  const confirmed = await confirmDialog("确定清空回收站？所有回收站中的便签将被永久删除，此操作不可恢复。", {
    kind: "warning",
    okLabel: "清空",
    cancelLabel: "取消",
  });
  if (!confirmed) return;

  try {
    await clearTrashedStickyNotes();
  } catch (e) {
    console.error("[sticky-manager] 清空回收站失败:", e);
  }
}

function makeColorBtn(note) {
  const btn = document.createElement("button");
  btn.className = "sticky-action-btn";
  btn.title = "改色";
  btn.innerHTML = iconHTML("paintbrush");

  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    showColorPicker(note, btn);
  });

  return btn;
}

/** 便签色板常量——与后端 StickyColor 枚举及 sticky.html 保持一致。 */
const STICKY_COLORS = ["theme", "yellow", "green", "blue", "pink", "purple", "gray"];

function showColorPicker(note, anchor) {
  const picker = document.createElement("div");
  picker.className = "color-picker-popup";

  const rect = anchor.getBoundingClientRect();
  picker.style.left = `${rect.left}px`;
  picker.style.top = `${rect.bottom + 4}px`;

  for (const color of STICKY_COLORS) {
    const swatch = document.createElement("button");
    swatch.className = `color-picker-swatch color-picker-${color}`;
    if (color === note.color) {
      swatch.classList.add("selected");
    }
    swatch.addEventListener("click", async () => {
      picker.remove();
      if (color === note.color) return;
      try {
        await updateStickyAppearance(note.id, color);
      } catch (e) {
        console.error("[sticky-manager] 更新颜色失败:", e);
      }
    });
    picker.appendChild(swatch);
  }

  document.body.appendChild(picker);

  // 点击外部关闭
  const close = (ev) => {
    if (!picker.contains(ev.target)) {
      picker.remove();
      document.removeEventListener("mousedown", close);
    }
  };
  setTimeout(() => document.addEventListener("mousedown", close), 0);
}

// ── 事件 ──────────────────────────────────────────────

function bindEvents() {
  listen(EVENTS.STICKY_CREATED, () => loadList());
  listen(EVENTS.STICKY_DELETED, () => loadList());
  listen(EVENTS.STICKY_VISIBILITY_CHANGED, () => loadList());
  listen(EVENTS.STICKY_APPEARANCE_CHANGED, () => loadList());
  listen(EVENTS.STICKY_CONTENT_CHANGED, () => loadList());
  listen(EVENTS.STICKY_TRASHED, () => loadList());
  listen(EVENTS.STICKY_RESTORED, () => loadList());
}

// ── 按钮绑定 ──────────────────────────────────────────

function bindButtons() {
  btnNewSticky.addEventListener("click", handleNewSticky);
  btnEmptyNew.addEventListener("click", handleNewSticky);
  btnClearTrash.addEventListener("click", handleClearTrash);
}

async function handleNewSticky() {
  try {
    const note = await createStickyNote("");
    await showStickyWindow(note.id);
  } catch (e) {
    console.error("[sticky-manager] 新建便签失败:", e);
  }
}

// ── 窗口控制 ──────────────────────────────────────────

function bindWindowControls() {
  const minBtn = document.getElementById("titlebar-minimize");
  const maxBtn = document.getElementById("titlebar-maximize");
  const closeBtn = document.getElementById("titlebar-close");

  // ESC：隐藏便签管理窗口（与设置页一致）。
  // 若有颜色选择弹窗打开，先关闭弹窗而不隐藏窗口。
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    const picker = document.querySelector(".color-picker-popup");
    if (picker) {
      picker.remove();
      return;
    }
    e.preventDefault();
    getCurrentWindow()?.hide();
  });

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
    closeBtn.addEventListener("click", () => {
      getCurrentWindow()?.close();
    });
  }
}

// ── 工具 ──────────────────────────────────────────────

/**
 * 创建 Lucide sprite 图标按钮。
 * @param {string} title 鼠标悬停提示
 * @param {string} iconName Lucide 图标名（如 "pencil" / "eraser"）
 * @param {() => void} handler 点击回调
 * @param {boolean} danger 是否为危险动作（删除等）
 */
function makeActionBtn(title, iconName, handler, danger = false) {
  const btn = document.createElement("button");
  btn.className = `sticky-action-btn${danger ? " danger" : ""}`;
  btn.title = title;
  btn.innerHTML = iconHTML(iconName);
  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    handler();
  });
  return btn;
}

function makeSummary(content) {
  if (!content) return "（空便签）";
  const firstLine = content.split("\n")[0].trim();
  if (!firstLine) return "（空便签）";
  return firstLine.length > 50 ? firstLine.slice(0, 50) + "…" : firstLine;
}

function formatTime(updatedAt) {
  if (!updatedAt) return "";
  const date = new Date(updatedAt * 1000);
  const now = new Date();
  const diff = (now - date) / 1000;
  if (diff < 60) return "刚刚";
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`;
  if (diff < 604800) return `${Math.floor(diff / 86400)} 天前`;
  return date.toLocaleDateString();
}

// ── 启动 ──────────────────────────────────────────────

init().catch((e) => console.error("[sticky-manager] init 失败:", e));
