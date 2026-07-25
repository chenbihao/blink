/**
 * chat 侧边栏（0.12.3 + 0.12.6 分组）。
 *
 * 对话列表树形渲染：分组（文件夹）+ 对话（叶节点），支持多层嵌套。
 *
 * 0.12.6 分组设计：
 * - 分组层级：侧边栏按分组折叠展示，区分文件夹与对话的样式
 * - 分组操作：新建 / 重命名 / 删除（对话移至默认组）/ 系统提示词
 * - 折叠状态：DB 持久化（`expanded` 列），默认组始终展开
 * - 拖拽排序：纯 mouse 事件（兼容 WebView2，HTML5 drag API 不可靠）
 * - 管理入口：hover 按钮触发内联操作
 *
 * 数据模型：
 * - groups: [{id, name, system_prompt?, parent_id?, sort_order, expanded, created_at}]
 * - conversations: [{id, title, group_id?, message_count, last_active_at, ...}]
 *
 * 渲染结构（两栏布局）：
 * ```
 * ── 分组 ──
 * 📁 工作
 *   ├─ 📁 项目A（子分组）
 *   │   └─ 对话C
 *   └─ 对话D
 * ── 默认 ──
 * 对话A
 * 对话B
 * ```
 */

import * as ipc from "./ipc.js";

/** @type {HTMLElement} 侧边栏容器 */
let sidebarEl = null;

/** @type {HTMLElement} 对话列表容器 */
let listEl = null;

/** @type {string|null} 当前活跃对话 id */
let activeId = null;

/** @type {(conversationId: string, groupId: string|null) => void} 切换对话回调 */
let onSwitch = null;

/** @type {(groupId: string|null) => void} 新对话回调 */
let onNew = null;

/** @type {(conversationId: string, newTitle: string) => void} 重命名回调 */
let onRenamed = null;

/** @type {(conversationId: string, title: string) => void} 导出回调 */
let onExport = null;


/** SVG 图标常量（Lucide 风格） */
const ICONS = {
  chevronDown: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"/></svg>',
  chevronRight: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 18 15 12 9 6"/></svg>',
  folder: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>',
  plus: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/></svg>',
  folderPlus: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/><line x1="12" y1="11" x2="12" y2="17"/><line x1="9" y1="14" x2="15" y2="14"/></svg>',
  rename: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/></svg>',
  prompt: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>',
  trash: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg>',
  export: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/></svg>',
};


/**
 * 初始化侧边栏。
 * @param {{ onSwitch: (conversationId: string, groupId: string|null) => void, onNew: (groupId: string|null) => void, onRenamed: (conversationId: string, newTitle: string) => void, onExport: (conversationId: string, title: string) => void }} callbacks
 */
