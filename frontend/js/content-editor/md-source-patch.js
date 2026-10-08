/**
 * source-aware Markdown 块级 patch（纯逻辑，无 DOM / 无 Tiptap 依赖）。
 *
 * ## 为什么需要它
 *
 * 修复前 `EditorAdapter.switchView()` 与 `MarkdownIrEngine.getText()` 用
 * `editor.getMarkdown()`（整篇序列化）作为 MD 视图的"正文"。Tiptap 的序列化器
 * 会把**整篇**文档按编辑器规范重写：`*`→`-`、`__`→`**`、Setext→ATX、空行收敛
 * 等。于是"在 MD 里改一个字"也会改写未编辑区间——用户正文被静默改变。
 *
 * 本模块把 MD 视图从"整篇序列化"改成**源码感知的最小块级 patch**：
 *
 * 1. 把 canonical 源文切成**顶层内容块**（连续非空行构成的区块，块与块之间的
 *    空白行原样保留为 gap）；
 * 2. 逐块与 Tiptap 文档的**顶层节点**对齐验证——验证不通过就不进入可编辑 MD
 *    （安全降级为只读预览，见 editor-adapter.js）；
 * 3. 用户编辑后，用顶层节点的「公共前缀 / 公共后缀」求**最小变更窗口**，
 *    只把该窗口对应的源文区间替换为新节点序列化结果；窗口外的源文字节
 *    **原样保留**（列表符号、Setext 标题、强调符号、空行数量都不动）。
 *
 * ## 安全边界
 *
 * - 对齐验证失败（块数/内容不符、罕见结构）→ `ok:false`，调用方降级为只读预览；
 * - patch 合法性由 `CanonicalBuffer.applyPatches` 二次校验（越界/重叠即拒绝）；
 * - patch 之后调用方仍用 `parse(newText)` 与当前文档做一次**整篇复核**，
 *   不一致则显式降级并提示——绝不静默整篇重写。
 * - 不扩大 Markdown 白名单，也不靠"比较序列化结果"冒充无损：这里比较的是
 *   **解析后的节点 JSON**（形态等价），保留的是**源文字节**。
 */

/** 围栏起始：最多 3 个前导空格 + 3 个以上同字符 ``` 或 ~~~ */
const RE_FENCE_OPEN = /^ {0,3}(`{3,}|~{3,})/;
/** 围栏结束：仅围栏字符与空白 */
const RE_FENCE_CLOSE = /^ {0,3}([`~]{3,})\s*$/;
/** ATX 标题 */
const RE_ATX = /^ {0,3}#{1,6}(\s|$)/;
/** Setext 下划线（`===` / `---`） */
const RE_SETEXT = /^ {0,3}(=+|-+)\s*$/;
/** 主题分隔线 */
const RE_THEMATIC = /^ {0,3}([-*_])(\s*\1){2,}\s*$/;
/** 引用 */
const RE_BLOCKQUOTE = /^ {0,3}>/;
/** 列表项标记 */
const RE_LIST_MARKER = /^ {0,3}([-+*]|\d{1,9}[.)])(\s|$)/;
/** 块级 HTML 起始 */
const RE_HTML_START = /^ {0,3}(?:<!--|<[a-zA-Z][a-zA-Z0-9-]*[\s/>]|<\/[a-zA-Z])/;
/** 数学块定界 `$$` */
const RE_MATH_FENCE = /^ {0,3}\$\$/;
/** 脚注定义 */
const RE_FOOTNOTE_DEF = /^ {0,3}\[\^[^\]\s]+\]:/;
/** GFM 表格分隔行（与 markdown-gate 同口径） */
const RE_TABLE_DELIM = /^ {0,3}\|?[\s:|-]*-{2,}[\s:|-]*\|?\s*$/;
/** 缩进代码块（4 空格或 tab） */
const RE_INDENTED = /^(?: {4}|\t)/;
/** Setext 下划线必须在段落后 */
const RE_BLANK_LINE_GAP = /\n\s*\n/;

/**
 * 按行切分，保留每行精确的绝对区间（`end` 不含换行符）。
 * 文本以换行结尾时补一个空行记录，保证区间连续覆盖全文。
 */
function splitLines(text) {
    const lines = [];
    let start = 0;
    while (start <= text.length) {
        const nl = text.indexOf("\n", start);
        if (nl === -1) {
            lines.push({start, end: text.length, text: text.slice(start)});
            break;
        }
        lines.push({start, end: nl, text: text.slice(start, nl)});
        start = nl + 1;
        if (start === text.length) {
            lines.push({start, end: text.length, text: ""});
            break;
        }
    }
    return lines;
}

