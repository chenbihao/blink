/**
 * chat 侧边栏（0.12.3 重新设计）。
 *
 * 对话列表渲染、切换、删除、重命名。
 *
 * 0.12.3 重新设计：
 * - push 布局（侧边栏推开主区域，非 overlay 覆盖）
 * - CSS class `data-closed` 控制显隐（不用 `hidden` 属性，支持宽度动画）
 * - 切换对话后自动关闭侧边栏
 * - 0.12.4 §6.3：删除按钮常驻弱可见 + 内联确认替代 confirm()
 * - 0.12.4 §6.4：重命名按钮（删除按钮左边）
 * - 重命名通过双击标题触发（内联编辑，保留为快捷方式）
 */

import * as ipc from "./ipc.js";

/** @type {HTMLElement} 侧边栏容器 */
let sidebarEl = null;

/** @type {HTMLElement} 对话列表容器 */
let listEl = null;

/** @type {string|null} 当前活跃对话 id */
let activeId = null;

/** @type {(conversationId: string) => void} 切换对话回调 */
let onSwitch = null;

/** @type {() => void} 新对话回调 */
let onNew = null;

/** @type {(conversationId: string, newTitle: string) => void} 重命名回调 */
let onRenamed = null;

/** @type {(conversationId: string, title: string) => void} 导出回调（0.12.5 §5.6） */
let onExport = null;

/**
 * 初始化侧边栏。
 * @param {{ onSwitch: (conversationId: string) => void, onNew: () => void, onRenamed: (conversationId: string, newTitle: string) => void }} callbacks
 */
export function initSidebar(callbacks) {
  sidebarEl = document.getElementById("chat-sidebar");
  listEl = document.getElementById("chat-sidebar-list");
  onSwitch = callbacks.onSwitch;
  onNew = callbacks.onNew;
  onRenamed = callbacks.onRenamed;
  onExport = callbacks.onExport;

  // 新对话按钮
  const newBtn = document.getElementById("chat-sidebar-new");
  if (newBtn) {
    newBtn.addEventListener("click", () => {
      if (onNew) onNew();
    });
  }

  // 列表项事件委托
  if (listEl) {
    listEl.addEventListener("click", handleListClick);
    listEl.addEventListener("dblclick", handleListDblClick);
  }
}

/** 显示侧边栏。 */
export function showSidebar() {
  if (!sidebarEl) return;
  sidebarEl.removeAttribute("data-closed");
}

/** 隐藏侧边栏。 */
export function hideSidebar() {
  if (!sidebarEl) return;
  sidebarEl.setAttribute("data-closed", "");
}

/** 切换侧边栏可见性。 */
export function toggleSidebar() {
  if (!sidebarEl) return;
  if (sidebarEl.hasAttribute("data-closed")) {
    sidebarEl.removeAttribute("data-closed");
    refreshSidebar();
  } else {
    sidebarEl.setAttribute("data-closed", "");
  }
}

/** 设置当前活跃对话 id（高亮对应项）。 */
export function setActiveConversation(id) {
  activeId = id;
  updateActiveHighlight();
}

/** 刷新对话列表。 */
export async function refreshSidebar() {
  if (!listEl) return;
  try {
    const convs = await ipc.listChatConversations();
    renderList(convs);
  } catch (e) {
    console.error("[chat] 加载对话列表失败:", e);
    listEl.innerHTML = '<div class="chat-sidebar-empty">加载失败</div>';
  }
}

// ── 内部 ─────────────────────────────────────────

/**
 * 渲染对话列表。
 * @param {Array<{id: string, title: string|null, created_at: number, last_active_at: number, message_count: number}>} convs
 */
function renderList(convs) {
  if (!listEl) return;
  if (!convs || convs.length === 0) {
    listEl.innerHTML = '<div class="chat-sidebar-empty">暂无对话<br><span class="chat-sidebar-empty-hint">点击右上角 + 新建</span></div>';
    return;
  }

  listEl.innerHTML = convs.map((c) => renderConversationItem(c)).join("");
  updateActiveHighlight();
}

