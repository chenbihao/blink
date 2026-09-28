//! 建议采纳共享路径（0.24 §3.6 / §5.6）。
//!
//! Tab / Shift+Tab / 点击共用同一 acceptance：staleness 校验 → 类型化 action
//! 本地执行 → fire-and-forget 遥测。ghost.js（Completion 影子）与
//! suggestion-bar.js（双槽建议行）都委托本模块，保证"采纳"只有一份实现。
//!
//! **0.24 采纳零执行能力**：action 只有 RouteQuery（改 query 重搜）与
//! EnterAiMode（进主窗口 AI 模式）两种无副作用变体——本地乐观执行即可，
//! 最坏情况立即可见可撤销。
//!
//! stalenessCheck 由 main.js 注入（search.isLiveRevision）——本模块不 import
//! search，避免 accept → search → ghost/bar → accept 的模块环。
//! ai-mode 的引用与 ghost.js 既有做法一致（静态 import，函数体内调用；
//! ai-mode ↔ ghost 的加载期环在现有代码已存在且安全）。

import {queryEl} from "./dom.js";
import {reportSuggestionAdoption} from "../shared/api.js";
import * as aiMode from "./ai-mode.js";

// 过期校验回调（main.js 注入）。null = 未注入（防御：视为不过期，保持可用）。
let stalenessCheck = null;

/**
 * 注入过期校验回调（0.24 §3.6）。main.js 启动时接 search.isLiveRevision。
 * @param {(revision: number|null) => boolean} fn 返回 false = 建议已过期
 */
export function setStalenessCheck(fn) {
    stalenessCheck = fn;
}

/**
 * 执行一次建议采纳（键盘与点击的统一入口）。
 *
 * 流程（§3.6）：revision 与前端当前 seq 不等 → 本地拒绝（零 IPC、零延迟）；
 * 相等则按 action 本地执行，随后 fire-and-forget 上报（失败静默）。
 *
 * @param {{id: string, kind: string, action?: object}|null} suggestion 建议对象
 * @param {"primary"|"secondary"} slot 槽位（遥测用）
 * @param {number|null} revision 渲染该建议时的 search seq
 * @param {{onAccepted?: (sug: object) => void, onStale?: (sug: object) => void}} [hooks]
 *   onAccepted：action 执行前调用（ghost/bar 各自清空呈现，与 ghost 旧行为一致）；
 *   onStale：过期/无 action 拒绝时调用（调用方决定是否清空本地呈现）。
 * @returns {boolean} true = 已采纳（调用方应 preventDefault）；false = 拒绝/无 action
 */
export function acceptSuggestion(suggestion, slot, revision, hooks = {}) {
    if (!suggestion) return false;

    // 过期防护：建议来自较早一轮搜索（用户已继续输入）→ 视为无建议，不吞键
    if (stalenessCheck && !stalenessCheck(revision)) {
        hooks.onStale?.(suggestion);
        return false;
    }

    const action = suggestion.action;

    // 防御：后端契约保证 action 必填；缺失时视为不可采纳，避免误吞 Tab
    if (!action?.routeQuery && !action?.enterAiMode) {
        hooks.onStale?.(suggestion);
        return false;
    }

    hooks.onAccepted?.(suggestion);
    // 采纳遥测（§3.6 单向 fire-and-forget）：本地执行后上报，失败静默
    reportSuggestionAdoption(suggestion.id, slot, revision).catch(() => {});

    if (action.enterAiMode) {
        // AskAi：不改输入框，直接进主窗口 AI 模式（ChatService ephemeral 对话）
        aiMode.enterAiMode(action.enterAiMode.prompt);
        return true;
    }

    // RouteQuery：回写输入框 + 光标置尾 + 派发 input 走一轮新搜索
    //（补全采纳与翻译采纳同用；翻译即 query 变为 "翻译 xxx" 走确定路由）
    const rep = action.routeQuery.query;
    queryEl.value = rep;
    queryEl.setSelectionRange(rep.length, rep.length);
    queryEl.dispatchEvent(new Event("input", {bubbles: true}));
    return true;
}
