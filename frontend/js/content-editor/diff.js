/**
 * 编辑器整理候选的稳定 Diff（0.23.4，phase 文档 §3.7 / §3.10）。
 *
 * **纯函数集合**：终稿完整到达后一次性生成 Diff（不显示跳动增量）。
 * 算法与 0.23.0 perf spike（testdata/editor/markdown/lib/bench-lib.mjs）同源：
 * - tokenize：CJK 单字成 token、ASCII 字母数字连续串成词（中文按字符/词组级比较）；
 * - Myers O(ND)：公共前后缀预剪 + D 上限熔断（冻结值 4000），本版为带
 *   回溯的正式实现（spike 版只计成本，本版构建编辑脚本）；
 * - 输入门（§3.10 冻结）：输入任一侧超过 200KB 不生成内联 Diff（退化为
 *   仅复制展示）。
 */

/** Diff 输入门（§3.10 冻结值 200KB，按 UTF-8 字节数）。 */
export const MAX_DIFF_BYTES = 200 * 1024;

/** Myers D 上限（§3.10 冻结值）：超过熔断，返回 null（仅复制展示）。 */
export const MAX_DIFF_D = 4000;

/**
 * token 化：CJK 单字成 token（§3.7 中文按字符/词组级比较），ASCII 字母/数字
 * 连续串成词，空白单字符成 token，其余连续符号聚合。
 * （与 bench-lib.mjs 的 spike 版不同点：spike 版 CJK 串整段成单 token，只服务
 * 成本估算；产品 Diff 必须按字粒度才能产出可读的差异段。）
 * @param {string} text
 * @returns {string[]}
 */
export function tokenize(text) {
    const tokens = [];
    let buf = "";
    const isAsciiWord = (ch) => /[A-Za-z0-9]/.test(ch);
    let asciiRun = "";
    for (const ch of text) {
        const code = ch.codePointAt(0);
        const isCjk = (code >= 0x4e00 && code <= 0x9fff)
            || (code >= 0x3000 && code <= 0x303f)
            || (code >= 0xff00 && code <= 0xffef);
        if (isAsciiWord(ch)) {
            if (buf) { tokens.push(buf); buf = ""; }
            asciiRun += ch;
        } else if (isCjk) {
            if (buf) { tokens.push(buf); buf = ""; }
            if (asciiRun) { tokens.push(asciiRun); asciiRun = ""; }
            tokens.push(ch);
        } else {
            if (asciiRun) { tokens.push(asciiRun); asciiRun = ""; }
            if (ch === " " || ch === "\n" || ch === "\t") {
                if (buf) { tokens.push(buf); buf = ""; }
                tokens.push(ch);
            } else {
                buf += ch;
            }
        }
    }
    if (asciiRun) tokens.push(asciiRun);
    if (buf) tokens.push(buf);
    return tokens;
}

/**
 * 稳定 Diff 入口（§3.10 输入门 + Myers）。
 * @param {string} sourceText - 原文（冻结范围文本）
 * @param {string} revisedText - AI 终稿
 * @returns {Array<{kind: "eq"|"del"|"add", text: string}>|null}
 *   熔断或超门返回 null（调用方退化为仅复制展示）
 */
export function computeDiff(sourceText, revisedText) {
    const encoder = new TextEncoder();
    if (encoder.encode(sourceText).length > MAX_DIFF_BYTES
        || encoder.encode(revisedText).length > MAX_DIFF_BYTES) {
        return null;
    }
    return diffTokens(tokenize(sourceText), tokenize(revisedText), MAX_DIFF_D);
}

/**
 * token 级 Myers O(ND) diff（回溯构建编辑脚本）。
 * @param {string[]} a
 * @param {string[]} b
 * @param {number} maxD - D 上限，超过返回 null（熔断）
 * @returns {Array<{kind: "eq"|"del"|"add", text: string}>|null}
 */
export function diffTokens(a, b, maxD = MAX_DIFF_D) {
    const n = a.length;
    const m = b.length;

    // 公共前后缀预剪：原样段直接输出，缩小 Myers 工作区
    let start = 0;
    while (start < n && start < m && a[start] === b[start]) start++;
    let endA = n;
    let endB = m;
    while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) { endA--; endB--; }

    const ops = [];
    const push = (kind, text) => {
        if (!text) return;
        const last = ops[ops.length - 1];
        if (last && last.kind === kind) last.text += text;
        else ops.push({kind, text});
    };
    for (let i = 0; i < start; i++) push("eq", a[i]);
    const midOps = myersScript(a, b, start, endA, endB, maxD);
    if (midOps === null) return null;
    for (const op of midOps) {
        push(op.kind, op.kind === "eq" ? a[op.aIndex] : op.kind === "del" ? a[op.aIndex] : b[op.bIndex]);
    }
    for (let i = endA; i < n; i++) push("eq", a[i]);
    return ops;
}

