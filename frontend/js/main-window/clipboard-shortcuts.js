//! 剪贴板模式快捷键判定与目标解析纯逻辑（0.20.8）。
//!
//! 无 DOM 依赖，可单测覆盖纯 Alt、大小写、IME、非空 query、
//! 文本/图片、无 active、已有多选等组合。
//!
//! **快捷键契约**（§3.7）：
//! - `Alt+E` 编辑 active 文本项（复用 `edit_text_item` action）
//! - `Alt+D` 删除 active 文本或图片历史
//! - 裸 `Delete` 仅在 query 为空时等价 `Alt+D`；query 非空时保留输入框前删字符语义
//! - IME composition 期间全部放行
//! - 图片项不响应 `Alt+E`（不误开文本编辑器）
//! - 颜色降级结果等非历史项不响应删除
//! - AltGr 组合键（Windows 上表现为 Ctrl+Alt）必须被 `!e.ctrlKey` 排除

// ── 快捷键类型 ─────────────────────────────────────────────────────────────────

/** @typedef {"edit" | "delete" | "none"} ShortcutAction */

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
 * @returns {ShortcutAction} "edit" | "delete" | "none"
 *
 * 规则：
 * - IME composition → "none"（放行）
 * - 纯 Alt + E（大小写不敏感）→ "edit"
 * - 纯 Alt + D（大小写不敏感）→ "delete"
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
    if (!activeItem) return null;
    if (activeItem.isImage) return null; // 图片项不响应 Alt+E

    const actions = activeItem.actions;
    if (!Array.isArray(actions)) return null;

    return actions.find((a) => a.kind === "run" && a.runId === "edit_text_item") ?? null;
}

/**
 * 从 active 项数据中解析删除目标。
 *
 * @param {{ actions?: Array<{kind?: string, hitId?: string}>, isImage?: boolean, lnkPath?: string, source?: string } | null} activeItem
 * @returns {{ type: "text", id: string } | { type: "image", id: string } | null}
 *   文本项返回 { type: "text", id: hitId }；图片项返回 { type: "image", id: lnkPath }。
 *   无 active 项、非 clipboard 来源、或缺少 id 时返回 null。
 */
export function findDeleteTarget(activeItem) {
    if (!activeItem) return null;

    // 只有 clipboard 来源的历史项可删除
    // 颜色降级结果 (source="color") 等非历史项不得删除
    if (activeItem.source !== "clipboard") return null;

    if (activeItem.isImage) {
        // 图片项：lnkPath 持有 image_id
        const id = activeItem.lnkPath;
        if (!id) return null;
        return {type: "image", id};
    }

    // 文本项：hitId 在首个 action（copy）上
    const actions = activeItem.actions;
    if (!Array.isArray(actions)) return null;

    const hitId = actions[0]?.hitId;
    if (!hitId) return null;

    return {type: "text", id: hitId};
}

// ── 重置（单测用）───────────────────────────────────────────────────────────

/** 无状态模块，无内部状态可重置。保留以保持与其他模块单测的一致性。 */
export function _resetForTest() {
    // no-op
}
