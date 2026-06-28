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
import { invoke } from "./tauri.js";

/** 每页条数（对齐 Alt+1~9：每页都能用数字键选中）。 */
let PAGE_SIZE = 9;

/** 融合后最大显示条数（AppConfig.max_results）。
 *  后端单次 emit 已截断，但前端 merge 多次增量会累积，故此处对 allItems 再做最终上限。
 *  启动读一次 + lifecycle shown 时 refreshMaxResults 刷新（设置页改动下次唤起生效）。 */
let maxResults = 50;

(async function loadConfig() {
  try {
    const cfg = await invoke("get_config");
    if (cfg) {
      if (cfg.max_results) maxResults = cfg.max_results;
      if (cfg.page_size) PAGE_SIZE = cfg.page_size;
    }
  } catch (e) {
    /* 读失败保持默认 */
  }
})();

/** 重新读取配置（lifecycle shown 时调用）。 */
export async function refreshMaxResults() {
  try {
    const cfg = await invoke("get_config");
    if (cfg) {
      if (cfg.max_results) maxResults = cfg.max_results;
      if (cfg.page_size) PAGE_SIZE = cfg.page_size;
    }
  } catch (e) {
    /* 保持原值 */
  }
}

/** 后端返回的全部结果（数据，非 DOM）。 */
let allItems = [];
/** 当前结果所属的请求 seq；render / merge 据此协调（同一 seq 合并，新 seq 重置）。 */
let currentSeq = -1;
/** 当前页号（0-based）。 */
let page = 0;
/** 当前页内的相对选中索引（0 .. 本页条数-1）。 */
let selected = 0;

/**
 * 渲染 sync lane 首批结果（AppEntry[]）。
 * @param seq 该结果所属请求序号。
 *
 * 与 merge 共用 seq 协调：sync 首批(render)与 async 增量(merge)是两条独立异步流，
 * 对同一 seq 可能任意顺序到达。新 seq → 重置为该批；同一 seq → 并入（避免后到的
 * render 用空结果冲掉已 merge 的插件结果，反之亦然）。
 */
export function render(apps, seq) {
  const didReset = ensureSeq(seq);
  appendNew(apps, didReset);
}

/**
 * 合并 async lane 增量结果（blink://results）：append 去重，不全局重排，不打断选中。
 * @param seq 该增量所属请求序号。
 *
 * 渐进式设计（0.2 §2.3）：慢引擎结果到达后追加。若增量先于 render 到达（插件比
 * sync lane 快，进程预热后常见），以它为新基底，render 随后并入。
 */
export function merge(items, seq) {
  const didReset = ensureSeq(seq);
  appendNew(items, didReset);
}

/** 切到新 seq 时重置结果集；返回是否发生了重置。 */
function ensureSeq(seq) {
  if (seq === currentSeq) return false;
  currentSeq = seq;
  allItems = [];
  page = 0;
  selected = 0;
  return true;
}

/** 去重 append 新项；有新增或刚重置则重渲染。
 * 后端 fuse_items 已按 score 降序 + source tie-break 排好序,
 * 此处只按 score 降序重排（source 优先级已 bake 进 score）。
 * 光标行为:sort 后**不** restoreSelection——保持页内索引不变。
 * 这样 priority 新项置顶后，selected=0 自动指向新项；用户已移动的光标留在原地。
 */
function appendNew(items, didReset) {
  const seen = new Set(allItems.map(itemKey));
  let changed = false;

  // 检测空结果标记(后端发送的特殊项,score=-2 是约定的空标记)
  const emptyResultMarker = (items || []).find(x => x.score === -2);
  const hasEmptyResult = !!emptyResultMarker;
  // 过滤掉空结果标记项,不加入结果列表
  const realItems = (items || []).filter(x => x.source !== "empty_result");

  for (const item of realItems) {
    const key = itemKey(item);
    if (!seen.has(key)) {
      allItems.push(item);
      seen.add(key);
      changed = true;
    }
  }
  if (!changed && !didReset && !hasEmptyResult) return; // 无变化且非新批：不抖动
  // 增量结果到达时,只清掉对应插件的占位项(真实结果已到)。
  // 其他插件的占位保留(还在查询中)。引擎(File/StartMenu)结果不清占位。
  // 插件占位的 source 就是 plugin_id（如 "builtin.weather"），直接匹配即可。
  if ((changed || hasEmptyResult) && !didReset) {
    // 收集所有返回的插件来源（包括错误信息和空结果标记）
    const pluginsReturned = new Set(
      [...realItems, ...(hasEmptyResult ? [emptyResultMarker] : [])]
        .map((x) => x.source)
        .filter((s) => s && !["file", "start_menu", "calc"].includes(s))
    );
    // 清除对应插件的占位符
    allItems = allItems.filter((x) => {
      if (!x.is_placeholder) return true;
      return !pluginsReturned.has(x.source);
    });
  }
  // 按 score 降序排序。source 优先级已由后端 bake_source_boost 处理,
  // 不需要前端重复 sourceRank 逻辑。
  allItems.sort((a, b) => (b.score || 0) - (a.score || 0));
  // 最终上限：多次增量累积后截断到 max_results（高 score 在前，保留 top-N）
  if (allItems.length > maxResults) {
    allItems.length = maxResults;
  }
  renderPage();
}

