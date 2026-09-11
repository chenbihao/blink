/**
 * 0.23.0 Markdown round-trip spike — 共享引擎封装。
 *
 * 目标：以与生产编辑器（0.18.3 起 content-editor / Tiptap IR）一致的
 * 解析/序列化路径，对语料做 headless round-trip 分类。
 *
 * 一致性依据（0.23.0 定案）：
 * - Manager 构造参数与 `@tiptap/markdown` 的 Markdown 扩展 onBeforeCreate 完全一致：
 *   indentation {style:'space', size:2}、markedOptions:{}、扩展面 = StarterKit + TaskList + TaskItem。
 * - 生产链路 `editor.getMarkdown()` = manager.serialize(editor.getJSON())；
 *   `setContent(md, {contentType:'markdown'})` = manager.parse(md)。
 * - 生产入口在载入时统一 `\r\n → \n`（content-editor/main.js loadPayload），
 *   harness 读语料同样归一化，避免把宿主换行差异误判为 round-trip 差异。
 *
 * 依赖来源：target/editor-markdown-build/node_modules（与 bundle-tiptap.js 相同的
 * 四包锁定版本，dev-time only，不入 vendor、不进运行时）。缺失时给出修复指令。
 */

import {existsSync, readFileSync} from "node:fs";
import {fileURLToPath, pathToFileURL} from "node:url";
import {join} from "node:path";

const HERE = fileURLToPath(new URL(".", import.meta.url));
// testdata/editor/markdown/lib/ → 仓库根
export const REPO_ROOT = join(HERE, "..", "..", "..", "..");
export const DEPS_DIR = join(REPO_ROOT, "target", "editor-markdown-build");

/** 锁定版本，与 xtask/scripts/bundle-tiptap.js 保持一致（升级需同步）。 */
export const TIPTAP_VERSION = "3.29.2";

export function ensureDeps() {
    const pkgDir = join(DEPS_DIR, "node_modules", "@tiptap");
    if (!existsSync(pkgDir)) {
        throw new Error(
            `缺少 spike 依赖：${DEPS_DIR}\n` +
            `请执行一次（dev-time only，产物在 target/ 下，不入库）：\n` +
            `  cd ${DEPS_DIR} && npm install --silent --no-audit --no-fund\n` +
            `（目录不存在时先 mkdir -p ${DEPS_DIR} 并放入与 bundle-tiptap.js 同版本的 package.json）`,
        );
    }
    return true;
}

/** Windows 下动态 import 绝对路径必须转 file:// URL。 */
function importFromDeps(...segments) {
    return import(pathToFileURL(join(DEPS_DIR, "node_modules", ...segments)).href);
}

/** 构造与生产编辑器一致的 MarkdownManager。 */
export async function createManager() {
    ensureDeps();
    const {MarkdownManager} = await importFromDeps("@tiptap", "markdown", "dist", "index.js");
    const {default: StarterKit} = await importFromDeps("@tiptap", "starter-kit", "dist", "index.js");
    const {default: TaskList} = await importFromDeps("@tiptap", "extension-task-list", "dist", "index.js");
    const {default: TaskItem} = await importFromDeps("@tiptap", "extension-task-item", "dist", "index.js");

    return new MarkdownManager({
        indentation: {style: "space", size: 2},
        markedOptions: {},
        extensions: [StarterKit, TaskList, TaskItem],
    });
}

/** 生产入口同款换行归一化。 */
export function normalizeEol(text) {
    return (text ?? "").replace(/\r\n/g, "\n");
}

/**
 * 单样本 round-trip：
 * - rt1 = serialize(parse(src))
 * - rt2 = serialize(parse(rt1))，用于不动点判定
 */
export function roundtrip(manager, src) {
    const json1 = manager.parse(src);
    const rt1 = manager.serialize(json1);
    const json2 = manager.parse(rt1);
    const rt2 = manager.serialize(json2);
    return {rt1, rt2, json1};
}

