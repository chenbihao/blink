/**
 * 多手柄时序条纯逻辑（0.23.17）。
 *
 * 一条 0..max 的时间轴上放多个可拖手柄（如 软窗口/硬窗口/未提交上限），
 * 手柄之间按「顶住」语义互相约束（不能越过相邻手柄），并各自收敛到
 * 自己的 [min, max] 与步长。DOM 装配在 voice-advanced-ui.js；本模块
 * 不碰 DOM，可被 node 单测直接覆盖。
 *
 * 约束语义与后端 sanitize 对齐：
 * - 切分窗口条：soft < hard <= max_uncommitted（normalizeVadWindows）
 * - 停顿条：min_silence < strong_pause < long_pause（排序呈现；后端硬约束
 *   只有 long > strong + 50ms，排序由顶住交互保证）
 * - 预览节奏条：preview_refresh < preview_window（normalizeRecognitionConfig）
 */

/**
 * 创建时序条模型。
 *
 * @param {object} options
 * @param {number} options.max 轴上限（与所有手柄同一量纲，如 30000ms / 30s）
 * @param {Array<{key: string, min: number, max: number, step?: number,
 *                order?: number, gapPrev?: number}>} options.handles 手柄定义。
 *   order 为轴上期望的排序位置（默认 0）；order 相同或为 0 的手柄互不顶住。
 *   gapPrev 是本手柄与**左侧**相邻手柄的最小间距（默认 1；0 允许重合），
 *   对应后端约束的不等式方向——如 切分窗口 hard ≤ cap 允许相等（默认
 *   12/12），故 cap.gapPrev = 0；soft < hard 严格小于，故 hard.gapPrev = 1。
 * @returns {object} 控制器：值读写 / 拖拽换算 / 键盘步进 / 顶住约束
 */
export function createTimespanModel(options) {
    const axisMax = options.max;
    const handles = options.handles;
    if (!Number.isFinite(axisMax) || axisMax <= 0) {
        throw new Error("timespan axis max must be positive");
    }
    if (!Array.isArray(handles) || handles.length === 0) {
        throw new Error("timespan requires at least one handle");
    }
    for (const handle of handles) {
        if (!handle.key || !Number.isFinite(handle.min) || !Number.isFinite(handle.max)) {
            throw new Error(`timespan handle ${handle.key} missing key/min/max`);
        }
    }

    /** 手柄键 → 当前值（浮点或整数，与调用方一致）。 */
    const values = new Map();

    function clampToHandle(handle, value) {
        const clamped = Math.min(handle.max, Math.max(handle.min, value));
        const step = handle.step > 0 ? handle.step : 1;
        // 步长吸附：先取整到 step 网格，再夹回边界（保证边界值可达）
        const grid = Math.round((clamped - handle.min) / step) * step + handle.min;
        return Math.min(handle.max, Math.max(handle.min, grid));
    }

    function gapBelow(handle) {
        return handle.gapPrev === undefined ? 1 : handle.gapPrev;
    }

    /**
     * 顶住约束：把手柄 value 收敛到 [自身 min/max] 与排序邻居之间。
     * 返回实际生效值（不修改其他手柄——顶住 = 自己停下，不是推别人）。
     */
    function applyNeighborConstraints(key, value) {
        const handle = handles.find((item) => item.key === key);
        if (!handle) return value;
        const order = handle.order ?? 0;
        let effective = clampToHandle(handle, value);
        if (order > 0) {
            for (const other of handles) {
                const otherOrder = other.order ?? 0;
                if (other.key === key || otherOrder === 0 || otherOrder === order) continue;
                const otherValue = values.get(other.key);
                if (!Number.isFinite(otherValue)) continue;
                if (otherOrder > order) {
                    // 右侧邻居：不得超过 邻居值 − 邻居的左间距
                    effective = Math.min(effective, otherValue - gapBelow(other));
                } else {
                    // 左侧邻居：不得低于 邻居值 + 自身左间距
                    effective = Math.max(effective, otherValue + gapBelow(handle));
                }
            }
        }
        // 约束可能把手柄推出自身边界（邻居已越界的防御态）：以自身边界优先
        return clampToHandle(handle, effective);
    }

    return {
        axisMax,
        handles,
        setValues(initial) {
            for (const handle of handles) {
                const raw = initial[handle.key];
                values.set(handle.key, clampToHandle(handle, Number.isFinite(raw) ? raw : handle.min));
            }
        },
        getValues() {
            const result = {};
            for (const handle of handles) {
                result[handle.key] = values.get(handle.key);
            }
            return result;
        },
        /** 拖拽/键盘设置单手柄：顶住约束后返回生效值。 */
        setHandle(key, value) {
            const effective = applyNeighborConstraints(key, value);
            values.set(key, effective);
            return effective;
        },
        /** 键盘步进（delta 为正负步长）。 */
        nudge(key, delta) {
            const current = values.get(key);
            if (!Number.isFinite(current)) return undefined;
            return this.setHandle(key, current + delta);
        },
        /** 值 → 轴百分比（0..100，用于定位手柄/选区）。 */
        percentOf(key) {
            const value = values.get(key);
            if (!Number.isFinite(value)) return 0;
            return (value / axisMax) * 100;
        },
        /** 轴上比例（0..1）→ 值（拖拽换算；不落状态）。 */
        valueAtPosition(ratio) {
            const clamped = Math.min(1, Math.max(0, ratio));
            return clamped * axisMax;
        },
        /** 找出轴上某位置应抓取的手柄（最近者）。 */
        nearestHandle(ratio) {
            const value = this.valueAtPosition(ratio);
            let nearest = null;
            let nearestDistance = Infinity;
            for (const handle of handles) {
                const handleValue = values.get(handle.key);
                if (!Number.isFinite(handleValue)) continue;
                const distance = Math.abs(handleValue - value);
                if (distance < nearestDistance) {
                    nearestDistance = distance;
                    nearest = handle.key;
                }
            }
            return nearest;
        },
    };
}