function isBlank(line) {
    return line.text.trim() === "";
}

/** 该行是否开始一个新的顶层块（段落/引用/列表的边界判定） */
function startsNewBlock(line) {
    const t = line.text;
    if (isBlank(line)) return true;
    return RE_FENCE_OPEN.test(t)
        || RE_ATX.test(t)
        || RE_THEMATIC.test(t)
        || RE_BLOCKQUOTE.test(t)
        || RE_LIST_MARKER.test(t)
        || RE_FOOTNOTE_DEF.test(t)
        || RE_HTML_START.test(t)
        || RE_MATH_FENCE.test(t);
}

function isTableDelimiter(line) {
    const t = line.text.trim();
    if (!t.includes("|")) return false;
    return RE_TABLE_DELIM.test(line.text) && /-{2,}/.test(t);
}

/** 跳过空行，返回下一个非空行下标（无则返回 lines.length） */
function nextNonBlank(lines, from) {
    let k = from;
    while (k < lines.length && isBlank(lines[k])) k += 1;
    return k;
}

/**
 * 从 `from` 行开始确定一个块的**最后一行下标**（含）。
 * 保守实现：拿不准的形态宁可切错——调用方的对齐验证会拒绝并降级，
 * 不会写出错误正文。
 */
function consumeBlock(lines, from) {
    const first = lines[from];
    const t = first.text;

    // 围栏代码块
    const fence = t.match(RE_FENCE_OPEN);
    if (fence) {
        const char = fence[1][0];
        const len = fence[1].length;
        for (let j = from + 1; j < lines.length; j += 1) {
            const close = lines[j].text.match(RE_FENCE_CLOSE);
            if (close && close[1][0] === char && close[1].length >= len) return j;
        }
        return Math.max(from, lines.length - 1);
    }

    // 数学块 $$...$$
    if (RE_MATH_FENCE.test(t)) {
        for (let j = from + 1; j < lines.length; j += 1) {
            const trimmed = lines[j].text.trim();
            if (trimmed === "$$" || trimmed.endsWith("$$")) return j;
        }
        return Math.max(from, lines.length - 1);
    }

    // 块级 HTML：注释消费到 `-->`，其余消费到空行
    if (RE_HTML_START.test(t)) {
        if (t.includes("<!--")) {
            for (let j = from; j < lines.length; j += 1) {
                if (lines[j].text.includes("-->")) return j;
            }
        }
        let j = from;
        while (j + 1 < lines.length && !isBlank(lines[j + 1])) j += 1;
        return j;
    }

    // 表格（表头行 + 分隔行 + 数据行）
    if (t.includes("|") && from + 1 < lines.length && isTableDelimiter(lines[from + 1])) {
        let j = from + 1;
        while (j + 1 < lines.length && !isBlank(lines[j + 1])) j += 1;
        return j;
    }

    // 脚注定义：本行 + 后续缩进续行
    if (RE_FOOTNOTE_DEF.test(t)) {
        let j = from;
        while (j + 1 < lines.length) {
            const next = lines[j + 1];
            if (isBlank(next)) {
                const k = nextNonBlank(lines, j + 1);
                if (k < lines.length && RE_INDENTED.test(lines[k].text)) {
                    j = k;
                    continue;
                }
                break;
            }
            if (RE_INDENTED.test(next.text)) {
                j += 1;
                continue;
            }
            break;
        }
        return j;
    }

    // 引用：连续 `>` 行（允许空行后继续引用）/ 懒续行
    if (RE_BLOCKQUOTE.test(t)) {
        let j = from;
        while (j + 1 < lines.length) {
            const next = lines[j + 1];
            if (RE_BLOCKQUOTE.test(next.text)) {
                j += 1;
                continue;
            }
            if (isBlank(next)) {
                const k = nextNonBlank(lines, j + 1);
                if (k < lines.length && RE_BLOCKQUOTE.test(lines[k].text)) {
                    j = k;
                    continue;
                }
                break;
            }
            if (!startsNewBlock(next)) {
                j += 1;
                continue;
            }
            break;
        }
        return j;
    }

    // 列表：列表项 / 缩进续行 / 空行后仍是列表项或缩进续行
    if (RE_LIST_MARKER.test(t)) {
        let j = from;
        while (j + 1 < lines.length) {
            const next = lines[j + 1];
            if (isBlank(next)) {
                const k = nextNonBlank(lines, j + 1);
                if (k < lines.length
                    && (RE_LIST_MARKER.test(lines[k].text) || RE_INDENTED.test(lines[k].text))) {
                    j = k;
                    continue;
                }
                break;
            }
            if (RE_LIST_MARKER.test(next.text) || RE_INDENTED.test(next.text)) {
                j += 1;
                continue;
            }
            break;
        }
        return j;
    }

    if (RE_ATX.test(t)) return from;
    if (RE_THEMATIC.test(t)) return from;

    // 缩进代码块
    if (RE_INDENTED.test(t)) {
        let j = from;
        while (j + 1 < lines.length) {
            const next = lines[j + 1];
            if (RE_INDENTED.test(next.text)) {
                j += 1;
                continue;
            }
            if (isBlank(next)) {
                const k = nextNonBlank(lines, j + 1);
                if (k < lines.length && RE_INDENTED.test(lines[k].text)) {
                    j = k;
                    continue;
                }
                break;
            }
            break;
        }
        return j;
    }

    // 默认：段落（连续非空行）；块内的 Setext 下划线属于本段（`Title\n===`）
    let j = from;
    while (j + 1 < lines.length) {
        const next = lines[j + 1];
        if (!startsNewBlock(next)) {
            j += 1;
            continue;
        }
        if (RE_SETEXT.test(next.text)) {
            j += 1;
            continue;
        }
        break;
    }
    return j;
}

