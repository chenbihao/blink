//! Tauri invoke 返回 request_id 前，后台流事件可能已经到达。
//! 这里只接住当前 conversation 等待 request_id 期间的事件；建立 id 后仅回放同一请求。

export function createEarlyStreamBuffer() {
    let awaitingConversationId = null;
    let events = [];
    return {
        begin(conversationId) {
            awaitingConversationId = conversationId;
            events = [];
        },
        capture(payload) {
            if (!awaitingConversationId || payload?.conversation_id !== awaitingConversationId) return false;
            events.push(payload);
            return true;
        },
        resolve(requestId) {
            const matched = events.filter((payload) => payload.request_id === requestId);
            awaitingConversationId = null;
            events = [];
            return matched;
        },
        clear() {
            awaitingConversationId = null;
            events = [];
        },
    };
}