export function initSidebar(callbacks) {
  sidebarEl = document.getElementById("chat-sidebar");
  listEl = document.getElementById("chat-sidebar-list");
  onSwitch = callbacks.onSwitch;
  onNew = callbacks.onNew;
  onRenamed = callbacks.onRenamed;
  onExport = callbacks.onExport;

  // 新对话按钮（默认组）
  const newBtn = document.getElementById("chat-sidebar-new");
  if (newBtn) {
    newBtn.addEventListener("click", () => {
      if (onNew) onNew(null);
    });
  }

  // 新建分组按钮
  const newGroupBtn = document.getElementById("chat-sidebar-new-group");
  if (newGroupBtn) {
    newGroupBtn.addEventListener("click", () => {
      startNewGroupInput(null);
    });
  }

  // 列表事件委托
  if (listEl) {
    listEl.addEventListener("click", handleListClick);
    listEl.addEventListener("dblclick", handleListDblClick);
    // 拖拽排序（纯 mouse 事件，兼容 WebView2 — HTML5 drag API 在 WebView2 中不可靠）
    listEl.addEventListener("mousedown", handleMouseDown);
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

/**
 * 刷新对话列表（0.12.6：同时拉取分组 + 对话，构建树形渲染）。
 */
export async function refreshSidebar() {
  if (!listEl) return;
  try {
    const [groups, convs] = await Promise.all([
      ipc.listConversationGroups(),
      ipc.listChatConversations(),
    ]);
    renderTree(groups, convs);
  } catch (e) {
    console.error("[chat] 加载对话列表失败:", e);
    listEl.innerHTML = '<div class="chat-sidebar-empty">加载失败</div>';
  }
}

// ── 树构建 ─────────────────────────────────────────

/**
 * 构建树形结构。
 *
 * @param {Array} groups 平铺分组列表
 * @param {Array} convs 平铺对话列表
 * @returns {{defaultNode: object, topGroups: Array<object>}}
 */
function buildTree(groups, convs) {
  // 按分组 ID 索引对话
  const convsByGroup = new Map();
  for (const c of convs) {
    const gid = c.group_id || "";
    if (!convsByGroup.has(gid)) convsByGroup.set(gid, []);
    convsByGroup.get(gid).push(c);
  }

  // 按 parent_id 索引子分组
  const childrenByParent = new Map();
  for (const g of groups) {
    const pid = g.parent_id || "";
    if (!childrenByParent.has(pid)) childrenByParent.set(pid, []);
    childrenByParent.get(pid).push(g);
  }

  // 递归构建分组节点
  function buildGroupNode(group) {
    const gid = group.id;
    return {
      id: gid,
      name: group.name,
      systemPrompt: group.system_prompt || null,
      expanded: group.expanded,
      sortOrder: group.sort_order,
      conversations: convsByGroup.get(gid) || [],
      children: (childrenByParent.get(gid) || []).map(buildGroupNode),
      isUserGroup: true,
    };
  }

  // 默认分组（虚拟节点，始终展开）
  const defaultNode = {
    id: null,
    name: "默认",
    systemPrompt: null,
    expanded: true,
    conversations: convsByGroup.get("") || [],
    children: [],
    isUserGroup: false,
  };

  const topGroups = (childrenByParent.get("") || []).map(buildGroupNode);
  return { defaultNode, topGroups };
}

// ── 渲染 ───────────────────────────────────────────

/**
 * 渲染分组+对话树。
 * @param {Array} groups 分组列表
 * @param {Array} convs 对话列表
 */
function renderTree(groups, convs) {
  if (!listEl) return;
  if (groups.length === 0 && convs.length === 0) {
    listEl.innerHTML = '<div class="chat-sidebar-empty">暂无对话<br><span class="chat-sidebar-empty-hint">点击右上角 + 新建</span></div>';
    return;
  }

  const { defaultNode, topGroups } = buildTree(groups, convs);

  let html = "";

  // 1. 用户分组（文件夹）—— 顶部区域
  if (topGroups.length > 0) {
    html += '<div class="chat-sidebar-section chat-sidebar-folders-section" data-section="folders">';
    html += '<div class="chat-sidebar-section-label">分组</div>';
    for (const g of topGroups) {
      html += renderGroupNode(g, 0);
    }
    html += '</div>';
  }

  // 2. 默认分组（未分组对话）—— 底部区域，始终展开
  html += renderDefaultSection(defaultNode.conversations, topGroups.length > 0);

  listEl.innerHTML = html;
  updateActiveHighlight();
}

/**
 * 渲染默认分组区域（底部，始终展开）。
 * @param {Array} conversations 未分组对话列表
 * @param {boolean} hasFolders 是否有用户分组（用于决定是否显示分隔线）
 * @returns {string} HTML
 */
function renderDefaultSection(conversations, hasFolders) {
  let html = `<div class="chat-sidebar-default-section${hasFolders ? " has-folders-above" : ""}" data-section="default" data-group-id="">`;
  html += '<div class="chat-sidebar-default-header">';
  html += '<span class="chat-sidebar-default-label">默认</span>';
  html += '<div class="chat-sidebar-default-actions">';
  html += `<button class="chat-sidebar-group-btn" data-action="new-conv" title="新建对话">${ICONS.plus}</button>`;
  html += '</div>';
  html += '</div>';
  html += '<div class="chat-sidebar-default-items">';
  if (conversations.length === 0) {
    html += '<div class="chat-sidebar-default-empty">暂无对话</div>';
  } else {
    for (const conv of conversations) {
      html += renderConversationItem(conv, 0, "");
    }
  }
  html += '</div>';
  html += '</div>';
  return html;
}

/**
 * 构建分组操作按钮区 HTML（提取公共逻辑，供渲染和取消删除确认复用）。
 * @param {boolean} isUserGroup 是否用户分组
 * @param {string|null} systemPrompt 系统提示词
 * @returns {string} HTML
 */
function buildGroupActionsHtml(isUserGroup, systemPrompt) {
  let html = `<button class="chat-sidebar-group-btn" data-action="new-conv" title="在此分组新建对话">${ICONS.plus}</button>`;
  if (isUserGroup) {
    html += `<button class="chat-sidebar-group-btn" data-action="new-subgroup" title="新建子分组">${ICONS.folderPlus}</button>`;
    html += `<button class="chat-sidebar-group-btn" data-action="rename-group" title="重命名">${ICONS.rename}</button>`;
    // 提示词按钮：已设置时 accent 色高亮 + title 含内容预览
    const promptBtnCls = systemPrompt ? " chat-sidebar-group-btn-has-prompt" : "";
    const promptTitle = systemPrompt
      ? `编辑系统提示词：${systemPrompt.length > 60 ? systemPrompt.slice(0, 60) + "…" : systemPrompt}`
      : "设置系统提示词";
    html += `<button class="chat-sidebar-group-btn${promptBtnCls}" data-action="set-prompt" title="${escapeAttr(promptTitle)}">${ICONS.prompt}</button>`;
    html += `<button class="chat-sidebar-group-btn chat-sidebar-group-btn-danger" data-action="delete-group" title="删除分组">${ICONS.trash}</button>`;
  }
  return html;
}

/**
 * 渲染一个分组节点（递归）。
 *
 * @param {object} node 树节点 {
 *   id, name, systemPrompt, expanded, conversations, children, isUserGroup
 * }
 * @param {number} level 嵌套层级（0 = 顶层）
 * @returns {string} HTML
 */
function renderGroupNode(node, level) {
  const { id, name, expanded, conversations, children, isUserGroup, systemPrompt } = node;
  const gidAttr = id || "";
  const collapsedAttr = expanded ? "" : "data-collapsed";
  const indent = level * 16;
  const chevron = expanded ? ICONS.chevronDown : ICONS.chevronRight;
  // data-prompt 属性：存在即表示该分组有系统提示词，值存储完整内容供编辑器读取
  const promptDataAttr = systemPrompt ? ` data-prompt="${escapeAttr(systemPrompt)}"` : "";

  // 操作按钮区
  const actionsHtml = buildGroupActionsHtml(isUserGroup, systemPrompt);

  const sortOrderAttr = isUserGroup ? ` data-sort-order="${node.sortOrder ?? 0}"` : "";
  let html = `<div class="chat-sidebar-group${isUserGroup ? "" : " chat-sidebar-group-default"}" data-group-id="${escapeAttr(gidAttr)}"${collapsedAttr}${isUserGroup ? promptDataAttr : ''}${sortOrderAttr}>`;
  html += `<div class="chat-sidebar-group-header${isUserGroup ? " draggable" : ""}" style="--group-indent: ${indent}px">`;
  html += `<span class="chat-sidebar-chevron">${chevron}</span>`;
  html += `<span class="chat-sidebar-folder-icon">${ICONS.folder}</span>`;
  html += `<span class="chat-sidebar-group-name">${escapeText(name)}</span>`;
  html += `<div class="chat-sidebar-group-actions">${actionsHtml}</div>`;
  html += `</div>`;

  // 子内容区（折叠时 CSS 隐藏）
  html += `<div class="chat-sidebar-group-children">`;

  // 递归渲染子分组
  for (const cg of children) {
    html += renderGroupNode(cg, level + 1);
  }

  // 渲染对话项
  for (const conv of conversations) {
    html += renderConversationItem(conv, level + 1, gidAttr);
  }

  html += `</div></div>`;
  return html;
}

/**
 * 渲染单个对话项。
 * @param {object} conv 对话对象
 * @param {number} level 嵌套层级
 * @param {string} groupId 所属分组 ID（"" = 默认组）
 * @returns {string} HTML
 */
function renderConversationItem(conv, level, groupId) {
  const title = conv.title || "新对话";
  const time = formatRelativeTime(conv.last_active_at);
  const count = conv.message_count ?? 0;
  const isActive = conv.id === activeId;
  const escapedId = escapeAttr(conv.id);
  const escapedGid = escapeAttr(groupId);
  const escapedTitle = escapeText(title);
  const indent = level * 16;
  return `<div class="chat-sidebar-item${isActive ? " active" : ""} draggable" data-conv-id="${escapedId}" data-group-id="${escapedGid}" style="--item-indent: ${indent}px">
    <div class="chat-sidebar-item-info">
      <span class="chat-sidebar-item-title">${escapedTitle}</span>
      <span class="chat-sidebar-item-meta">${count} 条 · ${escapeText(time)}</span>
    </div>
    <div class="chat-sidebar-item-actions">
      <button class="chat-sidebar-item-export" title="导出" data-action="export">
        ${ICONS.export}
      </button>
      <button class="chat-sidebar-item-rename" title="重命名" data-action="rename">
        ${ICONS.rename}
      </button>
      <button class="chat-sidebar-item-delete" title="删除" data-action="delete">
        ${ICONS.trash}
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

// ── 事件处理（委托） ───────────────────────────────

/** 列表点击事件（委托给子处理器）。 */
async function handleListClick(e) {
  // 分组折叠/展开 + 分组操作按钮 + 分组删除确认
  if (await handleGroupClick(e)) return;
  // 对话项操作（切换/删除/重命名/导出）
  await handleConversationItemClick(e);
}

/** 分组相关点击：折叠/展开 + 操作按钮 + 删除确认。返回 true 表示已处理。 */
async function handleGroupClick(e) {
  // ── 分组折叠/展开：点击 group header（非按钮区） ──
  const groupHeader = e.target.closest(".chat-sidebar-group-header");
  if (groupHeader) {
    if (e.target.closest(".chat-sidebar-group-btn")) {
      // 按钮点击交由下方分组操作处理
    } else if (e.target.closest(".chat-sidebar-group-name") || e.target.closest(".chat-sidebar-chevron") || e.target.closest(".chat-sidebar-folder-icon")) {
      const groupEl = groupHeader.closest(".chat-sidebar-group");
      if (groupEl) toggleGroupExpand(groupEl);
      return true;
    }
  }

  // ── 分组操作按钮 ──
  const groupBtn = e.target.closest(".chat-sidebar-group-btn");
  if (groupBtn) {
    e.stopPropagation();
    const groupEl = groupBtn.closest(".chat-sidebar-group");
    const defaultSectionEl = groupBtn.closest(".chat-sidebar-default-section");
    if (!groupEl && !defaultSectionEl) return true;
    const groupId = groupEl ? (groupEl.dataset.groupId || null) : null;
    const action = groupBtn.dataset.action;

    switch (action) {
      case "new-conv":
        if (onNew) onNew(groupId);
        return true;
      case "new-subgroup":
        if (!groupEl) return true;
        startNewGroupInput(groupId, groupEl);
        return true;
      case "rename-group":
        if (!groupEl) return true;
        startRenameGroup(groupId, groupEl);
        return true;
      case "set-prompt":
        if (!groupEl) return true;
        startSetGroupPrompt(groupId, groupEl);
        return true;
      case "delete-group":
        if (!groupEl) return true;
        startDeleteGroup(groupId, groupEl);
        return true;
    }
  }

  // ── 分组删除确认/取消 ──
  const cancelGroupDeleteBtn = e.target.closest('[data-action="cancel-group-delete"]');
  if (cancelGroupDeleteBtn) {
    e.stopPropagation();
    const groupEl = cancelGroupDeleteBtn.closest(".chat-sidebar-group");
    if (groupEl) cancelGroupDeleteConfirm(groupEl);
    return true;
  }

  const confirmGroupDeleteBtn = e.target.closest('[data-action="confirm-group-delete"]');
  if (confirmGroupDeleteBtn) {
    e.stopPropagation();
    const groupEl = confirmGroupDeleteBtn.closest(".chat-sidebar-group");
    if (groupEl) {
      const gid = groupEl.dataset.groupId || null;
      if (gid) startDeleteGroup(gid, groupEl);
    }
    return true;
  }

  return false;
}

/** 对话项点击：切换/删除/重命名/导出。 */
async function handleConversationItemClick(e) {
  const item = e.target.closest(".chat-sidebar-item");
  if (!item) return;
  const convId = item.dataset.convId;
  if (!convId) return;
  const groupId = item.dataset.groupId || null;

  // 导出按钮
  if (e.target.closest('[data-action="export"]')) {
    e.stopPropagation();
    if (onExport) {
      const titleEl = item.querySelector(".chat-sidebar-item-title");
      onExport(convId, titleEl?.textContent || "");
    }
    return;
  }

  // 重命名按钮
  if (e.target.closest('[data-action="rename"]')) {
    e.stopPropagation();
    startInlineRename(item, convId);
    return;
  }

  // 删除按钮（内联确认）
  const deleteBtn = e.target.closest('[data-action="delete"]');
  if (deleteBtn) {
    e.stopPropagation();
    if (item.classList.contains("confirming-delete")) {
      try {
        const ok = await ipc.deleteChatConversation(convId);
        if (ok) {
          await refreshSidebar();
          if (convId === activeId && onNew) onNew(null);
        }
      } catch (err) {
        console.error("[chat] 删除对话失败:", err);
      }
      return;
    }
    item.classList.add("confirming-delete");
    const actionsEl = item.querySelector(".chat-sidebar-item-actions");
    if (actionsEl) {
      actionsEl.innerHTML = `
        <button class="chat-sidebar-item-cancel-delete" data-action="cancel-delete">取消</button>
        <button class="chat-sidebar-item-confirm-delete" data-action="confirm-delete">删除</button>
      `;
    }
    const timeoutId = setTimeout(() => {
      if (item.classList.contains("confirming-delete")) {
        cancelDeleteConfirm(item);
      }
    }, 3000);
    item._deleteTimeoutId = timeoutId;
    return;
  }

  // 取消删除
  if (e.target.closest('[data-action="cancel-delete"]')) {
    e.stopPropagation();
    cancelDeleteConfirm(item);
    return;
  }

  // 确认删除
  if (e.target.closest('[data-action="confirm-delete"]')) {
    e.stopPropagation();
    try {
      const ok = await ipc.deleteChatConversation(convId);
      if (ok) {
        await refreshSidebar();
        if (convId === activeId && onNew) onNew(null);
      }
    } catch (err) {
      console.error("[chat] 删除对话失败:", err);
    }
    return;
  }

  // 切换对话
  if (onSwitch) onSwitch(convId, groupId);
}

/** 列表双击事件（对话重命名，保留为快捷方式）。 */
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

// ── 分组操作 ───────────────────────────────────────

/**
 * 切换分组展开/折叠。
 * 用户分组 → 持久化到 DB（异步）；默认组 → 本地状态。
 */
function toggleGroupExpand(groupEl) {
  const groupId = groupEl.dataset.groupId || null;
  if (!groupId) return; // 默认组不再可折叠
  const isCollapsed = groupEl.hasAttribute("data-collapsed");
  const willExpand = isCollapsed;

  if (willExpand) {
    groupEl.removeAttribute("data-collapsed");
    // 更新箭头图标
    const chevron = groupEl.querySelector(".chat-sidebar-chevron");
    if (chevron) chevron.innerHTML = ICONS.chevronDown;
  } else {
    groupEl.setAttribute("data-collapsed", "");
    const chevron = groupEl.querySelector(".chat-sidebar-chevron");
    if (chevron) chevron.innerHTML = ICONS.chevronRight;
  }

  // 用户分组 → 持久化
  ipc.setGroupExpanded(groupId, willExpand).catch((e) => {
    console.warn("[chat] 持久化分组折叠状态失败:", e);
  });
}

/**
 * 新建分组内联输入。
 * @param {string|null} parentId 父分组 ID（null = 顶层）
 * @param {HTMLElement} [groupEl] 父分组元素（用于插入位置）
 */
function startNewGroupInput(parentId, groupEl) {
  // 创建输入行
  const inputRow = document.createElement("div");
  inputRow.className = "chat-sidebar-new-group-input-row";
  inputRow.innerHTML = `
    <span class="chat-sidebar-chevron">${ICONS.chevronRight}</span>
    <span class="chat-sidebar-folder-icon">${ICONS.folder}</span>
    <input type="text" class="chat-sidebar-group-name-input" placeholder="分组名称" />
  `;

  if (groupEl) {
    // 子分组：插入到 group children 顶部
    const childrenEl = groupEl.querySelector(".chat-sidebar-group-children");
    if (childrenEl) {
      childrenEl.insertBefore(inputRow, childrenEl.firstChild);
    }
  } else {
    // 顶层分组：插入到 folders section 顶部（或列表顶部）
    const foldersSection = listEl.querySelector(".chat-sidebar-folders-section");
    if (foldersSection) {
      const label = foldersSection.querySelector(".chat-sidebar-section-label");
      if (label) {
        label.insertAdjacentElement("afterend", inputRow);
      } else {
        foldersSection.insertBefore(inputRow, foldersSection.firstChild);
      }
    } else {
      listEl.insertBefore(inputRow, listEl.firstChild);
    }
  }

  const input = inputRow.querySelector("input");
  input.focus();

  let confirmed = false;

  const finishCreate = async () => {
    if (confirmed) return;
    confirmed = true;
    const name = input.value.trim();
    if (!name) {
      inputRow.remove();
      return;
    }
    try {
      const id = crypto.randomUUID();
      await ipc.createConversationGroup(id, name, parentId);
      await refreshSidebar();
    } catch (err) {
      console.error("[chat] 创建分组失败:", err);
      inputRow.remove();
    }
  };

  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      finishCreate();
    } else if (ev.key === "Escape") {
      confirmed = true;
      inputRow.remove();
    }
  });

  input.addEventListener("blur", finishCreate);
  // 阻止冒泡到 handleListClick
  input.addEventListener("click", (ev) => ev.stopPropagation());
  input.addEventListener("mousedown", (ev) => ev.stopPropagation());
}

/**
 * 重命名分组（内联编辑）。
 * @param {string} groupId
 * @param {HTMLElement} groupEl
 */
function startRenameGroup(groupId, groupEl) {
  const nameEl = groupEl.querySelector(".chat-sidebar-group-name");
  if (!nameEl) return;
  const oldName = nameEl.textContent;

  const input = document.createElement("input");
  input.type = "text";
  input.value = oldName;
  input.className = "chat-sidebar-group-name-input";
  nameEl.replaceWith(input);
  groupEl.classList.add("editing");
  input.focus();
  input.select();

  let confirmed = false;

  const finishEdit = async () => {
    if (confirmed) return;
    confirmed = true;
    const newName = input.value.trim();
    if (newName && newName !== oldName) {
      try {
        await ipc.renameConversationGroup(groupId, newName);
        await refreshSidebar();
      } catch (err) {
        console.error("[chat] 重命名分组失败:", err);
        groupEl.classList.remove("editing");
        input.replaceWith(nameEl);
      }
    } else {
      groupEl.classList.remove("editing");
      input.replaceWith(nameEl);
    }
  };

  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      finishEdit();
    } else if (ev.key === "Escape") {
      confirmed = true;
      groupEl.classList.remove("editing");
      input.replaceWith(nameEl);
    }
  });

  input.addEventListener("blur", finishEdit);
  input.addEventListener("click", (ev) => ev.stopPropagation());
  input.addEventListener("mousedown", (ev) => ev.stopPropagation());
}

/**
 * 设置/编辑分组系统提示词（内联编辑器）。
 * @param {string} groupId
 * @param {HTMLElement} groupEl
 */
function startSetGroupPrompt(groupId, groupEl) {
  // 避免重复打开
  const existing = groupEl.querySelector(".chat-sidebar-prompt-editor");
  if (existing) {
    existing.remove();
    return;
  }

  // 从 data-prompt 属性读取当前提示词内容
  const currentPrompt = groupEl.getAttribute("data-prompt") || "";

  const editor = document.createElement("div");
  editor.className = "chat-sidebar-prompt-editor";
  editor.innerHTML = `
    <textarea class="chat-sidebar-prompt-textarea" placeholder="输入分组系统提示词（留空清除）" rows="4">${escapeText(currentPrompt)}</textarea>
    <div class="chat-sidebar-prompt-editor-actions">
      <button class="chat-sidebar-prompt-save">保存</button>
      <button class="chat-sidebar-prompt-cancel">取消</button>
    </div>
  `;

  // 插入到 header 之后、children 之前
  const headerEl = groupEl.querySelector(".chat-sidebar-group-header");
  const childrenEl = groupEl.querySelector(".chat-sidebar-group-children");
  if (headerEl && childrenEl) {
    childrenEl.parentNode.insertBefore(editor, childrenEl);
  }

  groupEl.classList.add("editing");

  const textarea = editor.querySelector("textarea");
  const saveBtn = editor.querySelector(".chat-sidebar-prompt-save");
  const cancelBtn = editor.querySelector(".chat-sidebar-prompt-cancel");

  textarea.focus();

  let confirmed = false;

  const save = async () => {
    if (confirmed) return;
    confirmed = true;
    const prompt = textarea.value.trim();
    try {
      await ipc.updateConversationGroupSystemPrompt(
        groupId,
        prompt || null
      );
      editor.remove();
      await refreshSidebar();
    } catch (err) {
      console.error("[chat] 设置系统提示词失败:", err);
      confirmed = false;
    }
  };

  const cancel = () => {
    if (confirmed) return;
    confirmed = true;
    groupEl.classList.remove("editing");
    editor.remove();
  };

  saveBtn.addEventListener("click", (ev) => {
    ev.stopPropagation();
    save();
  });
  cancelBtn.addEventListener("click", (ev) => {
    ev.stopPropagation();
    cancel();
  });
  textarea.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" && (ev.ctrlKey || ev.metaKey)) {
      ev.preventDefault();
      save();
    } else if (ev.key === "Escape") {
      ev.preventDefault();
      cancel();
    }
  });
  // 阻止冒泡
  editor.addEventListener("click", (ev) => ev.stopPropagation());
  editor.addEventListener("mousedown", (ev) => ev.stopPropagation());
}

/**
 * 删除分组（内联确认）。
 * @param {string} groupId
 * @param {HTMLElement} groupEl
 */
function startDeleteGroup(groupId, groupEl) {
  // 已在确认态 → 执行删除
  if (groupEl.classList.contains("confirming-delete")) {
    ipc.deleteConversationGroup(groupId)
      .then(async (ok) => {
        if (ok) await refreshSidebar();
      })
      .catch((err) => console.error("[chat] 删除分组失败:", err));
    return;
  }

  // 进入确认态
  groupEl.classList.add("confirming-delete");
  const actionsEl = groupEl.querySelector(".chat-sidebar-group-actions");
  if (actionsEl) {
    actionsEl.innerHTML = `
      <button class="chat-sidebar-item-cancel-delete" data-action="cancel-group-delete">取消</button>
      <button class="chat-sidebar-item-confirm-delete" data-action="confirm-group-delete">删除</button>
    `;
  }

  // 3 秒超时自动取消
  const timeoutId = setTimeout(() => {
    if (groupEl.classList.contains("confirming-delete")) {
      cancelGroupDeleteConfirm(groupEl);
    }
  }, 3000);
  groupEl._deleteTimeoutId = timeoutId;
}

/** 取消分组删除确认态。 */
function cancelGroupDeleteConfirm(groupEl) {
  if (groupEl._deleteTimeoutId) {
    clearTimeout(groupEl._deleteTimeoutId);
    groupEl._deleteTimeoutId = null;
  }
  groupEl.classList.remove("confirming-delete");
  // 直接恢复操作按钮，避免全量 refreshSidebar
  const actionsEl = groupEl.querySelector(".chat-sidebar-group-actions");
  if (actionsEl) {
    const prompt = groupEl.getAttribute("data-prompt") || null;
    actionsEl.innerHTML = buildGroupActionsHtml(true, prompt);
  }
}

// ── 对话操作 ───────────────────────────────────────

/** 取消删除确认态，恢复正常的按钮。 */
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
        ${ICONS.export}
      </button>
      <button class="chat-sidebar-item-rename" title="重命名" data-action="rename">
        ${ICONS.rename}
      </button>
      <button class="chat-sidebar-item-delete" title="删除" data-action="delete">
        ${ICONS.trash}
      </button>
    `;
  }
}

/** 启动内联重命名（对话项）。 */
function startInlineRename(item, convId) {
  const titleEl = item.querySelector(".chat-sidebar-item-title");
  if (!titleEl) return;
  const oldTitle = titleEl.textContent;

  const input = document.createElement("input");
  input.type = "text";
  input.value = oldTitle;
  input.className = "chat-sidebar-rename-input";
  titleEl.replaceWith(input);
  item.classList.add("editing");
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
        if (onRenamed) onRenamed(convId, newTitle);
      } catch (err) {
        console.error("[chat] 重命名失败:", err);
      }
    } else {
      item.classList.remove("editing");
      input.replaceWith(titleEl);
    }
  };

  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      finishEdit();
    } else if (ev.key === "Escape") {
      confirmed = true;
      item.classList.remove("editing");
      input.replaceWith(titleEl);
    }
  });

  input.addEventListener("blur", finishEdit);
}

