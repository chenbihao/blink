//! AI 设置中必须与后端真实行为一致的纯语义 helper。

export const DEFAULT_AI_HARD_TIMEOUT_MS = 20_000;

export function effectiveAIHardTimeoutMs(configured) {
    return Number.isFinite(configured) ? configured : DEFAULT_AI_HARD_TIMEOUT_MS;
}

export function clampAIHardTimeoutMs(raw) {
    const value = Number.parseInt(raw, 10);
    return Number.isNaN(value) ? null : Math.max(500, Math.min(30_000, value));
}

export function memoryExpertVisibility(mode, recallEnabled) {
    return {
        fixedCount: mode === "fixed_count",
        tokenAware: mode === "token_aware",
        recallTopK: recallEnabled,
    };
}

export function formatModelContextWindow(contextWindow) {
    const value = Number(contextWindow);
    return Number.isFinite(value) && value > 0 ? `${value} tokens` : null;
}
