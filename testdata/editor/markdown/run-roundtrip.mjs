#!/usr/bin/env node
/**
 * 0.23.0 Markdown 语料 round-trip 回归 runner。
 *
 * 用法：
 *   node testdata/editor/markdown/run-roundtrip.mjs            # 人类可读报告
 *   node testdata/editor/markdown/run-roundtrip.mjs --json     # 机器可读结果
 *   node testdata/editor/markdown/run-roundtrip.mjs --diff x   # 展开指定样本的行级 diff
 *
 * 期望分类记录在 corpus/manifest.json（产品决策真值）；
 * 本 runner 验证"实际行为与期望一致"，任何不一致都以非零退出。
 * 生成 roundtrip-report.md 时用 --json 的输出作为数据源。
 */

import {join} from "node:path";
import {createManager, classify, judge, firstDiffHint, readCorpusFile, REPO_ROOT, TIPTAP_VERSION} from "./lib/engine.mjs";

const BASE = join(REPO_ROOT, "testdata", "editor", "markdown", "corpus");
const args = process.argv.slice(2);
const wantJson = args.includes("--json");
const diffIdx = args.indexOf("--diff");

const manifest = JSON.parse(readCorpusFile(join(BASE, "manifest.json")));

function readSample(file) {
    // manifest.json 自身不参与 round-trip
    return readCorpusFile(join(BASE, file));
}

async function main() {
    const manager = await createManager();
    const rows = [];

    for (const sample of manifest.samples) {
        const src = readSample(sample.file);
        let result;
        try {
            const {actual, rt1} = classify(manager, src);
            result = {
                file: sample.file,
                expect: sample.expect,
                actual,
                pass: judge(sample.expect, actual),
                note: sample.note,
                hint: actual === "identical" ? "" : firstDiffHint(src, rt1),
                rt1,
            };
        } catch (e) {
            result = {
                file: sample.file,
                expect: sample.expect,
                actual: "error",
                pass: false,
                note: sample.note,
                hint: String(e?.message ?? e).slice(0, 200),
                rt1: "",
            };
        }
        rows.push(result);
    }

    const failed = rows.filter((r) => !r.pass);

    if (diffIdx !== -1) {
        const target = rows.find((r) => r.file.includes(args[diffIdx + 1]));
        if (!target) {
            console.error(`未找到样本：${args[diffIdx + 1]}`);
            process.exit(2);
        }
        const {lineDiff} = await import("./lib/engine.mjs");
        const src = readSample(target.file);
        for (const part of lineDiff(src, target.rt1)) {
            const prefix = part.kind === "add" ? "+ " : part.kind === "del" ? "- " : "  ";
            console.log(prefix + part.text);
        }
        return;
    }

    if (wantJson) {
        console.log(JSON.stringify({
            tiptap_version: TIPTAP_VERSION,
            manifest_version: manifest.tiptap_version,
            total: rows.length,
            failed: failed.length,
            rows: rows.map(({rt1, ...rest}) => ({...rest, rt1_bytes: rt1.length})),
        }, null, 2));
    } else {
        const pad = (s, n) => (s.length >= n ? s : s + " ".repeat(n - s.length));
        console.log(`Tiptap ${TIPTAP_VERSION}（manifest 锁定 ${manifest.tiptap_version}）\n`);
        console.log(`${pad("样本", 34)} ${pad("期望", 11)} ${pad("实际", 11)} 结果  说明`);
        console.log("-".repeat(100));
        for (const r of rows) {
            const mark = r.pass ? "✓" : "✗";
            console.log(`${pad(r.file, 34)} ${pad(r.expect, 11)} ${pad(r.actual, 11)} ${mark}     ${r.note}`);
            if (!r.pass && r.hint) console.log(`${" ".repeat(34)} ↳ ${r.hint}`);
            if (r.actual === "error") console.log(`${" ".repeat(34)} ↳ ${r.hint}`);
        }
        console.log("-".repeat(100));
        console.log(`共 ${rows.length} 个样本，与期望不符 ${failed.length} 个`);
    }

    if (failed.length > 0) process.exit(1);
}

main().catch((e) => {
    console.error("[run-roundtrip] 失败:", e?.message ?? e);
    process.exit(2);
});
