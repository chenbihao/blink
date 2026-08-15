//! 剪贴板模式状态机（Alt+C 独占模式）。
//!
//! 与 AI 模式（Tab on Ghost 进入）、命令模式（`> ` 前缀进入）并列，
//! 构成主窗口三大独占模式之一。
//!
//! **设计动机**（0.19.15）：
//! 原 Alt+C 走 `CHORD_FILL_QUERY "剪贴板 "` → 前端填搜索框 →
//! SearchService::search → IntentRouter route → EngineTakeover → ClipboardEngine。
//! 整条链路多走路由检测 + keyword 剥离 + history 加载。
//!
//! 剪贴板模式直接 bypass SearchService pipeline：
//! Alt+C → `CHORD_ENTER_MODE { mode: "clipboard" }` → 前端进入模式 →
//! 用户输入直接调 `search_clipboard` IPC → ClipboardEngine。
//!
//! **模式切换铁则**（§3.5）：进入剪贴板模式后，搜索逻辑完全切支
//! （不触发 searchApps、不显示 ghost）。退出则完整恢复。
//!
//! **UX**：
//! - 进入：输入框清空 + placeholder 变为 "搜索剪贴板历史…" + 模式徽章
//! - 输入：每个字符直接调 searchClipboard（40ms 防抖）
//! - ESC：退出剪贴板模式 → 回到正常搜索（不隐藏窗口）
//! - 选中外壳隐藏 / SHOWN / HIDDEN：自动复位

import { queryEl, resultsEl } from "./dom.js";
import * as results from "./results.js";
import * as ghost from "./ghost.js";
import { syncWindowSize } from "./window-size.js";
import { searchClipboard, copyToClipboard, getClipboardTextBatch, recordClipboardHit, deleteClipboardItem, deleteClipboardImage } from "../shared/api.js";
import { t } from "../i18n/index.js";
import * as selection from "./clipboard-selection.js";
import { parse as parseColor } from "../shared/color.js";
import { activateItem } from "./actions.js";
import { resolveShortcutAction, findEditAction, findDeleteTarget } from "./clipboard-shortcuts.js";

// 0.20.1: 从 results.js 导入 setClipboardMode，进入/退出模式时设置标志。

/** 当前是否处于剪贴板模式。 */
let active = false;

/** 原始 placeholder（退出时恢复）。 */
let savedPlaceholder = "";

/** 防抖定时器。 */
let timer = null;

/** 请求序号（防竞态，与 search.js 独立）。 */
let seq = 0;

/** 0.20.2: 上次搜索的 trim 后 query——避免相同 query 重复搜索导致光标重置。 */
let lastQuery = "";

/** 模式徽章 DOM 元素。 */
let badgeEl = null;

// ── 状态查询 ──────────────────────────────────────────────────────────────

/** 当前是否处于剪贴板模式。 */
export function isActive() {
  return active;
}

/** 0.20.2: 当前是否有任何选中项。 */
export function hasSelection() {
  return selection.hasSelection();
}

/** 0.20.2: 当前选中项数量。 */
export function getSelectionCount() {
  return selection.selectedCount();
}

/** 0.20.2: 普通点击项时移动光标到该项（不激活/不复制）。
 *  剪贴板模式下点击只移动 active 光标，Enter 才执行复制。 */
export function setActiveByLi(li) {
  // 找到 li 在 pageLis 中的索引，调用 results.setActive
  results.setActiveByEl(li);
}

/** 0.20.2: 翻页后重新投影多选 CSS（clipboard-selected 可能因 DOM 重建丢失）。 */
export function onPageChanged() {
  refreshSelectionCss();
}

/** 0.20.2: 清空多选状态（不清 epoch/generation）。 */
export function clearSelection() {
  selection.clearSelection();
  refreshSelectionCss();
}

/** 0.20.2: 刷新列表（右键删除后调用），维持当前页号。 */
export function reloadList() {
  // 不清多选、不清 generation——只是刷新列表数据
  // 清防抖，直接重新搜索当前 query
  clearTimeout(timer);
  doSearch(lastQuery, true);
}

// ── 生命周期 ──────────────────────────────────────────────────────────────

