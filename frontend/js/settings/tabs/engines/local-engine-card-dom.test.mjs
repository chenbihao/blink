/**
 * 引擎卡片 DOM 装配测试（中密度纵向重设计，最小 DOM shim）。
 *
 * Node 环境无 DOM——用支持本模块所需特性的最小元素 shim（createElement /
 * appendChild / textContent / dataset / attribute / querySelector / closest /
 * 事件）验证 renderer 行为：
 * 1. 两个引擎共用同一 renderer（结构一致，无 engine_id 分支复制的 DOM）
 * 2. 唯一主操作按钮随状态正确切换
 * 3. 维护/模型列表/日志默认折叠，aria-expanded 同步
 * 4. 新日志事件不重建 tools/primary DOM（hover/aria 不丢）
 * 5. backend mismatch / operation stage 默认可见
 * 6. 模型区折叠不丢 selected/active 摘要
 * 7. 单 compute profile → 静态文本；配置保存走受限 command
 */

import assert from "node:assert/strict";

// ── Tauri mock（须在 import 前设置）─────────────────────────────────────────
const invokeCalls = [];
const preferenceCalls = [];
globalThis.window = {
    __TAURI__: {
        core: {
            invoke: async (cmd, args) => {
                invokeCalls.push({cmd, args});
                if (cmd === "get_local_engine_catalog") return [];
                return {};
            },
        },
        event: {listen: async () => () => {}},
    },
};
globalThis.CSS = {escape: (s) => String(s)};

// ── 最小 DOM shim ─────────────────────────────────────────────────────────────

class ShimElement {
    constructor(tag) {
        this.tagName = tag.toUpperCase();
        this.nodeType = 1;
        this.className = "";
        this.id = "";
        this.hidden = false;
        this._children = [];
        this._parent = null;
        this._text = "";
        this._attrs = {};
        this._listeners = {};
        this.value = "";
        this.disabled = false;
        this.checked = false;
        this.scrollHeight = 0;
        this.scrollTop = 0;
        this.clientHeight = 0;
        // dataset ↔ data-* 属性双向同步（真实 DOM 语义）
        const self = this;
        this.dataset = new Proxy({}, {
            set(obj, prop, value) {
                obj[prop] = value;
                self._attrs[`data-${String(prop).replace(/[A-Z]/g, (m) => `-${m.toLowerCase()}`)}`] = String(value);
                return true;
            },
            deleteProperty(obj, prop) {
                delete obj[prop];
                delete self._attrs[`data-${String(prop).replace(/[A-Z]/g, (m) => `-${m.toLowerCase()}`)}`];
                return true;
            },
            get(obj, prop) {
                return obj[prop];
            },
        });
    }

    appendChild(child) {
        if (child && child.nodeType === 11 && child._children) {
            for (const c of child._children) this.appendChild(c);
            return child;
        }
        if (child._parent) child._parent.removeChild(child);
        child._parent = this;
        this._children.push(child);
        return child;
    }

    insertBefore(child, ref) {
        if (!ref) return this.appendChild(child);
        const idx = this._children.indexOf(ref);
        if (idx < 0) return this.appendChild(child);
        if (child._parent) child._parent.removeChild(child);
        child._parent = this;
        this._children.splice(idx, 0, child);
        return child;
    }

    removeChild(child) {
        const idx = this._children.indexOf(child);
        if (idx >= 0) this._children.splice(idx, 1);
        child._parent = null;
        return child;
    }

    remove() {
        if (this._parent) this._parent.removeChild(this);
    }

    get textContent() {
        return this._text + this._children.map((c) => c.textContent).join("");
    }

    set textContent(value) {
        this._children = [];
        this._text = String(value);
    }

    setAttribute(name, value) {
        this._attrs[name] = String(value);
    }

    getAttribute(name) {
        return name in this._attrs ? this._attrs[name] : null;
    }

    hasAttribute(name) {
        return name in this._attrs;
    }

    addEventListener(type, fn) {
        (this._listeners[type] = this._listeners[type] || []).push(fn);
    }

    _fire(type, event = {}) {
        const listeners = this._listeners[type] || [];
        event.target = event.target || this;
        event.type = type;
        for (const fn of listeners) fn(event);
    }

    click() {
        this._fire("click");
    }
}