/**
 * EOF 换行容差比较。
 *
 * 0.23.0 定案：`MarkdownManager.serialize` 不在文档末尾输出换行；
 * 0.23.1 的 MD→文本保存路径在序列化后统一补一个末尾换行（EOF newline 约定）。
 * 因此 round-trip 的"逐字节一致"按补 EOF 换行后的比较执行；
 * "纯切换逐字符保真"与此无关——由会话级原文检查点保证（phase §3.3）。
 */
export function eqModEofNewline(a, b) {
    if (a === b) return true;
    const trimEdge = (s) => (s.endsWith("\n") ? s.slice(0, -1) : s);
    return trimEdge(a) === trimEdge(b) && a.length > 0 && b.length > 0;
}

/**
 * 实际行为分类：
 * - identical：rt1 与源逐字节一致（EOF 换行容差内，见 eqModEofNewline）
 * - normalized：字节有差，但二次 round-trip 已到不动点（结构收敛、无震荡）
 * - unstable：二次 round-trip 仍变化（解析不稳定，禁止进入可编辑 MD）
 */
export function classify(manager, src) {
    const {rt1, rt2} = roundtrip(manager, src);
    if (eqModEofNewline(rt1, src)) return {actual: "identical", rt1, rt2};
    if (eqModEofNewline(rt2, rt1)) return {actual: "normalized", rt1, rt2};
    return {actual: "unstable", rt1, rt2};
}

/**
 * 期望判定：
 * - expect=identical  → 通过 ⇔ actual=identical
 * - expect=normalized → 通过 ⇔ actual ∈ {identical, normalized}
 *   （标注为"会被规范重写"的样本若恰好逐字节一致，同样安全）
 * - expect=reject     → 通过 ⇔ actual ≠ identical
 *   （必须存在重写/丢失，才能作为"拒绝进入可编辑 MD"的依据）
 */
export function judge(expect, actual) {
    switch (expect) {
        case "identical":
            return actual === "identical";
        case "normalized":
            return actual === "identical" || actual === "normalized";
        case "reject":
            return actual !== "identical";
        default:
            return false;
    }
}

/** 首个差异行（用于 reject/normalized 证据展示）。 */
export function firstDiffHint(a, b, maxChars = 160) {
    if (a === b) return "";
    const al = a.split("\n");
    const bl = b.split("\n");
    for (let i = 0; i < Math.max(al.length, bl.length); i++) {
        if (al[i] !== bl[i]) {
            const clip = (s) => {
                const t = (s ?? "").slice(0, maxChars);
                return t === "" ? "<空行>" : t;
            };
            return `L${i + 1}: 源「${clip(al[i])}」 → 出「${clip(bl[i])}」`;
        }
    }
    return "仅行尾/长度差异";
}

/** 极简 LCS 行级 diff（报告用，性能不敏感）。 */
export function lineDiff(a, b) {
    const al = a.split("\n");
    const bl = b.split("\n");
    const n = al.length;
    const m = bl.length;
    // 防御：语料都是小文件，但 perf 生成物不会走到这里
    if (n * m > 4_000_000) return [{kind: "same", text: "(diff 省略：样本过大)"}];
    const dp = Array.from({length: n + 1}, () => new Uint32Array(m + 1));
    for (let i = n - 1; i >= 0; i--) {
        for (let j = m - 1; j >= 0; j--) {
            dp[i][j] = al[i] === bl[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
        }
    }
    const out = [];
    let i = 0;
    let j = 0;
    while (i < n && j < m) {
        if (al[i] === bl[j]) {
            out.push({kind: "same", text: al[i]});
            i++;
            j++;
        } else if (dp[i + 1][j] >= dp[i][j + 1]) {
            out.push({kind: "del", text: al[i++]});
        } else {
            out.push({kind: "add", text: bl[j++]});
        }
    }
    while (i < n) out.push({kind: "del", text: al[i++]});
    while (j < m) out.push({kind: "add", text: bl[j++]});
    return out;
}

/** 读语料文件并做生产同款换行归一化。 */
export function readCorpusFile(absPath) {
    return normalizeEol(readFileSync(absPath, "utf8"));
}
