/**
 * chat 侧边栏（0.12.3 重新设计）。
 *
 * 对话列表渲染、切换、删除、重命名。
 *
 * 0.12.3 重新设计：
 * - push 布局（侧边栏推开主区域，非 overlay 覆盖）
 * - CSS class `data-closed` 控制显隐（不用 `hidden` 属性，支持宽度动画）
 * - 切换对话后自动关闭侧边栏
 * - 删除按钮 hover 时才显示（更干净）
 * - 重命名通过双击标题触发（内联编辑）
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

/**
 * 初始化侧边栏。
 * @param {{ onSwitch: (conversationId: string) => void, onNew: () => void }} callbacks
 */
export function initSidebar(callbacks) {
  sidebarEl = document.getElementById("chat-sidebar");
  listEl = document.getElementById("chat-sidebar-list");
  onSwitch = callbacks.onSwitch;
  onNew = callbacks.onNew;

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
    <button class="chat-sidebar-item-delete" title="删除" data-action="delete">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg>
    </button>
  </div>`;
}

/** 高亮当前活跃对话项。 */
function updateActiveHighlight() {
  if (!listEl) return;
  listEl.querySelectorAll(".chat-sidebar-item").forEach((el) => {
    el.classList.toggle("active", el.dataset.convId === activeId);
  });
}

/** 列表点击事件（切换对话 / 删除）。 */
async function handleListClick(e) {
  const item = e.target.closest(".chat-sidebar-item");
  if (!item) return;
  const convId = item.dataset.convId;
  if (!convId) return;

  // 删除按钮
  const deleteBtn = e.target.closest('[data-action="delete"]');
  if (deleteBtn) {
    e.stopPropagation();
    if (!confirm("确定删除此对话？所有消息将被清除。")) return;
    try {
      const ok = await ipc.deleteChatConversation(convId);
      if (ok) {
        await refreshSidebar();
        // 如果删的是当前对话，新建一个
        if (convId === activeId && onNew) onNew();
      }
    } catch (err) {
      console.error("[chat] 删除对话失败:", err);
    }
    return;
  }

  // 切换对话
  if (onSwitch) onSwitch(convId);
  // 自动关闭侧边栏
  hideSidebar();
}

/** 列表双击事件（重命名）。 */
async function handleListDblClick(e) {
  const item = e.target.closest(".chat-sidebar-item");
  if (!item) return;
  if (e.target.closest('[data-action="delete"]')) return;

  const convId = item.dataset.convId;
  if (!convId) return;

  // 获取当前标题
  const titleEl = item.querySelector(".chat-sidebar-item-title");
  if (!titleEl) return;
  const oldTitle = titleEl.textContent;

  // 内联编辑
  const input = document.createElement("input");
  input.type = "text";
  input.value = oldTitle;
  input.className = "chat-sidebar-rename-input";
  titleEl.replaceWith(input);
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
      } catch (err) {
        console.error("[chat] 重命名失败:", err);
      }
    } else {
      // 取消，恢复原 span
      input.replaceWith(titleEl);
    }
  };

  // Enter 确认 / Escape 取消
  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      finishEdit();
    } else if (ev.key === "Escape") {
      confirmed = true;
      input.replaceWith(titleEl);
    }
  });

  // 失焦确认
  input.addEventListener("blur", finishEdit);
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
