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
//! - 筛选：0.25.23 顶部类型 chips（全部/文本/图片），Tab/Shift+Tab 循环切换，
//!   kind 参数下推后端（文本/图片分表，筛选即跳过另一张表）
//! - ESC：有多选时先清空选择；无多选直接隐藏窗口（模式由 HIDDEN reset 兜底复位）
//! - 选中外壳隐藏 / SHOWN / HIDDEN：自动复位

import {queryEl, resultsEl} from "./dom.js";
import * as results from "./results.js";
import * as ghost from "./ghost.js";
import * as suggestionBar from "./suggestion-bar.js";
import * as statusbar from "./statusbar.js";
import {dismiss as dismissFeedback, show as showFeedback} from "./transient-feedback.js";
import {syncWindowSize, resyncWindowSize, setClipboardLayout, markClipboardLayoutReady} from "./window-size.js";
import {
    copyToClipboard,
    createStickyNote,
    manageClipboardEntries,
    copyClipboardImage,
    getClipboardText,
    getClipboardTextBatch,
    hideWindow,
    openContentEditor,
    pasteToInput,
    recordClipboardHit,
    searchClipboard,
    showStickyWindow
} from "../shared/api.js";
import {t} from "../i18n/index.js";
import * as selection from "./clipboard-selection.js";
import {parse as parseColor} from "../shared/color.js";
import {activateItem} from "./actions.js";
import {showActionError} from "./action-error.js";
import {findEditAction, resolveShortcutAction} from "./clipboard-shortcuts.js";
import {KIND_ORDER, nextKind} from "./clipboard-kind.js";
import * as inputState from "./input-state.js";
import {entryRef, refFromLi, refFromKey, refKey, supportsTextOperation} from './clipboard-entry.js';
import {createClipboardBatch} from './clipboard-batch.js';

// 0.20.1: 从 results.js 导入 setClipboardMode，进入/退出模式时设置标志。

/** 当前是否处于剪贴板模式。 */
let active = false;

/** 0.25.23: 当前类型筛选（"all" | "text" | "image"，Tab/Shift+Tab 循环）。 */
let kind = "all";

/** 原始 placeholder（退出时恢复）。 */
let savedPlaceholder = "";

/** 防抖定时器。 */
let timer = null;

/** 请求序号（防竞态，与 search.js 独立）。 */
let seq = 0;

/** 模式徽章 DOM（搜索栏右上角浅提示）。 */
let badgeEl = null;

/** 0.25.23: 类型 chips 容器（#clipboard-type-chips）。 */
let chipsEl = null;
/** 0.25.23: chips 按钮列表（顺序同 KIND_ORDER）。 */
let chipBtns = [];

/** 0.25.23: kind → i18n key（chips 文案）。 */
const KIND_LABEL_KEYS = {
    all: "clipboard.kind_all",
    text: "clipboard.kind_text",
    image: "clipboard.kind_image",
};

/** 0.20.2: 上次搜索的 trim 后 query——避免相同 query 重复搜索导致光标重置。 */
let lastQuery = "";
/** 最近一次 doSearch 时间戳（reloadList 短窗去重用）。 */
let lastSearchAt = 0;

// ── 0.25.29 流式加载状态 ───────────────────────────────────────────────────

/** 下一批游标（上一批末条 {created_at, id}）；null = 无更多/未加载。 */
let pageCursor = null;
/** 后端是否还有下一批（has_more）。 */
let hasMore = false;
/** 在途追加持有的游标对象，false 表示空闲；旧请求不能释放新请求的标记。 */
let loadingMore = false;

// ── 0.25.29 滚轮模式（clipboard.wheel_mode）─────────────────────────────────

/** 默认逐条；配置启动读取、SHOWN 和广播均由 lifecycle 同步。 */
let wheelMode = "step";
let wheelRevision = 0;

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

/** 0.20.2: 翻页后重新投影多选 CSS（clipboard-selected 可能因 DOM 重建丢失）。
 *  0.25.29: 兼任流式预取触发点——renderViewport 末尾调用，末页/距底时拉下一批。 */
