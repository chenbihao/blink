//! 右键上下文菜单（0.5.3+）：独立 Popup 窗口模式。
//!
//! 所有右键菜单都通过独立无边框窗口渲染，真正突破主窗口边界限制，
//! 点击外部/ESC/左键自动关闭，菜单项动作回传主窗口执行。

import {queryEl, resultsEl} from "./dom.js";
import {activateItem} from "./actions.js";
import {
    copyToClipboard,
    manageClipboardEntries,
    createStickyNote,
    hideWindow,
    invoke,
    listChordActions,
    openContainingFolder,
    openImageEditorFromHistory,
    openLnkTarget,
    pinClipboardImage,
    resetItemHistory,
    runBuiltinAction,
    saveClipboardFileAsImage,
    showStickyManager,
    showStickyWindow,
    triggerChord,
    pinClipboardEntries,
} from "../shared/api.js";
import {retrigger} from "./search.js";
import * as ghost from "./ghost.js";
import {getLang, t} from "../i18n/index.js";
import {showActionError} from "./action-error.js";
import {normalizeError} from "../shared/tauri.js";
import {EVENTS} from "../shared/event-names.js";
import {parse as parseColor} from "../shared/color.js";
import * as clipboardMode from "./clipboard-mode.js";
import {refFromLi, refKey, selectionActions} from './clipboard-entry.js';
import {getClipboardPreviewContext, holdClipboardPreview} from './clipboard-preview.js';

/** 0.20.3：判断 shortcut 文本是否为颜色格式预览（#xxx / rgb(...) / hsl(...)）。 */
function isColorShortcut(s) {
    return !!s && (s.startsWith("#") || s.startsWith("rgb") || s.startsWith("hsl"));
}

/**
 * 0.25.27：按内容估算菜单宽度（CSS 像素）。
 *
 * 文本宽度近似：中文全角 ≈ 字号（--text-base 13px），半角字母/数字 ≈ 7.5px；
 * 行宽 = label + gap + shortcut（--space-md 16），再加 item 水平 padding
 * （--space-md 16 × 2）与容器 padding（--space-sm 8 × 2）。宁可略宽留白，
 * 不可裁切（label nowrap，宽度不足时被 shortcut 挤出可视区）。
 * @param {object[]} items 菜单项数组
 * @returns {number} 估算宽度（不含 H_BUFFER，调用方另行叠加）
 */
function estimateMenuWidth(items) {
    let maxRow = 0;
    for (const it of items) {
        if (it.separator) continue;
        const labelW = estimateTextWidth(it.label);
        const shortcutW = it.shortcut ? estimateTextWidth(it.shortcut) : 0;
        const row = labelW + (it.shortcut ? 16 + shortcutW : 0);
        if (row > maxRow) maxRow = row;
    }
    return maxRow + 32 + 16;
}

/** 0.25.27：单段文本宽度近似（全角 13px / 半角 7.5px）。 */
function estimateTextWidth(text) {
    let w = 0;
    for (const ch of String(text ?? "")) {
        w += ch.codePointAt(0) > 0xff ? 13 : 7.5;
    }
    return w;
}

/**
 * 0.20.3：为颜色项构建 HEX/RGB/HSL 复制 + 贴便签菜单项。
 * @param {string} colorHex — canonical HEX
 * @returns {object[]} 菜单项数组（不含前导 separator，由调用方追加）
 */
function colorMenuItems(colorHex) {
    const colorResult = parseColor(colorHex);
    if (!colorResult) return [];
    return [
        {
            label: t("menu.copyHex"), shortcut: colorResult.hex, run: () => {
                copyToClipboard(colorResult.hex).catch((e) => console.error("copy hex failed:", e));
                hideWindow();
            }
        },
        {
            label: t("menu.copyRgb"), shortcut: colorResult.rgb, run: () => {
                copyToClipboard(colorResult.rgb).catch((e) => console.error("copy rgb failed:", e));
                hideWindow();
            }
        },
        {
            label: t("menu.copyHsl"), shortcut: colorResult.hsl, run: () => {
                copyToClipboard(colorResult.hsl).catch((e) => console.error("copy hsl failed:", e));
                hideWindow();
            }
        },
        {separator: true},
        {
            label: t("menu.createStickyFromColor"),
            run: async () => {
                try {
                    const note = await createStickyNote(colorResult.hex);
                    await showStickyWindow(note.id, true);
                    hideWindow();
                } catch (e) {
                    showActionError("create_sticky_from_color", e);
                }
            },
        },
    ];
}

