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

/** 深比较两个 JSON 值（节点 JSON；键序无关） */
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
    const ka = Object.keys(a);
    const kb = Object.keys(b);
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
    if (typeof raw !== "string") return "";
    return raw.replace(/^\n+/, "").replace(/\s+$/, "");
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
 * 语义：
 * - 窗口内的**内容块**被替换为新节点序列化结果，块之间以 `\n\n` 连接；
 * - 窗口**两端**的空段落由窗口外的 gap 承载，因此不参与替换区间（gap 原样保留）；
 * - 窗口内**中间**的空段落由 `\n\n` 连接自然表达（空段落序列化为空串）；
 * - 纯空段落增删（窗口内无内容块）→ 按 gap 长度调整（1 个空段落 ↔ 2 个换行）。
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

    // 节点空间 → 块空间：窗口之前的内容节点数即内容块下标
    const bi0 = countContentNodes(before, 0, win.beforeStart);
    const bi1 = countContentNodes(before, 0, win.beforeEnd);
    const blockCount = blocks.length;

    const afterWindow = after.slice(win.afterStart, win.afterEnd);
    const afterWindowText = afterWindow.map((n) => (isEmptyParagraphNode(n) ? "" : nodeText(n, serializeNode)));

    const beforeContent = countContentNodes(before, win.beforeStart, win.beforeEnd);
    const afterContent = countContentNodes(afterWindow, 0, afterWindow.length);
    const emptyDelta = (afterWindow.length - afterContent) - ((win.beforeEnd - win.beforeStart) - beforeContent);

    // 纯空段落增删（窗口内无内容块变化）→ 按 gap 长度调整：1 个空段落 ↔ 2 个换行
    if (afterContent === 0 && beforeContent === 0) {
        if (emptyDelta === 0) return [];
        if (emptyDelta > 0) {
            const insertAt = bi0 < blockCount ? blocks[bi0].contentStart : sourceText.length;
            const eol = preferredEol(sourceText, insertAt, insertAt);
            return [{start: insertAt, end: insertAt, text: eol.repeat(2 * emptyDelta)}];
        }
        const gapEnd = bi0 < blockCount ? blocks[bi0].contentStart : sourceText.length;
        const eol = preferredEol(sourceText, gapEnd, gapEnd);
        const removed = eol.repeat(2 * -emptyDelta);
        const from = gapEnd - removed.length;
        if (from < 0 || sourceText.slice(from, gapEnd) !== removed) return [];
        return [{start: from, end: gapEnd, text: ""}];
    }

    // 窗口两端的空段落由窗口外的 gap 承载，剔除后按节点顺序拼接块文本
    let head = 0;
    while (head < afterWindowText.length && afterWindowText[head] === "" && isEmptyParagraphNode(afterWindow[head])) {
        head += 1;
    }
    let tail = afterWindowText.length;
    while (tail > head && afterWindowText[tail - 1] === "" && isEmptyParagraphNode(afterWindow[tail - 1])) {
        tail -= 1;
    }
    const patchStart = bi0 < blockCount ? blocks[bi0].contentStart : sourceText.length;
    const patchEnd = bi1 > bi0 && bi1 <= blockCount
        ? contentPatchEnd(sourceText, blocks[bi1 - 1].contentEnd)
        : patchStart;
    const eol = preferredEol(sourceText, patchStart, patchEnd);
    const chunk = withEol(afterWindowText.slice(head, tail).join("\n\n"), eol);

    if (bi1 > bi0) {
        if (chunk === "") {
            // 窗口内内容块被整体删除：吸掉左侧 gap，保留右侧 gap 作为唯一分隔
            const leftEnd = bi0 > 0
                ? contentPatchEnd(sourceText, blocks[bi0 - 1].contentEnd)
                : 0;
            const sep = bi0 > 0 && bi1 < blockCount
                ? sourceText.slice(blocks[bi0].start, blocks[bi0].contentStart)
                : "";
            const rightStart = bi1 < blockCount ? blocks[bi1].contentStart : sourceText.length;
            return [{start: leftEnd, end: rightStart, text: sep}];
        }
        return [{
            start: blocks[bi0].contentStart,
            end: patchEnd,
            text: chunk,
        }];
    }

    // 纯插入（窗口内无内容块）：不重写任何已有块内容
    if (bi0 >= blockCount) {
        if (blockCount === 0) {
            // 空文档（或全空白文档）：空白不承载任何节点，直接以块内容作为新源文
            return [{start: 0, end: sourceText.length, text: chunk}];
        }
        const at = contentPatchEnd(sourceText, blocks[blockCount - 1].contentEnd);
        const insertEol = preferredEol(sourceText, at, at);
        return [{start: at, end: at, text: `${insertEol}${insertEol}${chunk}`}];
    }
    const next = blocks[bi0];
    const insertEol = preferredEol(sourceText, next.start, next.contentStart);
    // Insert after the existing gap.  In CRLF input `contentEnd` includes the
    // CR while the following gap starts at LF; inserting at `next.start` would
    // split that delimiter and create a bare LF.  Keeping the old gap wholly
    // before the new block preserves it byte-for-byte.
    return [{start: next.contentStart, end: next.contentStart, text: `${chunk}${insertEol.repeat(2)}`}];
}
