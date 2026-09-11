/**
 * 0.23.0 性能 spike 共享库：文档生成器 / token 化 / Myers diff / 测量框架。
 * 纯函数集合，无副作用；run-perf.mjs 与 run-diff-bench.mjs 作为入口引用。
 */

// ── 合成文档生成 ──────────────────────────────────────────────────────────────

/** 中文段落文档：每段带序号避免重复行被 diff 剪枝过度美化。 */
export function genProseCn(targetBytes) {
    const sentences = [
        "这是一个用于性能测量的中文段落，内容本身没有语义。",
        "编辑器需要在中长文档上保持输入与切换的流畅，因此我们要测量解析与序列化的耗时分布。",
        "文本工作台的边界必须由数据决定，而不是由直觉决定，所以这份基准覆盖多档尺寸。",
        "连续听写会把确认片段追加到文末，长文档的追尾事务不能因为体积变大而退化。",
    ];
    const parts = [];
    let bytes = 0;
    let i = 0;
    while (bytes < targetBytes) {
        const para = `${i + 1}. ${sentences[i % sentences.length]}（第 ${i + 1} 段）`;
        parts.push(para);
        bytes += Buffer.byteLength(para, "utf8") + 1;
        i++;
    }
    return parts.join("\n\n");
}

/** 重标记文档：标题/列表/引用/代码块交替，节点数远高于同体积纯段落。 */
export function genMarkdownMixed(targetBytes) {
    const parts = [];
    let bytes = 0;
    let i = 0;
    while (bytes < targetBytes) {
        const block = [
            `## 第 ${i + 1} 节`,
            "",
            `- 项目 **${i + 1}-A**：包含\`行内代码 ${i}\`与[链接](https://example.com/${i})`,
            `- 项目 ${i + 1}-B：普通中文条目，附一点说明文字保证长度`,
            `  - 嵌套条目 ${i + 1}-B-1`,
            "",
            `> 引用块第 ${i + 1} 行：决定必须有数据支撑，测量必须有固定样本。`,
            "",
            "```txt",
            `fenced block #${i + 1}`,
            "line2 中文内容行",
            "```",
            "",
        ].join("\n");
        parts.push(block);
        bytes += Buffer.byteLength(block, "utf8");
        i++;
    }
    return parts.join("\n");
}

/** 代码密集文档：大围栏代码块 + 少量正文。 */
export function genCodeHeavy(targetBytes) {
    const parts = [];
    let bytes = 0;
    let i = 0;
    while (bytes < targetBytes) {
        const lines = [];
        for (let j = 0; j < 40; j++) {
            lines.push(`    const value_${i}_${j} = compute(${j}, "${i}-${j}"); // comment`);
        }
        const block = `正文第 ${i + 1} 段：\n\n\`\`\`js\n${lines.join("\n")}\n\`\`\`\n`;
        parts.push(block);
        bytes += Buffer.byteLength(block, "utf8");
        i++;
    }
    return parts.join("\n");
}

// ── Diff spike 实现（0.23.4 参考实现种子）────────────────────────────────────