function matchesSelector(el, selector) {
    if (!el || el.nodeType !== 1) return false;
    const s = selector.trim();
    const attrMatch = s.match(/^\[([^\]=]+)(?:="([^"]*)")?\]$/);
    if (attrMatch) {
        const [, name, value] = attrMatch;
        const v = el.getAttribute(name);
        if (v == null) return false;
        return value === undefined || v === value;
    }
    if (s.startsWith(".")) {
        const classes = s.slice(1).split(".");
        const have = String(el.className || "").split(/\s+/).filter(Boolean);
        return classes.every((c) => have.includes(c));
    }
    return el.tagName === s.toUpperCase();
}

function queryAll(root, selector) {
    const out = [];
    const walk = (el) => {
        if (matchesSelector(el, selector)) out.push(el);
        for (const c of el._children) walk(c);
    };
    for (const c of root._children) walk(c);
    return out;
}

// 挂到元素原型（querySelector / querySelectorAll / closest）
ShimElement.prototype.querySelector = function (selector) {
    return queryAll(this, selector)[0] || null;
};
ShimElement.prototype.querySelectorAll = function (selector) {
    return queryAll(this, selector);
};
ShimElement.prototype.closest = function (selector) {
    let el = this;
    while (el) {
        if (matchesSelector(el, selector)) return el;
        el = el._parent;
    }
    return null;
};

const documentShim = {
    createElement: (tag) => new ShimElement(tag),
    createElementNS: (_ns, tag) => new ShimElement(tag),
    createTextNode: (text) => {
        const el = new ShimElement("#text");
        el.nodeType = 3;
        el._text = String(text);
        return el;
    },
    createDocumentFragment: () => {
        const frag = new ShimElement("#fragment");
        frag.nodeType = 11;
        return frag;
    },
    querySelector: () => null,
    querySelectorAll: () => [],
    body: new ShimElement("body"),
};
globalThis.document = documentShim;

// ── mock 设置后动态导入被测模块 ─────────────────────────────────────────────

const {renderEngineCard} = await import("./local-engine-card.js");
const {registerLocalEngineHooks} = await import("./local-engine-hooks.js");
const {t} = await import("../../../i18n/index.js");
const {
    createInitialState,
    setCatalog,
    setModels,
    setPreferences,
    mergeStatus,
    appendLog,
} = await import("./local-engine-state.js");
const {
    makeCatalog,
    makeStatus,
    makeModel,
    makePreferences,
    makeLog,
    processState,
} = await import("./local-engine-fixtures.js");

registerLocalEngineHooks();

// ── 辅助 ──────────────────────────────────────────────────────────────────────

let testCount = 0;
let passCount = 0;

async function test(name, fn) {
    testCount++;
    try {
        await fn();
        passCount++;
        console.log(`  ✓ ${name}`);
    } catch (e) {
        console.error(`  ✗ ${name}`);
        console.error(`    ${e.message}`);
        throw e;
    }
}

function idleOperation() {
    return {kind: "idle", operation_id: "", stage: "pending", cancellable: false};
}

/** 构造 entry。 */
function makeEntry(engineId, overrides = {}) {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    const {status, models, preferences, logs} = overrides;
    state = mergeStatus(state, makeStatus({
        engine_id: engineId,
        service_epoch: "epoch-1",
        revision: "1",
        ...(status ? {status} : {}),
    }));
    if (models) state = setModels(state, engineId, models);
    if (preferences) state = setPreferences(state, engineId, preferences);
    if (logs) {
        for (const log of logs) state = appendLog(state, log);
    }
    return state.get(engineId);
}

const controllerStub = {
    isMounted: () => false,
    savePreferences: async (engineId, patch) => {
        preferenceCalls.push({engineId, patch});
        return {engine_id: engineId, ...patch};
    },
    openEngineFolder: async () => {},
    install: async () => {},
    start: async () => {},
    stop: async () => {},
    repair: async () => {},
    cancel: async () => {},
    getDiagnostics: async (engineId) => ({
        engine_id: engineId,
        environment: "ready",
        process: {state: "stopped"},
        service: "unknown",
        model: "not_loaded",
        adapter_diagnostics: [
            {key: "gguf_deployment_ready", value: "true", label: "info"},
            {key: "protocol_version", value: "1", label: "info"},
        ],
        recent_logs: [],
        orphan_recovery: {present: false, actionable: false, reason: "no_lease"},
    }),
};

const READY_STOPPED = {
    environment: "ready",
    process: processState.stopped(),
    service: "unknown",
    model: "not_loaded",
    available: false,
};

