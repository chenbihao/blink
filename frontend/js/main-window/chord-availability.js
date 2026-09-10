//! Chord action 可用性纯函数：隔离 query 门禁与动作查找，供 DOM 层和单测共用。

/** 非空 query 下是否仍可触发。兼容尚未携带新字段的旧响应。 */
export function isAvailableWithQuery(action) {
    return action?.available_with_query === true || action?.requires_input === true;
}

/** 当前 query 状态下是否可触发。 */
export function isAvailableNow(action, hasQuery) {
    return !hasQuery || isAvailableWithQuery(action);
}

/** 按动作 id 查找当前可触发的 tap action。 */
export function findTapActionById(actions, actionId, hasQuery) {
    return actions.find((action) =>
        action.id === actionId
        && action.semantic === "tap"
        && isAvailableNow(action, hasQuery)
    ) ?? null;
}

/** 按生效键查找当前可触发的 tap action。 */
export function findTapActionByKey(actions, key, hasQuery) {
    const lower = String(key).toLowerCase();
    return actions.find((action) =>
        String(action.key).toLowerCase() === lower
        && action.semantic === "tap"
        && isAvailableNow(action, hasQuery)
    ) ?? null;
}

/** 剪贴板历史是主窗内原地模式切换，不应经后端再次 invoke 主窗。 */
export function isClipboardModeSwitch(action) {
    return action?.id === "clipboard_history";
}