/** 初始化：创建徽章 DOM 元素。main.js 启动时调一次。 */
export function init() {
  badgeEl = document.createElement("div");
  badgeEl.id = "clipboard-mode-badge";
  badgeEl.className = "clipboard-mode-badge hidden";
  badgeEl.innerHTML =
    '<svg class="icon"><use href="#icon-copy"/></svg><span>剪贴板</span>';
  const searchMode = document.getElementById("search-mode");
  searchMode.appendChild(badgeEl);
}

/** 复位：退出剪贴板模式（lifecycle shown/hidden 调用）。 */
export function reset() {
  if (active) {
    // 0.20.2: 窗口隐藏时清空选择状态
    selection.onWindowHidden();
    exit();
  }
}

// ── 模式切换 ──────────────────────────────────────────────────────────────

/**
 * 进入剪贴板模式。
 * 清空输入框 + 切换 placeholder + 显示徽章 + 立即拉取最近剪贴板历史。
 */
export function enter() {
  if (active) return;
  active = true;
  // 0.20.2: 进入剪贴板模式时递增 epoch，清空旧选择
  selection.onEnterMode();
  // 0.20.2: 重置 lastQuery，确保首次空 query 搜索能执行
  lastQuery = null;
  // 0.20.1: 通知 results.js 不做 maxResults 截断
  results.setClipboardMode(true);

  // 保存并切换 placeholder
  savedPlaceholder = queryEl.placeholder;
  queryEl.placeholder = t("clipboard.mode_placeholder");

  // 清空输入框
  queryEl.value = "";
  queryEl.focus();

  // 显示徽章
  if (badgeEl) {
    badgeEl.classList.remove("hidden");
  }

  // 清空 ghost + 结果
  ghost.clear();
  results.clear();

  // 标记 body——CSS 据此隐藏 chord 提示（独占模式下不显示 Alt+字母 待命列表）
  document.body.classList.add("clipboard-mode-active");

  // 立即拉取最近剪贴板历史（空 query）
  doSearch("");

  syncWindowSize();
}

/**
 * 退出剪贴板模式。
 * 恢复 placeholder + 隐藏徽章 + 清结果 + 恢复正常搜索。
 */
export function exit() {
  if (!active) return;
  active = false;
  // 0.20.2: 退出模式时递增 epoch+generation，清空选择
  selection.onExitMode();
  // 0.20.1: 恢复 results.js 的 maxResults 截断
  results.setClipboardMode(false);

  // 恢复 placeholder
  queryEl.placeholder = savedPlaceholder;

  // 隐藏徽章
  if (badgeEl) {
    badgeEl.classList.add("hidden");
  }

  // 取消防抖
  clearTimeout(timer);
  seq++;

  // 移除 body 标记——恢复 chord 提示可见性
  document.body.classList.remove("clipboard-mode-active");

  // 清空输入框 + 结果
  queryEl.value = "";
  results.clear();
  ghost.clear();

  // 恢复正常搜索的 Context Suggestion
  // 不直接调 search.fetchContextSuggestions 避免循环依赖，
  // 由调用方（keyboard.js ESC / lifecycle reset）触发
  syncWindowSize();
}

// ── 输入处理 ──────────────────────────────────────────────────────────────

/**
 * 处理输入变化，在剪贴板模式下拦截搜索。
 * 由 search.js::onInput 在搜索逻辑之前调用。
 *
 * @param {string} value 当前输入框值
 * @returns {boolean} true = 剪贴板模式已处理（search.js 应跳过搜索）；false = 非剪贴板模式（继续搜索）
 */
export function handleInput(value) {
  if (!active) return false;

  const q = value.trim();

  // 0.20.2: 相同 query（trim 后）不重新搜索——避免空格等无效输入触发重渲染
  // 导致光标重置到第一项（图片项 active 跳动的根因）
  if (q === lastQuery) {
    return true;
  }

  // 0.20.2: query 真正变化时清空选择状态（递增 generation 使旧请求失效）
  selection.onQueryChanged();

  // 0.20.3：颜色字面量优先——在剪贴板模式中输入颜色也立即返回颜色结果
  const colorResult = parseColor(q);
  if (colorResult) {
    // 构造与 ColorEngine 一致的颜色 AppEntry
    const entry = {
      name: colorResult.hex,
      description: `${colorResult.rgb} · ${colorResult.hsl}`,
      source: "color",
      score: 1.0,
      actions: [{ kind: "copy", payload: colorResult.hex }],
    };
    clearTimeout(timer);
    lastQuery = q;
    seq++;
    results.render([entry], seq);
    refreshSelectionCss();
    return true;
  }

  // 40ms 防抖（与正常搜索一致，合并极快连打）
  clearTimeout(timer);
  timer = setTimeout(() => {
    doSearch(q);
  }, 40);

  return true;
}