/**
 * 扫描顶层内容块。返回的块**连续覆盖**全文：
 * `blocks[0].start === 0`、`blocks[i].start === blocks[i-1].end`、
 * `blocks[last].end === text.length`（尾部空白归入最后一块）。
 *
 * 每块：`{start, contentStart, contentEnd, end, text}`，
 * `text = slice(contentStart, contentEnd)`；`slice(start, contentStart)` 是前导 gap，
 * `slice(contentEnd, end)` 是尾随 gap。
 *
 * @param {string} text - canonical 源文（保留原始换行）
 */
export function scanTopLevelBlocks(text) {
    const source = typeof text === "string" ? text : "";
    if (source.length === 0) return [];
    const lines = splitLines(source);
    const blocks = [];
    let regionStart = 0;
    let i = 0;

    while (i < lines.length) {
        const contentLine = nextNonBlank(lines, i);
        if (contentLine >= lines.length) break;
        const contentStart = lines[contentLine].start;
        const last = consumeBlock(lines, contentLine);
        const contentEnd = lines[last].end;
        blocks.push({
            start: regionStart,
            contentStart,
            contentEnd,
            end: contentEnd,
            text: source.slice(contentStart, contentEnd),
        });
        regionStart = contentEnd;
        i = last + 1;
    }

    if (blocks.length > 0) blocks[blocks.length - 1].end = source.length;
    return blocks;
}

/** Count line endings in a source fragment without treating CRLF as two. */
function countEols(text) {
    let crlf = 0;
    let lf = 0;
    let cr = 0;
    for (let i = 0; i < text.length; i += 1) {
        if (text[i] === "\r") {
            if (text[i + 1] === "\n") {
                crlf += 1;
                i += 1;
            } else {
                cr += 1;
            }
        } else if (text[i] === "\n") {
            lf += 1;
        }
    }
    return {crlf, lf, cr};
}

/**
 * Choose a deterministic newline style for newly serialized Markdown.
 * A nearby block wins; ties fall back to the document's dominant style, and
 * the first encountered style breaks a document-wide tie.  Existing source
 * gaps are never normalized by this module.
 */
function preferredEol(sourceText, start = 0, end = sourceText.length) {
    const local = sourceText.slice(Math.max(0, start - 512), Math.min(sourceText.length, end + 512));
    const localCounts = countEols(local);
    const totalLocal = localCounts.crlf + localCounts.lf + localCounts.cr;
    const counts = totalLocal > 0 ? localCounts : countEols(sourceText);
    const max = Math.max(counts.crlf, counts.lf, counts.cr);
    if (max === 0) return "\n";
    const first = sourceText.search(/\r\n|\r|\n/);
    const firstEol = first >= 0
        ? (sourceText.slice(first, first + 2) === "\r\n" ? "\r\n" : sourceText[first])
        : "\n";
    if (counts.crlf === max && counts.crlf > counts.lf && counts.crlf > counts.cr) return "\r\n";
    if (counts.lf === max && counts.lf > counts.crlf && counts.lf > counts.cr) return "\n";
    if (counts.cr === max && counts.cr > counts.crlf && counts.cr > counts.lf) return "\r";
    return firstEol;
}

