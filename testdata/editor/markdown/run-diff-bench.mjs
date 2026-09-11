#!/usr/bin/env node
/**
 * 0.23.0 Diff 阈值单项测量（复用 run-perf.mjs 的算法与生成器）。
 * 单独成文件的原因：run-perf.mjs 是脚本（main() 直接执行），不能作为 lib 复用。
 * 完整套件仍以 run-perf.mjs 为准；本文件只服务 Diff 阈值复测与回归。
 *
 * 用法：node --expose-gc testdata/editor/markdown/run-diff-bench.mjs
 */

import {tokenize, myersDiffCost, genProseCn, applyEdits, bench} from "./lib/bench-lib.mjs";

const ms = (v) => (v >= 100 ? v.toFixed(0) : v.toFixed(1));

const diffSizes = [
    {label: "4KB", bytes: 4 * 1024},
    {label: "16KB", bytes: 16 * 1024},
    {label: "64KB", bytes: 64 * 1024},
    {label: "200KB", bytes: 200 * 1024},
    {label: "500KB", bytes: 500 * 1024},
];
const rates = [
    {label: "1%", ratio: 0.01, seed: 11},
    {label: "5%", ratio: 0.05, seed: 23},
];

console.log("size   edits  tokensA  tokenize(p50/p95)  diff(p50/p95)    D       bail");
for (const size of diffSizes) {
    for (const rate of rates) {
        const base = genProseCn(size.bytes);
        const edited = applyEdits(base, rate.ratio, rate.seed);
        const tokA = tokenize(base);
        const tokB = tokenize(edited);
        const reps = size.bytes <= 64 * 1024 ? 15 : 5;
        const tokBench = bench(reps, () => tokenize(base));
        const diffBench = bench(reps, () => myersDiffCost(tokA, tokB));
        const probe = myersDiffCost(tokA, tokB);
        console.log(
            `${size.label.padEnd(6)} ${rate.label.padEnd(6)} ${String(tokA.length).padEnd(8)} ` +
            `${ms(tokBench.p50)}/${ms(tokBench.p95)}`.padEnd(19) +
            `${ms(diffBench.p50)}/${ms(diffBench.p95)}`.padEnd(17) +
            `${String(probe.edits).padEnd(7)} ${probe.bailed}`,
        );
    }
}

// 30% 重写：熔断行为演示
{
    const base = genProseCn(64 * 1024);
    const edited = applyEdits(base, 0.3, 77);
    const tokA = tokenize(base);
    const tokB = tokenize(edited);
    const t0 = performance.now();
    const probe = myersDiffCost(tokA, tokB);
    console.log(
        `${"64KB".padEnd(6)} ${"30%".padEnd(6)} ${String(tokA.length).padEnd(8)} ` +
        `${ms(bench(5, () => tokenize(base)).p50)}/${ms(bench(5, () => tokenize(base)).p95)}`.padEnd(19) +
        `${ms(performance.now() - t0)}/-`.padEnd(17) +
        `${String(probe.edits).padEnd(7)} ${probe.bailed}`,
    );
}