const RUNNING_READY = {
    environment: "ready",
    process: processState.running(4321),
    service: "healthy",
    model: "ready",
    available: true,
    backend: {
        requested_preference: "cpu",
        backend_verification: {
            state: "verified", expected_backend: "cpu", actual_backend: "cpu",
            device_name: null, mismatch_reason: null,
        },
        fallback_reasons: [],
    },
};

function makeContainer() {
    return documentShim.createElement("div");
}

// ── 1. 两个引擎共用同一 renderer ─────────────────────────────────────────────

await test("两个引擎共用 renderer：结构一致（无 engine_id 分支 DOM）", () => {
    const container = makeContainer();
    const funasr = makeEntry("funasr", {status: READY_STOPPED, preferences: makePreferences()});
    const paddle = makeEntry("paddleocr", {
        status: READY_STOPPED,
        models: [makeModel({engine_id: "paddleocr", model_id: "PP-OCRv6", display_name: "PP-OCRv6", is_selected: true})],
        preferences: makePreferences({engine_id: "paddleocr", compute_preference: "auto", ocr_backend: "windows", lifecycle: "on_demand"}),
    });
    renderEngineCard(container, funasr, controllerStub, undefined);
    renderEngineCard(container, paddle, controllerStub, undefined);

    const cards = queryAll(container, ".le-card");
    assert.equal(cards.length, 2, "两张卡片");

    // 顶层结构类名序列一致（共用 renderer 的结构断言）
    const structureOf = (card) => queryAll(card, "[data-engine-id]").length
        ? card._children.map((c) => c.className).join(">")
        : "";
    const [a, b] = cards;
    assert.equal(structureOf(a), structureOf(b), "两张卡顶层结构一致");

    // 本地引擎卡必须复用设置页通用 extension-card 视觉骨架，避免再次
    // 演化出独立的 header/body 间距、背景和字号体系。
    for (const card of cards) {
        assert.ok(card.className.includes("extension-card"), "复用 extension-card");
        assert.ok(card.querySelector(".le-card-head").className.includes("extension-header"),
            "头部复用 extension-header");
        assert.ok(card.querySelector(".le-card-info").className.includes("extension-info"),
            "信息区复用 extension-info");
        assert.ok(card.querySelector(".le-card-summary").className.includes("extension-desc"),
            "摘要复用 extension-desc");
        assert.ok(card.querySelector(".le-card-body").className.includes("extension-body"),
            "主体复用 extension-body");
    }

    for (const cls of [".le-card-head", ".le-card-summary", ".le-card-primary",
        ".le-keyline", ".le-card-config", ".le-feedback", ".le-card-tools",
        ".le-model-list", ".le-card-log", ".le-maintenance", ".le-diagnostic-inline"]) {
        assert.ok(a.querySelector(cls), `funasr 卡含 ${cls}`);
        assert.ok(b.querySelector(cls), `paddle 卡含 ${cls}`);
    }

    // capability 用户可读名称
    assert.ok(a.textContent.includes("语音识别"), "funasr capability 名称");
    assert.ok(b.textContent.includes("截图文字识别"), "paddle capability 名称");
});

// ── 2. 唯一主操作 ─────────────────────────────────────────────────────────────

await test("头部只有一个主操作按钮，且随状态切换", () => {
    const container = makeContainer();
    const entry = makeEntry("funasr", {
        status: RUNNING_READY,
        models: [makeModel({is_active: true, is_selected: true})],
    });
    renderEngineCard(container, entry, controllerStub, undefined);
    const card = container.querySelector(".le-card");
    const primary = card.querySelector(".le-card-primary");
    assert.equal(queryAll(primary, "button").length, 1, "唯一主操作");
    assert.ok(primary.textContent.includes("停止服务"), primary.textContent);

    // 状态切换为 missing → 重建为 安装环境
    const entryMissing = makeEntry("funasr", {status: {environment: "missing"}});
    renderEngineCard(container, entryMissing, controllerStub, undefined);
    assert.ok(primary.textContent.includes("安装环境"), primary.textContent);
    assert.equal(queryAll(primary, "button").length, 1);
});

// ── 3. 维护默认折叠 ──────────────────────────────────────────────────────────

