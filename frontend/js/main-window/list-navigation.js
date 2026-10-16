//! 主窗口列表滚轮偏好与余量；不依赖具体结果来源、DOM 或 IPC。
const WHEEL_STEP = 100;

export function createListNavigation() {
    let mode = "step", remainder = 0, context = null;
    return {
        refreshFromConfigData(cfg) {
            const next = cfg?.wheel_mode === "page" ? "page" : "step";
            if (next !== mode) { mode = next; remainder = 0; }
        },
        reset() { remainder = 0; context = null; },
        consumeWheel(event, nextContext) {
            if (context !== nextContext) { remainder = 0; context = nextContext; }
            const scale = event.deltaMode === 1 ? 40 : event.deltaMode === 2 ? WHEEL_STEP : 1;
            const delta = event.deltaY * scale;
            if (!Number.isFinite(delta) || delta === 0) return {mode, steps: 0};
            if (remainder * delta < 0) remainder = 0;
            remainder += delta;
            const steps = Math.trunc(remainder / WHEEL_STEP);
            remainder -= steps * WHEEL_STEP;
            return {mode, steps};
        },
    };
}

export const listNavigation = createListNavigation();
