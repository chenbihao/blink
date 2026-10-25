/** Event/take 共用一条串行消费链；失败重放只重试尚未完成的步骤。 */
export function createPrefillConsumer({getConversationId, newConversation, insertText, adopt, add}) {
    let tail = Promise.resolve();
    let record = null;
    const pending = new Map();
    return function consume(payload, revision = 0) {
        if (revision && pending.has(revision)) return pending.get(revision);
        const task = tail.then(async () => {
            if (revision && record && revision < record.revision) return true;
            if (!revision || record?.revision !== revision) {
                record = {revision, started: false, inserted: false, done: false, conversationId: null};
            }
            const current = record;
            if (current.done) return true;
            if (!current.started) {
                const preparation = payload?.newConversation ? newConversation() : null;
                // 新会话在首个 await 前已创建；冻结它，防止准备期间切换会话串台。
                current.conversationId = getConversationId();
                current.started = true;
                await preparation;
            }
            if (current.conversationId !== getConversationId()) return false;
            if (!current.inserted) {
                if (typeof payload?.text === "string" && payload.text.trim()) insertText(payload.text);
                current.inserted = true;
            }
            const attachments = Array.isArray(payload?.attachments) ? payload.attachments : [];
            if (attachments.length) {
                const count = await adopt(attachments.map(a => a.ref), current.conversationId);
                if (count !== attachments.length || current.conversationId !== getConversationId()) return false;
                const result = add(attachments);
                if (result?.rejected?.length) return false;
            }
            current.done = true;
            return true;
        });
        const safe = task.catch(error => {
            console.warn("[chat] 预填消费失败，保留 pending:", error);
            return false;
        });
        tail = safe;
        if (revision) {
            pending.set(revision, safe);
            safe.then(() => pending.delete(revision));
        }
        return safe;
    };
}