/** 当前菜单数据（用于 Popup 点击时回调执行）。 */
let currentItems = [];
let currentMenuId = '';
/** 0.26.10：当前菜单内容代次（同 menuId 原地更新时递增；null = 初始内容）。 */
let currentGeneration = null;
let releasePreview;
let initialMenuTimer;
const SMART_FIRST_PAINT_DELAY = 32;
let menuSerial = 0;
/** 菜单内容代次计数器——跨菜单会话单调递增，更新与点击竞争由此区分。 */
let menuGeneration = 0;
const menuOwner = Date.now().toString(36) + '-' + Math.random().toString(36).slice(2);

/** Chord 动作缓存（shown 时预拉取，右键时同步读）。只含 semantic=tap 的动作。 */
let cachedChordActions = [];

/** 绑定全局右键事件（main.js 装配时调用一次）。 */
export function init() {
    // 左键点击任何地方 → 关闭菜单
    document.addEventListener("mousedown", (e) => {
        if (e.button === 0) close();
    });

    document.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        close(); // 先关闭旧的
        const target = e.target;

        let items = [];
        const preview = getClipboardPreviewContext(target);
        const li = preview?.anchor ?? closestLi(target);
        const previewError = target.closest?.(".clipboard-text-preview")?.dataset.errorText;
        if (!preview) clipboardMode.prepareContextMenu(li);
        if (previewError) {
            items = errorMenu(previewError);
        } else if (li) {
            items = itemMenu(li, {selectedText: preview?.selectedText});
        } else {
            items = unifiedMenu();
        }

        // 0.25.27: 多选时批量组完整接管——右键列表项只显示批量操作（单项操作
        // 与批量指向混杂会产生「复制的是哪条」歧义）；右键空白处保留通用菜单
        // （粘贴/设置等与选择集无指向冲突），批量组置顶插入。
        if (!preview && !previewError && li?.dataset.isError !== "true" && clipboardMode.isActive() && clipboardMode.hasSelection()) {
            const task = clipboardMode.getSelectionSnapshot();
            const selectionItems = li && task.refs.length === 1 && items.length ? items : buildSelectionMenu(task);
            items = li
                ? selectionItems
                : [...selectionItems, {separator: true}, ...items];
        }

        while (items[0]?.separator) items.shift();
        while (items.at(-1)?.separator) items.pop();
        items = items.filter((item, index) => !item.separator || !items[index - 1]?.separator);
        if (!items.length) return;
        currentMenuId = menuOwner + ":" + (++menuSerial);
        currentGeneration = null;

        // 0.26.10：单项文本/文件条目先冻结智能操作会话，菜单顶部插入不可点击的
        // 「正在识别操作」；结果回流按 menuId/条目身份/锚点连接校验后更新同一菜单。
        const smartSession = smartSessionFor(li, previewError, preview);
        if (smartSession) {
            smartSession.menuId = currentMenuId;
            currentGeneration = 0;
            smartSession.baseItems = items.map(item => ({...item,
                run: preview && item.run ? () => { if (preview.isCurrent()) return item.run(); } : item.run}));
            items = [smartLoadingItem(), {separator: true}, ...items];
        }

        items = items.map(item => ({...item, menuId: currentMenuId, generation: currentGeneration,
            run: preview && item.run ? () => { if (preview.isCurrent()) return item.run(); } : item.run}));
        currentItems = items;
        if (preview) releasePreview = holdClipboardPreview(preview);

        if (smartSession) {
            // 快速 DB 派生只展示最终内容；慢查询最多等一小帧预算，基础操作随后可用。
            initialMenuTimer = setTimeout(() => {
                initialMenuTimer = undefined;
                if (isSmartSessionCurrent(smartSession)) showItemsPopup(currentItems);
            }, SMART_FIRST_PAINT_DELAY);
            updateSmartActions(smartSession);
        } else {
            showItemsPopup(items);
        }
    });

    // ESC 关闭
    document.addEventListener("keydown", (e) => {
        if (e.key === "Escape" && currentMenuId) {
            e.preventDefault();
            e.stopPropagation();
            close();
        }
    }, true);
    // 注意：移除了 window.addEventListener("blur", close)
    // 点击 Popup 窗口时，主窗口先失焦会触发 blur，导致菜单提前关闭
    // Popup 窗口自己有关闭逻辑（点击菜单项/外部/ESC），主窗口被看门狗隐藏时也会联动关闭菜单

    // 监听来自 Popup 窗口的菜单项点击事件
    import("../shared/tauri.js").then(({listen}) => {
        listen(EVENTS.CONTEXT_MENU_ACTION, (event) => {
            const {actionId, menuId, generation} = event.payload || {};
            if (!menuId || menuId !== currentMenuId) return;
            // 0.26.10：旧代次点击（内容已更新）不执行——不按新数组位置执行另一操作
            if ((generation ?? null) !== currentGeneration) return;
            const item = currentItems[actionId];
            clearCurrentMenu();
            if (item && item.run && !item.separator && !item.disabled) {
                item.run();
            }
        });
        listen(EVENTS.CONTEXT_MENU_CLOSED, (event) => {
            if (event.payload?.menuId === currentMenuId) clearCurrentMenu();
        });

        // 主窗 shown 时预拉取 chord 动作，右键时同步读缓存（无延迟）
        listen(EVENTS.SHOWN, refreshChordActions);
        listen(EVENTS.HIDDEN, close);
    });
}

