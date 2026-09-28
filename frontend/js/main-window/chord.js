//! Chord 提示（0.8.5 §6.1 / §6.4；0.24.4 提示呈现迁移）：
//! chord 配置快照 + 可用动作集的唯一真源。提示的**呈现**已迁出本模块——
//! Alt 按住时的完整键帽表由 cheat-sheet.js 投影（独立覆盖层），
//! 空 query 闲置一行提示由 statusbar.js（ResultFooter）投影。
//! 本模块只保留：配置快照、动作过滤（available_with_query）与触发键集合。
//!
//! **开关**（0.8.5.1 §6.6）：
//! - `chord_enabled=false` → refresh 清空 actions，keyboard.js 的触发门禁通过
//!   `isEnabled()` 读同一 flag，保触发链一致
//! - `chord_hint_visible=false` → getActions() 返回空（提示方据此不显示），
//!   但触发键集合仍含全部 tap 键（用户仍可 Alt+字母 触发，只是不显示提示）
//!
//! 悬浮球形态（Alt+Q 划词）是独立 webview（chord-ball.html），不在此模块。
//!
//! 0.11: Alt+Q 划词翻译 chord 已移除，chord-ball 悬浮球已删除。Alt+Space 语音输入
//! 作为 display-only chord 条目加入提示（触发仍走 native hotkey hold，不走 trigger_chord）。

import {invoke} from "../shared/tauri.js";
import {listChordActions} from "../shared/api.js";
import {queryEl} from "./dom.js";
import {findTapActionById, findTapActionByKey, isAvailableWithQuery,} from "./chord-availability.js";

let chordActions = [];

// 配置快照（启动预热 / lifecycle shown / config-changed 时刷新）
// 初值 false：config 还没到时保守禁用,避免"用户没开却弹提示"的一瞬闪现
let chordEnabled = false;
let hintVisible = true;

/** 初始化：预热 chord 配置。main.js 启动时调一次。 */
export function init() {
    // 0.21.x：启动预热 chord 配置（chordEnabled/chordActions）。
    // 若等首个 SHOWN 的异步 get_config 才刷新，首次唤起时 chordEnabled 仍是初值
    // false——readOnly 禁 IME 兜底（input-state.js）与 onChordTrigger 兜底
    // （keyboard.js）全部失效，字母落入输入法。预热后首次唤起与后续行为一致。
    refresh();
}

/** 是否启用 Chord（keyboard.js chordEligible 读此值,统一门禁）。 */
export function isEnabled() {
    return chordEnabled;
}

/**
 * 当前 Chord 动作列表（提示方渲染用：cheat-sheet 完整表 / statusbar 闲置行）。
 *
 * 输入框有文本时只返回 `available_with_query=true` 的动作；无文本时返回全部。
 * `chord_hint_visible=false` 时返回空（触发不受影响，见 getTapKeys）。
 * 该字段通常与 requires_input 一致，但剪贴板属于“以 query 过滤的模式切换”，
 * query 由前端模式消费，不是 Capability 入参。
 */
export function getActions() {
    if (!chordEnabled || !hintVisible) return [];
    const hasText = !!queryEl.value.trim();
    return hasText
        ? chordActions.filter(isAvailableWithQuery)
        : chordActions;
}

/** 按动作 id 取当前可触发的 tap action（全局跟随键回落路径使用）。 */
export function getTapActionById(actionId) {
    if (!chordEnabled) return null;
    return findTapActionById(chordActions, actionId, !!queryEl.value.trim());
}

/** 按生效键取当前可触发的 tap action（WebView keydown 兜底路径使用）。 */
export function getTapActionByKey(key) {
    if (!chordEnabled) return null;
    return findTapActionByKey(chordActions, key, !!queryEl.value.trim());
}

/**
 * 当前生效的 tap 语义 chord 键集合（0.10.7）。
 *
 * keyboard.js 的 `onChordTrigger` 用此集合判断 Alt+字母是否触发 chord。
 * 只含 `semantic === "tap"` 的动作--hold 语义（如语音输入 Alt+Space）
 * 由 native hotkey hook 的 hold 状态机处理，不走前端 keydown 路径。
 *
 * 键已 toLowerCase，与 `e.key.toLowerCase()` 直接比对。
 *
 * 非空 query 时保留 available_with_query=true 的键（如 chat、clipboard）；
 * 截图等普通入口仍只在空 query 下触发。
 */
export function getTapKeys() {
    const set = new Set();
    if (!chordEnabled) return set;
    const hasText = !!queryEl.value.trim();
    for (const a of chordActions) {
        if (a.semantic !== "tap") continue;
        if (hasText && !isAvailableWithQuery(a)) continue;
        set.add(String(a.key).toLowerCase());
    }
    return set;
}

// ── 可见性变化回调（statusbar / cheat-sheet 订阅用）───────────────────────
let onVisibilityChangeCallbacks = [];

/**
 * 订阅 chord 提示相关状态变化（0.24.4 起多订阅者：statusbar 闲置行 + cheat-sheet）。
 * 变化源：input-state projectUi 的 chord-visible 翻转、refresh 后配置/动作集刷新。
 * @param {() => void} cb
 */
export function onVisibilityChange(cb) {
    onVisibilityChangeCallbacks.push(cb);
}

/** 通知全部订阅者重投影（input-state.js projectUi / refresh 完成后调用）。 */
export function notifyVisibilityChange() {
    for (const cb of onVisibilityChangeCallbacks) cb();
}

/** 拉取 Chord 动作列表（shown / config-changed 时调）。 */
export async function refresh() {
    // 先刷新配置快照
    try {
        const cfg = await invoke("get_config");
        if (cfg) {
            // 精确消费持久化配置；新安装默认值由后端 ChordConfig 单一真源提供。
            chordEnabled = cfg.chord_enabled === true;
            hintVisible = cfg.chord_hint_visible !== false;
        }
    } catch (e) {
        /* 保持默认 false */
    }

    if (!chordEnabled) {
        // 总开关关 → 清空动作集
        chordActions = [];
        notifyVisibilityChange();
        return;
    }

    try {
        chordActions = await listChordActions();
    } catch (e) {
        console.warn("[chord] list_chord_actions 失败", e);
        chordActions = [];
    }
    notifyVisibilityChange();
}

/** 从已读好的 config 对象刷新 chord 配置并拉取动作列表（避免重复 invoke get_config）。
 * @param {object} cfg - get_config 返回的 AppConfig 对象 */
export async function refreshFromConfigData(cfg) {
    // 先刷新配置快照
    if (cfg) {
        chordEnabled = cfg.chord_enabled === true;
        hintVisible = cfg.chord_hint_visible !== false;
    }

    if (!chordEnabled) {
        chordActions = [];
        notifyVisibilityChange();
        return;
    }

    try {
        chordActions = await listChordActions();
    } catch (e) {
        console.warn("[chord] list_chord_actions 失败", e);
        chordActions = [];
    }
    notifyVisibilityChange();
}

/**
 * 把 chord action 的 key 转成渲染用的键名。
 * key=' '（语音输入）→ "Space"；其它直接大写。
 * kbd.js 的 normalize() 会把 "Space" 映射到 KEY_META.space → 显示 "Space"。
 */
export function chordKeyLabel(key) {
    return key === " " ? "Space" : key.toUpperCase();
}
