/**
 * 0.22.9 Handoff 09 测试——设置 UI 与只读 Runtime 诊断。
 *
 * 覆盖（handoff 验收约定）：
 * 1. selection 状态机：switching / rolled_back / rollback_failed / active=None
 * 2. 竞态防护：requestId 陈旧结果不覆盖、状态事件不清除 switching、
 *    reconcile 单一方向收敛、失败态不被无关快照淹没
 * 3. 卡片投影：feedback / summary / primaryAction 在切换事务各阶段说真话
 * 4. GGUF↔ONNX 业务特征矩阵：真伪流式 / 多语言 / 资源占用 / 中文质量
 * 5. DOM：模型行渲染（特征行 / 切换徽章 / 按钮禁用）+ selected≠active 徽章
 * 6. active implementation 只读诊断映射（GGUF / ONNX / 未运行）
 */

import assert from "node:assert/strict";

// ── Tauri mock（须在任何业务模块 import 前设置——tauri.js 在 import 期读 window）──
globalThis.window = {
    __TAURI__: {
        core: {invoke: async () => ({})},
        event: {listen: async () => () => {}},
    },
};
globalThis.CSS = {escape: (s) => String(s)};

// 最小 DOM shim（Node 无 DOM；renderer 只用到 createElement/appendChild 等子集）
class ShimElement {
    constructor(tag) {
        this.tagName = tag.toUpperCase();
        this.nodeType = 1;
        this.className = "";
        this.hidden = false;
        this._children = [];
        this._parent = null;
        this._text = "";
        this._listeners = {};
        const self = this;
        this.dataset = new Proxy({}, {
            set(obj, prop, val) {
                obj[prop] = String(val);
                return true;
            },
            get(obj, prop) {
                return obj[prop];
            },
        });
    }

    appendChild(child) {
        if (child && child.nodeType === 11 && child._children) {
            for (const c of [...child._children]) this.appendChild(c);
            return child;
        }
        if (child._parent) {
            const siblings = child._parent._children;
            const i = siblings.indexOf(child);
            if (i >= 0) siblings.splice(i, 1);
        }
        child._parent = this;
        this._children.push(child);
        return child;
    }

    setAttribute(k, v) {
        if (k === "class") this.className = v;
        this[`_${k}`] = v;
    }

    getAttribute(k) {
        return this[`_${k}`] ?? null;
    }

    addEventListener(type, fn) {
        (this._listeners[type] ||= []).push(fn);
    }

    get textContent() {
        if (this._children.length === 0) return this._text;
        return this._children.map((c) => c.textContent).join("");
    }

    set textContent(v) {
        this._children = [];
        this._text = String(v);
    }

    get disabled() {
        return this._disabled === true;
    }

    set disabled(v) {
        this._disabled = v === true;
    }

    /** 测试辅助：递归收集子树。 */
    _all() {
        const out = [this];
        for (const c of this._children) out.push(...c._all());
        return out;
    }

    _cls(name) {
        return this._all().filter((el) => el.className?.split?.(" ").includes(name));
    }
}

globalThis.document = {
    createElement: (tag) => new ShimElement(tag),
    createElementNS: (_ns, tag) => new ShimElement(tag),
    createDocumentFragment: () => {
        const frag = new ShimElement("#fragment");
        frag.nodeType = 11;
        return frag;
    },
};

// ── 业务模块（mock 就绪后动态 import）─────────────────────────────────────────
const {
    beginModelSwitch,
    resolveModelSwitch,
    reconcileSelection,
    clearSelection,
    createSelectionRequestId,
    getSelection,
    isSwitching,
    hasSelectionFailure,
} = await import("./local-engine-selection.js");
const {
    createInitialState,
    setCatalog,
    setModels,
    mergeStatus,
} = await import("./local-engine-state.js");
const {
    computeEngineSummary,
    computeFeedback,
    computeSelectionFeedback,
    primaryActionView,
} = await import("./local-engine-summary.js");
const {
    modelRowSignature,
    modelTraitChips,
    engineBusinessNote,
    updateModelList,
} = await import("./local-engine-models.js");
const {
    funasrCatalog,
    makeStatus,
} = await import("./local-engine-fixtures.js");
const {activeImplementationLabel} = await import("./local-engine-hooks.js");