/** 拉取 chord 动作并缓存（只保留 tap 语义——截图/剪贴板等，排除 hold 语义的语音输入）。 */
async function refreshChordActions() {
    try {
        const cfg = await invoke("get_config");
        if (!cfg || cfg.chord_enabled !== true) {
            cachedChordActions = [];
            return;
        }
        const all = await listChordActions();
        cachedChordActions = all.filter((a) => a.semantic === "tap");
    } catch {
        cachedChordActions = [];
    }
}

function closestLi(target) {
    if (!target || !target.closest || !resultsEl) return null;
    const li = target.closest("li");
    return li && resultsEl.contains(li) ? li : null;
}

/**
 * 调用后端创建独立 Popup 窗口显示菜单。
 * `width/height` 是 CSS 像素尺寸。光标物理坐标 + DPI 缩放 + 多屏边界翻转
 * 均由后端 `clamp_context_menu`（`GetCursorPos`）处理。
 */
function showPopupWindow(width, height, items) {
    const menuId = currentMenuId;
    invoke("show_context_menu", {
        width: width,
        height: height,
        items: JSON.stringify(items),
    }).catch((e) => {
        console.error("Failed to show context menu popup:", e);
        if (currentMenuId === menuId) close();
    });
}

/**
 * 0.26.10：估算尺寸并弹出菜单（初始展示与智能组原地更新共用）。
 *
 * item: padding 8px * 2 + font 13px * line-height 1.5 ≈ 35.5px；
 * separator 9px；container padding 16px；缓冲量 8px（阴影呼吸位）。
 * 宽度按最宽项内容估算，下限 180（颜色项 260）。
 */
function showItemsPopup(items) {
    const rows = items.reduce((h, it) => h + (it.separator ? 9 : 36), 0);
    const estimatedHeight = rows + 16 + 8;
    const hasColorShortcut = items.some((it) => isColorShortcut(it.shortcut));
    const estimatedWidth = Math.max(hasColorShortcut ? 260 : 180, estimateMenuWidth(items)) + 8;
    showPopupWindow(estimatedWidth, estimatedHeight, items);
}

// ── 0.26.10 右键智能操作（打开类条目派生）─────────────────────────────

/** 打开类 Kind → Lucide 图标（与 SuggestionBar iconFor 同映射）。 */
const SMART_ACTION_ICONS = {
    openUrl: "globe",
    openPath: "folder-open",
    revealInExplorer: "folder-search",
};
/** 智能组目标摘要截断（与 SuggestionBar OPEN_TARGET_PREVIEW_MAX 对齐）。 */
const SMART_TARGET_PREVIEW = 40;

/**
 * 多选接管/错误项/非剪贴板模式不注入智能组；只有单项文本/文件条目参与打开类派生。
 * 预览右键按所属单项处理（不改变已有选择集），不受列表多选影响；
 * 列表右键在多选时由批量菜单接管。
 */
function smartSessionFor(li, previewError, preview) {
    if (previewError || li?.dataset.isError === "true") return null;
    if (!clipboardMode.isActive()) return null;
    if (!preview && clipboardMode.getSelectionSnapshot().refs.length > 1) return null;
    const ref = refFromLi(li);
    if (!ref || (ref.kind !== "text" && ref.kind !== "file")) return null;
    const snapshot = clipboardMode.getSelectionSnapshot();
    return {li, ref, snapshot, preview};
}

function smartLoadingItem() {
    return {label: t("menu.smart_loading"), disabled: true};
}