/** 清空列表与状态。 */
export function clear() {
  allItems = [];
  currentSeq = -1;
  pageLis = [];
  liCache = new Map();
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

/** 去重键：应用按路径（小写），计算结果按名，占位按名，其余按 kind+名+描述。 */
function itemKey(item) {
  if (item.lnk_path) return "open:" + item.lnk_path.toLowerCase();
  if (item.is_calc) return "calc:" + item.name;
  if (item.is_placeholder) return "placeholder:" + item.name;
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

/** key → 已构建的 <li> 节点缓存,跨 renderPage 复用,避免图标等 DOM 重建导致的白闪。 */
let liCache = new Map();

function hasNextPage() {
  return (page + 1) * PAGE_SIZE < allItems.length;
}

/** 第 p 页（0-based）的实际条数（末页可能不足 PAGE_SIZE）。 */
function pageLength(p) {
  return Math.min(PAGE_SIZE, allItems.length - p * PAGE_SIZE);
}

/**
 * 渲染当前页：按 key 复用已有 <li>（不重建未变项的 DOM，图标 img 不重新加载 → 不白闪），
 * 仅新建缺失项、更新页内 badge 编号，并按本页 key 集合重排/裁剪缓存。
 */
function renderPage() {
  const start = page * PAGE_SIZE;
  const slice = allItems.slice(start, start + PAGE_SIZE);

  const nextCache = new Map();
  const lis = slice.map((app, i) => {
    const key = itemKey(app);
    let li = liCache.get(key);
    if (!li) {
      li = createItem(app, i);
    } else {
      updateBadge(li, i); // 复用节点：页内位置可能变,刷新 Alt 角标编号
    }
    nextCache.set(key, li);
    return li;
  });

  // 用本页节点重排 DOM:replaceChildren 对已存在的子节点是移动而非重建(不触发 img
  // 重载),不在本页的旧节点被自动移除。
  resultsEl.replaceChildren(...lis);
  pageLis = lis;
  liCache = nextCache;

  resultsEl.classList.toggle("has-items", slice.length > 0);
  // selected 防越界（末页不足一页时）
  if (selected > slice.length - 1) selected = Math.max(slice.length - 1, 0);
  updateSelection();
  syncWindowSize();
}

/** 刷新某 <li> 的 Alt 数字角标编号（复用节点、页内位置变化时调用）。 */
function updateBadge(li, i) {
  const badge = li.querySelector(".item-badge");
  if (badge && i < 9) {
    badge.textContent = String(i + 1);
  } else if (badge) {
    badge.remove();
  }
}

/** @param i 页内索引（0-based）——Alt 角标编号与之对齐，每页从 1 开始。 */
function createItem(app, i) {
  const li = document.createElement("li");
  li.dataset.lnkPath = app.lnk_path;
  li.dataset.source = app.source || "";
  // 动作信息存入 dataset，激活与提示栏共用（action 由后端提供）
  if (app.action) {
    li.dataset.actionKind = app.action.kind;
    if (app.action.hint) li.dataset.actionHint = app.action.hint;
    if (app.action.payload != null) li.dataset.actionPayload = app.action.payload;
  }
  if (app.is_calc) {
    li.classList.add("calc-result");
    // 计算结果原始值（去显示用的 "= " 前缀），激活时复制
    li.dataset.calcValue = app.name.replace(/^=\s*/, "");
  }

  // 插件错误信息：橙色警告样式，不可点击
  if (app.is_error) {
    li.classList.add("error-item");
    li.dataset.isError = "true";
  }

  // 插件命中占位：加载动画+灰字
  if (app.is_placeholder) {
    li.classList.add("is-loading");
    const spinner = document.createElement("span");
    spinner.className = "loading-spinner";
    li.appendChild(spinner);
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

  // 用 mousedown 代替 click：避免拖动区域检测拦截了第一次点击（"需要双击" bug）
  li.addEventListener("mousedown", (e) => {
    if (e.button === 0) activateItem(itemData(li)); // 只响应左键
  });
  return li;
}

/** 从 <li> 读出激活/提示所需数据。 */
function itemData(li) {
  return {
    lnkPath: li.dataset.lnkPath,
    calcValue: li.dataset.calcValue,
    payload: li.dataset.actionPayload,
    isError: li.dataset.isError === "true",
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