await test("维护面板默认折叠，展开/收起同步 aria-expanded", () => {
    const container = makeContainer();
    renderEngineCard(container, makeEntry("funasr", {status: READY_STOPPED}), controllerStub, undefined);
    const card = container.querySelector(".le-card");

    const maint = card.querySelector(".le-maintenance");
    const toggle = card.querySelector(".le-maintenance-toggle");
    assert.equal(maint.hidden, true, "默认折叠");
    assert.equal(toggle.getAttribute("aria-expanded"), "false");

    toggle.click();
    assert.equal(maint.hidden, false, "点击展开");
    assert.equal(toggle.getAttribute("aria-expanded"), "true");
    assert.ok(card.textContent.includes("修复环境"), "维护操作在面板内");
    assert.ok(card.textContent.includes("清理引擎缓存"), "清理入口在维护面板");

    toggle.click();
    assert.equal(maint.hidden, true, "再次点击收起");
    assert.equal(toggle.getAttribute("aria-expanded"), "false");
});

await test("模型/日志/维护面板互斥，打开目录位于工具栏右侧", () => {
    const container = makeContainer();
    renderEngineCard(container, makeEntry("funasr", {status: READY_STOPPED}), controllerStub, undefined);
    const card = container.querySelector(".le-card");
    const modelsToggle = card.querySelector(".le-models-toggle");
    const logToggle = card.querySelector(".le-log-toggle");
    const maintToggle = card.querySelector(".le-maintenance-toggle");

    modelsToggle.click();
    assert.equal(card.querySelector(".le-model-list").hidden, false);
    logToggle.click();
    assert.equal(card.querySelector(".le-model-list").hidden, true, "打开日志会收起模型");
    assert.equal(modelsToggle.getAttribute("aria-expanded"), "false");
    assert.equal(card.querySelector(".le-card-log").hidden, false);
    maintToggle.click();
    assert.equal(card.querySelector(".le-card-log").hidden, true, "打开维护会收起日志");
    assert.equal(logToggle.getAttribute("aria-expanded"), "false");
    assert.equal(card.querySelector(".le-maintenance").hidden, false);

    const actions = card.querySelector(".le-card-tools")._children
        .map((child) => child.dataset.actionKind || "spacer");
    assert.deepEqual(actions, ["models", "log", "diagnostics", "maintenance", "spacer", "open-dir"]);
});

await test("诊断头部中重新诊断位于复制诊断左侧", async () => {
    const container = makeContainer();
    renderEngineCard(container, makeEntry("funasr", {status: READY_STOPPED}), controllerStub, undefined);
    const card = container.querySelector(".le-card");
    card.querySelector(".le-diagnostic-toggle").click();
    await Promise.resolve();
    await Promise.resolve();

    const actions = card.querySelector(".le-diagnostic-actions");
    assert.ok(actions, "诊断操作应位于头部操作组");
    assert.ok(actions._children[0].className.includes("le-diagnostic-refresh"));
    assert.ok(actions._children[1].className.includes("le-diagnostic-copy"));
    assert.ok(card.textContent.includes("引擎部署"), "诊断应以检查清单展示部署状态");
    assert.ok(card.textContent.includes("GGUF Worker"), "adapter 专属诊断不得被前端丢弃");
    assert.ok(card.textContent.includes("模型文件"), "诊断应展示模型下载/校验状态");
});

// ── 4. 模型列表默认折叠 + 摘要不丢 ───────────────────────────────────────────

await test("模型列表默认折叠，selected/active 摘要仍在默认卡片可见", () => {
    const container = makeContainer();
    const entry = makeEntry("funasr", {
        status: RUNNING_READY,
        models: [
            makeModel({model_id: "iic/SenseVoiceSmall", display_name: "SenseVoiceSmall", is_selected: true, is_active: false}),
            makeModel({model_id: "iic/paraformer-zh", display_name: "Paraformer-zh", is_selected: false, is_active: true}),
        ],
        preferences: makePreferences(),
    });
    renderEngineCard(container, entry, controllerStub, undefined);
    const card = container.querySelector(".le-card");

    const modelList = card.querySelector(".le-model-list");
    const modelsToggle = card.querySelector(".le-models-toggle");
    assert.equal(modelList.hidden, true, "默认折叠");
    assert.equal(modelsToggle.getAttribute("aria-expanded"), "false");

    // 折叠态下当前模型与待重启提示仍可见（config 区 + 反馈槽）
    assert.ok(card.textContent.includes("当前模型"), "配置组标签可见");
    assert.ok(card.textContent.includes("SenseVoiceSmall"), "当前配置模型可见");
    assert.ok(card.textContent.includes("待重启"), "待重启提示可见");

    // 已安装数量进入按钮文案
    assert.ok(modelsToggle.textContent.includes("管理模型"), modelsToggle.textContent);

    // 展开后可见模型行
    modelsToggle.click();
    assert.equal(modelList.hidden, false);
    assert.ok(modelList.textContent.includes("Paraformer-zh"), "模型行可见");
});

