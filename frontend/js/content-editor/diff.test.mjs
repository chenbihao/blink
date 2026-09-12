/**
 * 稳定 Diff 纯函数测试（0.23.4，phase 文档 §3.7 / §3.10）。
 *
 * 覆盖：
 * - tokenize：CJK 单字 / ASCII 词串 / 空白 / 符号聚合；
 * - diffTokens：相同/替换/插入/删除/空串；重建性质（eq+del 还原 a、eq+add 还原 b）；
 *   相邻同 kind 合并；D 上限熔断返回 null；
 * - computeDiff：200KB 输入门（§3.10 冻结值）；
 * - diffToHtml：删除/新增的非颜色标识（−/＋）与 HTML 转义。
 */

globalThis.window = globalThis;

const {test} = await import("node:test");
const assert = (await import("node:assert/strict")).default;
const {
    tokenize,
    diffTokens,
    computeDiff,
    diffToHtml,
    hasChanges,
    MAX_DIFF_BYTES,
} = await import("./diff.js");

const escapeHtml = (s) => s.replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
}[c]));

/** 重建性质：脚本必须能无损还原两侧原文 */
function assertReconstructable(a, b, ops) {
    let ra = "";
    let rb = "";
    for (const op of ops) {
        if (op.kind === "eq" || op.kind === "del") ra += op.text;
        if (op.kind === "eq" || op.kind === "add") rb += op.text;
    }
    assert.equal(ra, a, "eq+del 段必须还原原文");
    assert.equal(rb, b, "eq+add 段必须还原终稿");
}

test("diff: tokenize CJK 单字、ASCII 词串与空白", () => {
    assert.deepEqual(tokenize("你好ab12，世界"), ["你", "好", "ab12", "，", "世", "界"]);
    assert.deepEqual(tokenize("a b"), ["a", " ", "b"]);
    assert.deepEqual(tokenize(""), []);
});

test("diff: 相同文本只产 eq 段", () => {
    const ops = diffTokens(["你", "好"], ["你", "好"]);
    assert.deepEqual(ops, [{kind: "eq", text: "你好"}]);
});

test("diff: 替换产 del+add，重建无损且相邻同 kind 合并", () => {
    const a = tokenize("今天天气很好");
    const b = tokenize("今天气很好"); // 删一个"天"
    const ops = diffTokens(a, b);
    assertReconstructable(a.join(""), b.join(""), ops);
    assert.ok(ops.some((o) => o.kind === "del"));
    assert.equal(ops.filter((o) => o.kind === "eq").length, 2, "前后 eq 各一段");
});

test("diff: 纯插入 / 纯删除 / 全改写", () => {
    const ins = diffTokens(["a"], ["a", "b"]);
    assert.deepEqual(ins.map((o) => o.kind), ["eq", "add"]);

    const del = diffTokens(["a", "b"], ["a"]);
    assert.deepEqual(del.map((o) => o.kind), ["eq", "del"]);

    const rewrite = diffTokens(["你", "好"], ["再", "见"]);
    assertReconstructable("你好", "再见", rewrite);
});

test("diff: 空文本边界", () => {
    assert.deepEqual(diffTokens([], []), []);
    const add = diffTokens([], ["你"]);
    assert.deepEqual(add, [{kind: "add", text: "你"}]);
    const del = diffTokens(["你"], []);
    assert.deepEqual(del, [{kind: "del", text: "你"}]);
});

test("diff: D 超上限熔断返回 null（§3.10 D>4000）", () => {
    // 3000 个互异 token 对：编辑距离超过 maxD=100
    const a = Array.from({length: 3000}, (_, i) => `a${i}`);
    const b = Array.from({length: 3000}, (_, i) => `b${i}`);
    assert.equal(diffTokens(a, b, 100), null);
    // 放宽上限后可解
    const ok = diffTokens(a.slice(0, 10), b.slice(0, 10), 4000);
    assert.ok(Array.isArray(ok));
});

test("diff: computeDiff 超过 200KB 输入门返回 null", () => {
    const small = computeDiff("第一版文本。", "第一版文本，已整理。");
    assert.ok(Array.isArray(small));
    assert.equal(hasChanges(small), true);

    const big = "a".repeat(MAX_DIFF_BYTES + 1);
    assert.equal(computeDiff(big, big), null);
    assert.equal(computeDiff("小", big), null, "任一侧超门都熔断");

    const sameBig = "a".repeat(MAX_DIFF_BYTES);
    assert.ok(Array.isArray(computeDiff(sameBig, sameBig)), "恰达门槛放行");
});

test("diff: computeDiff 无可见改动时 hasChanges=false", () => {
    const ops = computeDiff("原样文本", "原样文本");
    assert.equal(hasChanges(ops), false);
});

test("diff: 模糊测试——随机编辑对重建无损且编辑数 ≤ D 界", () => {
    // 确定性伪随机（mulberry32，同 bench-lib 惯例）
    let seed = 0x2f6e2b1;
    const rnd = () => {
        seed |= 0;
        seed = (seed + 0x6d2b79f5) | 0;
        let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
        t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
    const chars = ["今", "天", "气", "很", "好", "a", "b", "，", " "];
    const randomText = (len) => {
        let s = "";
        for (let i = 0; i < len; i++) s += chars[Math.floor(rnd() * chars.length)];
        return s;
    };
    const mutate = (text) => {
        const arr = [...text];
        const edits = 1 + Math.floor(rnd() * 8);
        for (let i = 0; i < edits && arr.length > 0; i++) {
            const pos = Math.floor(rnd() * arr.length);
            const kind = rnd();
            if (kind < 0.35) arr[pos] = chars[Math.floor(rnd() * chars.length)];
            else if (kind < 0.7) arr.splice(pos, 1);
            else arr.splice(pos, 0, chars[Math.floor(rnd() * chars.length)]);
        }
        return arr.join("");
    };
    for (let i = 0; i < 200; i++) {
        const a = randomText(1 + Math.floor(rnd() * 40));
        const b = mutate(a);
        const ops = computeDiff(a, b);
        assert.ok(Array.isArray(ops), `case ${i} 应可 diff：a=${a} b=${b}`);
        assertReconstructable(a, b, ops);
        // 无相邻同 kind 段（push 合并契约）
        for (let j = 1; j < ops.length; j++) {
            assert.notEqual(ops[j].kind, ops[j - 1].kind, `case ${i} 相邻段未合并`);
        }
    }
});

test("diff: diffToHtml 带非颜色标识并转义 HTML", () => {
    const ops = [
        {kind: "eq", text: "前"},
        {kind: "del", text: "<b>"},
        {kind: "add", text: "&后"},
    ];
    const html = diffToHtml(ops, escapeHtml);
    assert.ok(html.includes(`前`));
    assert.ok(html.includes(`class="diff-del"`));
    assert.ok(html.includes(`class="diff-add"`));
    assert.ok(html.includes("\u2212"), "删除段带 − 标识");
    assert.ok(html.includes("\uFF0B"), "新增段带 ＋ 标识");
    assert.ok(html.includes("&lt;b&gt;"), "HTML 已转义");
    assert.ok(html.includes("&amp;后"));
    assert.equal(diffToHtml(null, escapeHtml), "");
});