// ── 拖拽排序（纯 mouse 事件，兼容 WebView2） ─────────
//
// WebView2 的 HTML5 drag-and-drop API（dragstart/dragover/drop）在无边框 Tauri 窗口中
// 不可靠——dragstart 可能不触发。改用 mousedown/mousemove/mouseup 实现，与设置页
// AI 供应商排序的方案一致。
//
// 交互流程：
// 1. mousedown 在 .draggable 元素上 → 记录起始位置
// 2. mousemove 移动超过 4px 阈值 → 正式开始拖拽，添加 .dragging 样式
// 3. 拖拽中 → 根据鼠标位置高亮 drop target
// 4. mouseup → 执行移动/排序，清理样式

/** 拖拽状态 */
let mouseDrag = null; // { type, el, id, fromGroup, sortOrder, startY, startX, started, offsetY, offsetX }

/** 拖拽预览幽灵元素（跟随鼠标的克隆体） */
let dragGhost = null;

/** 当前高亮的 drop target */
let dragOverEl = null;

/** drop 指示线元素 */
let dropIndicator = null;

/** 阈值：鼠标移动超过此距离才正式开始拖拽（避免点击误触） */
const DRAG_THRESHOLD = 4;

/**
 * mousedown：检测拖拽源，绑定全局 mousemove/mouseup。
 * 只在 .draggable 元素上（且非操作按钮区）触发。
 */