// ── 5. 日志默认折叠 + aria-expanded 在更新中保持 ─────────────────────────────

await test("日志展开/收起保持 aria-expanded，新日志不重建按钮", () => {
    const container = makeContainer();
    const entry = makeEntry("funasr", {
        status: RUNNING_READY,
        models: [makeModel({is_selected: true, is_active: true})],
        logs: [makeLog("funasr", "inst-1", 1, "server started")],
    });
    renderEngineCard(container, entry, controllerStub, undefined);
    const card = container.querySelector(".le-card");

    const logArea = card.querySelector(".le-card-log");
    const logToggle = card.querySelector(".le-log-toggle");
    assert.equal(logArea.hidden, true, "默认折叠");
    assert.equal(logToggle.getAttribute("aria-expanded"), "false");

    // 展开
    logToggle.click();
    assert.equal(logArea.hidden, false);
    assert.equal(logToggle.getAttribute("aria-expanded"), "true");
    assert.ok(logArea.textContent.includes("server started"), "历史日志渲染");

    // 新日志事件 → updateCardContent（renderEngineCard 幂等更新）
    const toolsBefore = card.querySelector(".le-card-tools")._children.map((c) => c);
    const primaryBtnBefore = queryAll(card.querySelector(".le-card-primary"), "button")[0];
    const logLinesBefore = queryAll(logArea, ".le-log-line").length;

    const entry2 = makeEntry("funasr", {
        status: RUNNING_READY,
        models: [makeModel({is_selected: true, is_active: true})],
        logs: [
            makeLog("funasr", "inst-1", 1, "server started"),
            makeLog("funasr", "inst-1", 2, "new log line"),
        ],
    });
    renderEngineCard(container, entry2, controllerStub, undefined);

    // 按钮节点身份不变（不重建 → hover/aria 不丢）
    const toolsAfter = card.querySelector(".le-card-tools")._children.map((c) => c);
    assert.deepEqual(toolsAfter, toolsBefore, "tools 行按钮未被重建");
    const primaryBtnAfter = queryAll(card.querySelector(".le-card-primary"), "button")[0];
    assert.equal(primaryBtnAfter, primaryBtnBefore, "主操作按钮未被重建");
    assert.equal(logToggle.getAttribute("aria-expanded"), "true", "展开态保持");
    assert.equal(queryAll(logArea, ".le-log-line").length, logLinesBefore + 1, "日志行追加");
    assert.ok(logArea.textContent.includes("new log line"));
});

// ── 6. backend mismatch / operation stage 默认可见 ──────────────────────────

await test("backend mismatch 在默认卡片直接可见", () => {
    const container = makeContainer();
    const entry = makeEntry("funasr", {status: {
        ...RUNNING_READY,
        available: false,
        backend: {
            requested_preference: "cpu",
            backend_verification: {
                state: "mismatched", expected_backend: "cpu", actual_backend: "cuda",
                device_name: null, mismatch_reason: "identity mismatch",
            },
        },
    }});
    renderEngineCard(container, entry, controllerStub, undefined);
    const summary = container.querySelector(".le-card-summary");
    assert.ok(summary.textContent.includes("启动失败 · 后端身份不匹配"), summary.textContent);
});

await test("operation stage 在默认卡片直接可见（反馈槽）", () => {
    const container = makeContainer();
    const entry = makeEntry("funasr", {status: {
        operation: {kind: "installing", operation_id: "op-1", stage: "verifying", cancellable: true},
    }});
    renderEngineCard(container, entry, controllerStub, undefined);
    const feedback = container.querySelector(".le-feedback");
    assert.ok(feedback.textContent.includes("校验中"), feedback.textContent);

    // cancellable → 头部出现取消按钮
    const primary = container.querySelector(".le-card-primary");
    assert.ok(primary.textContent.includes("取消"), primary.textContent);
});

await test("last_error 默认可见 + detail 折叠源", () => {
    const container = makeContainer();
    const entry = makeEntry("funasr", {status: {
        ...READY_STOPPED,
        last_error: {code: "start_failed", message: "expected=cpu, actual=cuda", action_hint: null, detail: "traceback...", phase: "start"},
    }});
    renderEngineCard(container, entry, controllerStub, undefined);
    const feedback = container.querySelector(".le-feedback");
    assert.ok(feedback.textContent.includes("expected=cpu"), feedback.textContent);
    const detail = feedback.querySelector(".le-feedback-detail");
    assert.ok(detail, "detail 折叠存在");
    assert.ok(detail.textContent.includes("traceback..."), "detail 内容为 textContent");
});

