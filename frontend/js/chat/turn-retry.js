/** 只在当前会话保留有界的授权引用快照，不写 DOM、存储或历史。 */
export function createTurnRetryStore({limit = 32, ttl = 30 * 60 * 1000, now = Date.now} = {}) {
    const turns = new Map();
    return {
        remember(id, attachments) {
            turns.set(id, {at: now(), attachments: attachments.map(a => ({...a}))});
            while (turns.size > limit) turns.delete(turns.keys().next().value);
        },
        get(id, badges = []) {
            if (!badges.length) return [];
            const turn = turns.get(id);
            if (!turn || now() - turn.at >= ttl || turn.attachments.length !== badges.length) {
                turns.delete(id);
                return null;
            }
            return turn.attachments.map(a => ({...a}));
        },
        clear() { turns.clear(); },
        delete(id) { turns.delete(id); },
    };
}

/** 从后端真实用户轮解析行 id，工具回投已在历史投影层排除。 */
export function resolvePersistedTurn(messages, id, snapshots) {
    const users = messages.filter(msg => msg.role === "user");
    const persisted = snapshots.filter(msg => msg.role === "user");
    const index = users.findIndex(msg => msg.id === id);
    if (index < 0 || users.length !== persisted.length || users.some((msg, i) =>
        msg.content !== persisted[i].text || (msg.messageId != null && msg.messageId !== persisted[i].message_id))) {
        throw new Error("消息已变化，请重新加载会话后重试");
    }
    const messageId = persisted[index]?.message_id;
    if (!Number.isSafeInteger(messageId) || messageId <= 0) throw new Error("消息标识无效，请重新加载会话");
    return messageId;
}