function handleMouseDown(e) {
  // 只响应左键
  if (e.button !== 0) return;

  // 从操作按钮区发起 → 不启动拖拽
  if (e.target.closest(".chat-sidebar-group-btn") ||
      e.target.closest(".chat-sidebar-item-actions button")) return;

  // 对话项
  const item = e.target.closest(".chat-sidebar-item.draggable");
  if (item) {
    mouseDrag = {
      type: "conv",
      el: item,
      id: item.dataset.convId,
      fromGroup: item.dataset.groupId || "",
      startY: e.clientY,
      startX: e.clientX,
      started: false,
    };
    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);
    return;
  }

  // 分组 header（仅用户分组）
  const groupHeader = e.target.closest(".chat-sidebar-group-header.draggable");
  if (groupHeader) {
    const groupEl = groupHeader.closest(".chat-sidebar-group");
    if (!groupEl) return;
    mouseDrag = {
      type: "group",
      el: groupHeader,
      groupEl,
      id: groupEl.dataset.groupId,
      sortOrder: parseInt(groupEl.dataset.sortOrder || "0", 10),
      startY: e.clientY,
      startX: e.clientX,
      started: false,
    };
    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);
  }
}

/** mousemove：超过阈值开始拖拽 + 实时高亮 target。 */
function handleMouseMove(e) {
  if (!mouseDrag) return;

  // 阈值检测
  if (!mouseDrag.started) {
    if (Math.abs(e.clientY - mouseDrag.startY) < DRAG_THRESHOLD &&
        Math.abs(e.clientX - mouseDrag.startX) < DRAG_THRESHOLD) return;
    mouseDrag.started = true;
    mouseDrag.el.classList.add("dragging");
    document.body.style.cursor = "grabbing";
    document.body.style.userSelect = "none";
    // 创建跟随鼠标的幽灵预览
    createDragGhost(mouseDrag.el, e);
  }

  // 移动幽灵预览
  if (dragGhost) {
    dragGhost.style.left = (e.clientX - mouseDrag.offsetX) + "px";
    dragGhost.style.top = (e.clientY - mouseDrag.offsetY) + "px";
  }

  // 查找鼠标下的 drop target
  const target = findDropTarget(e.clientX, e.clientY);
  if (target === dragOverEl) {
    // target 未变
    // 分组排序 → 更新指示线位置（鼠标在同一 target 内移动）
    if (target && mouseDrag.type === "group") updateDropIndicator(target, e.clientY);
    return;
  }

  if (dragOverEl) {
    dragOverEl.classList.remove("drag-over");
  }
  if (target) {
    target.classList.add("drag-over");
  }
  dragOverEl = target;
  if (target) {
    // 分组排序 → 显示指示线；对话移入 → 仅高亮，无指示线
    if (mouseDrag.type === "group") updateDropIndicator(target, e.clientY);
  } else {
    removeDropIndicator();
  }
}

