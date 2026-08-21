/** Normalize legacy string entries and current ModelMeta objects into a lookup map. */
export function buildModelMetaMap(models) {
    return new Map((models || []).map((model) => {
        const meta = typeof model === "string" ? {id: model} : model;
        return [meta.id, meta];
    }));
}

/** Return a fetched/catalog context window without inventing a value for manual entries. */
export function fetchedContextWindow(models, modelId) {
    return buildModelMetaMap(models).get(modelId)?.context_window ?? null;
}

/** Format capacities using the decimal units advertised by model vendors. */
export function formatContextWindowLabel(contextWindow) {
    if (contextWindow >= 1_000_000) {
        const millions = contextWindow / 1_000_000;
        return `${Number.isInteger(millions) ? millions : millions.toFixed(1)}M`;
    }
    return `${Math.round(contextWindow / 1_000)}K`;
}
