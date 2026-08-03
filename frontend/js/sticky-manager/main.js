/**
 * 便签管理窗口入口（0.16.10）。
 *
 * 从后端拉取便签列表，按更新时间倒序渲染。支持：
 * - 聚焦（已显示的便签）
 * - 恢复（已隐藏的便签）
 * - 编辑（打开内容编辑器）
 * - 改色
 * - 删除（二次确认，默认拒绝）
 *
 * 关闭管理窗口不影响桌面便签。
 */

import { applyThemeFromConfig } from "../shared/theme.js";
import { ensureSpriteLoaded, iconHTML } from "../shared/icon.js";
import { getCurrentWindow, confirmDialog, listen } from "../shared/tauri.js";
import {
  listStickyNotes,
  createStickyNote,
  showStickyWindow,
  setStickyVisible,
  deleteStickyNote,
  destroyStickyWindow,
  updateStickyAppearance,
  openContentEditor,
} from "../shared/api.js";
import { EVENTS } from "../shared/event-names.js";

// ── DOM 引用 ──────────────────────────────────────────

const listEl = document.getElementById("sticky-list");
const emptyEl = document.getElementById("empty-state");
const countEl = document.getElementById("sticky-count");

// ── 初始化 ────────────────────────────────────────────

async function init() {
  await ensureSpriteLoaded();
  await applyThemeFromConfig();

  bindWindowControls();
  bindButtons();
  bindEvents();

  await loadList();

  // 注册窗口复用回调
  window.__stickyManagerReload = loadList;

  // 0.16.12：新窗口由后端以 visible(false) 创建，前端 init 完成后自行 show——消除白屏闪烁。
  // 复用窗口由后端直接 show，无需前端再调。
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

// ── 数据加载 ──────────────────────────────────────────

async function loadList() {
  try {
    const notes = await listStickyNotes();
    renderList(notes);
  } catch (e) {
    console.error("[sticky-manager] 加载便签列表失败:", e);
  }
}

function renderList(notes) {
  listEl.replaceChildren();
  countEl.textContent = notes.length ? `${notes.length} 条` : "";

  if (!notes.length) {
    emptyEl.hidden = false;
    listEl.hidden = true;
    return;
  }

  emptyEl.hidden = true;
  listEl.hidden = false;

  for (const note of notes) {
    listEl.appendChild(createItem(note));
  }
}

function createItem(note) {
  const item = document.createElement("div");
  item.className = "sticky-item";
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

  const meta = document.createElement("div");
  meta.className = "sticky-meta";

  // 可见性 badge
  if (!note.visible) {
    const badge = document.createElement("span");
    badge.className = "sticky-badge hidden";
    badge.textContent = "已隐藏";
    meta.appendChild(badge);
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

  // 聚焦/恢复
  if (note.visible) {
    actions.appendChild(makeActionBtn("聚焦", "move-up-right", () => {
      showStickyWindow(note.id).catch((e) => console.error("showStickyWindow failed:", e));
    }));
  } else {
    actions.appendChild(makeActionBtn("恢复", "plus", () => {
      setStickyVisible(note.id, true)
        .then(() => showStickyWindow(note.id))
        .catch((e) => console.error("restore failed:", e));
    }));
  }

  // 编辑
  actions.appendChild(makeActionBtn("编辑", "pencil", async () => {
    await openContentEditor({
      body: note.content || "",
      format: note.format || "plain",
      title: "编辑便签内容",
      origin: "sticky",
      originRef: note.id,
      savePolicy: "sticky_update",
    }).catch((e) => console.error("openContentEditor failed:", e));
  }));

  // 改色
  actions.appendChild(makeColorBtn(note));

  // 隐藏
  if (note.visible) {
    actions.appendChild(makeActionBtn("隐藏", "eye-off", () => {
      setStickyVisible(note.id, false).catch((e) => console.error("hide failed:", e));
    }));
  }

  // 删除
  actions.appendChild(makeActionBtn("删除", "eraser", () => {
    handleDelete(note);
  }, true));

  item.appendChild(actions);
  return item;
}

// ── 操作 ──────────────────────────────────────────────

async function handleDelete(note) {
  const confirmed = await confirmDialog("确定删除此便签？删除后不可恢复。", {
    kind: "warning",
    okLabel: "删除",
    cancelLabel: "取消",
  });
  if (!confirmed) return;

  try {
    await deleteStickyNote(note.id);
    await destroyStickyWindow(note.id);
  } catch (e) {
    console.error("[sticky-manager] 删除便签失败:", e);
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
}

// ── 按钮绑定 ──────────────────────────────────────────

function bindButtons() {
  document.getElementById("btn-new-sticky").addEventListener("click", handleNewSticky);
  document.getElementById("btn-empty-new").addEventListener("click", handleNewSticky);
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

  if (minBtn) {
    minBtn.addEventListener("click", () => {
      getCurrentWindow()?.minimize();
    });
  }

  if (maxBtn) {
    maxBtn.addEventListener("click", () => {
      const win = getCurrentWindow();
      if (!win) return;
      if (win.isMaximized()) {
        win.unmaximize();
      } else {
        win.maximize();
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