/**
 * 创建跟随鼠标的幽灵预览元素。
 * 克隆拖拽源的 DOM，固定定位，半透明 + 阴影。
 */
function createDragGhost(sourceEl, e) {
  const rect = sourceEl.getBoundingClientRect();
  dragGhost = sourceEl.cloneNode(true);
  dragGhost.classList.add("chat-sidebar-drag-ghost");
  dragGhost.style.position = "fixed";
  dragGhost.style.pointerEvents = "none";
  dragGhost.style.zIndex = "99999";
  dragGhost.style.width = rect.width + "px";
  // 记录鼠标相对元素左上角的偏移，使幽灵跟随鼠标自然
  mouseDrag.offsetX = e.clientX - rect.left;
  mouseDrag.offsetY = e.clientY - rect.top;
  dragGhost.style.left = (e.clientX - mouseDrag.offsetX) + "px";
  dragGhost.style.top = (e.clientY - mouseDrag.offsetY) + "px";
  document.body.appendChild(dragGhost);
}

/**
 * 根据鼠标坐标查找 drop target。
 *
 * 对话拖拽 → 只允许移入其他文件夹（分组 header / 默认区域），
 *             不支持同文件夹内排序，不匹配对话项本身。
 * 分组拖拽 → 其他分组 header（排除自身及后代），用指示线显示插入位置。
 */