function withEol(text, eol) {
    return String(text ?? "").replace(/\r\n|\r|\n/g, eol);
}

/** `contentEnd` includes the CR of a CRLF line because splitLines excludes
 * only the LF.  Leave that CR+LF delimiter untouched when patching a block. */
function contentPatchEnd(sourceText, end) {
    return end > 0 && sourceText[end - 1] === "\r" ? end - 1 : end;
}

/** 空段落节点判定（Tiptap 对连续/尾部空行会产出空 paragraph，序列化可能为 `&nbsp;`） */
export function isEmptyParagraphNode(node) {
    if (!node || node.type !== "paragraph") return false;
    const content = node.content;
    if (!Array.isArray(content) || content.length === 0) return true;
    let text = "";
    for (const child of content) {
        if (!child || typeof child.text !== "string") return false;
        text += child.text;
    }
    return text.replace(/[\s\u00a0]+/g, "") === "";
}

/**
 * 归一化节点 JSON：递归删除空数组 `content` key。
 *
 * ProseMirror `Node.toJSON()` 对无内容节点省略 `content`，而
 * @tiptap/markdown 解析产物显式输出 `content: []`——「整篇复核」两侧
 * （parse 产物 vs toJSON 快照）口径不同会使 jsonEqual 恒假：文档含
 * 尾部空行/连续空行（空段落产物）时，任何编辑触发的复核都失败并误转
 * 只读（0.25.18）。空数组与缺失在 ProseMirror 节点语义中等价，比较前
 * 统一剥离为缺失（配合 jsonEqual 的 undefined 值 key 忽略）。
 */
export function normalizeNodeJson(node) {
    if (Array.isArray(node)) return node.map(normalizeNodeJson);
    if (!node || typeof node !== "object") return node;
    const out = {};
    for (const [k, v] of Object.entries(node)) {
        if (k === "content" && Array.isArray(v) && v.length === 0) continue;
        out[k] = normalizeNodeJson(v);
    }
    return out;
}

/**
 * 深比较两个 JSON 值（节点 JSON；键序无关；undefined 值 key 视为不存在）。
 *
 * undefined 值 key 必须忽略：@tiptap/markdown 解析 codeBlock 内 text node 产出
 * `{type, text, marks: undefined}`（脏 key），而 ProseMirror `toJSON()` 产出
 * `{type, text}`——对齐验证两边口径不同，若按 key 数比较，任何含代码块的
 * 文档都会误判"对齐失败"而锁定只读（0.25.18）。JSON.stringify 口径同样
 * 跳过 undefined 值 key，本函数语义与其一致。
 */
export function jsonEqual(a, b) {
    if (a === b) return true;
    if (a === null || b === null || a === undefined || b === undefined) return false;
    if (typeof a !== typeof b) return false;
    if (typeof a !== "object") return false;
    if (Array.isArray(a) !== Array.isArray(b)) return false;
    if (Array.isArray(a)) {
        if (a.length !== b.length) return false;
        for (let i = 0; i < a.length; i += 1) {
            if (!jsonEqual(a[i], b[i])) return false;
        }
        return true;
    }
    const ka = Object.keys(a).filter((k) => a[k] !== undefined);
    const kb = Object.keys(b).filter((k) => b[k] !== undefined);
    if (ka.length !== kb.length) return false;
    for (const k of ka) {
        if (!Object.prototype.hasOwnProperty.call(b, k)) return false;
        if (!jsonEqual(a[k], b[k])) return false;
    }
    return true;
}

/**
 * 顶层节点序列的最小变更窗口（公共前缀 + 公共后缀，二者不重叠）。
 *
 * 节点 JSON 由调用方缓存：ProseMirror 未改动的子树保持同一对象引用，
 * 因此"未变化"的判定是一次引用比较，整轮开销接近 O(变更量)。
 *
 * @returns {{beforeStart: number, beforeEnd: number, afterStart: number, afterEnd: number}}
 *   左闭右开；四个值相等时表示无变化。
 */
