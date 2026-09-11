#!/usr/bin/env node
/**
 * 0.23.0 编辑器性能 spike —— MD round-trip envelope 与 Diff 阈值测量（完整套件）。
 *
 * 用法：
 *   node --expose-gc testdata/editor/markdown/run-perf.mjs          # 人类可读
 *   node --expose-gc testdata/editor/markdown/run-perf.mjs --json   # 机器可读
 *   （只要 Diff 阈值时可用 run-diff-bench.mjs，秒级出结果）
 *
 * 测什么：
 * 1) MD round-trip：三类合成文档 × 5 档尺寸，parse / serialize / 全程 的 p50/p95；
 *    由此定"可编辑 MD 文档上限"（envelope 的一部分）。
 * 2) Diff：0.23.4 拟用的 token 级 Myers（CJK 单字成 token、ASCII 按词、公共前后缀
 *    预剪、编辑距离 D 上限熔断），在 1%/5% 编辑率与 30% 重写下测 p50/p95；
 *    由此定"Diff 安全输入上限"与熔断降级策略。
 * 3) 内存：--expose-gc 下每档 gc 后 heapUsed 与峰值。
 *
 * 局限（诚实声明，写入报告）：本脚本在 Node 上测量纯 JS 计算成本；
 * WebView2 的 textarea/ProseMirror 渲染、layout 与输入延迟属于浏览器侧成本，
 * 由 0.23.5 在真实窗口回归复测。Source 硬上限（2,000,000 字符）依据内存预算
 * 冻结，其打字/滚动体验留待 0.23.5 验证。
 *
 * 注意：2MB markdown-mixed 单格 parse 即 ~2 分钟，完整套件约 15 分钟。
 */

import {createManager, TIPTAP_VERSION} from "./lib/engine.mjs";
import {
    genProseCn, genMarkdownMixed, genCodeHeavy,
    tokenize, myersDiffCost, applyEdits,
    bench, gcAndHeap,
} from "./lib/bench-lib.mjs";

const args = process.argv.slice(2);
const wantJson = args.includes("--json");
const HAS_GC = typeof globalThis.gc === "function";