// ── 内部 ──────────────────────────────────────────────────────────────────

/**
 * 调用 search_clipboard IPC 并渲染结果。
 * @param {string} query 搜索词（已 trim）
 */
/**
 * 调用 search_clipboard IPC 并渲染结果。
 * @param {string} query 搜索词（已 trim）
 * @param {boolean} keepPage true 时维持当前页号（右键删除后刷新用）
 */
async function doSearch(query, keepPage = false) {
  const mySeq = ++seq;
  lastQuery = query;
  // 刷新（keepPage）时记住当前页号和光标位置
  const savedPage = keepPage ? results.getPage() : 0;
  const savedSelected = keepPage ? results.getSelectedIndex() : 0;
  try {
    const resp = await searchClipboard(query, mySeq);
    // 竞态防护：用户已输入新 query 或已退出模式
    if (mySeq !== seq) return;
    if (!active) return;
    const entries = resp.entries || [];
    results.render(entries, mySeq);
    // 0.20.2: keepPage 时恢复页号和光标位置（render 会重置 page/selected）
    if (keepPage) {
      results.restorePosition(savedPage, savedSelected);
    }
    // 0.20.2: 渲染后保存全局文本项 hitId 列表，并重投影选择 CSS
    lastTextHitIds = extractTextHitIds(entries);
    selection.reconcileAfterReorder(lastTextHitIds);
    refreshSelectionCss();
    if (keepPage) {
      console.log("[clipboard-mode] reloaded after delete, page=", savedPage, "selected=", savedSelected);
    }
  } catch (e) {
    console.error("[clipboard-mode] searchClipboard failed:", e);
  }
}

// ── 0.20.2 多选键盘处理 ────────────────────────────────────────────────────

/**
 * 剪贴板模式键盘事件拦截。
 *
 * 在 keyboard.js::onNavigation 中调用，返回 true 表示已处理（拦截）。
 *
 * 键盘规则：
 * 1. IME composition 优先，期间不接管快捷键。
 * 2. query 为空时 Ctrl+A 全选当前返回的全部文本项；
 *    query 非空时保留输入框全选。
 * 3. 存在多选时 Ctrl+C 批量复制；不存在多选时保留输入框原生复制。
 * 4. Esc 顺序：清空多选 → 退出 clipboard mode（由 keyboard.js onEscape 处理）。
 *
 * 鼠标规则（在 results.js mousedown 中由 handleMousedown 处理）：
 * - 单击文本项 → 切换选中态（不需要 Ctrl 修饰键）
 * - 单击图片项 → 只移动 active 光标，不选中
 * - Enter → 激活当前项（复制单个项）
 *
 * @param {KeyboardEvent} e
 * @returns {boolean} true = 已处理，keyboard.js 应跳过后续导航逻辑
 */
export function handleKeydown(e) {
  if (!active) return false;
  // IME 组字优先放行
  if (e.isComposing || e.keyCode === 229) return false;

  const ctrl = e.ctrlKey || e.metaKey;

  // Ctrl+A：query 为空时全选文本项
  if (ctrl && e.key === "a") {
    if (queryEl.value.trim()) {
      console.log("[clipboard-mode] Ctrl+A skipped: query non-empty");
      return false; // 非空 query 保留输入框全选
    }
    e.preventDefault();
    console.log("[clipboard-mode] Ctrl+A: selecting all", lastTextHitIds.length, "text items");
    selection.selectAll(lastTextHitIds);
    refreshSelectionCss();
    return true;
  }

  // Ctrl+C：存在多选时批量复制
  if (ctrl && e.key === "c") {
    if (!selection.hasSelection()) return false; // 无多选保留原生复制
    e.preventDefault();
    batchCopy();
    return true;
  }

  // 0.20.8：Alt+E / Alt+D / Delete 快捷操作
  const queryIsEmpty = !queryEl.value.trim();
  const shortcutAction = resolveShortcutAction(e, queryIsEmpty);
  if (shortcutAction === "edit") {
    e.preventDefault();
    handleEditActive();
    return true;
  }
  if (shortcutAction === "delete") {
    e.preventDefault();
    handleDeleteActive();
    return true;
  }

  return false;
}

