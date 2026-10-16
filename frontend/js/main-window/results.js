//! 结果列表：视口渲染 + 选中态管理。
//!
//! 视口模型（每屏固定 PAGE_SIZE 条）：只渲染当前视口的 DOM，列表不滚动、不截断，
//! 由内容自然撑开高度（window-size.js 测真实 DOM 精确贴合，无需猜项高）。
//! 选中态与视口位置收敛在模块内部，对外只暴露语义方法。
//! 「激活某项」交给 actions.js；选中/翻页后通知 statusbar 刷新提示。
//!
//! 0.25.29：页对齐起点 `page` 泛化为任意条对齐的 `viewStart`——
//! - page 模式（搜索/剪贴板可选）：viewStart 每次按 PAGE_SIZE 移动；
//! - step 模式（普通搜索/剪贴板共用）：滚轮平移 viewStart ±N，表现形式（一屏条数、
//!   窗口贴合高度、Alt+1~9 指向当前显示条目）完全不变，只是数据窗口滑动。
//! 另新增流式加载配套：appendItems（追加专用路径）、removeItems（删除局部移除）。

import {listNavigation} from "./list-navigation.js";
import {resultsEl} from "./dom.js";
import {resetMaxHeight, syncWindowSize} from "./window-size.js";
import {activateItem} from "./actions.js";
import * as statusbar from "./statusbar.js";
import {renderIcon} from "../shared/icon.js";
import {createSwatch, getColorPayload, isColorEntry, parse as parseColor} from "../shared/color.js";
import * as clipboardMode from "./clipboard-mode.js";
import {entryRef, refKey, refFromLi} from './clipboard-entry.js';
import {bindClipboardPreview, bindErrorPreview, hideClipboardPreview} from "./clipboard-preview.js";

// 0.17.6: showAiConfirm / getAiConfirmData / updateAiStream 已删除。
// 主窗口 AI 改走 ChatService，流式渲染由 ai-mode.js 负责。

/** 每页条数（对齐 Alt+1~9：每页都能用数字键选中）。 */
let PAGE_SIZE = 9;

/** 融合后最大显示条数（AppConfig.max_results）。
 *  后端单次 emit 已截断，但前端 merge 多次增量会累积，故此处对 allItems 再做最终上限。
 *  lifecycle 统一在启动、shown、配置广播时刷新。
 *
 *  剪贴板流式加载使用会话总量保护，不受 maxResults 截断。 */
let maxResults = 50;

/** 0.20.1: 是否处于剪贴板模式（clipboard-mode.js 进入时设 true，退出时 false）。
 *  为 true 时，appendNew 不做 maxResults 截断——engine 内部已按单批钳制截断。 */
let clipboardModeActive = false;

/** 0.25.29: 流式加载的会话总量保护——allItems 累积上限（防 DOM 无限增长），
 *  取代后端已退役的 EFFECTIVE_LIMIT_MAX=400 一次性上限。到底后 statusbar 显示总数。 */
const MAX_LOADED_ITEMS = 2000;

/** 从已读好的 config 对象刷新最大结果数（避免重复 invoke get_config）。
 * @param {object} cfg - get_config 返回的 AppConfig 对象 */
export function refreshMaxResultsFromConfigData(cfg) {
    if (cfg) {
        const previousPageSize = PAGE_SIZE;
        if (cfg.max_results) maxResults = cfg.max_results;
        if (cfg.page_size) PAGE_SIZE = cfg.page_size;
        if (PAGE_SIZE !== previousPageSize) {
            listNavigation.reset();
            if (allItems.length) renderViewport();
        }
    }
}

/** 0.20.1: 设置/清除剪贴板模式标志（clipboard-mode.js 进入/退出时调用）。
 *  为 true 时 appendNew 不做 maxResults 截断。 */
export function setClipboardMode(active) {
    clipboardModeActive = active;
}

/** 后端返回的全部结果（数据，非 DOM）。 */
let allItems = [];
/** 当前结果所属的请求 seq；render / merge 据此协调（同一 seq 合并，新 seq 重置）。 */
let currentSeq = -1;
/** 0.25.29: 视口起始索引（0-based，任意条对齐）。
 *  整页操作按 PAGE_SIZE 平移；逐条与方向键允许非页对齐起点。 */