// ── 测试框架 ─────────────────────────────────────────────────────────────────

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
        console.error(`    ${e.stack?.split("\n")[1]?.trim() || ""}`);
        throw e;
    }
}

// ── fixtures ─────────────────────────────────────────────────────────────────

/** 模型目录 fixture：3 GGUF + 1 ParaformerOnline（wire shape 与 DTO 一致）。 */
function makeFunasrModels(overrides = {}) {
    const sensevoice = {
        engine_id: "funasr",
        model_id: "gguf/sensevoice-small-q8",
        display_name: "SenseVoice Small (GGUF Q8)",
        description: "五语种 ASR",
        revision: "gguf-v0.2.6",
        estimated_size_mb: 242,
        install_state: "installed",
        verification_state: "verified",
        cache_size_bytes: 254208320,
        is_selected: true,
        is_active: true,
        compatibility: "compatible",
        stt_capabilities: {
            languages: ["zh", "en", "ja", "ko", "yue"],
            pseudo_streaming: {supported: "yes"},
            true_streaming: {supported: "no", reason: "stt.capability.streaming.no_incremental_encoder"},
            timestamps: {supported: "no", reason: "stt.capability.timestamps.not_exposed"},
        },
        business: {chinese_quality: "corpus_baseline", resource_footprint: "shared_gguf_worker"},
    };
    const nano = {
        ...sensevoice,
        model_id: "gguf/fun-asr-nano-q4km",
        display_name: "Fun-ASR-Nano (Q4_K_M)",
        is_selected: false,
        is_active: false,
        stt_capabilities: {
            languages: ["zh"],
            pseudo_streaming: {supported: "yes"},
            true_streaming: {supported: "no", reason: "stt.capability.streaming.kv_cleared_per_request"},
            timestamps: {supported: "no", reason: "stt.capability.timestamps.not_exposed"},
        },
    };
    const paraformerZh = {
        ...sensevoice,
        model_id: "gguf/paraformer-zh-q8",
        display_name: "Paraformer-zh (GGUF Q8)",
        install_state: "not_installed",
        is_selected: false,
        is_active: false,
        estimated_size_mb: 285,
        stt_capabilities: {
            languages: ["zh"],
            pseudo_streaming: {supported: "yes"},
            true_streaming: {supported: "no", reason: "stt.capability.streaming.kv_cleared_per_request"},
            timestamps: {supported: "no", reason: "stt.capability.timestamps.not_exposed"},
        },
        business: {chinese_quality: "corpus_baseline", resource_footprint: "shared_gguf_worker"},
    };
    const models = [sensevoice, nano, paraformerZh];
    if (overrides.models) return overrides.models;
    return models;
}

/** 带模型列表的初始 state。 */
function makeState({models = null, status = null} = {}) {
    let state = createInitialState();
    state = setCatalog(state, [structuredClone(funasrCatalog)]);
    if (models) state = setModels(state, "funasr", models);
    if (status) state = mergeStatus(state, status);
    return state;
}

// ── 1. selection 状态机 ──────────────────────────────────────────────────────

await test("beginModelSwitch：进入 switching，记录 target 与 requestId", () => {
    let state = makeState();
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    const selection = getSelection(state.get("funasr"));
    assert.ok(selection, "应有 selection");
    assert.equal(selection.phase, "switching");
    assert.equal(selection.targetModelId, "gguf/fun-asr-nano-q4km");
    assert.equal(selection.requestId, "switch-1");
    assert.ok(isSwitching(state.get("funasr")));
    assert.ok(!hasSelectionFailure(state.get("funasr")));
});

await test("resolveModelSwitch：匹配 requestId + ok → selection 清除", () => {
    let state = makeState();
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    state = resolveModelSwitch(state, "funasr", "switch-1", {ok: true});
    assert.equal(getSelection(state.get("funasr")), null, "成功后 selection 应清除");
    assert.ok(!isSwitching(state.get("funasr")));
});

