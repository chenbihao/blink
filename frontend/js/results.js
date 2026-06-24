//! 结果列表：分页渲染 + 选中态管理。
//!
//! 分页模型（每页固定 PAGE_SIZE 条）：只渲染当前页的 DOM，列表不滚动、不截断，
//! 由内容自然撑开高度（window-size.js 测真实 DOM 精确贴合，无需猜项高）。
//! 选中态与页号收敛在模块内部，对外只暴露语义方法。
//! 「激活某项」交给 actions.js；选中/翻页后通知 statusbar 刷新提示。

import { resultsEl } from "./dom.js";
import { syncWindowSize } from "./window-size.js";
import { activateItem } from "./actions.js";
import * as statusbar from "./statusbar.js";

/** 每页条数（对齐 Alt+1~9：每页都能用数字键选中）。 */
const PAGE_SIZE = 9;

/** 后端返回的全部结果（数据，非 DOM）。 */
let allItems = [];
/** 当前页号（0-based）。 */
let page = 0;
/** 当前页内的相对选中索引（0 .. 本页条数-1）。 */
let selected = 0;

/** 渲染结果数组（AppEntry[]）：重置到第一页。 */
export function render(apps) {
  allItems = apps;
  page = 0;
  selected = 0;
  renderPage();
}

/**
 * 合并 async lane 增量结果（blink://results）：append 去重，不全局重排，不打断当前选中。
 *
 * 渐进式设计（0.2 §2.3）：sync 首批已渲染，慢引擎结果到达后追加到列表末尾。
 * 用去重键避免与首批重复；合并后按当前选中项的 key 恢复选中位置（通常 append 不影响
 * 选中，恢复逻辑为防御性）。无新项则不重渲染，避免无谓抖动。
 */
export function merge(items) {
  if (!items || !items.length || !allItems.length) return;

  const activeKey = activeItemKey();
  const seen = new Set(allItems.map(itemKey));
  let changed = false;
  for (const item of items) {
    const key = itemKey(item);
    if (!seen.has(key)) {
      allItems.push(item);
      seen.add(key);
      changed = true;
    }
  }
  if (!changed) return;

  restoreSelection(activeKey);
  renderPage();
}

/** 清空列表与状态。 */
export function clear() {
  allItems = [];
  pageLis = [];
  page = 0;
  selected = 0;
  resultsEl.innerHTML = "";
  resultsEl.classList.remove("has-items");
  refreshStatusbar();
  syncWindowSize();
}

/** 上下移动选中（delta: -1 上 / +1 下）；到当前页边界自动翻页。 */
export function move(delta) {
  if (!allItems.length) return;
  const next = selected + delta;
  const lastIdx = pageItems().length - 1;

  if (next > lastIdx) {
    // 越过下边界：有下一页则翻页选第一个，否则停住
    if (hasNextPage()) {
      page++;
      selected = 0;
      renderPage();
    }
  } else if (next < 0) {
    // 越过上边界：有上一页则翻页选最后一条
    // （selected 必须基于目标页条数算，不能读旧页的 pageItems —— renderPage 尚未执行）
    if (page > 0) {
      page--;
      selected = pageLength(page) - 1;
      renderPage();
    }
  } else {
    selected = next;
    updateSelection(); // 页内移动：只更新高亮，不重建 DOM
  }
}

/** 向下翻一页，保持页内相对位置（夹紧到目标页实际条数）。 */
export function pageDown() {
  if (!hasNextPage()) return;
  page++;
  selected = Math.min(selected, pageLength(page) - 1);
  renderPage();
}

/** 向上翻一页，保持页内相对位置。 */
export function pageUp() {
  if (page === 0) return;
  page--;
  selected = Math.min(selected, pageLength(page) - 1);
  renderPage();
}

/** 取当前选中项的数据；无结果返回 null。 */
export function getActive() {
  const items = pageItems();
  return items[selected] ? itemData(items[selected]) : null;
}

/** 取当前页第 n 项（1-based，用于 Alt+数字）的数据；越界返回 null。 */
export function getNth(n) {
  const li = pageItems()[n - 1];
  return li ? itemData(li) : null;
}

/** 是否有结果。 */
export function hasItems() {
  return allItems.length > 0;
}

// ── 内部 ──────────────────────────────────────────────────────────────────────

/** 去重键：应用按路径（小写），计算结果按名，其余按 kind+名+描述。 */
function itemKey(item) {
  if (item.lnk_path) return "open:" + item.lnk_path.toLowerCase();
  if (item.is_calc) return "calc:" + item.name;
  return (item.action?.kind || "x") + ":" + item.name + ":" + (item.description || "");
}