function findDropTarget(x, y) {
  // 临时隐藏拖拽元素以便 elementFromPoint 穿透
  const draggingEl = mouseDrag?.el;
  const wasPointerEvents = draggingEl?.style.pointerEvents;
  if (draggingEl) draggingEl.style.pointerEvents = "none";

  const elBelow = document.elementFromPoint(x, y);

  if (draggingEl) draggingEl.style.pointerEvents = wasPointerEvents || "";

  if (!elBelow || !listEl?.contains(elBelow)) return null;

  if (mouseDrag.type === "conv") {
    // 对话只能移入其他文件夹：分组 header（排除当前所在分组）或默认区域
    const groupHeader = elBelow.closest(".chat-sidebar-group-header");
    if (groupHeader && !groupHeader.classList.contains("dragging")) {
      const targetGroupEl = groupHeader.closest(".chat-sidebar-group");
      const targetGroupId = targetGroupEl?.dataset.groupId || "";
      // 排除当前所在分组（同组内不排序）
      if (targetGroupId !== mouseDrag.fromGroup) return groupHeader;
    }

    // 默认区域（仅当对话不在默认组时才可 drop）
    if (mouseDrag.fromGroup !== "") {
      const defaultSection = elBelow.closest(".chat-sidebar-default-section");
      if (defaultSection) return defaultSection;
    }
  } else if (mouseDrag.type === "group") {
    // 分组可拖到：其他分组 header（排除自身及后代）
    const groupHeader = elBelow.closest(".chat-sidebar-group-header.draggable");
    if (groupHeader && !groupHeader.classList.contains("dragging")) {
      const targetGroupEl = groupHeader.closest(".chat-sidebar-group");
      if (targetGroupEl && !isDescendantGroup(targetGroupEl, mouseDrag.id)) {
        return groupHeader;
      }
    }
  }

  return null;
}