// ── 0.20.2 批量原子复制 ────────────────────────────────────────────────────

/**
 * 批量原子复制：所有请求成功且 generation/epoch 仍有效后，使用 `\n` 拼接并单次写剪贴板。
 * 任一失败、取消或 epoch 失效均不修改原剪贴板。
 */
export async function batchCopy() {
  const keys = selection.getSelectedKeys();
  if (keys.length === 0) return;

  console.log("[clipboard-mode] batchCopy: starting with", keys.length, "items:", keys);
  const gen = selection.beginCopy();
  showStatusLoading(keys.length);

  try {
    // 后端单次批量查询（逐 id 查询但一次 IPC 往返，SQLite 连接池排队）
    const batchResults = await getClipboardTextBatch(keys);

    // generation / epoch 失效检查
    if (!selection.isCopyStillValid(gen)) return;

    // 检查是否有未找到的项
    const texts = [];
    for (const item of batchResults) {
      if (item.text === null || item.text === undefined) {
        // 有项未找到，整体放弃
        showStatusError(t("clipboard.copy_failed", { message: t("clipboard.item_unavailable") }));
        return;
      }
      texts.push(item.text);
    }

    // 所有成功，拼接写剪贴板
    const combined = texts.join("\n");
    await copyToClipboard(combined);

    // 再次检查 generation（写剪贴板是异步的）
    if (!selection.isCopyStillValid(gen)) return;

    // fire-and-forget 记录命中
    for (const key of keys) {
      recordClipboardHit(key).catch(() => {});
    }

    showStatusCopied(texts.length);
    // 复制成功后清空选择
    selection.clearSelection();
    refreshSelectionCss();
  } catch (e) {
    if (!selection.isCopyStillValid(gen)) return;
    console.error("[clipboard-mode] batchCopy failed:", e);
    showStatusError(e?.message || "unknown");
  }
}

// ── 0.20.2 辅助函数 ─────────────────────────────────────────────────────────

/**
 * 从结果项列表中提取文本项的 hitId（过滤图片项）。
 * @param {Array} items results.render 的 appEntries
 * @returns {string[]} hitId 列表（按全局顺序）
 */
function extractTextHitIds(items) {
  const ids = [];
  for (const item of items) {
    if (item.is_image) continue; // 图片项不参与多选
    const hitId = item.actions?.[0]?.hitId;
    if (hitId) ids.push(hitId);
  }
  return ids;
}

/** 最后一次渲染的文本项 hitId 列表（跨页全局，供 Ctrl+A 全选用）。 */
let lastTextHitIds = [];

// ── 0.20.2 鼠标多选支持 ──────────────────────────────────────────────────────

/**
 * 鼠标点击项时的多选拦截。
 * 在 results.js createItem 的 mousedown handler 中调用。
 *
 * 剪贴板模式下：
 * - 单击文本项 → 切换选中态（不需要 Ctrl 修饰键）
 * - 单击图片项 → 只移动光标，不选中（返回 false 让 results.js setActiveByLi 处理）
 * - 非剪贴板模式 → 不拦截（返回 false）
 *
 * @param {MouseEvent} e
 * @param {HTMLElement} li 被点击的 <li>
 * @returns {boolean} true = 已处理（选中/取消），false = 未处理（走 setActiveByLi 或正常激活）
 */
export function handleMousedown(e, li) {
  if (!active) return false;
  if (e.button !== 0) return false;

  // 图片项不参与多选，交给 results.js setActiveByLi 移动光标
  if (li.dataset.isImage === "true") return false;

  let hitId = null;
  try {
    const actions = JSON.parse(li.dataset.actions || "[]");
    hitId = actions[0]?.hitId;
  } catch {
    // ignore
  }
  if (!hitId) return false;

  e.preventDefault();
  e.stopPropagation();

  // 单击：切换选中态
  selection.toggleSelection(hitId);
  refreshSelectionCss();
  return true;
}

// ── 0.20.2 选择 CSS 投影 ────────────────────────────────────────────────────