/** 当前选中项（数据，从 allItems 全局索引取）的去重键；无则 null。 */
function activeItemKey() {
  const item = allItems[page * PAGE_SIZE + selected];
  return item ? itemKey(item) : null;
}

/** merge 后按 key 恢复选中位置（找不到则保持原 page/selected，renderPage 内会夹紧）。 */
function restoreSelection(key) {
  if (!key) return;
  const idx = allItems.findIndex((x) => itemKey(x) === key);
  if (idx < 0) return;
  page = Math.floor(idx / PAGE_SIZE);
  selected = idx % PAGE_SIZE;
}

/** 当前页的 DOM 列表（renderPage 时构建并缓存）。 */
let pageLis = [];

/** 当前页的 <li> 数组。 */
function pageItems() {
  return pageLis;
}

function hasNextPage() {
  return (page + 1) * PAGE_SIZE < allItems.length;
}

/** 第 p 页（0-based）的实际条数（末页可能不足 PAGE_SIZE）。 */
function pageLength(p) {
  return Math.min(PAGE_SIZE, allItems.length - p * PAGE_SIZE);
}

/** 渲染当前页：切片 → 重建 DOM → 更新高亮、提示栏、窗口高度。 */
function renderPage() {
  const start = page * PAGE_SIZE;
  const slice = allItems.slice(start, start + PAGE_SIZE);

  resultsEl.innerHTML = "";
  pageLis = slice.map((app, i) => createItem(app, i));
  pageLis.forEach((li) => resultsEl.appendChild(li));

  resultsEl.classList.toggle("has-items", slice.length > 0);
  // selected 防越界（末页不足一页时）
  if (selected > slice.length - 1) selected = Math.max(slice.length - 1, 0);
  updateSelection();
  syncWindowSize();
}

/** @param i 页内索引（0-based）——Alt 角标编号与之对齐，每页从 1 开始。 */
function createItem(app, i) {
  const li = document.createElement("li");
  li.dataset.lnkPath = app.lnk_path;
  // 动作信息存入 dataset，激活与提示栏共用（action 由后端提供）
  if (app.action) {
    li.dataset.actionKind = app.action.kind;
    if (app.action.hint) li.dataset.actionHint = app.action.hint;
  }
  if (app.is_calc) {
    li.classList.add("calc-result");
    // 计算结果原始值（去显示用的 "= " 前缀），激活时复制
    li.dataset.calcValue = app.name.replace(/^=\s*/, "");
  }

  // 图标：自定义协议按需懒加载（calc 无 lnk_path 不显示）。
  // Windows/WebView2 下 scheme 映射为 http://<scheme>.localhost/<path>
  if (!app.is_calc && app.lnk_path) {
    const img = document.createElement("img");
    img.src = "http://blink-icon.localhost/" + encodeURIComponent(app.lnk_path);
    img.className = "app-icon";
    img.alt = app.name;
    img.onerror = () => img.remove(); // 提取失败/无图标时不留破图
    li.appendChild(img);
  }

  // 正文：名称（主行）+ 描述（副行，可选）
  const body = document.createElement("div");
  body.className = "item-body";

  const name = document.createElement("span");
  name.className = "item-name";
  name.textContent = app.name;
  body.appendChild(name);

  if (app.description) {
    const desc = document.createElement("span");
    desc.className = "item-desc";
    desc.textContent = app.description;
    desc.title = app.description; // 过长省略时悬浮看全
    body.appendChild(desc);
  }
  li.appendChild(body);

  // Alt 数字角标：每页 1~9，按住 Alt 时显示（CSS 控制）
  if (i < 9) {
    const badge = document.createElement("span");
    badge.className = "item-badge";
    badge.textContent = String(i + 1);
    li.appendChild(badge);
  }

  li.addEventListener("click", () => activateItem(itemData(li)));
  return li;
}

/** 从 <li> 读出激活/提示所需数据。 */
function itemData(li) {
  return {
    lnkPath: li.dataset.lnkPath,
    calcValue: li.dataset.calcValue,
    action: {
      kind: li.dataset.actionKind,
      hint: li.dataset.actionHint,
    },
  };
}

function updateSelection() {
  pageLis.forEach((li, i) => li.classList.toggle("active", i === selected));
  refreshStatusbar();
}

/** 把当前选中项 + 翻页信息推给提示栏。 */
function refreshStatusbar() {
  const pageCount = allItems.length ? Math.ceil(allItems.length / PAGE_SIZE) : 0;
  statusbar.update(getActive(), { page: page + 1, pageCount });
}
