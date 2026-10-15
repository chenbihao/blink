/** 尺寸均为截图位图像素；保留既有工具倍率。 */
export const SIZE_LIMITS = Object.freeze({stroke: {min: 1, max: 32}, brush: {min: 2, max: 128}});

export function clampSize(category, value) {
    const limits = SIZE_LIMITS[category];
    const number = Number(value);
    return limits && Number.isFinite(number)
        ? Math.max(limits.min, Math.min(limits.max, Math.round(number))) : null;
}

export function stepSize(category, value, direction) {
    const current = clampSize(category, value);
    if (current === null || !direction) return current;
    // 用同一档位序列双向移动，跨细/粗边界时可逆。
    const {min, max} = SIZE_LIMITS[category];
    const stops = [];
    for (let n = min; n <= max; n++) {
        if (category === 'stroke' ? (n <= 8 || n % 2 === 0)
            : (n <= 16 || (n <= 48 ? n % 2 === 0 : n % 4 === 0))) stops.push(n);
    }
    return direction > 0 ? (stops.find(n => n > current) ?? max)
        : (stops.findLast(n => n < current) ?? min);
}

export function brushDiameter(tool, size) {
    switch (tool) {
        case 'pencil': return size;
        case 'highlight-multiply':
        case 'highlight-translucent': return size * 4;
        case 'eraser': return Math.max(6, size * 3) * 2;
        case 'mosaic':
        case 'pixelate':
        case 'blur': return size * 2;
        case 'number': return Math.max(10, size * 1.2) * 2;
        default: return 0;
    }
}

/** 触控板小 delta 累积；上下文/方向改变不继承旧余量。 */
export function createSizeWheel() {
    let remainder = 0, context;
    return {
        reset() { remainder = 0; context = undefined; },
        steps(event, key) {
            const delta = event.deltaY * (event.deltaMode === 1 ? 40 : event.deltaMode === 2 ? 100 : 1);
            if (!Number.isFinite(delta) || !delta) return 0;
            if (context !== key || remainder * delta < 0) remainder = 0;
            context = key;
            remainder += delta;
            const steps = Math.trunc(remainder / 100);
            remainder -= steps * 100;
            return Math.max(-16, Math.min(16, steps)) || 0;
        },
    };
}