let viewStart = 0;
/** 当前视口内的相对选中索引（0 .. 本视口条数-1）。 */
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
    listNavigation.reset();
    currentSeq = seq;
    allItems = [];
    viewStart = 0;
    selected = 0;
    resetMaxHeight(); // 新搜索会话：归零 maxHeight，让窗口按实际内容收缩
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
        // 0.25.20：迟到占位丢弃——Tauri 的 invoke 返回与事件推送到达顺序
        // 不保证，插件查询极快时（进程已热，毫秒级）结果事件先于本 sync 批
        // 到达：结果已立为该 seq 基底，后到的占位若照常 append 将永远无人
        // 清理（清理块只看本批 items，占位自己被排除）——结果与转圈同屏。
        if (item.is_placeholder && item.score !== -2
            && allItems.some((x) => x.source === item.source && !x.is_placeholder)) {
            continue;
        }
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
    // 0.20.1: 剪贴板模式不受 maxResults 截断（流式总量由 MAX_LOADED_ITEMS 保护）
    if (!clipboardModeActive && allItems.length > maxResults) {
        allItems.length = maxResults;
    }
    renderViewport();
}

/** 清空列表与状态。 */
export function clear() {
    listNavigation.reset();
    hideClipboardPreview();
    allItems = [];
    currentSeq = -1;
    pageLis = [];
    liCache = new Map();
    viewStart = 0;
    selected = 0;
    resultsEl.innerHTML = "";
    resultsEl.classList.remove("has-items");
    resetMaxHeight(); // 清空时归零，让窗口缩回输入框高度
    refreshStatusbar();
    syncWindowSize();
}

/** 方向键始终逐条导航，越过视口边界时平移一条，与滚轮偏好无关。 */
export function move(delta) {
    if (!allItems.length) return;
    const next = selected + delta;
    if (next >= viewportLength()) scrollItems(1);
    else if (next < 0) scrollItems(-1);
    else { selected = next; updateSelection(); }
}

/** 整屏移动，保留相对高亮和非页对齐视口，末页允许不足一屏。 */
export function pageDown(pages = 1) {
    if (!hasNextPage()) return;
    const available = Math.ceil((allItems.length - viewStart) / PAGE_SIZE) - 1;
    viewStart += PAGE_SIZE * Math.min(pages, available);
    selected = Math.min(selected, viewportLength() - 1);
    renderViewport();
}

export function pageUp(pages = 1) {
    if (viewStart === 0) return;
    viewStart = Math.max(0, viewStart - PAGE_SIZE * pages);
    selected = Math.min(selected, viewportLength() - 1);
    renderViewport();
}

/** 0.25.29: step 滚轮模式——数据窗口逐条平移（delta 条，正 = 向更旧数据）。
 *  表现形式不变：仍渲染一屏 PAGE_SIZE 条；selected 保持视口内相对行（光标总可见）。
 *  平移到边界（顶/底）时停住。 */
export function scrollItems(delta) {
    if (!allItems.length || delta === 0) return;
    const maxStart = Math.max(viewStart, allItems.length - PAGE_SIZE);
    const next = Math.min(Math.max(viewStart + delta, 0), maxStart);
    if (next === viewStart) return;
    viewStart = next;
    selected = Math.min(selected, Math.max(viewportLength() - 1, 0));
    renderViewport();
}

/** 取当前选中项的数据；无结果返回 null。 */
export function getActive() {
    const items = pageItems();
    return items[selected] ? itemData(items[selected]) : null;
}

/** 0.20.2: 通过 DOM 元素设置 active 选中（剪贴板模式下点击项只移动光标，不激活）。 */
export function setActiveByEl(li) {
    const idx = pageLis.indexOf(li);
    if (idx >= 0 && idx !== selected) {
        selected = idx;
        updateSelection();
    }
}

/** 0.20.2: 获取当前页号（0-based；step 模式按 viewStart 换算）。 */
export function getPage() {
    return Math.floor(viewStart / PAGE_SIZE);
}

/** 0.20.2: 获取当前页内选中索引。 */
export function getSelectedIndex() {
    return selected;
}