/**
 * 选区带（draft 目标窗口 [target−tolerance, target+tolerance]）纯计算。
 * target = 0 表示关闭（无选区）。
 */
export const draftTargetBand = {
    /** 选区 [start, end]（与轴同量纲）；关闭或非法时返回 null。 */
    range(target, tolerance) {
        if (!(target > 0)) return null;
        if (!Number.isFinite(tolerance) || tolerance < 0) return null;
        return {start: target - tolerance, end: target + tolerance};
    },
    /** 边缘拖拽：由边缘位置与中心推 tolerance（调用方负责 clamp/取整）。 */
    toleranceFromEdge(edge, target) {
        if (!(target > 0) || !Number.isFinite(edge)) return 0;
        return Math.abs(edge - target);
    },
    /**
     * 目标中心在当前"未提交上限"下可达的最大值（0.23.17）。
     *
     * 与后端 `RecognitionConfig::sanitize` 同一约束：优选窗上界
     * `target + tolerance` 不得超过 `maxUncommittedS`，否则目标永远等不到
     * 合格停顿（等到的只会是硬窗口强制切），sanitize 会把目标关闭。因此
     * 拖动手柄必须停在 `cap − tolerance`：返回 0 表示空间不足（放不下
     * 目标下限 4s），调用方应提示用户先调大未提交上限。
     *
     * @param {number} capS 未提交上限（秒）；非有限值表示无约束
     * @param {number} toleranceS 宽容（秒）
     * @param {{minS?: number, maxS?: number}} [bounds] 中心的安全边界
     * @returns {number} 可达的最大中心（秒）；0 = 不可达
     */
    maxAchievableTarget(capS, toleranceS, bounds = {}) {
        const minS = Number.isFinite(bounds.minS) ? bounds.minS : 4;
        const maxS = Number.isFinite(bounds.maxS) ? bounds.maxS : 30;
        const tolerance = Number.isFinite(toleranceS) && toleranceS >= 0 ? toleranceS : 0;
        const ceiling = Number.isFinite(capS) ? capS - tolerance : maxS;
        if (ceiling < minS) return 0;
        return Math.min(maxS, ceiling);
    },
};

/**
 * 手柄标签分行（0.23.17）：轴下方每个标签都钉在自己的手柄位置，手柄靠近
 * 时标签会互相压字。这里按标签的实际像素占用做首次适配装箱——同一行内
 * 标签之间至少留 `gapPx`，放不下就落到下一行。
 *
 * 纯函数：不碰 DOM，只按调用方量好的宽度/位置计算，便于单测。
 *
 * @param {Array<{key: string, percent: number, width: number}>} items
 * @param {{trackWidth: number, gapPx?: number}} options
 * @returns {{rows: Record<string, number>, rowCount: number}} 每个 key 的行号（0 起）
 */
export function assignLabelRows(items, options) {
    const trackWidth = Number.isFinite(options?.trackWidth) && options.trackWidth > 0
        ? options.trackWidth
        : 0;
    const gapPx = Number.isFinite(options?.gapPx) ? Math.max(0, options.gapPx) : 8;
    const rows = [];
    const assignment = {};
    if (trackWidth <= 0) {
        for (const item of items) assignment[item.key] = 0;
        return {rows: assignment, rowCount: 1};
    }
    const ordered = [...items].sort((left, right) => left.percent - right.percent);
    for (const item of ordered) {
        const width = Number.isFinite(item.width) && item.width > 0 ? item.width : 0;
        const center = (Math.min(100, Math.max(0, item.percent)) / 100) * trackWidth;
        // 与 syncVisual 的对齐规则一致：轴两端贴边防溢出
        let left = center - width / 2;
        let right = center + width / 2;
        if (item.percent < 8) {
            left = center;
            right = center + width;
        } else if (item.percent > 92) {
            left = center - width;
            right = center;
        }
        let row = 0;
        for (; row < rows.length; row++) {
            const fits = rows[row].every(
                ([slotLeft, slotRight]) => right + gapPx <= slotLeft || left >= slotRight + gapPx,
            );
            if (fits) break;
        }
        if (row === rows.length) rows.push([]);
        rows[row].push([left, right]);
        assignment[item.key] = row;
    }
    return {rows: assignment, rowCount: Math.max(1, rows.length)};
}