/**
 * 把选中状态投影到 DOM（添加/移除 .selected class）。
 * 重渲染只重投影 CSS，不按 DOM index 保存状态。
 */
function refreshSelectionCss() {
  const lis = resultsEl.querySelectorAll("li");
  for (const li of lis) {
    // 图片项永不上 clipboard-selected
    if (li.dataset.isImage === "true") {
      li.classList.remove("clipboard-selected");
      continue;
    }
    let hitId = null;
    try {
      const actions = JSON.parse(li.dataset.actions || "[]");
      hitId = actions[0]?.hitId;
    } catch {
      // ignore
    }
    li.classList.toggle("clipboard-selected", !!hitId && selection.isSelected(hitId));
  }
  // 更新状态栏
  if (selection.hasSelection()) {
    showStatusSelected(selection.selectedCount());
  } else {
    // 恢复正常状态栏
    results.refreshStatusbar?.();
  }
}

// ── 0.20.2 状态栏反馈 ───────────────────────────────────────────────────────

/** 简单状态栏更新（直接写 DOM）。 */
function showStatusSelected(count) {
  const el = document.getElementById("statusbar");
  if (!el) return;
  el.classList.add("visible");
  el.replaceChildren();
  const span = document.createElement("span");
  span.className = "hint-primary";
  span.textContent = t("clipboard.hint_selected", { count });
  el.appendChild(span);
}

function showStatusLoading(count) {
  const el = document.getElementById("statusbar");
  if (!el) return;
  el.classList.add("visible");
  el.replaceChildren();
  const span = document.createElement("span");
  span.className = "hint-primary";
  span.textContent = t("clipboard.loading_count", { count });
  el.appendChild(span);
}

function showStatusCopied(count) {
  const el = document.getElementById("statusbar");
  if (!el) return;
  el.classList.add("visible");
  el.replaceChildren();
  const span = document.createElement("span");
  span.className = "hint-primary";
  span.textContent = t("clipboard.copied_count", { count });
  el.appendChild(span);
  // 2 秒后恢复
  setTimeout(() => {
    if (selection.hasSelection()) return;
    results.refreshStatusbar?.();
  }, 2000);
}

function showStatusError(message) {
  const el = document.getElementById("statusbar");
  if (!el) return;
  el.classList.add("visible");
  el.replaceChildren();
  const span = document.createElement("span");
  span.className = "hint-primary";
  span.textContent = t("clipboard.copy_failed", { message });
  el.appendChild(span);
  setTimeout(() => {
    results.refreshStatusbar?.();
  }, 3000);
}

// ── 0.20.8 快捷编辑/删除 ────────────────────────────────────────────────────

/**
 * 0.20.8：Alt+E — 编辑 active 文本项。
 * 从 active 项 actions 中查找 edit_text_item 并复用 activateItem 分派；
 * 图片项或无 edit_text_item action 时不误开文本编辑器。
 */
function handleEditActive() {
  const activeItem = results.getActive();
  const editAction = findEditAction(activeItem);
  if (!editAction) {
    console.log("[clipboard-mode] Alt+E: no edit_text_item action for active item");
    return;
  }
  // 复用 activateItem——它内部会处理 LazyCopy 拉取和内容编辑器打开
  activateItem({ ...activeItem, actions: [editAction] });
}

/**
 * 0.20.8：Alt+D / Delete — 删除 active 文本或图片历史。
 * 只对 source=clipboard 的 active 项复用既有单删命令；
 * 颜色降级结果等非历史项不响应。删除成功后维持 clipboard mode 并刷新当前页。
 */
async function handleDeleteActive() {
  const activeItem = results.getActive();
  const target = findDeleteTarget(activeItem);
  if (!target) {
    console.log("[clipboard-mode] Alt+D: no deletable active item");
    return;
  }

  try {
    if (target.type === "text") {
      await deleteClipboardItem(target.id);
    } else {
      await deleteClipboardImage(target.id);
    }
    console.log(`[clipboard-mode] deleted ${target.type} item: ${target.id}`);
    // 刷新列表（维持当前页号），由 reconcile 清理已不存在的多选 key
    reloadList();
  } catch (e) {
    console.error(`[clipboard-mode] delete failed for ${target.type} ${target.id}:`, e);
    // 失败时保留结果、模式和选择，显示可见错误
    showStatusError(e?.message || "delete failed");
  }
}