export function diffTopNodes(before, after) {
    const max = Math.min(before.length, after.length);
    let prefix = 0;
    while (prefix < max && jsonEqual(before[prefix], after[prefix])) prefix += 1;

    let suffix = 0;
    while (
        suffix < max - prefix
        && jsonEqual(before[before.length - 1 - suffix], after[after.length - 1 - suffix])
    ) {
        suffix += 1;
    }
    return {
        beforeStart: prefix,
        beforeEnd: before.length - suffix,
        afterStart: prefix,
        afterEnd: after.length - suffix,
    };
}

/** 统计节点表中指定区间内的内容节点（非空段落）个数 */
function countContentNodes(nodes, start, end) {
    let n = 0;
    for (let k = Math.max(0, start); k < end && k < nodes.length; k += 1) {
        if (!isEmptyParagraphNode(nodes[k])) n += 1;
    }
    return n;
}

/** 单节点的块文本（去掉序列化器可能附加的首尾换行/空白） */
function nodeText(node, serializeNode) {
    const raw = serializeNode(node);
    if (typeof raw === "string" && raw.length === 0) return raw;
    if (typeof raw !== "string") return "";
    // 只剥序列化器附加的首尾换行，保留行尾空白：段落尾随空格是用户内容
    // （@tiptap/markdown 的 parser 逐字保留），此前 `\s+$` 全量剃除使
    // 「abc 」物化成「abc」→ 整篇复核必败（0.25.18）。
    return raw.replace(/^\n+/, "").replace(/\n+$/, "");
}

/**
 * 建立「源文内容块 ↔ 文档顶层内容节点」映射并做对齐验证。
 *
 * 空段落节点允许出现在任意位置（连续空行 / 尾部空行的产物），不占源文块；
 * 内容节点必须与块**一一对应且顺序一致**。
 *
 * @param {string} sourceText - canonical 源文
 * @param {{content?: any[]}} docJson - 当前文档 JSON
 * @param {(blockText: string) => any} parseBlock - 单块解析（返回节点 JSON；无法解析返回 null）
 * @returns {{ok: boolean, reason: null|"align", blocks: Array, mismatchAt: number|null}}
 */
export function buildBlockMap(sourceText, docJson, parseBlock) {
    const blocks = scanTopLevelBlocks(sourceText);
    const nodes = Array.isArray(docJson?.content) ? docJson.content : [];

    if (blocks.length === 0 && nodes.length === 0) {
        return {ok: true, reason: null, blocks, mismatchAt: null};
    }

    let i = 0;
    for (let b = 0; b < blocks.length; b += 1) {
        while (i < nodes.length && isEmptyParagraphNode(nodes[i])) i += 1;
        if (i >= nodes.length) {
            return {ok: false, reason: "align", blocks, mismatchAt: b};
        }
        let parsed = null;
        try {
            parsed = parseBlock(blocks[b].text);
        } catch {
            parsed = null;
        }
        if (!parsed || !jsonEqual(parsed, nodes[i])) {
            return {ok: false, reason: "align", blocks, mismatchAt: b};
        }
        i += 1;
    }
    while (i < nodes.length) {
        if (!isEmptyParagraphNode(nodes[i])) {
            return {ok: false, reason: "align", blocks, mismatchAt: null};
        }
        i += 1;
    }
    return {ok: true, reason: null, blocks, mismatchAt: null};
}

/**
 * 生成针对 canonical 源文的最小块级 patch（纯函数）。
 *
 * ## 区间统一重建模型（0.25.18 重构）
 *
 * 早期实现按编辑形态分 6 个特例分支手工拼 gap 换行数（纯空段增删 / 空段→
 * 内容 / 整体删除 / 纯插入 / 块间插入 / 块替换），多分支各自推导「空段落 ↔
 * 换行数」映射，漏掉一种形态就丢/多空段落 → 整篇复核必败 → 用户被打断并
 * 锁只读（0.25.18 实测两轮）。本版收敛为单一模型：
 *
 * 1. `diffTopNodes` 求最小变更窗口（不变）；
 * 2. 以**内容块**为锚点定替换区间：`[前一个内容块 contentEnd, 后一个内容
 *    块 contentStart)`，文档首尾以 0 / 文末为界——被重写的只有变更窗口及
 *    其相邻 gap，其余源文逐字节保留；
 * 3. 区间内节点 = 窗口向两侧扩展覆盖的相邻空段落（含未变化空段——其源文
 *    表达在被重写的 gap 里，必须一并重发）；
 * 4. 按唯一一组映射公式重发区间文本：
 *    - 相邻两个内容节点之间 E 个空段落 ↔ `2(E+1)` 个换行；
 *    - 文档首部 / 尾部 E 个空段落 ↔ `2E` 个换行；
 *    - 文档尾部保留原 gap 的换行**奇偶性**（EOF 换行约定，§3.10）。
 *
 * 空段落的增删 / 转变 / 块的插入删除替换全部落进同一组公式，不再有形态
 * 特例；正确性仍由调用方的整篇复核兜底（parse(candidate) ≡ 当前文档）。
 *
 * @param {object} args
 * @param {string} args.sourceText
 * @param {Array} args.blocks - 与**变更前**节点对齐的块表
 * @param {any[]} args.beforeNodes - 变更前顶层节点 JSON
 * @param {any[]} args.afterNodes - 变更后顶层节点 JSON
 * @param {(nodeJson: any) => string} args.serializeNode
 * @returns {Array<{start: number, end: number, text: string}>} 空数组 = 无需改动
 */