/**
 * Myers 中段搜索 + 回溯。返回中段编辑脚本（token 索引）或 null（熔断）。
 * 每层 d 只快照当前 k 范围（-d..d），内存 O(D²)。
 */
function myersScript(a, b, start, endA, endB, maxD) {
    const n = endA - start;
    const m = endB - start;
    if (n + m === 0) return [];
    if (Math.abs(n - m) > maxD) return null;

    const max = n + m;
    const offset = max;
    const v = new Int32Array(2 * max + 1);
    const trace = [];
    let foundD = -1;
    for (let d = 0; d <= max; d++) {
        if (d > maxD) return null;
        const snap = new Int32Array(2 * d + 1);
        for (let k = -d; k <= d; k += 2) {
            let x;
            if (k === -d || (k !== d && v[offset + k - 1] < v[offset + k + 1])) {
                x = v[offset + k + 1];      // 向下 = 新增（b 前进）
            } else {
                x = v[offset + k - 1] + 1;  // 向右 = 删除（a 前进）
            }
            let y = x - k;
            while (x < n && y < m && a[start + x] === b[start + y]) { x++; y++; }
            v[offset + k] = x;
            snap[k + d] = x;
            if (x >= n && y >= m) {
                foundD = d;
                break;
            }
        }
        trace.push(snap);
        if (foundD >= 0) break;
    }
    if (foundD < 0) return null;

    // 回溯：从 (n, m) 沿 trace 逐层恢复蛇线与前驱。
    // 索引统一换算回数组绝对下标（中段起点 start）。
    const steps = [];
    let x = n;
    let y = m;
    let d = foundD;
    while (d > 0) {
        const vPrev = trace[d - 1];
        const k = x - y;
        let prevK;
        if (k === -d || (k !== d && vPrev[k - 1 + (d - 1)] < vPrev[k + 1 + (d - 1)])) {
            prevK = k + 1; // 前驱来自下方：本层走了一次"新增"
        } else {
            prevK = k - 1; // 前驱来自右方：本层走了一次"删除"
        }
        const prevX = vPrev[prevK + (d - 1)];
        const prevY = prevX - prevK;
        // 蛇线（对角 eq 段）逆向入栈
        while (x > prevX && y > prevY) {
            steps.push({kind: "eq", aIndex: start + x - 1});
            x--;
            y--;
        }
        // 连接前驱的一次编辑
        if (x === prevX) {
            steps.push({kind: "add", bIndex: start + y - 1});
            y--;
        } else {
            steps.push({kind: "del", aIndex: start + x - 1});
            x--;
        }
        d--;
    }
    // d=0 层的初始蛇线
    while (x > 0 && y > 0) {
        steps.push({kind: "eq", aIndex: start + x - 1});
        x--;
        y--;
    }
    // 防御：理论不可达（d=0 无编辑，x/y 必须同时耗尽）
    while (x > 0) { steps.push({kind: "del", aIndex: start + x - 1}); x--; }
    while (y > 0) { steps.push({kind: "add", bIndex: start + y - 1}); y--; }
    steps.reverse();
    return steps;
}

/**
 * 编辑脚本 → 渲染段（安全 HTML 片段）。
 * 删除为 `.diff-del`（浅红底 + 删除线 + 前缀 −），新增为 `.diff-add`
 *（浅绿底 + 前缀 ＋）——颜色之外有符号标识（§3.7 非颜色双通道）。
 * @param {Array<{kind: string, text: string}>|null} segments - computeDiff 结果
 * @param {(s: string) => string} escapeHtml
 * @returns {string} HTML；segments 为 null 返回空串（调用方走仅复制展示）
 */
export function diffToHtml(segments, escapeHtml) {
    if (!segments) return "";
    let html = "";
    for (const seg of segments) {
        const escaped = escapeHtml(seg.text);
        if (seg.kind === "del") html += `<span class="diff-del">\u2212${escaped}</span>`;
        else if (seg.kind === "add") html += `<span class="diff-add">\uFF0B${escaped}</span>`;
        else html += escaped;
    }
    return html;
}

/** 是否存在可见改动（决定候选卡展示内联 Diff 还是纯文本）。 */
export function hasChanges(segments) {
    return !!segments && segments.some((s) => s.kind !== "eq");
}
