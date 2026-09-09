/** 0.22.16：一次性 WAV 转写结果的纯解析与展示投影。 */

export function parseAudioTranscriptionCapability(result) {
    if (result?.kind !== "items" || !Array.isArray(result.items) || result.items.length !== 1) {
        throw new Error("invalid transcribe_audio result shape");
    }
    const data = result.items[0]?.data;
    if (!data || typeof data !== "object"
        || typeof data.text !== "string"
        || typeof data.engine_id !== "string"
        || typeof data.model_id !== "string"
        || typeof data.duration_ms !== "number"
        || typeof data.no_speech !== "boolean") {
        throw new Error("invalid transcribe_audio result data");
    }
    return {
        text: data.text,
        engineId: data.engine_id,
        modelId: data.model_id,
        durationMs: data.duration_ms,
        noSpeech: data.no_speech,
    };
}

export function formatAudioTranscriptionIdentity(data) {
    const seconds = Math.max(0, data.durationMs) / 1000;
    return `${data.engineId} · ${data.modelId} · ${seconds.toFixed(1)}s`;
}
