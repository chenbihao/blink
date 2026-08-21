//! 模型容量控件的离散常用档位。range 使用档位索引，number 保留任意精确值。

export const MAX_OUTPUT_TOKEN_STOPS = [1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144];
export const CONTEXT_WINDOW_STOPS = [8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576, 2097152];

export function nearestStopIndex(stops, value) {
    const target = Number(value);
    if (!Number.isFinite(target)) return 0;
    let best = 0;
    for (let i = 1; i < stops.length; i += 1) {
        if (Math.abs(stops[i] - target) < Math.abs(stops[best] - target)) best = i;
    }
    return best;
}

export function formatCapacityStop(value) {
    if (value >= 1048576) {
        const mib = value / 1048576;
        return `${Number.isInteger(mib) ? mib : mib.toFixed(1)}M`;
    }
    return `${Math.round(value / 1024)}K`;
}