await test("竞态：旧 requestId 的迟到结果被丢弃，不覆盖新状态", () => {
    let state = makeState();
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-2"});
    // 旧请求 switch-1 的迟到 resolve（ok）
    state = resolveModelSwitch(state, "funasr", "switch-1", {ok: true});
    assert.equal(getSelection(state.get("funasr"))?.phase, "switching",
        "迟到结果不得清除进行中的切换");
    // 旧请求的迟到失败同样被丢弃
    state = resolveModelSwitch(state, "funasr", "switch-1",
        {ok: false, errorCode: "switch_rollback_failed", error: {message: "old"}});
    assert.equal(getSelection(state.get("funasr"))?.phase, "switching");
});

await test("竞态：新 begin 覆盖旧失败态（用户重试）", () => {
    let state = makeState();
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    state = resolveModelSwitch(state, "funasr", "switch-1",
        {ok: false, errorCode: "switch_rolled_back", error: {message: "旧失败"}});
    assert.ok(hasSelectionFailure(state.get("funasr")));
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-2"});
    const selection = getSelection(state.get("funasr"));
    assert.equal(selection.phase, "switching");
    assert.equal(selection.targetModelId, "gguf/fun-asr-nano-q4km");
    assert.equal(selection.error, null, "新 begin 应清除旧错误");
});

await test("rollback：switch_rolled_back → rolled_back（恢复成功，错误保留）", () => {
    let state = makeState();
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    state = resolveModelSwitch(state, "funasr", "switch-1", {
        ok: false,
        errorCode: "switch_rolled_back",
        error: {code: "switch_rolled_back", message: "模型 gguf/fun-asr-nano-q4km 启动失败，已恢复原模型"},
    });
    const selection = getSelection(state.get("funasr"));
    assert.equal(selection.phase, "rolled_back");
    assert.equal(selection.error.message, "模型 gguf/fun-asr-nano-q4km 启动失败，已恢复原模型");
    assert.ok(hasSelectionFailure(state.get("funasr")));
    assert.ok(!isSwitching(state.get("funasr")));
});

await test("rollback 双重失败：switch_rollback_failed → rollback_failed", () => {
    let state = makeState();
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    state = resolveModelSwitch(state, "funasr", "switch-1", {
        ok: false,
        errorCode: "switch_rollback_failed",
        error: {
            code: "switch_rollback_failed",
            message: "目标失败 AND 恢复失败（服务已停止）",
        },
        detail: {engine_id: "funasr", target_model_id: "gguf/fun-asr-nano-q4km"},
    });
    const selection = getSelection(state.get("funasr"));
    assert.equal(selection.phase, "rollback_failed");
    assert.ok(selection.detail, "双失败应保留 detail");
});

await test("target failure（未达回滚）：其他错误码 → selection 清除，走 transientError", () => {
    let state = makeState();
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    state = resolveModelSwitch(state, "funasr", "switch-1", {
        ok: false,
        errorCode: "model_not_ready",
        error: {code: "model_not_ready", message: "目标模型未就绪"},
    });
    assert.equal(getSelection(state.get("funasr")), null,
        "Target 验证失败零状态变更，不需要 selection 状态");
});

await test("reconcile：selected===active===target 一致快照收敛 switching（单一方向）", () => {
    let state = makeState();
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});

    // 中间态：仅 selected 已提交（active 未变）→ 不收敛
    const midModels = makeFunasrModels().map((m) =>
        m.model_id === "gguf/fun-asr-nano-q4km" ? {...m, install_state: "installed", is_selected: true} : {...m, is_selected: false});
    state = setModels(state, "funasr", midModels);
    state = reconcileSelection(state, "funasr");
    assert.equal(getSelection(state.get("funasr"))?.phase, "switching",
        "stop/commit/start 中间态不得收敛");

    // 一致快照：selected+active 都指向 target → 收敛
    const doneModels = midModels.map((m) =>
        m.model_id === "gguf/fun-asr-nano-q4km" ? {...m, is_active: true} : {...m, is_active: false});
    state = setModels(state, "funasr", doneModels);
    state = reconcileSelection(state, "funasr");
    assert.equal(getSelection(state.get("funasr")), null, "一致快照应收敛 switching");
});