/**
 * 异步派生回流：校验菜单会话 / 条目身份 / 锚点连接后更新同一 Popup。
 *
 * 空组移除 loading 分组；识别失败显示不可点击失败态；基础菜单始终可用，
 * 不能把基础操作推迟到识别完成。更新推进内容代次（menuGeneration），
 * 旧内容上的点击由后端代次校验拒绝，前端 currentGeneration 同步比对。
 * 关闭/切换条目/过滤重排/删除/退出模式/主窗隐藏都会使 currentMenuId 变化，
 * 迟到结果不重开已关闭菜单。
 */
async function updateSmartActions(session) {
    const {menuId, ref, baseItems} = session;
    let actions = null;
    let failed = false;
    try {
        actions = await invoke("get_clipboard_item_actions", {entry: ref});
    } catch {
        failed = true;
    }
    if (!isSmartSessionCurrent(session)) return;
    clearTimeout(initialMenuTimer);
    initialMenuTimer = undefined;

    let group;
    if (failed) {
        group = [{label: t("menu.smart_failed"), disabled: true}];
    } else if (!Array.isArray(actions) || actions.length === 0) {
        group = [];                                       // 空组：移除 loading 分组
    } else {
        group = actions.map(action => ({
            label: t(`suggestionbar.${action.kind}_with`, {
                text: truncateTarget(action.target || ""),
            }),
            icon: SMART_ACTION_ICONS[action.kind] || "zap",
            run: () => {
                // 点击处理已消费 currentMenuId，此处仍核对条目/选择/预览快照。
                if (!isSmartSessionCurrent(session, false)) return;
                runBuiltinAction(action.capabilityId, action.args)
                    .then(() => reportSmartAction(action, "ok"))
                    .catch(e => {
                        showActionError("run_builtin_action", e);
                        reportSmartAction(action, "error");
                    });
            },
        }));
    }

    menuGeneration += 1;
    currentGeneration = menuGeneration;
    const stamp = item => ({...item, menuId, generation: menuGeneration});
    const items = group.length
        ? [...group.map(stamp), {separator: true, menuId}, ...baseItems.map(stamp)]
        : baseItems.map(stamp);
    currentItems = items;
    showItemsPopup(items);
}

function isSmartSessionCurrent({menuId, li, ref, snapshot, preview}, checkMenu = true) {
    if (checkMenu && menuId !== currentMenuId) return false;
    if (!li.isConnected || !clipboardMode.isActive() || (preview && !preview.isCurrent())) return false;
    const current = refFromLi(li), selection = clipboardMode.getSelectionSnapshot();
    return !!current && refKey(current) === refKey(ref)
        && selection.epoch === snapshot.epoch && selection.revision === snapshot.revision;
}

function truncateTarget(text) {
    return text.length > SMART_TARGET_PREVIEW
        ? text.slice(0, SMART_TARGET_PREVIEW) + "…" : text;
}

/** 采纳遥测：只记 Kind/动作 id/结果，不记目标内容（fire-and-forget）。 */
function reportSmartAction(action, outcome) {
    invoke("report_clipboard_item_action", {
        kind: action.kind,
        capabilityId: action.capabilityId,
        outcome,
    }).catch(() => {});
}

/** 关闭菜单（通知后端销毁 Popup 窗口）。 */
function close() {
    const menuId = currentMenuId;
    clearCurrentMenu();
    if (!menuId) return;
    invoke("hide_context_menu", {menuId}).catch(() => {
    });
}

function clearCurrentMenu() {
    clearTimeout(initialMenuTimer);
    initialMenuTimer = undefined;
    currentMenuId = "";
    currentItems = [];
    currentGeneration = null;
    const release = releasePreview;
    releasePreview = undefined;
    release?.();
}

// ── 各区域菜单构建 ──────────────────────────────────────────────────────────

/**
 * 0.25.27：剪贴板多选批量菜单组——非破坏操作（复制/上屏/编辑/便签）一组，
 * 删除独立一组（danger），取消选择收尾（escape hatch）。
 * 标注的快捷键与键盘实际行为一致（菜单即契约）：
 * 只有纯文本选择标注文本快捷键；混选显式操作类型子集，Esc 清空选择。
 */
