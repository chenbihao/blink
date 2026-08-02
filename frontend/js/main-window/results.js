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
import { invoke } from "../shared/tauri.js";
import { renderIcon } from "../shared/icon.js";

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
    } else if (item.source === "ai") {
      // 0.11.0 §3.2 去重规则：同 key 时 AI 项优先（AI 工具结果 vs 查询路径同 path）
      // 替换已存在的非 AI 项，让 AI 结果置顶展示
      const idx = allItems.findIndex((x) => itemKey(x) === key);
      if (idx >= 0 && allItems[idx].source !== "ai") {
        allItems[idx] = item;
        changed = true;
      }
    }
  }
  if (!changed && !didReset && !hasEmptyResult) return; // 无变化且非新批：不抖动
  // 增量结果到达时,只清掉对应插件的占位项(真实结果已到)。
  // 其他插件的占位保留(还在查询中)。引擎(File/StartMenu)结果不清占位。
  // 插件占位的 source 就是 plugin_id（如 "builtin.weather"），直接匹配即可。
  if ((changed || hasEmptyResult) && !didReset) {
    // 收集所有返回**真结果**的来源(含错误信息、空结果标记)。
    // **必须排除"占位本身"**——AI lane 独立 emit 一次占位(is_placeholder=true,
    // score>=0, source="ai"),若收进 pluginsReturned 会把自己刚 push 的占位清掉,
    // 导致"AI 正在回答…"这行从未出现在 UI 上(0.9.2 §6.4 tab 采纳后无反馈 bug)。
    // 但"空结果标记"(is_placeholder=true, score=-2)必须保留——它就是"我这个
    // 插件没结果,请清掉我的 loading"的信号。
    // 白名单里是"引擎来源"（sync/async 引擎），它们的结果不占 placeholder，
    // 不该被这条清理逻辑误消。ClipboardEngine (0.8.5 §6.4) 走 sync lane，加入白名单。
    const pluginsReturned = new Set(
      [...realItems, ...(hasEmptyResult ? [emptyResultMarker] : [])]
        .filter((x) => !x.is_placeholder || x.score === -2)
        .map((x) => x.source)
        .filter((s) => s && !["file", "start_menu", "calc", "clipboard"].includes(s))
    );
    // 清除对应插件的占位符 + 流式中断的 AI 残留
    //   is_placeholder=true → 常规占位清理
    //   _isStreaming=true   → 流式首 chunk 后 is_placeholder 被清除,但流未正常结束,
    //                         clear marker 到达时仍需清理(否则残留不完整 AI 文本)
    allItems = allItems.filter((x) => {
      if (!x.is_placeholder && !x._isStreaming) return true;
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

/**
 * 是否有\"用户主动交互\"的结果项（0.10.8 §11.2 方案 1）。
 *
 * 与 `hasItems()` 的区别：过滤掉 `context_aware=true` 的项
 * （空 query + Context-only 命中，由 BuiltinEngine 标记）。
 *
 * 用于 `chordEligible` 判定：仅有\"环境自动填充\"候选时视为\"用户未开始交互\"，
 * chord 提示条仍允许显示，解锁\"复制 URL 后按 Alt 触发 chord\"场景。
 */
export function hasUserItems() {
  return allItems.some((x) => !x.context_aware);
}

// ── 内部 ──────────────────────────────────────────────────────────────────────

/** 去重键：应用按路径（小写），计算结果按名，占位按名，其余按 kind+名+描述。 */
function itemKey(item) {
  if (item.lnk_path) return "open:" + item.lnk_path.toLowerCase();
  if (item.is_calc) return "calc:" + item.name;
  if (item.is_placeholder) return "placeholder:" + item.name;
  return (item.actions?.[0]?.kind || "x") + ":" + item.name + ":" + (item.description || "");
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
  // 0.16.4：图片项标记（右键菜单钉图用）
  if (app.is_image) {
    li.dataset.isImage = "true";
  }
  // 0.16.1: actions 数组整体存入 dataset（JSON），激活/提示栏/右键菜单共用
  li.dataset.actions = JSON.stringify(app.actions || []);
  // 0.8.0 §1.3 智能感知：内置动作 + Run + 携带 Context 参数 → 加 .context-aware
  //   CSS 挂左侧强调条 + badge 显示"来自剪贴板 · <预览>"
  const firstAction = app.actions?.[0];
  const isContextAware =
    app.source === "builtin" &&
    firstAction?.kind === "run" &&
    firstAction?.run_arg != null;
  if (isContextAware) {
    li.classList.add("context-aware");
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

  // 0.9.2 §6.4 + 0.11.0 §3.1:AI lane 结果——区分两种形态
  //   - AI 总结项 (is_ai_summary=true): .ai-item 完整样式(pre-wrap + 24px 徽章),item[0] 的文本回答
  //   - AI 工具结果项 (is_ai_tool_result=true): .ai-tool-item 单行 + 12px 小号 AI 图标,item[1..] 的工具 items
  //   - 占位/错误/确认卡片仍走 .ai-item(复用 24×24 徽章位,视觉稳定)
  //   为了让"占位 → 真结果"几何**完全稳定**(不跳变高度、图标位不错位):
  //   占位与真结果共用 `.ai-icon-badge` 24×24 位置——占位时内嵌一个小 spinner,
  //   真结果时展示"AI"字样。整条 li 的高度只随 name 文本行数变化。
  const isAiItem = app.source === "ai";
  const isAiSummary = app.is_ai_summary === true;
  const isAiToolResult = app.is_ai_tool_result === true;
  if (isAiItem) {
    if (isAiToolResult) {
      // 0.11.0 §3.1: AI 工具结果项——nowrap 单行 + 12px 小号 AI 图标
      li.classList.add("ai-tool-item");
    } else {
      // AI 总结项 / 占位 / 错误 / 确认卡片——完整 .ai-item 样式(24px 徽章)
      li.classList.add("ai-item");
    }
  }

  // 插件命中占位:加载动画+灰字(引擎/插件的通用占位路径)
  if (app.is_placeholder && !isAiItem) {
    li.classList.add("is-loading");
    const spinner = document.createElement("span");
    spinner.className = "loading-spinner";
    li.appendChild(spinner);
  } else if (isAiItem) {
    // AI 占位与真结果**共用**同一 24×24 徽章位:
    //   - 占位:徽章里嵌 .ai-badge-spinner 小圆环(6px),`.is-loading` 类挂在 li 上
    //   - 真结果:徽章里显示 "AI" 字样
    //   - 确认卡片:徽章里显示 Lucide triangle-alert 图标（0.10.8：从 emoji ⚠ 迁移）
    // 这样 spinner → 结果切换时,徽章外框位置/尺寸完全不动,视觉稳定。
    const isConfirm = !!app._aiConfirm;
    if (isConfirm) {
      li.classList.add("ai-confirm");
      // 存储确认数据供 Enter 处理
      li.dataset.aiConfirmActionName = app._aiConfirm.actionName;
      li.dataset.aiConfirmArguments = JSON.stringify(app._aiConfirm.arguments);
    }
    if (app.is_placeholder) li.classList.add("is-loading");
    const badge = document.createElement("span");
    badge.className = "ai-icon-badge";
    if (isConfirm) {
      badge.appendChild(renderIcon("triangle-alert", { ariaLabel: "需要确认" }));
    } else if (app.is_placeholder) {
      const spinner = document.createElement("span");
      spinner.className = "ai-badge-spinner";
      badge.appendChild(spinner);
      badge.setAttribute("aria-label", "AI 加载中");
    } else {
      badge.textContent = "AI";
      badge.setAttribute("aria-label", "AI");
    }
    li.appendChild(badge);
  }

  // 图标：自定义协议按需懒加载（calc 无 lnk_path 不显示）。
  // Windows/WebView2 下 scheme 映射为 http://<scheme>.localhost/<path>
  // 0.16.4：剪贴板图片项 is_image=true 时用 blink-clipimg 协议加载缩略图。
  if (app.is_image && app.lnk_path) {
    const img = document.createElement("img");
    img.src = "http://blink-clipimg.localhost/" + encodeURIComponent(app.lnk_path);
    img.className = "app-icon clip-thumb";
    img.alt = app.name;
    img.onerror = () => img.remove();
    li.appendChild(img);
  } else if (!app.is_calc && app.lnk_path) {
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
    // 智能感知候选（0.8.0 §1.3）：副行改成参数预览
    //   原 subtitle "用默认浏览器打开剪贴板中的 URL" 在用户复制了 URL 时反而冗余；
    //   直接展示会执行的目标（URL/路径）+ 左侧强调条 + monospace 字体，视觉更清晰。
    if (isContextAware && typeof firstAction?.run_arg === "string") {
      const raw = firstAction.run_arg;
      const preview = raw.replace(/\s+/g, " ").trim();
      desc.textContent = preview.length > 80 ? preview.slice(0, 80) + "…" : preview;
      desc.title = preview; // 悬浮看完整值
      desc.classList.add("item-desc-arg");
    } else {
      desc.textContent = app.description;
      desc.title = app.description; // 过长省略时悬浮看全
    }
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

/** 从 <li> 读出激活/提示所需数据。0.16.1: 返回 actions 数组，消费方取 actions[0]。 */
function itemData(li) {
  let actions = [];
  try {
    actions = JSON.parse(li.dataset.actions || "[]");
  } catch (e) {
    console.error("actions parse failed:", e);
  }
  // AI 确认卡片数据
  let aiConfirm = null;
  if (li.dataset.aiConfirmActionName) {
    try {
      aiConfirm = {
        actionName: li.dataset.aiConfirmActionName,
        arguments: JSON.parse(li.dataset.aiConfirmArguments || "{}"),
      };
    } catch (e) {
      console.error("aiConfirmArguments parse failed:", e);
    }
  }
  return {
    lnkPath: li.dataset.lnkPath,
    calcValue: li.dataset.calcValue,
    isError: li.dataset.isError === "true",
    aiConfirm,
    actions,
  };
}

/**
 * AI Dangerous 动作确认卡片(0.9.2 第二步)。
 * 后端 emit `blink://ai-confirm-action` → 前端替换 AI 占位为确认卡片,
 * 用户 Enter 确认执行 / Esc 取消。
 *
 * @param {{seq: number, action_name: string, action_title: string, arguments: object, danger_class: string}} payload
 */
export function showAiConfirm(payload) {
  // 替换 allItems 中的 AI 占位为确认卡片
  const idx = allItems.findIndex(
    (it) => it.source === "ai" && it.is_placeholder
  );
  const confirmItem = {
    name: payload.action_title,
    pinyinName: "",
    pinyinFull: "",
    lnkPath: "",
    isCalc: false,
    score: 0.8,
    isPlaceholder: false,
    isError: false,
    source: "ai",
    description: "Enter 确认执行 · Esc 取消",
    actions: [],
    // 确认卡片专用字段(不进后端,纯前端)
    _aiConfirm: {
      actionName: payload.action_name,
      arguments: payload.arguments,
    },
  };
  if (idx >= 0) {
    allItems[idx] = confirmItem;
  } else {
    allItems.unshift(confirmItem);
  }
  renderPage();
}

/** 获取当前 AI 确认卡片的数据(供 actions.js 调用)。 */
export function getAiConfirmData() {
  const li = pageLis[selected];
  if (!li) return null;
  return li.dataset.aiConfirmActionName
    ? {
        actionName: li.dataset.aiConfirmActionName,
        arguments: li.dataset.aiConfirmArguments,
      }
    : null;
}

/**
 * AI 流式 chunk 更新——后端逐段推送文本,前端增量替换 AI 结果项的 name。
 * @param {{seq: number, delta: string, accumulated: string, done: boolean}} payload
 */
export function updateAiStream(payload) {
  // 找 AI 占位或已有的 AI 结果项
  const idx = allItems.findIndex(
    (it) => it.source === "ai" && (it.is_placeholder || !it.is_error)
  );
  if (idx < 0) return;

  if (payload.done) {
    // 流结束——替换为完整结果项(与 ai_result_entry 对齐,支持 Copy action)
    allItems[idx] = {
      name: payload.accumulated,
      pinyinName: "",
      pinyinFull: "",
      lnkPath: "",
      isCalc: false,
      score: 0.7,
      isPlaceholder: false,
      isError: false,
      source: "ai",
      description: "回车复制回答",
      actions: [{
        kind: "copy",
        payload: payload.accumulated,
        hint: "复制回答",
      }],
    };
  } else {
    // 增量更新——只改 name,保持 placeholder 状态(前端样式靠 .is-loading 控制)
    allItems[idx].name = payload.accumulated;
    // 首次有内容时去掉 placeholder 标记(让 AI 文本样式生效)
    if (allItems[idx].is_placeholder && payload.accumulated.length > 0) {
      allItems[idx].is_placeholder = false;
      allItems[idx]._isStreaming = true; // 标记流式进行中——clear 机制依赖此标记
      allItems[idx].score = 0.7;
    }
  }
  renderPage();
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