/** mouseup：执行 drop + 清理。 */
async function handleMouseUp(e) {
  document.removeEventListener("mousemove", handleMouseMove);
  document.removeEventListener("mouseup", handleMouseUp);

  if (!mouseDrag) return;

  const started = mouseDrag.started;

  // 清理样式
  mouseDrag.el.classList.remove("dragging");
  document.body.style.cursor = "";
  document.body.style.userSelect = "";
  // 移除幽灵预览
  if (dragGhost) { dragGhost.remove(); dragGhost = null; }
  // 移除 drop 指示线
  removeDropIndicator();

  if (dragOverEl) {
    dragOverEl.classList.remove("drag-over");
  }

  if (!started) {
    mouseDrag = null;
    dragOverEl = null;
    return;
  }

  // 执行 drop
  const target = dragOverEl;
  const drag = mouseDrag;
  mouseDrag = null;
  dragOverEl = null;

  if (!target) return;

  if (drag.type === "conv") {
    // 对话移入文件夹：drop target 只能是分组 header 或默认区域
    let targetGroupId;
    if (target.classList.contains("chat-sidebar-group-header")) {
      const groupEl = target.closest(".chat-sidebar-group");
      targetGroupId = groupEl?.dataset.groupId || "";
    } else if (target.classList.contains("chat-sidebar-default-section")) {
      targetGroupId = ""; // 默认组
    } else {
      return;
    }

    // 同组 → no-op
    if (targetGroupId === drag.fromGroup) return;

    try {
      await ipc.moveConversationToGroup(drag.id, targetGroupId || null);
      await refreshSidebar();
    } catch (err) {
      console.error("[chat] 移动对话失败:", err);
    }
  } else if (drag.type === "group") {
    if (!target.classList.contains("chat-sidebar-group-header")) return;
    const targetGroupEl = target.closest(".chat-sidebar-group");
    if (!targetGroupEl) return;
    const targetId = targetGroupEl.dataset.groupId;
    const targetSortOrder = parseInt(targetGroupEl.dataset.sortOrder || "0", 10);

    // 同分组 → no-op
    if (targetId === drag.id) return;

    try {
      await ipc.setGroupSortOrder(drag.id, targetSortOrder);
      await ipc.setGroupSortOrder(targetId, drag.sortOrder);
      await refreshSidebar();
    } catch (err) {
      console.error("[chat] 分组排序失败:", err);
    }
  }
}