function buildSelectionMenu(task) {
    const operations = {copy: clipboardMode.batchCopy, copy_paths: clipboardMode.batchCopyPaths,
        paste: clipboardMode.batchPaste,
        edit: clipboardMode.batchEdit, sticky: clipboardMode.batchSticky,
        pin: clipboardMode.batchPin, delete: clipboardMode.batchDelete};
    const items = [];
    let destructive = false;
    for (const action of selectionActions(task.refs)) {
        if (action.danger && !destructive) { items.push({separator: true}); destructive = true; }
        const key = action.id === 'delete' ? 'menu.delete_scope_' + action.scope : 'menu.selection_' + action.id;
        const frozenTask = {...task, refs: action.refs, keys: action.refs.map(refKey)};
        items.push({label: t(key, {count: action.count}), shortcut: action.shortcut,
            danger: !!action.danger, run: () => operations[action.id](frozenTask)});
    }
    items.push({separator: true}, {label: t('menu.deselect'), shortcut: 'Esc',
        run: () => clipboardMode.clearSelection(task)});
    return items;
}

function unifiedMenu() {
    const exec = (cmd) => async () => {
        queryEl?.focus();
        setTimeout(() => {
            try {
                document.execCommand(cmd);
            } catch (e) {
                console.error("execCommand failed:", e);
            }
        }, 10);
    };
    const paste = async () => {
        queryEl?.focus();
        setTimeout(async () => {
            try {
                const text = await navigator.clipboard.readText();
                if (queryEl) {
                    const start = queryEl.selectionStart || 0;
                    const end = queryEl.selectionEnd || 0;
                    queryEl.value = queryEl.value.slice(0, start) + text + queryEl.value.slice(end);
                    queryEl.selectionStart = queryEl.selectionEnd = start + text.length;
                    // 0.25.12: 设值重置 scrollLeft 且程序化选区不触发自动滚动——
                    // 长文本粘贴后显式滚到末尾，光标（插入点）可见
                    ghost.scrollWithMargin();
                    queryEl.dispatchEvent(new Event("input", {bubbles: true}));
                }
            } catch (e) {
                try {
                    document.execCommand("paste");
                } catch (e2) {
                    console.error("paste failed:", e2);
                }
            }
        }, 10);
    };

    // 0.18.0: 除 input 选区外，也检查 div 选区（AI 输出区选中文本）
    const inputSelection = queryEl && queryEl.selectionStart !== queryEl.selectionEnd;
    const divSelection = (() => {
        try {
            const sel = window.getSelection();
            return sel && sel.toString().length > 0 ? sel.toString() : "";
        } catch {
            return "";
        }
    })();
    const hasText = queryEl && queryEl.value && queryEl.value.length > 0;
    const items = [];
    if (inputSelection) {
        items.push({label: t("menu.cut"), run: exec("cut")});
        items.push({label: t("menu.copy"), run: exec("copy")});
    } else if (divSelection) {
        // 0.18.0: AI 输出区 div 选区——复制走 copyToClipboard 而非 execCommand
        items.push({label: t("menu.copy"), run: () => copyToClipboard(divSelection)});
    }
    items.push({label: t("menu.paste"), run: paste});
    // 全选门控（0.16.0）：无文本时不显示全选——select all 空输入无意义
    if (hasText) {
        items.push({separator: true});
        items.push({label: t("menu.selectAll"), run: exec("selectAll")});
    }

    // Chord 快捷入口（tap 语义——截图/剪贴板等，排除 hold 语义的语音输入）
    // 0.18.4：快捷键用括号包裹并淡色显示（shortcut 字段单独渲染）
    if (cachedChordActions.length) {
        items.push({separator: true});
        const isZh = getLang() === "zh";
        for (const a of cachedChordActions) {
            const keyLabel = a.key === " " ? "Space" : a.key.toUpperCase();
            const shortcut = isZh
                ? `（Alt+${keyLabel}）`
                : ` (Alt+${keyLabel})`;
            items.push({
                label: a.label,
                shortcut,
                run: () => triggerChord(a.key),
            });
        }
    }

    // 0.20.0：显式“从查询创建便签”动作——Alt+S 永远创建空白便签后，
    // 用户仍可通过右键菜单从当前 query 文本创建带内容的便签
    if (hasText) {
        items.push({separator: true});
        items.push({
            label: t("menu.createStickyFromQuery"),
            run: async () => {
                try {
                    const text = queryEl.value;
                    const note = await createStickyNote(text);
                    await showStickyWindow(note.id, true);
                    hideWindow();
                } catch (e) {
                    showActionError("create_sticky_from_query", e);
                }
            },
        });
    }

    // 0.18.4：新增便签管理入口（独立分组，在打开设置上方）
    items.push({separator: true});
    items.push({label: t("menu.stickyManager"), run: () => showStickyManager()});
    items.push({separator: true});
    items.push({label: t("menu.openSettings"), run: () => runBuiltinAction("open_settings")});
    items.push({label: t("menu.exit"), run: () => runBuiltinAction("exit_blink"), danger: true});
    return items;
}