/**
 * 渲染单个对话项。
 */
function renderConversationItem(conv) {
  const title = conv.title || "新对话";
  const time = formatRelativeTime(conv.last_active_at);
  const count = conv.message_count ?? 0;
  const isActive = conv.id === activeId;
  const escapedId = escapeAttr(conv.id);
  const escapedTitle = escapeText(title);
  return `<div class="chat-sidebar-item${isActive ? " active" : ""}" data-conv-id="${escapedId}">
    <div class="chat-sidebar-item-info">
      <span class="chat-sidebar-item-title">${escapedTitle}</span>
      <span class="chat-sidebar-item-meta">${count} 条 · ${escapeText(time)}</span>
    </div>
    <div class="chat-sidebar-item-actions">
      <button class="chat-sidebar-item-export" title="导出" data-action="export">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>
      </button>
      <button class="chat-sidebar-item-rename" title="重命名" data-action="rename">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/></svg>
      </button>
      <button class="chat-sidebar-item-delete" title="删除" data-action="delete">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg>
      </button>
    </div>
  </div>`;
}

/** 高亮当前活跃对话项。 */
function updateActiveHighlight() {
  if (!listEl) return;
  listEl.querySelectorAll(".chat-sidebar-item").forEach((el) => {
    el.classList.toggle("active", el.dataset.convId === activeId);
  });
}

/** 列表点击事件（切换对话 / 删除 / 重命名）。 */
async function handleListClick(e) {
  const item = e.target.closest(".chat-sidebar-item");
  if (!item) return;
  const convId = item.dataset.convId;
  if (!convId) return;

  // 导出按钮（0.12.5 §5.6）
  const exportBtn = e.target.closest('[data-action="export"]');
  if (exportBtn) {
    e.stopPropagation();
    if (onExport) {
      const titleEl = item.querySelector(".chat-sidebar-item-title");
      onExport(convId, titleEl?.textContent || "");
    }
    return;
  }

  // 重命名按钮
  const renameBtn = e.target.closest('[data-action="rename"]');
  if (renameBtn) {
    e.stopPropagation();
    startInlineRename(item, convId);
    return;
  }

  // 删除按钮（0.12.4 §6.3：内联确认替代 confirm()）
  const deleteBtn = e.target.closest('[data-action="delete"]');
  if (deleteBtn) {
    e.stopPropagation();
    // 第二次点击 → 真正删除
    if (item.classList.contains("confirming-delete")) {
      try {
        const ok = await ipc.deleteChatConversation(convId);
        if (ok) {
          await refreshSidebar();
          if (convId === activeId && onNew) onNew();
        }
      } catch (err) {
        console.error("[chat] 删除对话失败:", err);
      }
      return;
    }
    // 第一次点击 → 进入确认态
    item.classList.add("confirming-delete");
    const actionsEl = item.querySelector(".chat-sidebar-item-actions");
    if (actionsEl) {
      actionsEl.innerHTML = `
        <button class="chat-sidebar-item-cancel-delete" data-action="cancel-delete">取消</button>
        <button class="chat-sidebar-item-confirm-delete" data-action="confirm-delete">删除</button>
      `;
    }
    // 3 秒超时自动取消
    const timeoutId = setTimeout(() => {
      if (item.classList.contains("confirming-delete")) {
        cancelDeleteConfirm(item);
      }
    }, 3000);
    item._deleteTimeoutId = timeoutId;
    return;
  }

  // 取消删除
  const cancelBtn = e.target.closest('[data-action="cancel-delete"]');
  if (cancelBtn) {
    e.stopPropagation();
    cancelDeleteConfirm(item);
    return;
  }

  // 确认删除
  const confirmBtn = e.target.closest('[data-action="confirm-delete"]');
  if (confirmBtn) {
    e.stopPropagation();
    try {
      const ok = await ipc.deleteChatConversation(convId);
      if (ok) {
        await refreshSidebar();
        if (convId === activeId && onNew) onNew();
      }
    } catch (err) {
      console.error("[chat] 删除对话失败:", err);
    }
    return;
  }

  // 切换对话（0.12.4：不自动关闭侧边栏，保持与新建对话行为一致）
  if (onSwitch) onSwitch(convId);
}

