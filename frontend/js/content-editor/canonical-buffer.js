/**
 * CanonicalBuffer —— 编辑会话正文的唯一真源（UTF-8 源文缓冲区）。
 *
 * 背景（本次修复的核心根因）：修复前会话正文没有单一真源——Source 视图的
 * 真源是 textarea.value，MD 视图的真源是「Tiptap 文档序列化结果」，切换视图
 * 时把当前引擎文本交给新引擎。于是 MD 一旦发生任何局部编辑，
 * `editor.getMarkdown()` 会把**整篇**文档重新序列化（列表符号、强调符号、
 * Setext 标题、空行数量等被规范化重写），未编辑区间也一并被改写。
 *
 * 本模块提供会话正文的唯一真源：
 * - 任何视图切换、保存、草稿持久化都只读写本缓冲区，不再从引擎取全文；
 * - Source 视图的 textarea 是缓冲区的**直接编辑面**（逐字符等价）；
 * - MD 视图是缓冲区的**投影**：局部编辑只产出针对缓冲区的最小 patch
 *   （见 md-source-patch.js），未编辑区块保持原始字节。
 *
 * 除文本外还维护：
 * - `revision`：单调内容版本。任何真实内容变更 +1，供提交协议与草稿
 *   revision 水位使用（旧 revision 的异步写入不得覆盖新正文）；
 * - `hash`：FNV-1a 64 内容摘要（与后端 `domain::editor::body_digest` 同算法、
 *   同为 UTF-8 字节序），用于「同 revision 是否同正文」的廉价判定与草稿幂等。
 */

/** FNV-1a 64 偏移基准 */
const FNV_OFFSET_BASIS = 0xcbf29ce484222325n;
/** FNV-1a 64 质数 */
const FNV_PRIME = 0x100000001b3n;
/** 64 位掩码 */
const U64_MASK = 0xffffffffffffffffn;

/** 共享 UTF-8 编码器（Node/DOM 均可用；缺省时退化为码点近似，不抛错）。 */
let _encoder = null;

function encodeUtf8(text) {
    if (typeof TextEncoder !== "undefined") {
        if (!_encoder) _encoder = new TextEncoder();
        return _encoder.encode(text);
    }
    return Buffer.from(text, "utf8");
}

/**
 * FNV-1a 64 摘要（对大端字节序逐字节）。算法与后端 `body_digest` 一致，
 * 故同一正文在两端得到同一个 u64。
 * @param {string} text
 * @returns {bigint}
 */
export function fnv1a64(text) {
    const bytes = encodeUtf8(text ?? "");
    let hash = FNV_OFFSET_BASIS;
    for (let i = 0; i < bytes.length; i++) {
        hash ^= BigInt(bytes[i]);
        hash = (hash * FNV_PRIME) & U64_MASK;
    }
    return hash;
}

/** 摘要的 16 位十六进制投影（IPC / 日志用）。 */
export function fnv1a64Hex(text) {
    return fnv1a64(text).toString(16).padStart(16, "0");
}

/** 严格判定 patch 区间合法性（整数、非负、升序、互不重叠、不越界）。 */
function isValidPatchList(patches, length) {
    let cursor = 0;
    for (const patch of patches) {
        if (!patch || typeof patch !== "object") return false;
        const {start, end} = patch;
        if (!Number.isInteger(start) || !Number.isInteger(end)) return false;
        if (start < cursor || end < start || end > length) return false;
        if (typeof patch.text !== "string") return false;
        cursor = end;
    }
    return true;
}

export class CanonicalBuffer {
    /** 单调内容版本：任何真实内容变更 +1（程序化载入不计） */
    revision = 0;

    /**
     * @param {string} [text] - 初始正文（按 JavaScript 字符串逐字符保留）
     */
    constructor(text = "") {
        this._text = typeof text === "string" ? text : "";
        this._hash = null;
    }

    /** 当前正文（逐字符真值）。 */
    get text() {
        return this._text;
    }

    /** 字符长度（UTF-16 code unit，与 textarea/DOM 一致）。 */
    get length() {
        return this._text.length;
    }

    /** 内容摘要（惰性计算并缓存）。 */
    get hash() {
        if (this._hash === null) this._hash = fnv1a64(this._text);
        return this._hash;
    }

    /** 摘要十六进制投影。 */
    get hashHex() {
        return this.hash.toString(16).padStart(16, "0");
    }

    /** 与给定文本是否逐字节一致。 */
    equals(text) {
        return this._text === text;
    }

    /** 指定区间文本。 */
    slice(start, end) {
        return this._text.slice(start, end);
    }

    /**
     * 程序化整体载入（会话绑定 / 外部同步 / AI 应用 / 视图切换衔接）。
     * **不计入用户编辑**，revision 不变——revision 表达的是"用户可感知的
     * 内容变更版本"，供提交协议与草稿水位使用，程序化载入由调用方另行处理。
     * @param {string} text
     */
    load(text) {
        const next = typeof text === "string" ? text : "";
        if (next === this._text) return;
        this._text = next;
        this._hash = null;
    }

    /**
     * Source 视图用户输入：textarea 的值就是真源，整体替换并推进 revision。
     * @param {string} text
     * @returns {boolean} 内容是否真的变化
     */
    setFromSource(text) {
        const next = typeof text === "string" ? text : "";
        if (next === this._text) return false;
        this._text = next;
        this._hash = null;
        this.revision += 1;
        return true;
    }

    /**
     * 应用一组块级 patch（MD 局部编辑的唯一落地方式）。
     * patches 按 start 升序排列且互不重叠；一次性重建文本，revision 只 +1。
     * 区间非法（越界/重叠/乱序/非整数）时**不产生任何修改**并返回 false——
     * 宁可拒绝也不写出不确定的正文。
     * @param {Array<{start: number, end: number, text: string}>} patches
     * @returns {boolean} 是否产生修改
     */
    applyPatches(patches) {
        if (!Array.isArray(patches) || patches.length === 0) return false;
        if (!isValidPatchList(patches, this._text.length)) return false;

        let out = "";
        let cursor = 0;
        let changed = false;
        for (const patch of patches) {
            out += this._text.slice(cursor, patch.start);
            out += patch.text;
            if (patch.text !== this._text.slice(patch.start, patch.end)) changed = true;
            cursor = patch.end;
        }
        out += this._text.slice(cursor);
        if (!changed) return false;

        this._text = out;
        this._hash = null;
        this.revision += 1;
        return true;
    }

    /**
     * 标记"发生了一次用户可感知的内容变更"，但不改变文本。
     * MD 视图的正文是**延迟物化**的（见 md-source-patch.js）：Tiptap 事务
     * 到达时正文尚未回写缓冲区，但内容版本必须立即推进——提交协议与草稿
     * revision 水位都依赖它单调。
     */
    markChanged() {
        this.revision += 1;
    }

    /** 完整重置（会话结束/引擎销毁）：正文与版本归零。 */
    reset() {
        this._text = "";
        this._hash = null;
        this.revision = 0;
    }
}