// ── 结果项菜单 ──────────────────────────────────────────────────────────────

/** action.kind → 默认菜单文案的 i18n key 映射。有 hint 时优先用 t(hint)。 */
const KIND_LABELS = {
    copy: () => t("menu.copy"),
    open: () => t("menu.open"),
    // 0.26.17：args.hints 引导项——与状态栏共用同一文案
    route: () => t("hint.route"),
};

/** 0.26.16：置顶档位（小时）——产品定案 1/6/12/24 四档平铺，不做永久置顶。 */
const PIN_TTL_HOURS = [1, 6, 12, 24];

/** Capability 当前支持的置顶类型（ClipboardEntryKind 三变体）。 */
const PINNABLE_KINDS = new Set(["text", "image", "file"]);

/** 置顶/续期/取消置顶统一执行：0 = 取消。完成后强制重查（排序与徽章都要更新），
 *  保持剪贴板模式不退出，焦点还给输入框（菜单 popup 点击后继续打字）。 */
function pinEntry(ref, hours) {
    const operation = hours > 0 ? "pin" : "unpin";
    // 走专用 pin_clipboard_entries（列表管理入口，不隐藏主窗）——
    // run_builtin_action 对所有能力统一收尾 hide，置顶后需停留在列表继续操作。
    pinClipboardEntries(operation, [ref], hours > 0 ? hours : undefined)
        .then(() => {
            if (clipboardMode.isActive()) {
                clipboardMode.reloadList({force: true});
                queryEl?.focus({preventScroll: true});
            }
        })
        .catch((e) => showActionError("pin_clipboard_entry", e));
}

/**
 * 0.16.1: 右键菜单按 actions 数组组装。
 * (a) 先展开 actions 全部动作（label 由 kind 派生，有 hint 用 hint）
 * (b) 若 lnkPath 存在且为真实路径（首个 action 非 run），追加文件管理附加项
 * (c) 0.16.0 止血分支删除，由 actions 派生取代
 */