await test("reconcile：失败态不由 reconcile 清除（不能猜测回滚是否完成）", () => {
    let state = makeState();
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    state = resolveModelSwitch(state, "funasr", "switch-1",
        {ok: false, errorCode: "switch_rollback_failed", error: {message: "双失败"}});
    // 即使 models 给出一致快照（双失败后 selected 恢复旧值、active=None，不会出现；
    // 但 reconcile 必须对失败态一律不作为）
    const models = makeFunasrModels();
    state = setModels(state, "funasr", models);
    state = reconcileSelection(state, "funasr");
    assert.equal(getSelection(state.get("funasr"))?.phase, "rollback_failed");
});

await test("requestId 严格单调（快速连点不撞号）", () => {
    const a = createSelectionRequestId();
    const b = createSelectionRequestId();
    const c = createSelectionRequestId();
    assert.notEqual(a, b);
    assert.notEqual(b, c);
});

// ── 2. 卡片投影 ──────────────────────────────────────────────────────────────

await test("computeSelectionFeedback：switching 显示目标模型名（busy）", () => {
    let state = makeState({models: makeFunasrModels()});
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    const entry = state.get("funasr");
    const feedback = computeSelectionFeedback(entry, null);
    assert.ok(feedback);
    assert.equal(feedback.tone, "busy");
    assert.ok(feedback.text.includes("Fun-ASR-Nano"), feedback.text);
});

await test("computeFeedback：switching 优先于事务中间态快照（不说谎）", () => {
    let state = makeState({models: makeFunasrModels()});
    // 事务中间帧：引擎已停止（stop 完成、start 未开始）——快照单独看像"已就绪"
    state = mergeStatus(state, makeStatus({
        revision: "2",
        status: {
            desired: "stopped",
            environment: "ready",
            process: {state: "stopped"},
            service: "unknown",
            model: "not_loaded",
            available: false,
        },
    }));
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    const entry = state.get("funasr");
    const feedback = computeFeedback(entry, null);
    assert.equal(feedback.tone, "busy", "中间态快照不得显示'已就绪'");
    assert.ok(feedback.text.includes("切换"), feedback.text);
});

await test("computeFeedback：rolled_back / rollback_failed 文案（错误可见）", () => {
    let state = makeState({models: makeFunasrModels()});
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    state = resolveModelSwitch(state, "funasr", "switch-1", {
        ok: false, errorCode: "switch_rolled_back",
        error: {code: "switch_rolled_back", message: "目标启动失败，已恢复原模型。原因: timeout"},
    });
    let feedback = computeFeedback(state.get("funasr"), null);
    assert.equal(feedback.tone, "error");
    assert.ok(feedback.text.includes("已恢复原模型"), feedback.text);
    assert.ok(feedback.detail.includes("timeout"));

    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-2"});
    state = resolveModelSwitch(state, "funasr", "switch-2", {
        ok: false, errorCode: "switch_rollback_failed",
        error: {code: "switch_rollback_failed", message: "目标失败: x; 恢复失败: y"},
    });
    feedback = computeFeedback(state.get("funasr"), null);
    assert.equal(feedback.tone, "error");
    assert.ok(feedback.text.includes("恢复原模型也失败"), feedback.text);
});

await test("computeEngineSummary：switching 覆盖可用快照", () => {
    let state = makeState({models: makeFunasrModels()});
    state = mergeStatus(state, makeStatus({
        revision: "3",
        status: {
            desired: "running",
            environment: "ready",
            process: {state: "running", pid: 100},
            service: "healthy",
            model: "ready",
            available: true,
        },
    }));
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    const summary = computeEngineSummary(state.get("funasr"), null);
    assert.equal(summary.tone, "busy", "切换中不得显示'运行中'");
    assert.ok(summary.text.includes("切换"), summary.text);
});