export function onPageChanged() {
    refreshSelectionCss();
    maybeLoadMore();
}

/** 0.20.2: 清空多选状态（不清 epoch/generation）。 */
export function clearSelection(task = null) {
    if (task && (!selection.isCurrent(task) || task.revision !== selection.getSelectionRevision())) return;
    selection.clearSelection();
    refreshSelectionCss();
}

/** 0.20.2: 刷新列表（右键删除后调用），维持当前页号。 */
export function reloadList() {
    // enter 首搜后短窗内跳过（0.24.7）：awareness-updated（选区提取完成 emit）
    // 经 retrigger 到这里，与 enter 的首搜重复——剪贴板模式不消费选区，
    // 这次重查纯浪费。右键删除刷新通常在 250ms 窗外，不受影响。
    if (Date.now() - lastSearchAt < 250) return;
    // 不清多选、不清 generation——只是刷新列表数据
    // 清防抖，直接重新搜索当前 query
    clearTimeout(timer);
    doSearch(lastQuery, true);
}

/** 0.25.23: 切换类型筛选（chips 点击 / Tab 循环共用）。
 *  结果集随筛选变化——与 query 变化同语义：清空选择 + 递增 generation，
 *  绕过「相同 query 不重搜」早退直接重查当前 query。 */
export function setKind(next) {
    if (!active || next === kind) return;
    kind = next;
    refreshChips();
    handleInput(queryEl.value, {force: true, immediate: true});
}

/** 0.25.23: 投影 chips 文案与 active 态。 */
function refreshChips() {
    for (const btn of chipBtns) {
        const key = KIND_LABEL_KEYS[btn.dataset.kind];
        if (key) btn.textContent = t(key);
        btn.classList.toggle("active", btn.dataset.kind === kind);
    }
}

// ── 生命周期 ──────────────────────────────────────────────────────────────

/** 初始化：创建搜索栏右上角模式徽章（0.24.7 回退恢复 0.19.15 形态——
 *  ModeHeader 左上角整行版本经用户裁决还原：剪贴板模式是轻量临时态，
 *  不值得占顶部一整行 + 缩小拖动热区）。 */