function itemMenu(li, {selectedText = ""} = {}) {
    const data = readData(li);
    if (data.isError) return errorMenu(li.dataset.errorText || li.querySelector(".item-name")?.textContent || "");
    const inClipboard = clipboardMode.isActive();
    const source = li.dataset.source || "";
    const lnkPath = li.dataset.lnkPath || "";
    const clipboardRef = refFromLi(li);
    const task = {...clipboardMode.getSelectionSnapshot(), refs: clipboardRef ? [clipboardRef] : [],
        keys: clipboardRef ? [refKey(clipboardRef)] : []};
    const closeAfterOpen = () => {
        const current = clipboardMode.getSelectionSnapshot();
        if (!inClipboard || (clipboardMode.isActive() && current.epoch === task.epoch && current.revision === task.revision)) hideWindow();
    };
    let actions = [];
    try {
        actions = JSON.parse(li.dataset.actions || "[]");
    } catch (e) {
        console.error("actions parse failed:", e);
    }
    const firstKind = actions[0]?.kind || "";
    // 内置动作（kind=run）不是真实文件路径，隐藏"打开文件夹/复制路径"等文件相关菜单
    const isRealPath = lnkPath && firstKind !== "run";
    const items = [];

    // (a) 展开 actions 全部动作（hint 存 i18n key，用 t() 渲染）
    // 0.20.3：颜色项不再跳过 (a) 段——当做文本处理，默认 Copy 动作正常显示。
    // 0.25.27：编辑/钉便签标注快捷键（与剪贴板模式键盘行为一致，菜单即契约）
    for (const action of actions) {
        const copySelection = !!selectedText && clipboardRef?.kind === 'text' && action.kind === 'copy';
        const label = copySelection ? t('menu.copySelection')
            : action.hint ? t(action.hint) : (KIND_LABELS[action.kind]?.() ?? null);
        if (!label) continue; // 无标签的动作（如无 hint 的 run）跳过
        const isClipboardCopy = inClipboard && (
            (clipboardRef?.kind === 'text' && action.kind === 'copy')
            || (clipboardRef?.kind === 'image' && action.kind === 'run' && action.runId === 'copy_clipboard_image')
        );
        const shortcut = copySelection || isClipboardCopy ? "Ctrl+C"
            : action.runId === "edit_text_item" ? "Alt+E"
            : action.runId === "pin_text_item" ? "Alt+S" : undefined;
        items.push({
            label,
            shortcut,
            run: copySelection ? () => copyText(selectedText) : () => activateItem({...data, actions: [action]}),
        });
    }

    // P2-#18: edit/pin 已由后端 actions 声明（edit_text_item / pin_text_item），
    // (a) 段从 actions 数组派生即可，不再在此硬编码追加。

    // 0.20.3：颜色结果项或剪贴板颜色项——追加多格式复制 + 贴便签
    let colorHex = null;
    if (source === "color") {
        colorHex = actions[0]?.payload || "";
    } else if (source === "clipboard") {
        // 剪贴板文本项降级检测：如果文本恰好是颜色字面量
        const clipText = li.querySelector(".item-name")?.textContent || "";
        const clipColor = parseColor(clipText);
        if (clipColor) colorHex = clipColor.hex;
    }
    if (colorHex) {
        const colorItems = colorMenuItems(colorHex);
        if (colorItems.length) {
            items.push({separator: true});
            items.push(...colorItems);
        }
    }

    // 图片操作使用显式类型引用；剪贴板模式复用批量管线及选择快照。
    if (clipboardRef?.kind === "image") {
        items.push({separator: true});
        items.push({
            label: t("menu.pin"),
            run: () => {
                if (inClipboard) clipboardMode.batchPin(task);
                else pinClipboardImage(clipboardRef.id).then(closeAfterOpen).catch(e => showActionError('pin_clipboard_image', e));
            },
        });
        // 0.20.4：历史图片右键"编辑"——直接进入图片编辑器，不先覆盖剪贴板
        items.push({
            label: t("menu.editImage"),
            run: () => {
                openImageEditorFromHistory(clipboardRef.id)
                    .then(closeAfterOpen)
                    .catch((e) => {
                        const err = normalizeError(e);
                        // 0.20.7：同时判断 code 与 detail.reason，未来新增其他 invalid_state 不会被误当作成功
                        if (err.code === "invalid_state" && err.detail?.reason === "already_active") {
                            // 后端已激活现有编辑器窗口，只需关闭主窗
                            closeAfterOpen();
                        } else {
                            showActionError('open_image_editor_from_history', err);
                        }
                    });
            },
        });
    }

    // 0.26.9：文件条目操作——复制路径列表（原始路径每行一个）与存为图片条目。
    // 「存为图片」以用户点击触发后端校验（单路径 + 受限路径 + 解码），
    // 菜单打开时不读取文件判断可用性。
    if (clipboardRef?.kind === "file") {
        items.push({separator: true});
        items.push({
            label: t("menu.copyFilePaths"),
            shortcut: "Ctrl+C",
            run: () => {
                if (inClipboard) clipboardMode.batchCopyPaths(task);
                else runBuiltinAction("copy_clipboard_file_paths", {ids: [clipboardRef.id]})
                    .catch(e => showActionError("copy_clipboard_file_paths", e));
            },
        });
        // 文件名只作菜单提示；点击后的后端按 magic 重检，伪装扩展名的 GIF 也拒绝入库。
        // 多文件标题带 +N，不匹配；GIF 不提供转成静态图片历史的入口。
        const fileName = li.querySelector(".item-name")?.textContent || "";
        if (/\.(png|jpe?g|webp)$/i.test(fileName)) {
            items.push({
                label: t("menu.saveAsImage"),
                run: () => {
                    runBuiltinAction("save_clipboard_file_as_image", {id: clipboardRef.id})
                        .catch(e => showActionError("save_clipboard_file_as_image", e));
                },
            });
            for (const [label, action, open] of [
                ["menu.saveAsImageAndPin", "pin_clipboard_image", id => pinClipboardImage(id)],
                ["menu.saveAsImageAndEdit", "open_image_editor_from_history", id => openImageEditorFromHistory(id)],
            ]) items.push({label: t(label), run: async () => {
                let imageId;
                try {
                    imageId = await saveClipboardFileAsImage(clipboardRef.id);
                } catch (e) {
                    showActionError("save_clipboard_file_as_image", e);
                    return;
                }
                try {
                    await open(imageId);
                    closeAfterOpen();
                } catch (e) {
                    const err = normalizeError(e);
                    if (action === "open_image_editor_from_history" && err.code === "invalid_state"
                        && err.detail?.reason === "already_active") closeAfterOpen();
                    else showActionError(action, {...err, message: t("clipboard.saved_image_action_failed", {message: err.message})});
                }
            }});
        }
    }

    // 0.26.16：置顶——三类历史条目（文本/图片/文件）统一支持。档位文案恒为
    // 「置顶 N 小时」：已置顶时再点同档位 = 从现在起重新计 N 小时（重置语义，
    // 不用「续期」等换词——徽章剩余时间已表达当前状态，避免叠加/重置歧义）；
    // 已置顶条目额外提供「取消置顶」。执行走 pin_clipboard_entries 专用
    // command（列表管理入口，不隐藏主窗），完成后强制刷新列表排序。
    // 限定已知三类：未来新类型（如 audio）未获 Capability 支持，不提供入口。
    if (clipboardRef && PINNABLE_KINDS.has(clipboardRef.kind)) {
        const pinnedUntil = Number(li.dataset.pinnedUntil || 0);
        const isPinned = pinnedUntil > Math.floor(Date.now() / 1000);
        items.push({separator: true});
        for (const hours of PIN_TTL_HOURS) {
            items.push({
                label: t("menu.pin_for", {hours}),
                run: () => pinEntry(clipboardRef, hours),
            });
        }
        if (isPinned) {
            items.push({label: t("menu.unpin"), run: () => pinEntry(clipboardRef, 0)});
        }
    }

    if (clipboardRef && selectionActions([clipboardRef]).some(action => action.id === 'delete')) {
        items.push({separator: true}, {label: t('menu.delete'), shortcut: 'Alt+D',
            danger: true, run: () => {
                if (inClipboard) clipboardMode.batchDelete(task);
                else manageClipboardEntries('delete', [clipboardRef]).then(outcomes => {
                    const failure = outcomes.find(outcome => !outcome.success);
                    if (failure) throw new Error(failure.error);
                    retrigger();
                }).catch(e => showActionError('delete_clipboard_item', e));
            }});
    }

    // (b) 文件管理附加项（不混入 capability actions，仍由前端按 lnkPath 追加）
    if (isRealPath && (source === "file" || source === "start_menu")) {
        const isShellPath = lnkPath.toLowerCase().startsWith("shell:");
        const fileName = lnkPath.split(/[\\/]/).pop() || lnkPath;
        const baseName = fileName.replace(/\.[^.]*$/, "");
        const dirPath = lnkPath.replace(/[\\/][^\\/]*$/, "");
        const isLnk = lnkPath.toLowerCase().endsWith(".lnk");

        items.push({separator: true});
        if (isShellPath) {
            // UWP/MSIX 应用：无文件路径，隐藏文件相关菜单项
            items.push({label: t("menu.copyId"), run: () => copyText(lnkPath)});
        } else {
            // 传统桌面应用：完整菜单
            items.push({
                label: t("menu.openFolder"), run: () => {
                    openContainingFolder(lnkPath).catch((e) => console.error(e));
                }
            });
            if (isLnk) {
                items.push({
                    label: t("menu.openLnkTarget"), run: () => {
                        openLnkTarget(lnkPath).catch((e) => console.error(e));
                    }
                });
            }
            items.push({separator: true});
            items.push({label: t("menu.copyPath"), run: () => copyText(dirPath)});
            items.push({label: t("menu.copyFullPath"), run: () => copyText(lnkPath)});
            items.push({label: t("menu.copyName"), run: () => copyText(baseName)});
            items.push({label: t("menu.copyFullName"), run: () => copyText(fileName)});
        }
        items.push({separator: true});
        items.push({
            label: t("menu.resetHistory"), run: () => {
                resetItemHistory(lnkPath).then(() => retrigger()).catch((e) => console.error(e));
            }, danger: true
        });
    }

    return items;
}

function errorMenu(text) {
    const snapshot = String(text);
    return snapshot ? [{label: t("menu.copyError"), run: () => copyText(snapshot)}] : [];
}

async function copyText(text) {
    try {
        await copyToClipboard(text);
    } catch (e) {
        console.error("clipboard write failed:", e);
    }
}

/** 0.16.1: 从 <li> 读出激活所需数据，返回 actions 数组。 */
function readData(li) {
    let actions = [];
    try {
        actions = JSON.parse(li.dataset.actions || "[]");
    } catch (e) {
        console.error("actions parse failed:", e);
    }
    return {
        lnkPath: li.dataset.lnkPath,
        calcValue: li.dataset.calcValue,
        isError: li.dataset.isError === "true",
        clipboardRef: refFromLi(li),
        actions,
    };
}