await test("primaryActionView：switching → 禁用等待态（不提供 start/stop 竞争按钮）", () => {
    let state = makeState({models: makeFunasrModels()});
    state = mergeStatus(state, makeStatus({
        revision: "2",
        status: {
            desired: "running",
            environment: "ready",
            process: {state: "running", pid: 100},
            service: "healthy",
            model: "ready",
            available: true,
        },
    }));
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-1"});
    const view = primaryActionView(state.get("funasr"), null);
    assert.equal(view.kind, null);
    assert.equal(view.disabled, true);
    assert.ok(view.label.includes("切换"), view.label);
});

await test("selected≠active（selection 清除后）：mismatch 提示不受 selection 残留影响", () => {
    let state = makeState();
    // selected=sensevoice（安装），active=nano（运行实例冻结）
    const models = makeFunasrModels().map((m) => {
        if (m.model_id === "gguf/sensevoice-small-q8") return {...m, is_selected: true, is_active: false};
        if (m.model_id === "gguf/fun-asr-nano-q4km") return {...m, is_selected: false, is_active: true};
        return m;
    });
    state = setModels(state, "funasr", models);
    // 走一次空切换再成功清除，确认无残留
    state = beginModelSwitch(state, "funasr", {modelId: "gguf/fun-asr-nano-q4km", requestId: "switch-x"});
    state = resolveModelSwitch(state, "funasr", "switch-x", {ok: true});
    const feedback = computeFeedback(state.get("funasr"), null);
    assert.equal(feedback.tone, "warn");
    assert.ok(feedback.text.includes("不一致"), feedback.text);
});

// ── 3. GGUF↔ONNX 业务特征矩阵 ───────────────────────────────────────────────

await test("业务特征：GGUF 模型 = 伪流式（真流式 chip 不渲染；资源/质量收敛到引擎级说明）", () => {
    const models = makeFunasrModels();
    const gguf = models[0];
    const zhOnly = models[2];

    const ggufChips = modelTraitChips(gguf);
    assert.ok(ggufChips.some((c) => c.kind === "pseudo_streaming"), "GGUF 应展示伪流式");
    assert.ok(!ggufChips.some((c) => c.kind === "true_streaming"), "GGUF 不应展示真流式");
    // 0.22.10：resource/quality 在候选间一致 → 引擎级说明行，不逐行重复
    assert.ok(!ggufChips.some((c) => c.kind === "resource"), "资源画像不再逐行展示");
    assert.ok(!ggufChips.some((c) => c.kind === "quality"), "中文质量不再逐行展示");
    // 五语种
    const langChip = ggufChips.find((c) => c.kind === "languages");
    assert.ok(langChip.text.includes("zh/en/ja/ko/yue"), langChip.text);

    // handoff-11：真流式 ONNX 线路退役——描述符驱动下无模型命中 true_streaming
    const zhChips = modelTraitChips(zhOnly);
    assert.ok(zhChips.some((c) => c.kind === "pseudo_streaming"), "单语种 GGUF 仍伪流式");
    assert.ok(!zhChips.some((c) => c.kind === "true_streaming"), "无真流式 chip");
    // 单语种
    const zhLang = zhChips.find((c) => c.kind === "languages");
    assert.equal(zhLang.text, "中文", "单 zh 应显示'中文'");
});

await test("engineBusinessNote：候选间一致 → 一行说明；不一致/缺省 → null", () => {
    const models = makeFunasrModels();
    const note = engineBusinessNote(models);
    assert.ok(note, "三候选 business 一致应产出说明行");
    assert.ok(note.includes("共享 GGUF worker"), note);
    assert.ok(note.includes("中文质量"), note);

    // 候选间不一致（未来多 runtime 并存）→ 退回逐行特征展示
    const mixed = models.map((m, i) =>
        i === 0 ? {...m, business: {...m.business, resource_footprint: "dedicated_onnx_worker"}} : m);
    assert.equal(engineBusinessNote(mixed), null, "不一致不得合并成一行");

    // 全部缺省 → 无说明
    assert.equal(engineBusinessNote(models.map((m) => ({...m, business: null}))), null);
});

await test("业务特征：能力未声明（languages 空 / business 缺省）→ 不展示该维度，不猜", () => {
    const chips = modelTraitChips({
        stt_capabilities: {languages: []},
        business: null,
    });
    assert.deepEqual(chips, []);
});

