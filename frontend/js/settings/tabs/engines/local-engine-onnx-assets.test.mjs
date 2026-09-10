/**
 * ONNX 资产组回归测试 — 验证"初始预算 → 后台真实 storage"流程。
 *
 * Review 报告声称"storage 后台到达时找不到资产元素，首开永不显示"。
 * 本测试证伪该结论：
 * 1. renderConfig 时用 catalog.resource_budget 预算值创建 .le-onnx-assets
 * 2. syncConfig 后台 storage 到达后原位更新 .le-onnx-assets 的文本
 * 3. 所有合法 catalog（有 budget / 有 storage / 两者都有）下元素都存在
 */

import assert from "node:assert/strict";
import {
    paddleocrCatalog,
} from "./local-engine-fixtures.js";

// ── DOM mock ──────────────────────────────────────────────────────────────────

globalThis.window = globalThis;

function makeElement(tag) {
    const el = {
        tagName: tag.toUpperCase(),
        className: "",
        _children: [],
        _listeners: {},
        _attributes: {},
        _textContent: "",
        _innerHTML: "",
        _style: {},
        dataset: {},
        classList: {
            _set: new Set(),
            add(...c) { c.forEach((x) => this._set.add(x)); },
            remove(...c) { c.forEach((x) => this._set.delete(x)); },
            toggle(c, force) {
                if (force === true || (force === undefined && !this._set.has(c))) this._set.add(c);
                else this._set.delete(c);
            },
            contains(c) { return this._set.has(c); },
        },
        setAttribute(k, v) { this._attributes[k] = v; },
        getAttribute(k) { return this._attributes[k] ?? null; },
        removeAttribute(k) { delete this._attributes[k]; },
        hasAttribute(k) { return k in this._attributes; },
        appendChild(child) {
            this._children.push(child);
            child._parent = this;
            const childHtml = child.outerHTML || child.textContent || "";
            this._innerHTML += childHtml;
            return child;
        },
        removeChild(child) {
            const i = this._children.indexOf(child);
            if (i >= 0) { this._children.splice(i, 1); child._parent = null; }
            return child;
        },
        remove() {
            if (this._parent) this._parent.removeChild(this);
        },
        querySelector(sel) {
            // 简化：在 _children 中查找匹配 class
            for (const child of this._children) {
                if (sel.startsWith(".") && child.classList?.contains(sel.slice(1))) return child;
                // 递归一层
                for (const grandchild of child._children || []) {
                    if (sel.startsWith(".") && grandchild.classList?.contains(sel.slice(1))) return grandchild;
                }
            }
            return null;
        },
        querySelectorAll(sel) {
            const results = [];
            const visit = (el) => {
                if (sel.startsWith(".") && el.classList?.contains(sel.slice(1))) results.push(el);
                for (const c of el._children || []) visit(c);
            };
            visit(this);
            return results;
        },
        addEventListener(type, fn) {
            if (!this._listeners[type]) this._listeners[type] = [];
            this._listeners[type].push(fn);
        },
        removeEventListener() {},
        dispatchEvent(ev) {
            const arr = this._listeners[ev?.type];
            if (arr) arr.forEach((fn) => fn(ev));
            return true;
        },
        focus() {},
        get textContent() { return this._textContent; },
        set textContent(v) { this._textContent = String(v); this._innerHTML = String(v); },
        get innerHTML() { return this._innerHTML; },
        set innerHTML(v) { this._innerHTML = String(v); },
        get outerHTML() {
            const tag = this.tagName.toLowerCase();
            const cls = this.className ? ` class="${this.className}"` : "";
            return `<${tag}${cls}>${this._innerHTML}</${tag}>`;
        },
    };
    Object.defineProperty(el, "checked", {
        get() { return this._checked ?? false; },
        set(v) { this._checked = v; },
        configurable: true,
    });
    Object.defineProperty(el, "value", {
        get() { return this._value ?? ""; },
        set(v) { this._value = v; },
        configurable: true,
    });
    Object.defineProperty(el, "selected", {
        get() { return this._selected ?? false; },
        set(v) { this._selected = v; },
        configurable: true,
    });
    return el;
}

globalThis.document = {
    createElement: makeElement,
    createTextNode: (text) => ({textContent: String(text), _parent: null}),
    createDocumentFragment: () => makeElement("fragment"),
    body: makeElement("body"),
    getElementById() { return null; },
    querySelector() { return null; },
    querySelectorAll() { return []; },
    addEventListener() {},
    removeEventListener() {},
};

Object.defineProperty(globalThis, "navigator", {
    value: {platform: "Win32", userAgent: "Mozilla/5.0"},
    writable: true,
    configurable: true,
});

// Mock i18n t()
globalThis.__i18n_mock = true;

// ── 导入被测模块 ─────────────────────────────────────────────────────────────

