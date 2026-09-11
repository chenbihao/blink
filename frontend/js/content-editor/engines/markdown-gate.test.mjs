/**
 * Markdown 风险门测试（0.23.1）。
 *
 * 两个层次：
 * 1. **语料回归**——遍历 testdata/editor/markdown/corpus（0.23.0 冻结语料，
 *    manifest.json 标注期望），断言 gate 拒绝集合与 `reject` 期望完全一致、
 *    `support`/`normalized` 全部放行。这是启发式扫描与 Tiptap 行为对齐的
 *    回归防线（真值链：roundtrip-report.md → manifest → gate）。
 * 2. **单元用例**——尺寸门分档、围栏代码块保护、货币/autolink 不误伤。
 */

import {test} from "node:test";
import assert from "node:assert/strict";
import {readFileSync, existsSync} from "node:fs";
import {fileURLToPath} from "node:url";
import {evaluateMarkdownGate} from "./markdown-gate.js";

const here = new URL(".", import.meta.url);
const corpusRoot = new URL("../../../../testdata/editor/markdown/corpus/", here);

test("gate: 冻结语料逐样本回归（corpus 存在时）", () => {
    const manifestPath = new URL("manifest.json", corpusRoot);
    if (!existsSync(manifestPath)) {
        console.warn("[markdown-gate.test] corpus 不存在，跳过语料回归");
        return;
    }
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    assert.ok(manifest.samples.length >= 30, "语料应不少于 30 样本");

    for (const sample of manifest.samples) {
        const text = readFileSync(new URL(sample.file, corpusRoot), "utf8");
        const result = evaluateMarkdownGate(text);
        if (sample.expect === "reject") {
            assert.equal(
                result.allowed,
                false,
                `reject 样本被放行: ${sample.file}（${sample.note}）`,
            );
        } else {
            assert.equal(
                result.allowed,
                true,
                `${sample.expect} 样本被拒绝: ${sample.file} reason=${result.reason}（${sample.note}）`,
            );
        }
    }
});

test("gate: 尺寸门三档", () => {
    const encoder = {
        encode: (s) => ({length: s.length}), // 1 char ≈ 1 byte 的测试替身
    };

    const small = evaluateMarkdownGate("hello", {encoder});
    assert.equal(small.allowed, true);
    assert.equal(small.autoEnter, true);

    const slow = evaluateMarkdownGate("x".repeat(64 * 1024), {encoder});
    assert.equal(slow.allowed, true);
    assert.equal(slow.autoEnter, false);
    assert.equal(slow.sizeWarn, true);

    const large = evaluateMarkdownGate("x".repeat(129 * 1024), {encoder});
    assert.equal(large.allowed, false);
    assert.equal(large.reason, "large");
});

test("gate: 结构拒绝逐类命中", () => {
    const rejects = {
        table: "| a | b |\n|---|---|\n| 1 | 2 |",
        footnote: "句子[^1]。\n\n[^1]: 注释",
        mathBlock: "$$\n\\int x dx\n$$",
        mathInline: "质能方程 $E = mc^2$ 是物理",
        htmlBlock: "<div class=\"warning\">\n文本\n</div>",
        htmlInline: "含 <b>粗体</b> 标签",
        htmlComment: "前文\n<!-- 注释 -->\n后文",
        backtickCode: "行内代码 ``a ` b`` 场景",
    };
    for (const [name, text] of Object.entries(rejects)) {
        const result = evaluateMarkdownGate(text);
        assert.equal(result.allowed, false, `${name} 应被拒绝`);
        assert.equal(result.reason, "structure", `${name} 拒绝原因应为 structure`);
    }
});

test("gate: 易混淆的合法内容不误伤", () => {
    const allows = [
        ("普通中文段落，含价格 $5 和 $3 的表述。\n"),
        ("autolink：<https://example.com> 规范化不丢失\n"),
        ("Setext 标题\n------\n正文（规范化不阻断）\n"),
        ("水平分隔线\n\n---\n\n后续段落\n"),
        ("引用 > blockquote 与 **粗体** [链接](https://a.b)\n"),
        ("- 列表项含连字符 - 与冒号 :\n"),
    ];
    for (const text of allows) {
        const result = evaluateMarkdownGate(text);
        assert.equal(result.allowed, true, `不应拒绝: ${JSON.stringify(text)}`);
    }
});

test("gate: 围栏代码块内的内容不参与结构判定", () => {
    const text = [
        "正文段落。",
        "",
        "```html",
        "<div>代码示例中的 HTML 不应阻断</div>",
        "| 表格 | 示例 |",
        "|------|------|",
        "$$math$$",
        "```",
        "",
        "结尾段落。",
    ].join("\n");
    const result = evaluateMarkdownGate(text);
    assert.equal(result.allowed, true, "围栏内内容不应触发拒绝");
});