/** 0.20.2: 恢复页号和光标位置（右键删除后刷新用）。 */
export function restorePosition(savedPage, savedSelected) {
    // 防越界：如果 savedPage 超出总页数，夹到最后一页
    const maxPage = allItems.length > 0 ? Math.ceil(allItems.length / PAGE_SIZE) - 1 : 0;
    viewStart = Math.min(savedPage, maxPage) * PAGE_SIZE;
    const pageLen = viewportLength();
    selected = Math.min(savedSelected, Math.max(pageLen - 1, 0));
    renderViewport();
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

/** 去重键：应用按路径（小写），计算结果按名，占位按名，颜色按 canonical hex，其余按 kind+名+描述。 */
function itemKey(item) {
    const clipboardRef = entryRef(item);
    if (clipboardRef) return 'clipboard:' + refKey(clipboardRef);
    const action = item.actions?.[0];
    if (action?.runId === "open_system_entry" && action.runArg?.entry_id) return action.runArg.entry_id;
    if (item.lnk_path) return "open:" + item.lnk_path.toLowerCase();
    if (item.is_calc) return "calc:" + item.name;
    // 0.20.3：颜色结果项按 canonical hex 去重
    if (item.source === "color") return "color:" + item.name;
    if (item.is_placeholder) return "placeholder:" + item.name;
    // 剪贴板历史项：用 hitId（= clipboard_history 表主键）做去重键，
    // 不用 name+description——preview 截断后多条可能相同（如长文件路径），导致误去重。
    const hitId = item.actions?.[0]?.hitId;
    if (hitId) return "clip:" + hitId;
    return (item.actions?.[0]?.kind || "x") + ":" + item.name + ":" + (item.description || "");
}

/** 当前选中项（数据，从 allItems 全局索引取）的去重键；无则 null。 */
function activeItemKey() {
    const item = allItems[viewStart + selected];
    return item ? itemKey(item) : null;
}

/** merge 后按 key 恢复选中位置（找不到则保持原 page/selected，renderPage 内会夹紧）。 */
function restoreSelection(key) {
    if (!key) return;
    const idx = allItems.findIndex((x) => itemKey(x) === key);
    if (idx < 0) return;
    viewStart = Math.floor(idx / PAGE_SIZE) * PAGE_SIZE;
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
    return viewStart + PAGE_SIZE < allItems.length;
}

/** 当前视口的实际条数（末屏可能不足 PAGE_SIZE）。 */
function viewportLength() {
    return Math.min(PAGE_SIZE, allItems.length - viewStart);
}

/** 0.25.29: 视口之后剩余条数是否不足一页（流式预取判据——page 模式命中末页附近、
 *  step 模式距底一屏内，都由此覆盖）。 */
export function nearTail() {
    return allItems.length - (viewStart + PAGE_SIZE) < PAGE_SIZE;
}

/** 0.25.29: 已加载总条数（流式状态机的总量保护判据）。 */
export function getLoadedCount() {
    return allItems.length;
}

/** 0.25.29: 已加载条目快照（clipboard-mode 删除后重算多选 hitId 用）。 */
export function getItems() {
    return allItems.slice();
}

/**
 * 渲染当前视口：按 key 复用已有 <li>（不重建未变项的 DOM，图标 img 不重新加载 → 不白闪），
 * 仅新建缺失项、更新页内 badge 编号，并按本页 key 集合重排/裁剪缓存。
 */
function renderViewport() {
    hideClipboardPreview();
    const slice = allItems.slice(viewStart, viewStart + PAGE_SIZE);

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

    // 0.20.2: 翻页后同步刷新剪贴板多选 CSS（clipboard-selected 可能因 DOM 重建丢失）
    resultsEl.classList.toggle("has-items", slice.length > 0);
    // selected 防越界（末页不足一页时）
    if (selected > slice.length - 1) selected = Math.max(slice.length - 1, 0);
    updateSelection();
    clipboardMode.onPageChanged?.();
    syncWindowSize();
}

/**
 * 0.25.29: 流式追加专用路径（剪贴板模式 loadMore 用）。
 *
 * 与 render/merge（appendNew）的区别——追加必须**保持既有顺序与视图位置**：
 * - 不做 score 全局重排：追加批的 score 从 0.9 头算，重排会插到已有条目前面；
 * - 不动 viewStart/selected：用户停留在当前浏览位置，新条目接在 allItems 尾部；
 * - 只 concat + 按 itemKey 去重（删除后重拉等场景防重）。
 * 总量超过 MAX_LOADED_ITEMS 时截断（调用方据此停手）。
 *
 * @returns {boolean} 是否有新增（false = 全部重复/空批）
 */
export function appendItems(items, seq) {
    if (seq !== currentSeq) return false; // 旧会话的迟到追加：丢弃
    const seen = new Set(allItems.map(itemKey));
    let added = false;
    for (const item of items || []) {
        const key = itemKey(item);
        if (seen.has(key)) continue;
        allItems.push(item);
        seen.add(key);
        added = true;
    }
    if (!added) return false;
    if (allItems.length > MAX_LOADED_ITEMS) allItems.length = MAX_LOADED_ITEMS;
    renderViewport();
    return true;
}

/**
 * 0.25.29: 删除局部移除（替代删除后全量 reloadList 的游标重置问题）。
 *
 * 按显式的 kind + id 匹配移除，跨类型同 id 不会互相删除，
 * 视口位置与光标就地夹紧——已加载几百条不退回首批。
 *
 * @param {{kind: string, id: string}[]} keys 要移除的历史条目引用
 */
export function removeItems(keys) {
    const ids = new Set([...keys].map(refKey));
    if (ids.size === 0 || !allItems.length) return;
    const before = allItems.length;
    allItems = allItems.filter((x) => {
        const ref = entryRef(x);
        if (ref && ids.has(refKey(ref))) return false;
        return true;
    });
    if (allItems.length === before) return;
    // 视口夹紧到「至少一条可见」——不重置页对齐（page 模式末页允许不满页）；
    // 后续平移保持连续，不能因满屏钳制导致向下滚动反而倒退
    const maxStart = Math.max(0, allItems.length - 1);
    viewStart = Math.min(viewStart, maxStart);
    selected = Math.min(selected, Math.max(viewportLength() - 1, 0));
    renderViewport();
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
    const clipboardRef = entryRef(app);
    if (clipboardRef) {
        li.dataset.clipboardKind = clipboardRef.kind;
        li.dataset.clipboardId = clipboardRef.id;
        bindClipboardPreview(li, clipboardRef);
    }
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
        firstAction?.runArg != null;
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
        li.dataset.errorText = [app.name, app.description].filter(Boolean).join("\n");
        bindErrorPreview(li, li.dataset.errorText);
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
            li.dataset.aiConfirmId = app._aiConfirm.confirmId;
        }
        if (app.is_placeholder) li.classList.add("is-loading");
        const badge = document.createElement("span");
        badge.className = "ai-icon-badge";
        if (isConfirm) {
            badge.appendChild(renderIcon("triangle-alert", {ariaLabel: "需要确认"}));
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

    // 0.20.3：颜色结果项——用大格子 swatch（比剪贴板图片缩略图更大一圈，
    // 颜色项里 swatch 就是内容主体，0.23.19 放大 36 → 44px 更醒目）。
    if (isColorEntry(app)) {
        const hex = getColorPayload(app) || app.name;
        const colorResult = parseColor(hex);
        if (colorResult) {
            const swatch = createSwatch(colorResult.rgba, {className: "app-icon clip-thumb", size: 44});
            li.appendChild(swatch);
        }
    }
        // 0.20：多行颜色列表——后端 is_color_list 标记 + color_list_hex 数组。
    // 渲染一排小 swatch（20px），布局由 .color-list-swatch CSS 控制（宽度自适应）。
    else if (app.is_color_list && Array.isArray(app.color_list_hex) && app.color_list_hex.length >= 2) {
        const container = document.createElement("div");
        container.className = "app-icon color-list-swatch";
        for (const hex of app.color_list_hex) {
            const colorResult = parseColor(hex);
            if (colorResult) {
                const swatch = createSwatch(colorResult.rgba, {size: 20});
                container.appendChild(swatch);
            }
        }
        if (container.children.length > 0) {
            li.appendChild(container);
        }
    }
    // 0.20.3：剪贴板文本项降级检测——如果文本恰好是颜色字面量，也显示大格子 swatch
    else if (!app.is_image && !app.lnk_path && !app.icon_path && !app.is_calc) {
        const clipColor = parseColor(app.name);
        if (clipColor) {
            const swatch = createSwatch(clipColor.rgba, {className: "app-icon clip-thumb", size: 44});
            li.appendChild(swatch);
        }
    }
        // 图标：自定义协议按需懒加载（calc 无 lnk_path 不显示）。
        // Windows/WebView2 下 scheme 映射为 http://<scheme>.localhost/<path>
    // 0.16.4：剪贴板图片项 is_image=true 时用 blink-clipimg 协议加载缩略图。
    else if (app.is_image && app.lnk_path) {
        const img = document.createElement("img");
        img.src = "http://blink-clipimg.localhost/" + encodeURIComponent(app.lnk_path);
        img.className = "app-icon clip-thumb";
        img.alt = app.name;
        img.onerror = () => img.remove();
        li.appendChild(img);
    } else if (!app.is_calc && (app.icon_path || app.lnk_path)) {
        const img = document.createElement("img");
        img.src = "http://blink-icon.localhost/" + encodeURIComponent(app.icon_path || app.lnk_path);
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
        if (isContextAware && typeof firstAction?.runArg === "string") {
            const raw = firstAction.runArg;
            const preview = raw.replace(/\s+/g, " ").trim();
            desc.textContent = preview.length > 80 ? preview.slice(0, 80) + "…" : preview;
            desc.title = preview; // 悬浮看完整值
            desc.classList.add("item-desc-arg");
        } else {
            desc.textContent = app.description;
            if (!app.is_error && clipboardRef?.kind !== "text") desc.title = app.description;
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
        if (e.button !== 0) return; // 只响应左键
        // 剪贴板历史项的单选、切换和区间选择统一交给 clipboard-mode。
        if (clipboardMode.handleMousedown(e, li)) return;
        // 0.20.2: 剪贴板模式下普通点击只移动光标到点击项，不直接激活
        if (clipboardMode.isActive()) {
            clipboardMode.setActiveByLi(li);
            return;
        }
        activateItem(itemData(li));
    });
    // 0.21.x: 剪贴板模式下双击 = Enter（文本项上屏 / 图片项复制）；
    // 普通模式首击 mousedown 已激活并隐藏，双击无需另处理。
    li.addEventListener("dblclick", (e) => {
        if (e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey) return;
        if (clipboardMode.isActive()) {
            e.preventDefault();
            clipboardMode.activateByLi(li);
        }
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
    // 0.17.6: AI 确认卡片相关代码已删除（主窗口 AI 改走 ai-mode.js）
    // 显式引用用于选择和操作；isImage 只保留给原有图片展示消费者。
    return {
        lnkPath: li.dataset.lnkPath,
        source: li.dataset.source || "",
        calcValue: li.dataset.calcValue,
        isError: li.dataset.isError === "true",
        isImage: li.dataset.isImage === "true",
        clipboardRef: refFromLi(li),
        actions,
    };
}

function updateSelection() {
    pageLis.forEach((li, i) => li.classList.toggle("active", i === selected));
    refreshStatusbar();
}

/** 按实际视口显示页码或条数范围，不依赖来源或滚轮偏好。 */
export function refreshStatusbar() {
    const isClipboard = clipboardMode.isActive();
    const stream = isClipboard ? (clipboardMode.getStreamInfo?.() || {}) : {};
    if (viewStart % PAGE_SIZE !== 0 || stream.hasMore) {
        statusbar.update(
            getActive(),
            {
                rangeStart: allItems.length ? viewStart + 1 : 0,
                rangeEnd: viewStart + viewportLength(),
                total: allItems.length,
                hasMore: !!stream.hasMore,
            },
            isClipboard
        );
        return;
    }
    const pageCount = allItems.length ? Math.ceil(allItems.length / PAGE_SIZE) : 0;
    statusbar.update(getActive(), {page: getPage() + 1, pageCount}, isClipboard);
}