export function init() {
    if (badgeEl) return;
    badgeEl = document.createElement("div");
    badgeEl.id = "clipboard-mode-badge";
    badgeEl.className = "clipboard-mode-badge hidden";
    badgeEl.innerHTML =
        '<svg class="icon"><use href="#icon-copy"/></svg><span></span>';
    document.getElementById("search-mode").appendChild(badgeEl);

    // 0.25.23: 类型 chips（index.html 静态容器，按钮按 KIND_ORDER 生成）
    chipsEl = document.getElementById("clipboard-type-chips");
    if (chipsEl) {
        chipBtns = KIND_ORDER.map((k) => {
            const btn = document.createElement("button");
            btn.type = "button";
            btn.className = "clip-kind-chip";
            btn.dataset.kind = k;
            btn.addEventListener("click", () => {
                setKind(k);
                // 点击抢走的焦点还给输入框——chips 是筛选器，不是输入终点
                queryEl.focus({preventScroll: true});
            });
            chipsEl.appendChild(btn);
            return btn;
        });
    }
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
 * 可选择保留当前输入作为剪贴板过滤词；否则显示最近历史。
 * @param {{preserveQuery?: boolean}} options
 */
export function enter({preserveQuery = false} = {}) {
    if (active) return;
    const initialQuery = preserveQuery ? queryEl.value : "";
    active = true;
    setClipboardLayout(true);
    // 0.20.2: 进入剪贴板模式时递增 epoch，清空旧选择
    selection.onEnterMode();
    // 0.20.2: 重置 lastQuery，确保首次空 query 搜索能执行
    lastQuery = null;
    // 0.20.1: 通知 results.js 不做 maxResults 截断
    results.setClipboardMode(true);

    // 保存并切换 placeholder
    savedPlaceholder = queryEl.placeholder;
    queryEl.placeholder = t("clipboard.mode_placeholder");

    // 模式切换可把普通搜索词直接转换为剪贴板过滤词
    queryEl.value = initialQuery;
    queryEl.focus();

    // 0.24.7: 徽章回退为搜索栏右上角浅提示（文案复用 ModeHeader 的 label key）
    if (badgeEl) {
        badgeEl.querySelector("span").textContent = t("mode.clipboard.label");
        badgeEl.classList.remove("hidden");
    }

    // 0.25.23: 显示类型 chips，筛选重置为「全部」（每次进入都从全量起步）
    kind = "all";
    if (chipsEl) {
        refreshChips();
        chipsEl.classList.remove("hidden");
    }

    // 清空 ghost + 建议 + 结果
    ghost.clear();
    suggestionBar.clear();
    results.clear();

    // 标记 body——CSS 据此隐藏 chord 提示（独占模式下不显示 Alt+字母 待命列表）
    document.body.classList.add("clipboard-mode-active");

    // 空 query 显示最近历史；非空 query 直接过滤剪贴板历史
    doSearch(initialQuery.trim());

    syncWindowSize();
}

/**
 * 退出剪贴板模式。
 * 恢复 placeholder + 隐藏徽章 + 清结果 + 恢复正常搜索。
 */
export function exit() {
    if (!active) return;
    active = false;
    setClipboardLayout(false);
    // 0.20.2: 退出模式时递增 epoch+generation，清空选择
    selection.onExitMode();
    // 生命周期负责清理本次模式的持久 loading，迟到的 IPC 不得清理新会话反馈。
    dismissFeedback();
    // 0.20.1: 恢复 results.js 的 maxResults 截断
    results.setClipboardMode(false);

    // 恢复 placeholder
    queryEl.placeholder = savedPlaceholder;

    // 0.24.7: 隐藏右上角模式徽章
    if (badgeEl) badgeEl.classList.add("hidden");
    // 0.25.23: 隐藏类型 chips（kind 已在 enter 时重置，这里只管显隐）
    if (chipsEl) chipsEl.classList.add("hidden");

    // 取消防抖
    clearTimeout(timer);
    seq++;
    // 0.25.29: 重置流式状态（在途 loadMore 由 seq++ 失效）
    pageCursor = null;
    hasMore = false;
    loadingMore = false;
    wheelRevision++;

    // 移除 body 标记——恢复 chord 提示可见性
    document.body.classList.remove("clipboard-mode-active");

    // 清空输入框 + 结果
    queryEl.value = "";
    results.clear();
    ghost.clear();
    suggestionBar.clear();
    statusbar.clearOverride();

    // 恢复正常搜索的 Context Suggestion
    // 不直接调 search.fetchContextSuggestions 避免循环依赖，
    // 由调用方（keyboard.js ESC / lifecycle reset）触发
    // 0.24.4: 模式退出是离散状态转换，resync 收缩被 ModeHeader/列表撑开的高度
    resyncWindowSize();
}

// ── 输入处理 ──────────────────────────────────────────────────────────────

/**
 * 处理输入变化，在剪贴板模式下拦截搜索。
 * 由 search.js::onInput 在搜索逻辑之前调用。
 *
 * @param {string} value 当前输入框值
 * @returns {boolean} true = 剪贴板模式已处理（search.js 应跳过搜索）；false = 非剪贴板模式（继续搜索）
 */
export function handleInput(value, {force = false, immediate = false} = {}) {
    if (!active) return false;

    const q = value.trim();

    // 0.20.2: 相同 query（trim 后）不重新搜索——避免空格等无效输入触发重渲染
    // 导致光标重置到第一项（图片项 active 跳动的根因）
    if (!force && q === lastQuery) {
        return true;
    }

    // 0.20.2: query 真正变化时清空选择状态（递增 generation 使旧请求失效）
    selection.onQueryChanged();
    // 防抖只延迟发请求；输入变化时立即使旧响应失效并记住当前 query。
    clearTimeout(timer);
    lastQuery = q;
    seq++;

    // 0.20.3：颜色字面量优先——在剪贴板模式中输入颜色也立即返回颜色结果。
    // 0.25.23：图片筛选下颜色项无意义（不是剪贴板历史），跳过降级走常规过滤。
    const colorResult = kind === "image" ? null : parseColor(q);
    if (colorResult) {
        // 构造与 ColorEngine 一致的颜色 AppEntry
        const entry = {
            name: colorResult.hex,
            description: `${colorResult.rgb} · ${colorResult.hsl}`,
            source: "color",
            score: 1.0,
            actions: [{kind: "copy", payload: colorResult.hex}],
        };
        results.render([entry], seq);
        markClipboardLayoutReady();
        refreshSelectionCss();
        return true;
    }

    // 40ms 防抖（与正常搜索一致，合并极快连打）
    if (immediate) {
        doSearch(q);
    } else {
        timer = setTimeout(() => doSearch(q), 40);
    }

    return true;
}

// ── 内部 ──────────────────────────────────────────────────────────────────

/**
 * 调用 search_clipboard IPC 并渲染结果（首批；流式追加走 loadMore）。
 * @param {string} query 搜索词（已 trim）
 * @param {boolean} keepPage true 时维持当前页号（右键删除后刷新用）
 */
async function doSearch(query, keepPage = false) {
    const mySeq = ++seq;
    wheelRevision++;
    lastQuery = query;
    lastSearchAt = Date.now();
    // 首批重置流式状态（query/kind 变化 = 新数据集；旧游标立即失效）
    pageCursor = null;
    hasMore = false;
    // 刷新（keepPage）时记住当前页号和光标位置
    loadingMore = false;
    const savedPage = keepPage ? results.getPage() : 0;
    const savedSelected = keepPage ? results.getSelectedIndex() : 0;
    try {
        const resp = await searchClipboard(query, mySeq, kind);
        // 竞态防护：用户已输入新 query 或已退出模式
        if (mySeq !== seq) return;
        if (!active) return;
        const entries = resp.entries || [];
        results.render(entries, mySeq);
        markClipboardLayoutReady();
        // 0.20.2: keepPage 时恢复页号和光标位置（render 会重置 page/selected）
        if (keepPage) {
            results.restorePosition(savedPage, savedSelected);
        }
        // 0.25.29: 首批游标（后续滚到底追加）。**必须先设置游标再触发预取**——
        // render 内部经 onPageChanged 调 maybeLoadMore 时游标尚未就绪会被跳过，
        // 数据只有一批时翻页/平移全是 no-op、预取再无人触发（滚轮死锁实测根因）。
        hasMore = !!resp.has_more;
        pageCursor = hasMore ? (resp.next_cursor || null) : null;
        // 0.20.2: 渲染后保存全局文本项 hitId 列表，并重投影选择 CSS
        selection.reconcileAfterReorder(collectEntryKeys(entries));
        refreshSelectionCss();
        if (keepPage) {
            console.log("[clipboard-mode] reloaded after delete, page=", savedPage, "selected=", savedSelected);
        }
        // 游标就绪后再补一次预取判定（首批不满一屏自动续拉满）
        maybeLoadMore();
    } catch (e) {
        if (mySeq !== seq || !active) return;
        markClipboardLayoutReady();
        console.error("[clipboard-mode] searchClipboard failed:", e);
    }
}

// ── 0.25.29 流式加载 ────────────────────────────────────────────────────────

/**
 * 预取判据满足时拉下一批（onPageChanged / loadMore 完成后调用）。
 * page 模式：翻到末页附近（nearTail 覆盖）；step 模式：数据窗口距底一屏内。
 */
function maybeLoadMore() {
    if (!active || !hasMore || loadingMore || !pageCursor) return;
    if (!results.hasItems() || !results.nearTail()) return;
    if (results.getLoadedCount() >= 2000) {
        // 会话总量保护：到底不再拉（与 results.js MAX_LOADED_ITEMS 同值）
        hasMore = false;
        results.refreshStatusbar();
        return;
    }
    void loadMore();
}

/**
 * 按当前游标拉下一批并追加渲染（batch 缺省由后端按一页 page_size 兜底）。
 *
 * 竞态防护（spec-frontend §5.1）：追加请求带当前 seq，响应到达时校验
 * seq / active / 发起时游标三者——期间 query/kind 变化（doSearch 重置游标）
 * 或退出模式都会使本次响应作废。追加走 results.appendItems 专用路径
 * （不重排、保持视图位置），完成后若仍距底不足一屏继续补（hasMore 终止）。
 */
async function loadMore() {
    if (loadingMore || !pageCursor) return;
    const mySeq = seq;
    const cursorAtStart = pageCursor;
    loadingMore = cursorAtStart;
    try {
        const resp = await searchClipboard(lastQuery, mySeq, kind, cursorAtStart);
        if (mySeq !== seq || !active || pageCursor !== cursorAtStart) return;
        const entries = resp.entries || [];
        results.appendItems(entries, mySeq);
        hasMore = !!resp.has_more;
        pageCursor = hasMore ? (resp.next_cursor || null) : null;
    } catch (e) {
        if (mySeq !== seq || !active || pageCursor !== cursorAtStart) return;
        console.error("[clipboard-mode] loadMore failed:", e);
        hasMore = false; // 失败即停（下轮 query/kind 变化或重新进入时重试）
    } finally {
        // 旧请求不能释放新会话的在途标记；同一会话的新查询仍需继续预取。
        if (loadingMore === cursorAtStart) loadingMore = false;
        maybeLoadMore();
    }
}

/** 0.25.29: 流式状态快照（results.refreshStatusbar 的 step 模式条数区间用）。 */
export function getStreamInfo() {
    return {hasMore, loaded: results.getLoadedCount()};
}

/** 0.25.29: 删除后局部移除（右键单删入口；批量删除 batchDelete / Alt+D 内联同路径）。
 *  替代 retrigger/reloadList 全量重查——保持已加载深度与浏览位置。 */
export function removeLocalItems(refs) {
    if (!active || !Array.isArray(refs) || refs.length === 0) return;
    results.removeItems(refs);
    selection.reconcileAfterReorder(collectEntryKeys(results.getItems?.() || []));
    refreshSelectionCss();
    results.refreshStatusbar();
}

/** 0.25.29: 当前滚轮模式是否为逐条平移（keyboard.js 滚轮分流用）。 */
export function isStepWheel() {
    return wheelMode === "step";
}

/** 供滚轮累积器区分查询、模式与配置切换，余量不能跨上下文继承。 */
export function getWheelRevision() {
    return wheelRevision;
}

/** 从 AppConfig 刷新滚轮模式快照（启动、SHOWN 与配置广播共用）。
 *  默认逐条平移（用户裁决），仅显式 "page" 才整页翻——与后端 serde default 同语义。 */
export function refreshWheelMode(clipCfg) {
    const next = clipCfg?.wheel_mode === "page" ? "page" : "step";
    if (next === wheelMode) return;
    wheelMode = next;
    wheelRevision++;
    if (active) results.refreshStatusbar();
}

// ── 0.20.2 多选键盘处理 ────────────────────────────────────────────────────

/**
 * 剪贴板模式键盘事件拦截。
 *
 * 在 keyboard.js::onNavigation 中调用，返回 true 表示已处理（拦截）。
 *
 * 键盘规则：
 * 1. IME composition 优先，期间不接管快捷键。
 * 2. Tab/Shift+Tab 循环类型筛选（0.25.23；Alt 按下放行——Alt+Tab 归系统）。
 * 3. query 为空时 Ctrl+A 全选当前已加载的全部历史项；
 *    query 非空时保留输入框全选。
 * 4. 选择全为文本时 Ctrl+C / Enter 批量复制 / 上屏；混选时提示从菜单选类型。
 * 5. Esc 顺序：清空多选 → 退出 clipboard mode（由 keyboard.js onEscape 处理）。
 * 6. 单项 Enter / Alt+数字：文本上屏或复制兜底，图片复制。
 * 7. Alt+D / 空查询 Delete 删除选择；没有选择时删除 active 历史项。
 *
 * 鼠标规则（在 results.js mousedown 中由 handleMousedown 处理）：
 * - 单击历史项 → 单选；Ctrl+单击 → 切换该项；类型不影响选择。
 * - Shift+单击 → 从锚点按完整已加载顺序选区间；Ctrl+Shift 追加区间。
 * - 双击 → activateByLi（与 Enter 同语义：文本上屏 / 图片复制）
 *
 * @param {KeyboardEvent} e
 * @returns {boolean} true = 已处理，keyboard.js 应跳过后续导航逻辑
 */
export function handleKeydown(e) {
    if (!active) return false;
    // IME 组字优先放行
    if (e.isComposing || e.keyCode === 229) return false;

    const ctrl = e.ctrlKey || e.metaKey;

    // 0.25.23: Tab/Shift+Tab 循环类型筛选。
    // Alt 按下放行（Alt+Tab 归系统，铁则）；Ctrl/Meta 组合不劫持（浏览器焦点语义）。
    // 用后端 Alt 快照 || e.altKey 防御式消费（spec-frontend §5.5）。
    if (e.key === "Tab" && !ctrl) {
        if (inputState.isAltDown() || e.altKey) return false;
        e.preventDefault();
        setKind(nextKind(kind, e.shiftKey));
        return true;
    }

    // Ctrl+A：query 为空时全选已加载的历史项
    if (ctrl && e.key === "a") {
        if (queryEl.value.trim()) {
            return false; // 非空 query 保留输入框全选
        }
        e.preventDefault();
        // 流式追加后实时取完整已加载顺序，包含所有类型。
        const allKeys = collectEntryKeys(results.getItems());
        selection.selectAll(allKeys);
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

    // 编辑/便签只接受纯文本选择；删除作用于完整选择，无选择时回退 active。
    const queryIsEmpty = !queryEl.value.trim();
    const shortcutAction = resolveShortcutAction(e, queryIsEmpty);
    if (shortcutAction === "edit") {
        e.preventDefault();
        if (selection.hasSelection()) {
            batchEdit();
        } else {
            handleEditActive();
        }
        return true;
    }
    if (shortcutAction === "sticky") {
        e.preventDefault();
        if (selection.hasSelection()) {
            batchSticky();
        } else {
            handleStickyActive();
        }
        return true;
    }
    if (shortcutAction === "delete") {
        e.preventDefault();
        if (selection.hasSelection()) batchDelete();
        else handleDeleteActive();
        return true;
    }

    return false;
}

// ── 选择快照与批量操作 ──────────────────────────────────────────────
export function getSelectionSnapshot() {
    const task = selection.capture();
    return {...task, refs: task.keys.map(refFromKey).filter(Boolean)};
}
function activeSnapshot() {
    const ref = entryRef(results.getActive());
    return {...selection.capture(), keys: ref ? [refKey(ref)] : [], refs: ref ? [ref] : []};
}
const batch = createClipboardBatch({selection, snapshot: getSelectionSnapshot, active: () => active,
    refresh: refreshSelectionCss, reload: reloadList, remove: removeLocalItems,
    api: {getClipboardTextBatch, copyToClipboard, pasteToInput, openContentEditor,
        createStickyNote, showStickyWindow, hideWindow, recordClipboardHit, manageClipboardEntries},
    feedback: showFeedback, dismiss: dismissFeedback, t});
export async function batchCopy(task = getSelectionSnapshot()) {
    if (!active || !selection.isCurrent(task) || task.revision !== selection.getSelectionRevision()) return;
    if (task.refs.length === 1 && task.refs[0].kind === 'image') {
        const gen = selection.beginCopy();
        try {
            await copyClipboardImage(task.refs[0].id);
            if (!active || !selection.isCurrent(task) || !selection.isCopyStillValid(gen)) return;
            selection.complete(task); refreshSelectionCss();
            showFeedback(t('clipboard.image_copied'), {type: 'success'});
        } catch (e) { if (selection.isCurrent(task)) showActionError('copy_clipboard_image', e); }
        return;
    }
    return batch.textOperation('copy', task);
}
export function batchPaste(task) { return batch.textOperation('paste', task); }
export function batchEdit(task) { return batch.textOperation('edit', task); }
export function batchSticky(task) { return batch.textOperation('sticky', task); }
export function batchDelete(task) { return batch.resourceOperation('delete', task); }
export function batchPin(task) { return batch.resourceOperation('pin', task); }
function handleStickyActive() { return batchSticky(activeSnapshot()); }
export function activateSelection() {
    const task = getSelectionSnapshot();
    if (!task.refs.length) return activateByData(results.getActive());
    if (task.refs.length === 1) {
        const key = refKey(task.refs[0]);
        const item = results.getItems().find(item => { const ref = entryRef(item); return ref && refKey(ref) === key; });
        if (item) return activateByData({...item, isImage: item.is_image, lnkPath: item.lnk_path, clipboardRef: entryRef(item)});
        return;
    }
    return batchPaste(task);
}

// ── 0.20.2 辅助函数 ─────────────────────────────────────────────────────────

/** 仅真实历史条目参与选择，颜色等临时结果不混入。 */
function collectEntryKeys(items) {
    return items.map(entryRef).filter(Boolean).map(refKey);
}

// ── 0.20.2 鼠标多选支持 ──────────────────────────────────────────────────────

/** 普通单选、Ctrl 切换、Shift 区间；只操作明确的历史资源身份。 */
export function handleMousedown(e, li) {
    if (!active || e.button !== 0) return false;
    const ref = refFromLi(li);
    if (!ref) return false;
    e.preventDefault(); e.stopPropagation();
    results.setActiveByEl(li);
    const key = refKey(ref);
    const ctrl = e.ctrlKey || e.metaKey;
    if (e.shiftKey) selection.selectRange(collectEntryKeys(results.getItems()), key, ctrl);
    else if (ctrl) selection.toggleSelection(key);
    else selection.selectSingle(key);
    refreshSelectionCss();
    return true;
}
export function prepareContextMenu(li) {
    if (!active || !li) return;
    const ref = refFromLi(li);
    if (!ref) { clearSelection(); return; }
    results.setActiveByEl(li);
    if (!selection.isSelected(refKey(ref))) selection.selectSingle(refKey(ref));
    refreshSelectionCss();
}
export function onNavigate(e) {
    const ctrl = e.ctrlKey || e.metaKey;
    if (!active || (ctrl && !e.shiftKey)) return;
    const ref = entryRef(results.getActive());
    if (!ref) return;
    const key = refKey(ref);
    if (e.shiftKey) selection.selectRange(collectEntryKeys(results.getItems()), key, !!ctrl);
    else selection.selectSingle(key);
    refreshSelectionCss();
}

// ── 0.20.2 选择 CSS 投影 ────────────────────────────────────────────────────

/**
 * 把选中状态投影到 DOM（添加/移除 .selected class）。
 * 重渲染只重投影 CSS，不按 DOM index 保存状态。
 */
function refreshSelectionCss() {
    const lis = resultsEl.querySelectorAll("li");
    for (const li of lis) {
        const ref = refFromLi(li);
        li.classList.toggle('clipboard-selected', !!ref && selection.isSelected(refKey(ref)));
    }
    // 更新状态栏覆写：有选择 → 多选计数覆写；无选择 → 恢复常规 footer
    if (selection.hasSelection()) {
        showStatusSelected(selection.selectedCount());
    } else {
        statusbar.clearOverride();
        results.refreshStatusbar?.();
    }
}

// ── 0.20.2 状态栏反馈（0.24.4 分工迁移）───────────────────────────────────
// 多选计数是持续状态 → ResultFooter override；loading/success/error 是瞬态
// → TransientFeedback。均不再直接改写 #statusbar DOM。

/** 多选计数覆写 ResultFooter（selection 存续期间持续显示）。 */
function showStatusSelected(count) {
    const refs = getSelectionSnapshot().refs;
    statusbar.setOverride(t(supportsTextOperation(refs) ? 'clipboard.hint_selected' : 'clipboard.hint_mixed',
        {count, text: refs.filter(r => r.kind === 'text').length, image: refs.filter(r => r.kind === 'image').length}));
}

/** 批量复制/删除失败（瞬态错误）。 */
function showStatusError(message) {
    showFeedback(t("clipboard.copy_failed", {message}), {type: "error"});
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
    activateItem({...activeItem, actions: [editAction]});
}

/**
 * 0.20.8：Alt+D / Delete — 删除 active 文本或图片历史。
 * 只处理带显式类型引用的 active 历史项；
 * 颜色降级结果等非历史项不响应。删除成功后维持 clipboard mode 并刷新当前页。
 */
async function handleDeleteActive() { return batchDelete(activeSnapshot()); }

// ── 0.21.x 激活分派：上屏 / 复制 ────────────────────────────────────────────

/**
 * 剪贴板模式「激活」统一分派（Enter / Alt+数字 / 双击 共用，保证路径一致）。
 *
 * 文本项（LazyCopy）：拉取全文并校验会话 → 频率加权 → `paste_to_input`。
 * 后端恢复并核验唤起前的原窗口后发粘贴键；目标失效时保留复制，不依赖 UIA 门控。
 * 图片项 / 其他：复用 activateItem 原复制路径。
 *
 * @param {object|null} data results.itemData 形状（actions 数组 / isImage / …）
 */
export async function activateByData(data) {
    if (!data) return;
    if (data.isError) return;

    const action = data.actions?.[0];
    const kind = action?.kind;
    const ref = entryRef(data);

    // 文本项走上屏（含复制兜底）
    if (kind === "copy" && ref?.kind === 'text') {
        const gen = selection.beginCopy();
        let text = action.payload ?? data.calcValue;
        if (!text) {
            try {
                text = await getClipboardText(ref.id);
            } catch (e) {
                if (!selection.isCopyStillValid(gen)) return;
                showActionError("get_clipboard_text", e);
                return; // 保留窗口，让用户察觉并重试
            }
        }
        if (!selection.isCopyStillValid(gen)) return;
        if (!text) return; // 无可上屏内容（空/缺省），保留窗口
        // fire-and-forget 频率加权（与 actions.js LazyCopy 一致；失败不阻塞上屏）
        recordClipboardHit(ref.id).catch((e) =>
            console.warn("record_clipboard_hit failed:", e)
        );
        try {
            await pasteToInput(text); // 后端负责隐藏窗口
        } catch (e) {
            if (!selection.isCopyStillValid(gen)) return;
            showActionError("paste_to_input", e);
        }
        return;
    }

    // 图片项 / 其他 → 原激活路径（复制图片等）
    activateItem(data);
}

/** 双击项：从 <li> 读出与 results.itemData 一致的数据后走统一分派。 */
export function activateByLi(li) {
    let actions = [];
    try {
        actions = JSON.parse(li.dataset.actions || "[]");
    } catch {
        // ignore
    }
    activateByData({
        lnkPath: li.dataset.lnkPath,
        source: li.dataset.source || "",
        calcValue: li.dataset.calcValue,
        isError: li.dataset.isError === "true",
        isImage: li.dataset.isImage === "true",
        clipboardRef: refFromLi(li),
        actions,
    });
}