/**
 * 检查 targetGroupEl 是否是 draggedGroupId 的后代分组。
 * 用于防止将分组拖入自己的子分组（循环）。
 */
function isDescendantGroup(targetGroupEl, draggedGroupId) {
  let el = targetGroupEl.parentElement;
  while (el && el !== listEl) {
    if (
      el.classList?.contains("chat-sidebar-group") &&
      el.dataset.groupId === draggedGroupId
    ) {
      return true;
    }
    el = el.parentElement;
  }
  return false;
}

/**
 * 更新 drop 指示线位置。
 * 根据鼠标 Y 坐标判断插入到 target 上方还是下方。
 */
function updateDropIndicator(target, clientY) {
  const rect = target.getBoundingClientRect();
  const isTopHalf = clientY < rect.top + rect.height / 2;
  const y = isTopHalf ? rect.top - 1 : rect.bottom + 1;

  if (!dropIndicator) {
    dropIndicator = document.createElement("div");
    dropIndicator.className = "chat-sidebar-drop-indicator";
    document.body.appendChild(dropIndicator);
  }

  dropIndicator.style.left = rect.left + "px";
  dropIndicator.style.width = rect.width + "px";
  dropIndicator.style.top = y + "px";
}

/** 移除 drop 指示线。 */
function removeDropIndicator() {
  if (dropIndicator) {
    dropIndicator.remove();
    dropIndicator = null;
  }
}

// ── 工具函数 ───────────────────────────────────────

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