export function planSourcePatch({sourceText, blocks, beforeNodes, afterNodes, serializeNode}) {
    const before = Array.isArray(beforeNodes) ? beforeNodes : [];
    const after = Array.isArray(afterNodes) ? afterNodes : [];
    const win = diffTopNodes(before, after);
    if (win.beforeStart === win.beforeEnd && win.afterStart === win.afterEnd) return [];

    const blockCount = blocks.length;
    // 节点空间 → 块空间：窗口之前的内容节点数即首个被窗口覆盖的内容块下标
    const bi0 = countContentNodes(before, 0, win.beforeStart);
    const bi1 = countContentNodes(before, 0, win.beforeEnd);
    const hasPrev = bi0 > 0;
    const hasNext = bi1 < blockCount;

    // 区间节点序列：窗口 + 两侧相邻空段落。公共前缀/后缀保证这些外层空段
    // 落是未变化节点，但它们的源文表达位于被重写的 gap 内，必须参与重发。
    let from = win.afterStart;
    while (from > 0 && isEmptyParagraphNode(after[from - 1])) from -= 1;
    let to = win.afterEnd;
    while (to < after.length && isEmptyParagraphNode(after[to])) to += 1;
    const nodes = after.slice(from, to);

    const regionFrom = hasPrev ? contentPatchEnd(sourceText, blocks[bi0 - 1].contentEnd) : 0;
    const regionTo = hasNext ? blocks[bi1].contentStart : sourceText.length;
    const eol = preferredEol(sourceText, regionFrom, regionTo);

    // 文档尾部的 EOF 换行（奇数个换行收尾）不属于任何空段落；重写尾部 gap
    // 时保留其奇偶位，`X\n` 编辑后仍是 `X…\n` 而非吞掉末换行（§3.10）。
    let tailPad = 0;
    if (!hasNext) {
        const tailFrom = blockCount > 0
            ? contentPatchEnd(sourceText, blocks[blockCount - 1].contentEnd)
            : 0;
        const counts = countEols(sourceText.slice(tailFrom, sourceText.length));
        tailPad = (counts.crlf + counts.lf + counts.cr) % 2;
    }

    const emitted = emitRegionText(nodes, hasPrev, hasNext, serializeNode, eol)
        + (tailPad > 0 ? eol : "");
    const text = withEol(emitted, eol);
    if (text === sourceText.slice(regionFrom, regionTo)) return [];
    return [{start: regionFrom, end: regionTo, text}];
}

/**
 * 重发区间文本：空段落只计入换行游程（不序列化），内容节点按序拼接。
 * 映射公式见 planSourcePatch 文档；`run` 是自上一个内容节点以来的空段落数。
 */
function emitRegionText(nodes, hasPrev, hasNext, serializeNode, eol) {
    const nl = (n) => eol.repeat(2 * n);
    const parts = [];
    let run = 0;
    let emitted = false;
    for (const node of nodes) {
        if (isEmptyParagraphNode(node)) {
            run += 1;
            continue;
        }
        // 首个内容节点：有前锚点（或前面已发过内容）时带分隔符，文档首部裸开头
        parts.push(nl(hasPrev || emitted ? run + 1 : run));
        parts.push(nodeText(node, serializeNode));
        run = 0;
        emitted = true;
    }
    if (!emitted) {
        // 区间内无内容节点：整段就是两锚点之间的 gap（或文档首/尾 gap）
        return nl(hasPrev && hasNext ? run + 1 : run);
    }
    return parts.join("") + nl(hasNext ? run + 1 : run);
}