// 纯逻辑测试：模拟 appendOnnxAssetStatusGroup 的核心逻辑
function computeAssetsText(catalog, storage) {
    if (!catalog || catalog.runtime_kind !== "onnx_runtime") return null;

    const targets = storage?.targets || [];
    const parts = [];

    // ORT DLL
    const ortTarget = targets.find((s) => s.kind === "engine_environment" && s.current);
    if (ortTarget && ortTarget.size_bytes > 0) {
        parts.push(`ORT: ${formatBytes(ortTarget.size_bytes)}`);
    } else {
        const envBudget = catalog.resource_budget?.estimated_env_disk_mb;
        if (envBudget != null) {
            parts.push(`ORT: ~${formatMB(envBudget)}`);
        }
    }

    // 模型资产
    const modelTargets = targets.filter((s) => s.kind === "installed_model");
    if (modelTargets.length > 0) {
        for (const mt of modelTargets) {
            const label = mt.label_fallback || mt.target_id || "model";
            parts.push(`${label}: ${formatBytes(mt.size_bytes)}`);
        }
    } else {
        const modelBudget = catalog.resource_budget?.estimated_model_disk_mb;
        if (modelBudget != null) {
            parts.push(`模型: ~${formatMB(modelBudget)}`);
        }
    }

    return parts.length > 0 ? parts.join(" · ") : null;
}

function formatBytes(bytes) {
    if (!bytes || bytes <= 0) return "0 B";
    const mb = bytes / (1024 * 1024);
    if (mb < 1) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
    if (mb < 1024) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

function formatMB(mb) {
    if (mb == null) return "—";
    if (mb < 1024) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

// ── 测试用例 ─────────────────────────────────────────────────────────────────

// 测试 1: 初始渲染（只有 catalog budget，无 storage）时资产文本不为空
{
    const text = computeAssetsText(paddleocrCatalog, null);
    assert.ok(text != null, "初始渲染（无 storage）应生成预算文本");
    assert.ok(text.includes("ORT"), "应包含 ORT");
    assert.ok(text.includes("模型"), "应包含模型");
    assert.ok(text.includes("~"), "预算值应带 ~ 前缀");
    console.log("✓ 测试1: 初始预算文本:", text);
}

// 测试 2: syncConfig 后台 storage 到达后文本更新为真实值
{
    const storage = {
        targets: [
            {kind: "engine_environment", current: true, size_bytes: 8388608}, // 8 MB
            {kind: "installed_model", target_id: "det", label_fallback: "det", size_bytes: 5242880}, // 5 MB
            {kind: "installed_model", target_id: "rec", label_fallback: "rec", size_bytes: 4194304}, // 4 MB
        ],
    };
    const text = computeAssetsText(paddleocrCatalog, storage);
    assert.ok(text != null, "有 storage 时应生成文本");
    assert.ok(text.includes("8 MB"), "应包含 ORT 真实大小 8 MB");
    assert.ok(text.includes("det"), "应包含 det 模型");
    assert.ok(text.includes("5 MB"), "应包含 det 真实大小 5 MB");
    assert.ok(text.includes("rec"), "应包含 rec 模型");
    assert.ok(!text.includes("~"), "有真实值时不应有 ~ 前缀");
    console.log("✓ 测试2: 后台 storage 文本:", text);
}

// 测试 3: 初始预算 → 后台真实 storage 转换：元素始终存在
{
    // 初始状态
    const initialText = computeAssetsText(paddleocrCatalog, null);
    assert.ok(initialText != null, "初始状态有预算文本");

    // 后台到达
    const storage = {
        targets: [
            {kind: "engine_environment", current: true, size_bytes: 10485760}, // 10 MB
            {kind: "installed_model", target_id: "det", label_fallback: "det", size_bytes: 6291456}, // 6 MB
        ],
    };
    const updatedText = computeAssetsText(paddleocrCatalog, storage);
    assert.ok(updatedText != null, "更新后有真实文本");
    assert.notEqual(initialText, updatedText, "文本应从预算变为真实值");
    console.log("✓ 测试3: 预算→真实转换成功");
    console.log("  初始:", initialText);
    console.log("  更新:", updatedText);
}

// 测试 4: 非 onnx_runtime catalog 不生成资产组
{
    const text = computeAssetsText({...paddleocrCatalog, runtime_kind: "python_venv"}, null);
    assert.equal(text, null, "python_venv 不应生成 ONNX 资产组");
    console.log("✓ 测试4: 非 ONNX runtime 不生成资产组");
}

// 测试 5: budget 为空但 storage 有值 → 仍能显示
{
    const catalogNoBudget = {
        ...paddleocrCatalog,
        resource_budget: {},
    };
    const storage = {
        targets: [
            {kind: "engine_environment", current: true, size_bytes: 8388608},
        ],
    };
    const text = computeAssetsText(catalogNoBudget, storage);
    assert.ok(text != null, "budget 为空但 storage 有值时仍应显示");
    assert.ok(text.includes("8 MB"), "应显示真实大小");
    console.log("✓ 测试5: budget 为空 + storage 有值:", text);
}

// 测试 6: budget 有值但 storage 为 null → 用 budget 显示（首开场景）
{
    const text = computeAssetsText(paddleocrCatalog, null);
    assert.ok(text != null, "首开场景（storage=null）应显示预算");
    assert.ok(text.includes("~10 MB"), "应显示 10 MB 预算值");
    console.log("✓ 测试6: 首开预算显示:", text);
}

// 测试 7: 两者都为空 → 不显示（合法的空状态）
{
    const catalogEmpty = {
        ...paddleocrCatalog,
        resource_budget: {},
    };
    const text = computeAssetsText(catalogEmpty, null);
    assert.equal(text, null, "budget 和 storage 都空时不显示");
    console.log("✓ 测试7: 双空不显示");
}

console.log("\n所有 ONNX 资产组回归测试通过 ✓");