/** 取消删除确认态，恢复正常的重命名+删除按钮。 */
function cancelDeleteConfirm(item) {
  if (item._deleteTimeoutId) {
    clearTimeout(item._deleteTimeoutId);
    item._deleteTimeoutId = null;
  }
  item.classList.remove("confirming-delete");
  const actionsEl = item.querySelector(".chat-sidebar-item-actions");
  if (actionsEl) {
    actionsEl.innerHTML = `
      <button class="chat-sidebar-item-export" title="导出" data-action="export">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>
      </button>
      <button class="chat-sidebar-item-rename" title="重命名" data-action="rename">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/></svg>
      </button>
      <button class="chat-sidebar-item-delete" title="删除" data-action="delete">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg>
      </button>
    `;
  }
}

/** 启动内联重命名（复用双击重命名逻辑）。 */
function startInlineRename(item, convId) {
  const titleEl = item.querySelector(".chat-sidebar-item-title");
  if (!titleEl) return;
  const oldTitle = titleEl.textContent;

  const input = document.createElement("input");
  input.type = "text";
  input.value = oldTitle;
  input.className = "chat-sidebar-rename-input";
  titleEl.replaceWith(input);
  // 阻止 click/mousedown 冒泡到 handleListClick，否则会触发对话切换
  input.addEventListener("click", (e) => e.stopPropagation());
  input.addEventListener("mousedown", (e) => e.stopPropagation());
  input.focus();
  input.select();

  let confirmed = false;

  const finishEdit = async () => {
    if (confirmed) return;
    confirmed = true;
    const newTitle = input.value.trim();
    if (newTitle && newTitle !== oldTitle) {
      try {
        await ipc.renameChatConversation(convId, newTitle);
        await refreshSidebar();
        // 同步给主窗口（更新 header 标题）
        if (onRenamed) onRenamed(convId, newTitle);
      } catch (err) {
        console.error("[chat] 重命名失败:", err);
      }
    } else {
      input.replaceWith(titleEl);
    }
  };

  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      finishEdit();
    } else if (ev.key === "Escape") {
      confirmed = true;
      input.replaceWith(titleEl);
    }
  });

  input.addEventListener("blur", finishEdit);
}

/** 列表双击事件（重命名，保留为快捷方式）。 */
async function handleListDblClick(e) {
  const item = e.target.closest(".chat-sidebar-item");
  if (!item) return;
  if (e.target.closest('[data-action="delete"]')) return;
  if (e.target.closest('[data-action="rename"]')) return;
  if (e.target.closest('[data-action="export"]')) return;
  if (e.target.closest('.confirming-delete')) return;

  const convId = item.dataset.convId;
  if (!convId) return;

  startInlineRename(item, convId);
}

/**
 * 格式化相对时间（如"3分钟前"、"2小时前"、"昨天"）。
 * @param {number} timestamp Unix 秒
 */
function formatRelativeTime(timestamp) {
  const now = Date.now() / 1000;
  const diff = now - timestamp;
  if (diff < 60) return "刚刚";
  if (diff < 3600) return `${Math.floor(diff / 60)}分钟前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}小时前`;
  if (diff < 86400 * 2) return "昨天";
  if (diff < 86400 * 7) return `${Math.floor(diff / 86400)}天前`;
  const d = new Date(timestamp * 1000);
  return `${d.getMonth() + 1}/${d.getDate()}`;
}

/** HTML 转义（防 XSS）。 */
function escapeText(text) {
  const div = document.createElement("div");
  div.textContent = String(text ?? "");
  return div.innerHTML;
}

/** 属性转义（用于 data-* 属性）。 */
function escapeAttr(text) {
  return String(text ?? "").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
