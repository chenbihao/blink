//! 图上文本划词的共享纯函数（0.23.12 自 ss-reading.js 下沉）。
//!
//! 截图 overlay 阅读模式与 pin 图文字图层共用同一套命中/距离/行块合并
//! 逻辑与"空白放行"语义：按下点到最近文字框超过 BLANK_RELEASE_THRESHOLD_CSS
//! 视为真空白（截图侧放行为移动选区手势，pin 侧放行为窗口原生拖动），
//! 阈值内仍按最近行兜底进入划词，保留"点行尾继续选词"的编辑器手感。
//!
//! 本模块无 DOM / 窗口状态依赖，坐标语义由调用方约定（通常为图片像素域）。

/** 空白放行的距离阈值（CSS px，视觉域）：超过此距离的按下不放行划词。 */
export const BLANK_RELEASE_THRESHOLD_CSS = 10;

/** 点到矩形的最短距离（同一坐标系内）；点在矩形内返回 0 */
export function pointToRectDistance(px, py, rect) {
    const dx = px < rect.x ? rect.x - px : (px > rect.x + rect.w ? px - (rect.x + rect.w) : 0);
    const dy = py < rect.y ? rect.y - py : (py > rect.y + rect.h ? py - (rect.y + rect.h) : 0);
    return Math.hypot(dx, dy);
}

/**
 * 点击点到最近文字框的距离。
 *
 * items 为当前选择轨（charBoxes 或 words 或译文行），返回到最近框的
 * 最短距离；items 为空返回 Infinity（无文字恒为空白）。
 */
export function nearestTextDistance(items, px, py) {
    let best = Infinity;
    for (let i = 0; i < items.length; i++) {
        const d = pointToRectDistance(px, py, items[i].rect);
        if (d < best) best = d;
    }
    return best;
}

/**
 * 命中测试：返回包含点 (px, py) 的 item 下标，未命中返回 -1。
 *
 * items 元素形如 `{rect: {x, y, w, h}}`，坐标域与 (px, py) 一致。
 */
export function hitTestItems(items, px, py) {
    for (let i = 0; i < items.length; i++) {
        const r = items[i].rect;
        if (px >= r.x && px <= r.x + r.w && py >= r.y && py <= r.y + r.h) return i;
    }
    return -1;
}

/**
 * 找接近点击点的最近行内 item——空白处点击时靠近哪个就选哪个。
 *
 * 先按框中心垂直距离找最近行（lineIndex），再在该行内取水平中心最近者。
 * 与截图阅读模式 nearestWordByLine 同语义；items 为空返回 -1。
 */
export function nearestItemByLine(items, px, py) {
    if (!items || items.length === 0) return -1;
    let bestLine = items[0].lineIndex;
    let bestDy = Infinity;
    for (const it of items) {
        const cy = it.rect.y + it.rect.h / 2;
        const dy = Math.abs(cy - py);
        if (dy < bestDy) {
            bestDy = dy;
            bestLine = it.lineIndex;
        }
    }
    let bestIdx = -1;
    let bestDx = Infinity;
    for (let i = 0; i < items.length; i++) {
        const it = items[i];
        if (it.lineIndex !== bestLine) continue;
        const cx = it.rect.x + it.rect.w / 2;
        const dx = Math.abs(cx - px);
        if (dx < bestDx) {
            bestDx = dx;
            bestIdx = i;
        }
    }
    return bestIdx;
}

/**
 * 把 [lo, hi] 范围内的选中框按行合并为连续块矩形。
 *
 * 选区是连续索引段，同一行合并成一个包围块（文本编辑器选区观感），块内不会
 * 盖到未选内容；跨行选区每行各一个块。合并后只在行块轮廓描一次边，不再逐
 * char/word 描框——相邻小框描边叠加会形成密集蓝网格。
 *
 * @param {{rect:{x,y,w,h}, lineIndex:number}[]} items - charBoxes / words / 译文行
 * @param {number} lo - 起始索引（含）
 * @param {number} hi - 结束索引（含）
 * @returns {{x:number,y:number,w:number,h:number}[]} 合并后的行块列表
 */
export function mergeRunsByLine(items, lo, hi) {
    const byLine = new Map();
    for (let i = lo; i <= hi; i++) {
        const it = items[i];
        if (!it) continue;
        const r = it.rect;
        const cur = byLine.get(it.lineIndex);
        if (!cur) {
            byLine.set(it.lineIndex, {x: r.x, y: r.y, w: r.w, h: r.h});
        } else {
            const right = Math.max(cur.x + cur.w, r.x + r.w);
            const bottom = Math.max(cur.y + cur.h, r.y + r.h);
            cur.x = Math.min(cur.x, r.x);
            cur.y = Math.min(cur.y, r.y);
            cur.w = right - cur.x;
            cur.h = bottom - cur.y;
        }
    }
    return [...byLine.values()];
}

/**
 * 取 [lo, hi] 连续索引段的选择文本。
 *
 * items 带 start/end 且传入 fullText 时优先切原始全文，保留词间空格和换行；
 * 缺少范围信息时才退化为逐项拼接 text。
 * hi < lo 时返回空串。
 */
export function selectionTextOf(items, lo, hi, fullText = null) {
    if (lo > hi) return '';
    const start = items[lo]?.start;
    const end = items[hi]?.end;
    if (typeof fullText === 'string' && Number.isInteger(start) && Number.isInteger(end)
        && start >= 0 && end >= start && end <= fullText.length) {
        return fullText.slice(start, end);
    }
    let out = '';
    for (let i = lo; i <= hi; i++) {
        const it = items[i];
        if (it && typeof it.text === 'string') out += it.text;
    }
    return out;
}