await test("签名：selection phase 变化触发模型行重渲染（按钮禁用态刷新）", () => {
    const model = makeFunasrModels()[2];
    const idle = modelRowSignature(model, "installed", "");
    const switching = modelRowSignature(model, "installed", "switching");
    const failed = modelRowSignature(model, "installed", "rollback_failed");
    assert.notEqual(idle, switching);
    assert.notEqual(switching, failed);
    // 0.22.10：行内消费 business 的只剩「推荐」徽章——recommended 变化参与签名
    const withRecommended = {...model, business: {...model.business, recommended: true}};
    assert.notEqual(idle, modelRowSignature(withRecommended, "installed", ""));
    // 资源/质量画像收敛到引擎级说明行——business 其他字段不再影响行签名
    const withoutBiz = {...model, business: null};
    assert.equal(idle, modelRowSignature(withoutBiz, "installed", ""));
});

// ── 4. active implementation 只读诊断 ────────────────────────────────────────

await test("activeImplementationLabel：GGUF / ONNX / 未运行 映射（只读）", () => {
    const mkEntry = (impl) => ({
        status: {status: impl ? {active_implementation: impl} : {}},
    });
    assert.ok(activeImplementationLabel(mkEntry("funasr_gguf_worker")).includes("GGUF"));
    assert.ok(activeImplementationLabel(mkEntry("paddleocr_onnx_in_process")).includes("ONNX"));
    assert.ok(activeImplementationLabel(mkEntry(null)).includes("未运行"));
    assert.ok(activeImplementationLabel(mkEntry(undefined)).includes("未运行"),
        "active=None（字段缺省）= 未运行");
    // 未知 wire 值 fail-closed：显示原值，不猜
    assert.equal(activeImplementationLabel(mkEntry("future_impl")), "future_impl");
});

await test("active=None 不说谎：running 快照缺 active_implementation → 诊断仍显示未运行", () => {
    // 防御性：即使进程 running，没有 active_implementation 字段就不声称有实现
    const entry = {status: {status: {process: {state: "running", pid: 1}}}};
    assert.ok(activeImplementationLabel(entry).includes("未运行"));
});

// ── 5. DOM 渲染（shim 见文件头部）──────────────────────────────────────────

/**
 * 渲染模型列表并返回便于断言的结构。
 * @param {Object[]} models
 * @param {Object} entryExtra - 追加到 entry 的字段（selection 等）
 */
function renderList(models, entryExtra = {}) {
    const container = new ShimElement("div");
    const entry = {catalog: structuredClone(funasrCatalog), models, ...entryExtra};
    updateModelList(container, entry, null, null);
    return container;
}

/** 找到模型行的名称单元格（第一列）。 */
function nameCellOf(container, modelId) {
    const rows = container._cls("le-model-row");
    const row = rows.find((r) => r.dataset.modelId === modelId);
    return row?._cls("le-model-name")[0] || null;
}

await test("DOM：候选行渲染差异特征行，共享画像收敛到引擎级说明行", () => {
    const container = renderList(makeFunasrModels());
    const traitsLines = container._cls("le-model-traits");
    assert.equal(traitsLines.length, 3, "三个已声明特征的模型各一行特征行");

    const ggufTraits = nameCellOf(container, "gguf/sensevoice-small-q8")
        ?._cls("le-model-traits")[0].textContent;
    assert.ok(ggufTraits.includes("多语种"), ggufTraits);
    assert.ok(ggufTraits.includes("伪流式"), ggufTraits);
    // 0.22.10：共享 worker / 中文质量在候选间一致 → 列表顶部说明行只说一次
    assert.ok(!ggufTraits.includes("共享 GGUF worker"), ggufTraits);
    assert.ok(!ggufTraits.includes("中文质量"), ggufTraits);

    const notes = container._cls("le-model-business-note");
    assert.equal(notes.length, 1, "引擎级说明行只渲染一次");
    assert.ok(notes[0].textContent.includes("共享 GGUF worker"), notes[0].textContent);
    assert.ok(notes[0].textContent.includes("中文质量"), notes[0].textContent);

    // handoff-11：真流式 ONNX 退役——所有 GGUF 行都不渲染真流式/独立 worker
    const zhTraits = nameCellOf(container, "gguf/paraformer-zh-q8")
        ?._cls("le-model-traits")[0].textContent;
    assert.ok(zhTraits.includes("伪流式"), zhTraits);
    assert.ok(!zhTraits.includes("真流式"), zhTraits);
    assert.ok(!zhTraits.includes("独立 ONNX worker"), zhTraits);
});