async function main() {
    const manager = await createManager();
    const heapStart = gcAndHeap();

    // ── 1) MD round-trip envelope ──
    const scales = [
        {label: "8KB", bytes: 8 * 1024},
        {label: "32KB", bytes: 32 * 1024},
        {label: "128KB", bytes: 128 * 1024},
        {label: "512KB", bytes: 512 * 1024},
        {label: "2MB", bytes: 2 * 1024 * 1024},
    ];
    const gens = [
        {name: "prose-cn", fn: genProseCn},
        {name: "markdown-mixed", fn: genMarkdownMixed},
        {name: "code-heavy", fn: genCodeHeavy},
    ];
    const roundtripResults = [];
    for (const gen of gens) {
        for (const scale of scales) {
            const src = gen.fn(scale.bytes);
            const realBytes = Buffer.byteLength(src, "utf8");
            const reps = scale.bytes <= 128 * 1024 ? 20 : scale.bytes <= 512 * 1024 ? 8 : 3;
            const jsonCache = manager.parse(src);
            gcAndHeap();
            const parseB = bench(reps, () => manager.parse(src));
            const serB = bench(reps, () => manager.serialize(jsonCache));
            gcAndHeap();
            const fullB = bench(reps, () => manager.serialize(manager.parse(src)));
            const heapAfter = gcAndHeap();
            roundtripResults.push({
                doc: gen.name,
                scale: scale.label,
                bytes: realBytes,
                chars: src.length,
                parse: parseB,
                serialize: serB,
                full: fullB,
                heap_after_mb: +(heapAfter / 1024 / 1024).toFixed(1),
            });
        }
    }

    // ── 2) Diff 阈值 ──
    const diffSizes = [
        {label: "4KB", bytes: 4 * 1024},
        {label: "16KB", bytes: 16 * 1024},
        {label: "64KB", bytes: 64 * 1024},
        {label: "200KB", bytes: 200 * 1024},
        {label: "500KB", bytes: 500 * 1024},
    ];
    const diffRates = [
        {label: "1%", ratio: 0.01, seed: 11},
        {label: "5%", ratio: 0.05, seed: 23},
    ];
    const diffResults = [];
    for (const size of diffSizes) {
        for (const rate of diffRates) {
            const base = genProseCn(size.bytes);
            const edited = applyEdits(base, rate.ratio, rate.seed);
            const tokA = tokenize(base);
            const tokB = tokenize(edited);
            gcAndHeap();
            const reps = size.bytes <= 64 * 1024 ? 15 : 5;
            const tokBench = bench(reps, () => tokenize(base));
            const diffBench = bench(reps, () => myersDiffCost(tokA, tokB));
            const probe = myersDiffCost(tokA, tokB);
            diffResults.push({
                size: size.label,
                edits: rate.label,
                tokens_a: tokA.length,
                tokenize: tokBench,
                diff: diffBench,
                edit_distance: probe.edits,
                bailed: probe.bailed,
            });
        }
    }
    // 30% 重写（找熔断行为）
    {
        const base = genProseCn(64 * 1024);
        const edited = applyEdits(base, 0.3, 77);
        const tokA = tokenize(base);
        const tokB = tokenize(edited);
        const t0 = performance.now();
        const probe = myersDiffCost(tokA, tokB);
        diffResults.push({
            size: "64KB",
            edits: "30%",
            tokens_a: tokA.length,
            tokenize: bench(5, () => tokenize(base)),
            diff: {p50: performance.now() - t0, p95: performance.now() - t0, min: 0, max: 0, reps: 1},
            edit_distance: probe.edits,
            bailed: probe.bailed,
        });
    }

    // ── 3) Source 文本操作抽查（envelope 佐证）──
    const big = genProseCn(2 * 1024 * 1024);
    const srcOps = bench(10, () => {
        const norm = big.replace(/\r\n/g, "\n");
        return norm.length;
    });

    const heapEnd = gcAndHeap();
    const summary = {
        tiptap_version: TIPTAP_VERSION,
        node: process.version,
        expose_gc: HAS_GC,
        machine_hint: `${process.arch} / ${process.platform}`,
        heap_start_mb: +(heapStart / 1024 / 1024).toFixed(1),
        heap_end_mb: +(heapEnd / 1024 / 1024).toFixed(1),
        roundtrip: roundtripResults,
        diff: diffResults,
        source_ops_2mb: srcOps,
    };

    if (wantJson) {
        console.log(JSON.stringify(summary, null, 2));
        return;
    }

    const ms = (v) => (v >= 100 ? v.toFixed(0) : v.toFixed(1));
    console.log(`Tiptap ${TIPTAP_VERSION} / Node ${process.version} / gc=${HAS_GC}\n`);
    console.log("== MD round-trip（p50 / p95，ms）==");
    console.log("doc              scale   chars   parse          serialize      full           heapAfter");
    for (const r of roundtripResults) {
        console.log(
            `${r.doc.padEnd(16)} ${r.scale.padEnd(7)} ${String(r.chars).padEnd(7)} ` +
            `${ms(r.parse.p50)}/${ms(r.parse.p95)}`.padEnd(15) +
            `${ms(r.serialize.p50)}/${ms(r.serialize.p95)}`.padEnd(15) +
            `${ms(r.full.p50)}/${ms(r.full.p95)}`.padEnd(15) +
            `${r.heap_after_mb}MB`,
        );
    }
    console.log("\n== Diff（token 级 Myers，p50/p95，ms）==");
    console.log("size   edits  tokensA  tokenize(p50/p95)  diff(p50/p95)   D       bail");
    for (const r of diffResults) {
        console.log(
            `${r.size.padEnd(6)} ${r.edits.padEnd(6)} ${String(r.tokens_a).padEnd(8)} ` +
            `${ms(r.tokenize.p50)}/${ms(r.tokenize.p95)}`.padEnd(19) +
            `${ms(r.diff.p50)}/${ms(r.diff.p95)}`.padEnd(16) +
            `${String(r.edit_distance).padEnd(7)} ${r.bailed}`,
        );
    }
    console.log(`\n== Source 2MB EOL 归一化（p50=${ms(srcOps.p50)}ms, p95=${ms(srcOps.p95)}ms）==`);
}

main().catch((e) => {
    console.error("[run-perf] 失败:", e?.message ?? e);
    process.exit(2);
});