// ── 7. 配置区：单 profile 静态文本 / 多候选 select / 受限 command ────────────

await test("FunASR 单 compute profile：静态文本，无下拉", () => {
    const container = makeContainer();
    const entry = makeEntry("funasr", {
        status: READY_STOPPED,
        models: [makeModel({is_selected: true})],
        preferences: makePreferences({compute_preference: "cpu"}),
    });
    renderEngineCard(container, entry, controllerStub, undefined);
    const config = container.querySelector(".le-card-config");
    assert.ok(config.textContent.includes("CPU"), config.textContent);
    assert.equal(queryAll(config, "select").length, 0, "无 select（不制造 CUDA 错觉）");
    assert.ok(config.textContent.includes("当前模型"), "当前模型组");
    assert.ok(config.textContent.includes("自动运行"), "自动启动开关组");
});

await test("FunASR 偏好刷新原位同步开关，不替换当前控件节点", () => {
    const container = makeContainer();
    const base = {
        status: READY_STOPPED,
        models: [makeModel({is_selected: true})],
    };
    renderEngineCard(container, makeEntry("funasr", {
        ...base,
        preferences: makePreferences({auto_start: false}),
    }), controllerStub, undefined);
    const config = container.querySelector(".le-card-config");
    const toggleBefore = config.querySelector(".le-switch-input");

    renderEngineCard(container, makeEntry("funasr", {
        ...base,
        preferences: makePreferences({auto_start: true}),
    }), controllerStub, undefined);

    const toggleAfter = config.querySelector(".le-switch-input");
    assert.equal(toggleAfter, toggleBefore, "偏好变化不得替换 checkbox 节点");
    assert.equal(toggleAfter.checked, true, "checkbox 原位同步后端真值");
});

await test("PaddleOCR 配置：OCR 后端/运行策略 select + 计算设备静态文本，保存走受限 command", async () => {
    const container = makeContainer();
    const entry = makeEntry("paddleocr", {
        status: READY_STOPPED,
        models: [makeModel({engine_id: "paddleocr", model_id: "PP-OCRv6", display_name: "PP-OCRv6", is_selected: true})],
        preferences: makePreferences({
            engine_id: "paddleocr",
            compute_preference: "cpu",
            ocr_backend: "windows",
            lifecycle: "on_demand",
        }),
    });
    renderEngineCard(container, entry, controllerStub, undefined);
    const config = container.querySelector(".le-card-config");
    const selects = queryAll(config, "select");
    // 0.22.8: PaddleOCR 只有 CPU 一个 compute option → 静态文本
    // 所以只有 OCR 后端 + 运行策略 两个 select
    assert.equal(selects.length, 2, "OCR 后端 + 运行策略（计算设备为静态文本）");
    // 验证计算设备为静态文本
    const computeStatic = config.querySelector(".le-compute-static");
    assert.ok(computeStatic, "单一 compute option 应渲染静态文本");

    // 修改 OCR 后端 → 走 set_local_engine_preferences（受限命令）
    preferenceCalls.length = 0;
    const backendSelect = selects[0];
    backendSelect.value = "paddleocr";
    backendSelect._fire("change", {target: backendSelect});
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(preferenceCalls.length, 1, "恰好保存一次");
    assert.equal(preferenceCalls[0].engineId, "paddleocr");
    assert.deepEqual(preferenceCalls[0].patch, {ocr_backend: "paddleocr"});
});

// ── 8. 状态更新幂等：同 entry 重复渲染不炸 ─────────────────────────────────

await test("重复渲染幂等：不重复创建卡片", () => {
    const container = makeContainer();
    const entry = makeEntry("funasr", {status: READY_STOPPED});
    renderEngineCard(container, entry, controllerStub, undefined);
    renderEngineCard(container, entry, controllerStub, undefined);
    renderEngineCard(container, entry, controllerStub, undefined);
    assert.equal(queryAll(container, ".le-card").length, 1, "只有一张卡");
});

// ── 汇总 ──────────────────────────────────────────────────────────────────────

console.log(`\n${passCount}/${testCount} tests passed`);
if (passCount !== testCount) {
    process.exit(1);
}
console.log("local-engine-card-dom tests passed");