await test("DOM：switching 在途 → 使用按钮禁用 + 目标行'切换中'徽章", () => {
    const container = renderList(makeFunasrModels(), {
        selection: {
            phase: "switching",
            targetModelId: "gguf/fun-asr-nano-q4km",
            requestId: "switch-1",
        },
    });
    // 所有"使用"按钮禁用
    const useButtons = container._cls("le-model-btn-primary");
    assert.ok(useButtons.length > 0);
    for (const btn of useButtons) {
        assert.equal(btn.disabled, true, "切换在途时写操作按钮必须禁用");
    }
    // 目标行有切换中徽章
    const badges = nameCellOf(container, "gguf/fun-asr-nano-q4km")
        ?._cls("le-model-badge-switching") || [];
    assert.equal(badges.length, 1, "目标行应有切换中徽章");
    assert.equal(badges[0].textContent, "切换中");
    // 非目标行不得有切换中徽章（否则会被误读为多模型并发切换）
    for (const otherId of ["gguf/sensevoice-small-q8", "gguf/paraformer-zh-q8"]) {
        const otherBadges = nameCellOf(container, otherId)?._cls("le-model-badge-switching") || [];
        assert.equal(otherBadges.length, 0, `非目标行 ${otherId} 不应有切换中徽章`);
    }
});

await test("DOM：selection 清除后按钮恢复可用、徽章消失", () => {
    const container = renderList(makeFunasrModels(), {});
    for (const btn of container._cls("le-model-btn-primary")) {
        assert.equal(btn.disabled, false);
    }
    assert.equal(container._cls("le-model-badge-switching").length, 0);
});

await test("DOM：selected≠active 徽章共存（配置 vs 实际加载）", () => {
    const models = makeFunasrModels().map((m) => {
        if (m.model_id === "gguf/sensevoice-small-q8") return {...m, is_selected: true, is_active: false};
        if (m.model_id === "gguf/fun-asr-nano-q4km") return {...m, is_selected: false, is_active: true};
        return m;
    });
    const container = renderList(models);
    // sensevoice 行：有"配置"徽章（le-badge-cap）
    const senseName = nameCellOf(container, "gguf/sensevoice-small-q8");
    assert.ok(senseName._cls("le-badge-cap").length >= 1, "selected 行应有配置徽章");
    // nano 行：有"实际加载"徽章（le-badge-lifecycle）
    const nanoName = nameCellOf(container, "gguf/fun-asr-nano-q4km");
    assert.ok(nanoName._cls("le-badge-lifecycle").length >= 1, "active 行应有实际加载徽章");
});

await test("DOM：脏检查——相同输入不重建，phase 变化才重建", () => {
    const container = new ShimElement("div");
    const entry = {catalog: structuredClone(funasrCatalog), models: makeFunasrModels()};
    updateModelList(container, entry, null, null);
    const firstPass = container._children.length;
    updateModelList(container, entry, null, null);
    assert.equal(container._children.length, firstPass, "签名未变 → 跳过重建");
    assert.ok(container.dataset.renderSig);

    // selection 变化 → 签名变化 → 重建
    const switchingEntry = {
        ...entry,
        selection: {phase: "switching", targetModelId: "gguf/fun-asr-nano-q4km", requestId: "s"},
    };
    updateModelList(container, switchingEntry, null, null);
    const useButtons = container._cls("le-model-btn-primary");
    assert.ok(useButtons.every((b) => b.disabled === true), "重渲染后按钮为禁用态");
});

// ── 汇总 ─────────────────────────────────────────────────────────────────────

console.log(`\n${passCount}/${testCount} tests passed.`);
if (passCount !== testCount) {
    process.exit(1);
}
console.log("local-engine-0229 tests passed");