/** token 化：CJK 单字成 token，ASCII 字母/数字连续串成词，其余单字符成 token。 */
export function tokenize(text) {
    const tokens = [];
    let buf = "";
    const isAsciiWord = (ch) => /[A-Za-z0-9]/.test(ch);
    let asciiRun = "";
    for (const ch of text) {
        const code = ch.codePointAt(0);
        const isCjk = (code >= 0x4e00 && code <= 0x9fff) || (code >= 0x3000 && code <= 0x303f) || (code >= 0xff00 && code <= 0xffef);
        if (isAsciiWord(ch)) {
            if (buf) { tokens.push(buf); buf = ""; }
            asciiRun += ch;
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
 * token 级 Myers O(ND) diff（公共前后缀预剪 + D 上限熔断）。
 * 返回 {edits, d, bailed}。edits=-1 表示熔断。只计成本不回溯构建完整脚本——
 * 0.23.0 只关心成本分布；回溯版（trace 快照）留给 0.23.4 正式实现。
 */
export function myersDiffCost(a, b, maxD = 4000) {
    let n = a.length;
    let m = b.length;
    let start = 0;
    while (start < n && start < m && a[start] === b[start]) start++;
    let endA = n;
    let endB = m;
    while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) { endA--; endB--; }
    n = endA - start;
    m = endB - start;
    if (n + m === 0) return {edits: 0, d: 0, bailed: false};
    const max = n + m;
    // 真下界熔断：D 至少为 |n-m|；其余交给循环内 d > maxD 兜底
    if (maxD > 0 && Math.abs(n - m) > maxD) {
        return {edits: -1, d: maxD, bailed: true};
    }
    const offset = max;
    let v = new Int32Array(2 * max + 1);
    const trace = [];
    for (let d = 0; d <= max; d++) {
        if (d > maxD) return {edits: -1, d, bailed: true};
        // 仅快照当前 k 范围（-d..d），避免 O(D·(N+M)) 内存
        const snap = new Int32Array(2 * d + 1);
        for (let k = -d; k <= d; k += 2) {
            let x;
            if (k === -d || (k !== d && v[offset + k - 1] < v[offset + k + 1])) {
                x = v[offset + k + 1];
            } else {
                x = v[offset + k - 1] + 1;
            }
            let y = x - k;
            while (x < n && y < m && a[start + x] === b[start + y]) { x++; y++; }
            v[offset + k] = x;
            snap[k + d] = x;
            if (x >= n && y >= m) {
                return {edits: d, d, bailed: false, traceSize: trace.length};
            }
        }
        trace.push(snap);
    }
    return {edits: -1, d: maxD, bailed: true};
}

// ── 测量框架 ──────────────────────────────────────────────────────────────────

export function percentile(sorted, p) {
    if (sorted.length === 0) return 0;
    const idx = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
    return sorted[idx];
}

export function bench(reps, fn) {
    const times = [];
    for (let i = 0; i < reps; i++) {
        const t0 = performance.now();
        fn();
        times.push(performance.now() - t0);
    }
    times.sort((a, b) => a - b);
    return {p50: percentile(times, 50), p95: percentile(times, 95), min: times[0], max: times[times.length - 1], reps};
}

export function gcAndHeap() {
    if (typeof globalThis.gc === "function") globalThis.gc();
    return process.memoryUsage().heapUsed;
}

/** 确定性伪随机（避免每次跑结果漂移）。 */
export function mulberry32(seed) {
    let a = seed >>> 0;
    return function () {
        a |= 0;
        a = (a + 0x6d2b79f5) | 0;
        let t = Math.imul(a ^ (a >>> 15), 1 | a);
        t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
}

/** 在文本上制造确定性的编辑率约 ratio 的改动（替换/删除/插入混合）。 */
export function applyEdits(text, ratio, seed) {
    const rnd = mulberry32(seed);
    const chars = [...text];
    const total = chars.length;
    const budget = Math.floor(total * ratio);
    let done = 0;
    while (done < budget) {
        const pos = Math.floor(rnd() * chars.length);
        const span = Math.min(chars.length - pos, Math.max(4, Math.floor(rnd() * 20)));
        const kind = rnd();
        if (kind < 0.4) {
            const repl = [..."编辑替换样本 xyz 123"];
            for (let i = 0; i < span; i++) chars[pos + i] = repl[(pos + i) % repl.length];
        } else if (kind < 0.7) {
            chars.splice(pos, span);
        } else {
            const ins = [];
            for (let i = 0; i < span; i++) ins.push(String.fromCharCode(0x4e00 + Math.floor(rnd() * 2000)));
            chars.splice(pos, 0, ...ins);
        }
        done += span;
    }
    return chars.join("");
}
