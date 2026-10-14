//! 剪贴板模式快捷键判定与目标解析纯逻辑（0.20.8）。
//!
//! 无 DOM 依赖，可单测覆盖纯 Alt、大小写、IME、非空 query、
//! 文本/图片、无 active、已有多选等组合。
//!
//! **快捷键契约**（§3.7）：
//! - `Alt+E` 编辑 active 文本项（复用 `edit_text_item` action）；
//!   0.25.27 起多选时改为合并编辑选中（菜单即契约，与右键标注一致）
//! - `Alt+D` / 空查询 Delete 删除选择集；无选择时删除 active 历史项
//! - `Alt+S` 0.25.27：active 文本项贴为便签；多选时合并选中为一张便签
//! - 裸 `Delete` 仅在 query 为空时等价 `Alt+D`；query 非空时保留输入框前删字符语义
//! - IME composition 期间全部放行
//! - 图片项不响应 `Alt+E`（不误开文本编辑器）
//! - 颜色降级结果等非历史项不响应删除
//! - AltGr 组合键（Windows 上表现为 Ctrl+Alt）必须被 `!e.ctrlKey` 排除

import {entryRef} from './clipboard-entry.js';

// ── 快捷键类型 ─────────────────────────────────────────────────────────────────

/** @typedef {"edit" | "delete" | "sticky" | "none"} ShortcutAction */

// ── 事件门禁 ─────────────────────────────────────────────────────────────────

/**
 * 判断事件是否处于 IME composition 状态。
 * @param {KeyboardEvent} e
 * @returns {boolean}
 */
export function isImeComposing(e) {
    return !!(e.isComposing || e.keyCode === 229);
}

/**
 * 判断是否为纯 Alt 修饰键组合（不含 Ctrl/Meta/Shift）。
 * AltGr 在 Windows 上表现为 Ctrl+Alt，必须被 !e.ctrlKey 排除。
 * @param {KeyboardEvent} e
 * @returns {boolean}
 */
export function isPureAlt(e) {
    return !!(e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey);
}

// ── 快捷键类型解析 ─────────────────────────────────────────────────────────────

/**
 * 从键盘事件解析快捷键动作类型。
 *
 * @param {KeyboardEvent} e
 * @param {boolean} queryIsEmpty — 输入框 trim 后是否为空
 * @returns {ShortcutAction} "edit" | "delete" | "sticky" | "none"
 *
 * 规则：
 * - IME composition → "none"（放行）
 * - 纯 Alt + E（大小写不敏感）→ "edit"
 * - 纯 Alt + D（大小写不敏感）→ "delete"
 * - 纯 Alt + S（大小写不敏感）→ "sticky"（0.25.27）
 * - 裸 Delete（无 Ctrl/Meta/Alt）→ query 为空时 "delete"，非空时 "none"
 * - 其它 → "none"
 */
export function resolveShortcutAction(e, queryIsEmpty) {
    if (isImeComposing(e)) return "none";

    const pureAlt = isPureAlt(e);

    if (pureAlt && (e.key === "e" || e.key === "E")) {
        return "edit";
    }

    if (pureAlt && (e.key === "d" || e.key === "D")) {
        return "delete";
    }

    if (pureAlt && (e.key === "s" || e.key === "S")) {
        return "sticky";
    }

    // 裸 Delete：仅 query 为空时等价 Alt+D
    if (!pureAlt && !e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey && e.key === "Delete") {
        return queryIsEmpty ? "delete" : "none";
    }

    return "none";
}

// ── Active 项目标解析 ───────────────────────────────────────────────────────────

/**
 * 从 active 项数据中查找 edit_text_item action。
 *
 * @param {{ actions?: Array<{kind?: string, runId?: string, runArg?: any, hitId?: string}>, isImage?: boolean, source?: string } | null} activeItem
 * @returns {{ kind: "run", runId: "edit_text_item", runArg?: any, hitId?: string } | null}
 *   找到则返回该 action 对象，否则返回 null。
 *   图片项或无 active 项返回 null。
 */
export function findEditAction(activeItem) {
    if (entryRef(activeItem)?.kind !== 'text') return null;

    const actions = activeItem.actions;
    if (!Array.isArray(actions)) return null;

    return actions.find((a) => a.kind === "run" && a.runId === "edit_text_item") ?? null;
}

// ── 重置（单测用）───────────────────────────────────────────────────────────

/** 无状态模块，无内部状态可重置。保留以保持与其他模块单测的一致性。 */
export function _resetForTest() {
    // no-op
}
