/**
 * Markdown 风险门（0.23.1，纯函数、无 DOM 依赖）。
 *
 * 冻结规则（docs/phases/0.23-editor-voice-ai-workflow.md §3.10，语料证据见
 * testdata/editor/markdown/roundtrip-report.md）：
 * - 结构拒绝：GFM 表格（整块丢失）、脚注（被改写为链接）、`$$` 数学块与行内
 *   公式（命令被转义污染）、HTML 块/内联/注释（标签转义为字面文本）、
 *   行内代码含反引号（Tiptap 3.29.2 序列化缺陷，二次解析不稳定）。
 * - 规范化重写（`*`→`-`、Setext→ATX 等）**不阻断**，由检查点机制保证
 *   未编辑时逐字符还原原文。
 * - 尺寸门：≤32KB 正常进；32–128KB 可进但不自动进入、提示卡顿；>128KB 拒绝。
 * - 风险门（结构）优先于来源偏好；Source 视图不受任何门限制。
 *
 * 本文件是启发式文本扫描（与 Tiptap 行为的对应关系由 corpus 回归测试保证：
 * frontend/js/content-editor/engines/markdown-gate.test.mjs 逐样本断言）。
 */

/** 尺寸门冻结阈值（字节） */
export const MD_SIZE_NORMAL_BYTES = 32 * 1024;
export const MD_SIZE_REJECT_BYTES = 128 * 1024;

/**
 * 评估文本能否进入可编辑 MD 视图。
 *
 * @param {string} text - 原始 Markdown 文本
 * @param {object} [deps] - 可注入依赖（测试用）
 * @param {TextEncoder} [deps.encoder] - 字节长度计算器
 * @returns {{
 *   allowed: boolean,        // 是否允许进入可编辑 MD
 *   autoEnter: boolean,      // 是否允许作为默认视图直接进入（preferred 来源）
 *   reason: null|"structure"|"large",  // 拒绝原因
 *   sizeWarn: boolean,       // 32–128KB 区间：可进但提示卡顿
 * }}
 */
export function evaluateMarkdownGate(text, deps = {}) {
    const encoder = deps.encoder ?? new TextEncoder();
    const bytes = encoder.encode(text).length;

    if (bytes > MD_SIZE_REJECT_BYTES) {
        return {allowed: false, autoEnter: false, reason: "large", sizeWarn: false};
    }

    const structuralReject = hasRejectStructure(text);
    if (structuralReject) {
        return {allowed: false, autoEnter: false, reason: "structure", sizeWarn: false};
    }

    const sizeWarn = bytes > MD_SIZE_NORMAL_BYTES;
    return {allowed: true, autoEnter: !sizeWarn, reason: null, sizeWarn};
}

/**
 * 结构拒绝扫描——逐行扫描，跟踪围栏代码块状态（围栏内的任意内容不参与判定，
 * 代码示例中的 HTML/表格等不应阻断真实文档）。
 */
export function hasRejectStructure(text) {
    const lines = text.split("\n");
    let fence = null; // { char: "`"|"~", len: number }

    for (const line of lines) {
        const fenceMatch = line.match(/^\s{0,3}([`~]{3,})/);
        if (fence) {
            // 围栏内：仅当出现同级或更长的同字符围栏时退出
            const closing = line.match(/^\s{0,3}([`~]{3,})\s*$/);
            if (closing && closing[1][0] === fence.char && closing[1].length >= fence.len) {
                fence = null;
            }
            continue;
        }
        if (fenceMatch) {
            fence = {char: fenceMatch[1][0], len: fenceMatch[1].length};
            continue;
        }
        if (lineHasRejectStructure(line)) return true;
    }
    return false;
}

/** 单行结构拒绝判定（假定不在围栏代码块内） */
function lineHasRejectStructure(line) {
    // 0. 行内代码含反引号：围栏外出现连续两个及以上反引号
    //    （合法代码围栏已在扫描层排除；行内多反引号定界即 Tiptap 缺陷场景）。
    //    必须在剥离行内代码之前判定——`` ` ``定界本身就是要抓的形态。
    if (/`{2,}/.test(line)) return true;

    // 剥离行内代码 span：code 内容是字面文本，其中的 <tag>、|表格|、$公式$
    // 不参与结构判定（corpus/support/inline-code.md 语料实证）。
    // 等长空格替换，保持同一行其余部分的相对位置不变。
    const stripped = line.replace(/`[^`\n]*`/g, (m) => " ".repeat(m.length));

    // 1. GFM 表格分隔行：仅由 |、-、:、空格构成，且含至少一段连续连字符。
    //    分隔行在正文中不会自然出现；表头行（含文字）交由分隔行特征抓取。
    if (isTableDelimiterRow(stripped)) return true;

    // 2. 脚注引用或定义：[^1] / [^note]
    if (/\[\^[^\]\s]+\]/.test(stripped)) return true;

    // 3. 数学：$$ 块（含定界符行）或行内 $...$（内容含 LaTeX 特征字符）
    if (stripped.includes("$$")) return true;
    if (/\$[^\s$][^$\n]*\$/.test(stripped)) {
        // 提取所有 $...$ 片段，任一含 LaTeX 特征（\ ^ _ { }）即视为公式，
        // 避免 "价格 $5 和 $3" 这类货币误伤（数字段不含特征字符）
        const inlineMaths = stripped.match(/\$[^\s$][^$\n]*?\$/g) ?? [];
        if (inlineMaths.some((m) => /[\^_{}\\]/.test(m.slice(1, -1)))) return true;
    }

    // 4. HTML：注释 / 标签。排除 autolink `<https://...>`（tag 名后跟 `:` 不匹配标签）。
    if (stripped.includes("<!--")) return true;
    if (/<\/?[a-zA-Z][a-zA-Z0-9-]*(\s[^<>]*)?>/.test(stripped)) return true;

    return false;
}

/** GFM 表格分隔行：仅由 |、-、:、空格构成，且含至少一段连续 2 个以上连字符 */
function isTableDelimiterRow(line) {
    const trimmed = line.trim();
    if (!trimmed.includes("|")) return false;
    if (!/^[\s|:\-]+$/.test(trimmed)) return false;
    return /-{2,}/.test(trimmed);
}
