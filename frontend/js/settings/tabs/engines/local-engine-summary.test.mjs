/**
 * 卡片紧凑投影纯函数测试（中密度纵向重设计）。
 *
 * 覆盖 spec「五、不同状态必须覆盖」的 16 类状态 + 主操作映射 + 反馈槽优先级 +
 * keyline + 模型三身份摘要 + 运行时顶部摘要（含 foundation 失败）。
 * 全部纯函数测试：无 DOM、无 Tauri mock。
 */

import assert from "node:assert/strict";
import {
    createInitialState,
    setCatalog,
    setModels,
    setPreferences,
    mergeStatus,
    appendLog,
    setStorage,
} from "./local-engine-state.js";
import {
    funasrCatalog,
    paddleocrCatalog,
    makeCatalog,
    makeStatus,
    makeModel,
    makePreferences,
    makeStorage,
    processState,
} from "./local-engine-fixtures.js";
import {
    computeEngineSummary,
    computeFeedback,
    computeKeyline,
    computeModelSummary,
    primaryActionView,
    computeRuntimeSummary,
} from "./local-engine-summary.js";

// i18n/index.js 的 import 链经过 shared/tauri.js（模块级访问 window）——
// 先设 window 再动态导入。
globalThis.window = globalThis.window || {};
const {t} = await import("../../../i18n/index.js");


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

await test("unknown 环境探测中不得显示已就绪", () => {
    const entry = makeEntry("funasr", {
        status: {environment: "unknown", available: false},
    });
    const summary = computeEngineSummary(entry, null);
    const feedback = computeFeedback(entry, null);
    assert.match(summary.text, /检查环境/);
    assert.doesNotMatch(summary.text, /已就绪/);
    assert.match(feedback.text, /确认引擎安装状态/);
    assert.doesNotMatch(feedback.text, /环境已就绪/);
});

/** 构造 entry：catalog + status overrides + models + preferences。 */
function makeEntry(engineId = "funasr", overrides = {}) {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    const {status, models, preferences, storage, logs} = overrides;
    state = mergeStatus(state, makeStatus({
        engine_id: engineId,
        service_epoch: "epoch-1",
        revision: "1",
        ...(status ? {status} : {}),
    }));
    if (models) state = setModels(state, engineId, models);
    if (preferences) state = setPreferences(state, engineId, preferences);
    if (storage) state = setStorage(state, storage);
    if (logs) {
        for (const log of logs) state = appendLog(state, log);
    }
    return state.get(engineId);
}

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
};

// ── 1. FunASR 未安装 ─────────────────────────────────────────────────────────

await test("FunASR 未安装：摘要/主操作/反馈（含预计空间）", () => {
    const entry = makeEntry("funasr", {status: {environment: "missing"}});
    const summary = computeEngineSummary(entry, t);
    assert.equal(summary.text, "未安装 · 需要安装环境");
    assert.equal(summary.tone, "muted");

    const action = primaryActionView(entry, t);
    assert.equal(action.kind, "install");
    assert.equal(action.label, "安装环境");
    assert.equal(action.disabled, false);

    // 预计空间来自 catalog resource_budget（3000 + 234 MB）
    const feedback = computeFeedback(entry, t);
    assert.ok(feedback.text.includes("3.2 GB"), `反馈应含预计空间: ${feedback.text}`);
});

// ── 2. 安装中 stage=verifying ────────────────────────────────────────────────

await test("安装中：摘要与反馈默认可见 operation 阶段", () => {
    const entry = makeEntry("funasr", {status: {
        operation: {kind: "installing", operation_id: "op-1", stage: "verifying", cancellable: true},
    }});
    const summary = computeEngineSummary(entry, t);
    assert.equal(summary.tone, "busy");
    assert.ok(summary.text.includes("安装"), summary.text);
    assert.ok(summary.text.includes("校验中"), summary.text);

    const feedback = computeFeedback(entry, t);
    assert.equal(feedback.tone, "busy");
    assert.ok(feedback.text.includes("校验中"), feedback.text);

    // cancellable → 主操作为取消
    assert.equal(primaryActionView(entry, t).kind, "cancel");
});

// ── 3. 已安装但停止 ──────────────────────────────────────────────────────────

await test("已安装但停止：已就绪 · 模型 · 策略；主操作=启动", () => {
    const entry = makeEntry("funasr", {
        status: READY_STOPPED,
        models: [makeModel({is_selected: true})],
        preferences: makePreferences({auto_start: false}),
    });
    const summary = computeEngineSummary(entry, t);
    assert.equal(summary.tone, "neutral");
    assert.ok(summary.text.startsWith("已就绪"), summary.text);
    assert.ok(summary.text.includes("SenseVoiceSmall"), summary.text);
    assert.ok(summary.text.includes("手动启动"), summary.text);

    const action = primaryActionView(entry, t);
    assert.equal(action.kind, "start");
    assert.equal(action.label, "启动");

    // auto_start=true → 策略显示自动启动
    const entryAuto = makeEntry("funasr", {
        status: READY_STOPPED,
        preferences: makePreferences({auto_start: true}),
    });
    assert.ok(computeEngineSummary(entryAuto, t).text.includes("自动启动"));
});

// ── 4. 正在启动（乐观 pending）──────────────────────────────────────────────

await test("正在启动：主操作为禁用的「启动中」", () => {
    // 4a. 乐观 pending（点击后后端状态未到）
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "e",
        revision: "1",
        status: READY_STOPPED,
    }));
    // 注入 pendingAction（模拟 controller._executeAction 的乐观 pending）
    const base = state.get("funasr");
    const entryPending = {...base, pendingAction: {kind: "start", operationId: "start-1", timestamp: 0}};
    const action = primaryActionView(entryPending, t);
    assert.equal(action.kind, null);
    assert.equal(action.disabled, true);
    assert.ok(action.label.includes("启动"), action.label);

    // 4b. 进程 starting
    const entryStarting = makeEntry("funasr", {status: {
        environment: "ready",
        process: processState.starting(),
    }});
    assert.equal(computeEngineSummary(entryStarting, t).text, "启动中");
    assert.equal(primaryActionView(entryStarting, t).disabled, true);
});

// ── 5. 运行且模型 ready ──────────────────────────────────────────────────────

await test("运行且模型 ready：运行中 · 模型 · 实际设备；主操作=停止服务", () => {
    const entry = makeEntry("funasr", {
        status: {
            ...RUNNING_READY,
            backend: {
                requested_preference: "cpu",
                resolved_profile: {profile_id: "cpu-x64", backend: "cpu"},
                backend_verification: {
                    state: "verified",
                    expected_backend: "cpu",
                    actual_backend: "cpu",
                    device_name: null,
                    mismatch_reason: null,
                },
                fallback_reasons: [],
            },
        },
        models: [makeModel({is_selected: true, is_active: true})],
    });
    const summary = computeEngineSummary(entry, t);
    assert.equal(summary.tone, "ok");
    assert.ok(summary.text.startsWith("运行中"), summary.text);
    assert.ok(summary.text.includes("SenseVoiceSmall"), summary.text);
    assert.ok(summary.text.includes("CPU"), summary.text);

    const action = primaryActionView(entry, t);
    assert.equal(action.kind, "stop");
    assert.equal(action.label, "停止服务");

    // 空闲反馈：手动引擎
    const feedback = computeFeedback(entry, t);
    assert.equal(feedback.tone, "ok");
    assert.ok(feedback.text.includes("服务运行中"), feedback.text);
});

// ── 6. backend mismatch ──────────────────────────────────────────────────────

await test("backend mismatch：默认卡片直接可见", () => {
    const entry = makeEntry("funasr", {status: {
        ...RUNNING_READY,
        available: false,
        backend: {
            requested_preference: "cpu",
            backend_verification: {
                state: "mismatched",
                expected_backend: "cpu",
                actual_backend: "cuda",
                device_name: null,
                mismatch_reason: "identity mismatch",
            },
        },
    }});
    const summary = computeEngineSummary(entry, t);
    assert.equal(summary.tone, "error");
    assert.equal(summary.text, "启动失败 · 后端身份不匹配");
});

// ── 7. PaddleOCR 已安装、按需待机 ────────────────────────────────────────────

await test("PaddleOCR 按需待机：策略与主操作（启动测试）", () => {
    const entry = makeEntry("paddleocr", {
        status: {
            environment: "ready",
            process: processState.stopped(),
            service: "unknown",
            model: "not_loaded",
            available: false,
        },
        models: [makeModel({
            engine_id: "paddleocr",
            model_id: "PP-OCRv6",
            display_name: "PP-OCRv6 Server",
            is_selected: true,
        })],
    });
    const summary = computeEngineSummary(entry, t);
    assert.ok(summary.text.includes("按需启动"), summary.text);
    assert.ok(summary.text.includes("PP-OCRv6 Server"), summary.text);

    const action = primaryActionView(entry, t);
    assert.equal(action.kind, "start");
    assert.equal(action.label, "启动测试");

    // 空闲反馈：按需说明
    const feedback = computeFeedback(entry, t);
    assert.ok(feedback.text.includes("按需启动"), feedback.text);
});

// ── 8. PaddleOCR 活跃 operation（非 idle）──────────────────────────────────

await test("PaddleOCR 活跃 operation：忙碌态摘要", () => {
    const entry = makeEntry("paddleocr", {status: {
        environment: "ready",
        process: processState.running(999),
        service: "healthy",
        model: "ready",
        available: true,
        operation: {kind: "updating", operation_id: "op-x", stage: "switching", cancellable: false},
    }});
    const summary = computeEngineSummary(entry, t);
    assert.equal(summary.tone, "busy");
    assert.ok(summary.text.includes("切换中"), summary.text);
});

// ── 9. 模型未下载 ────────────────────────────────────────────────────────────

await test("模型未下载：installed 计数为 0，反馈仍可行动", () => {
    const entry = makeEntry("funasr", {
        status: READY_STOPPED,
        models: [makeModel({install_state: "not_installed", is_selected: false})],
    });
    const modelSummary = computeModelSummary(entry);
    assert.equal(modelSummary.installedCount, 0);
    assert.equal(modelSummary.totalCount, 1);
    assert.equal(modelSummary.selectedName, null);
});

// ── 10. 模型下载中 ───────────────────────────────────────────────────────────

await test("模型下载中：反馈槽显示模型 + 阶段", () => {
    const entry = makeEntry("funasr", {
        status: READY_STOPPED,
        models: [makeModel({install_state: "downloading"})],
    });
    const feedback = computeFeedback(entry, t);
    assert.equal(feedback.tone, "busy");
    assert.ok(feedback.text.includes("SenseVoiceSmall"), feedback.text);
    assert.ok(feedback.text.includes("下载中"), feedback.text);

    // 校验中
    const entryVerify = makeEntry("funasr", {
        status: READY_STOPPED,
        models: [makeModel({install_state: "verifying"})],
    });
    assert.ok(computeFeedback(entryVerify, t).text.includes("校验中"));
});

// ── 11. selected 与 active 不一致（待重启）─────────────────────────────────

await test("selected ≠ active：反馈槽显示待重启", () => {
    const entry = makeEntry("funasr", {
        status: RUNNING_READY,
        models: [
            makeModel({model_id: "iic/SenseVoiceSmall", display_name: "SenseVoiceSmall", is_selected: true, is_active: false}),
            makeModel({model_id: "iic/paraformer-zh", display_name: "Paraformer-zh", is_selected: false, is_active: true}),
        ],
    });
    const modelSummary = computeModelSummary(entry);
    assert.equal(modelSummary.mismatch, true);
    assert.equal(modelSummary.selectedName, "SenseVoiceSmall");
    assert.equal(modelSummary.activeName, "Paraformer-zh");

    const feedback = computeFeedback(entry, t);
    assert.equal(feedback.tone, "warn");
    assert.ok(feedback.text.includes("待重启"), feedback.text);
});

// ── 12. 环境/模型损坏 ────────────────────────────────────────────────────────

await test("环境损坏：主操作=修复环境", () => {
    const entry = makeEntry("funasr", {status: {environment: "broken"}});
    assert.equal(computeEngineSummary(entry, t).tone, "error");
    const action = primaryActionView(entry, t);
    assert.equal(action.kind, "repair");
    assert.equal(action.label, "修复环境");
});

await test("模型损坏（verification corrupted）：keyline 呈失败色", () => {
    const entry = makeEntry("funasr", {status: {...RUNNING_READY, model: "failed", available: false}});
    const summary = computeEngineSummary(entry, t);
    assert.equal(summary.tone, "error");
    assert.ok(summary.text.includes("模型加载失败"), summary.text);
});

// ── 13. operation cancellable ────────────────────────────────────────────────

await test("cancellable operation：主操作=取消（enabled）", () => {
    const entry = makeEntry("funasr", {status: {
        environment: "ready",
        operation: {kind: "installing", operation_id: "op-9", stage: "downloading", cancellable: true},
    }});
    const action = primaryActionView(entry, t);
    assert.equal(action.kind, "cancel");
    assert.equal(action.disabled, false);
});

// ── 14. operation 不可取消 ───────────────────────────────────────────────────

await test("不可取消 operation：主操作为禁用按钮，无可用取消", () => {
    const entry = makeEntry("funasr", {status: {
        environment: "ready",
        operation: {kind: "installing", operation_id: "op-9", stage: "verifying", cancellable: false},
    }});
    const action = primaryActionView(entry, t);
    assert.equal(action.kind, null);
    assert.equal(action.disabled, true);
});

// ── 15. foundation/storage 摘要加载失败 ──────────────────────────────────────

await test("foundation 加载失败：顶部摘要显示明确反馈", () => {
    const summary = computeRuntimeSummary(new Map(), {foundationError: true}, t);
    assert.equal(summary.tone, "error");
    assert.ok(summary.text.includes("加载失败"), summary.text);
});

// ── 16. 两个引擎均正常 ───────────────────────────────────────────────────────

await test("两引擎正常：顶部运行时摘要（引擎数/运行数/占用）", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "e",
        revision: "1",
        status: RUNNING_READY,
    }));
    state = mergeStatus(state, makeStatus({
        engine_id: "paddleocr",
        service_epoch: "e",
        revision: "1",
        status: READY_STOPPED,
    }));
    state = setStorage(state, makeStorage("funasr", {total_size_bytes: 1.2 * 1024 * 1024 * 1024}));
    state = setStorage(state, makeStorage("paddleocr", {total_size_bytes: 0.64 * 1024 * 1024 * 1024}));

    const summary = computeRuntimeSummary(state, {}, t);
    assert.equal(summary.tone, "ok");
    assert.ok(summary.text.includes("2 个引擎"), summary.text);
    assert.ok(summary.text.includes("1 个运行中"), summary.text);
    assert.ok(summary.text.includes("1.8 GB"), summary.text);
});

await test("运行时摘要：attention 计数（broken/last_error）", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr", service_epoch: "e", revision: "1",
        status: {environment: "broken"},
    }));
    state = mergeStatus(state, makeStatus({
        engine_id: "paddleocr", service_epoch: "e", revision: "1",
        status: READY_STOPPED,
    }));
    const summary = computeRuntimeSummary(state, {}, t);
    assert.equal(summary.tone, "warn");
    assert.ok(summary.text.includes("1 个引擎需要关注"), summary.text);
});

await test("运行时摘要：空状态 loading", () => {
    const summary = computeRuntimeSummary(createInitialState(), {}, t);
    assert.equal(summary.tone, "muted");
});

// ── 补充：服务异常但进程存活必须显式暴露 ────────────────────────────────────

await test("进程存活但服务不可达：不能显示为运行中", () => {
    const entry = makeEntry("funasr", {status: {
        environment: "ready",
        process: processState.running(777),
        service: "unreachable",
        model: "not_loaded",
        available: false,
    }});
    const summary = computeEngineSummary(entry, t);
    assert.equal(summary.tone, "error");
    assert.equal(summary.text, "服务异常 · 进程仍在运行");
});

await test("last_error：反馈槽直接显示错误 + detail 折叠源", () => {
    const entry = makeEntry("funasr", {status: {
        ...READY_STOPPED,
        last_error: {
            code: "start_failed",
            message: "expected=cpu, actual=cuda",
            action_hint: null,
            detail: "traceback...",
            phase: "start",
        },
    }});
    const feedback = computeFeedback(entry, t);
    assert.equal(feedback.tone, "error");
    assert.ok(feedback.text.includes("expected=cpu"), feedback.text);
    assert.equal(feedback.detail, "traceback...");
});

// ── 补充：keyline ────────────────────────────────────────────────────────────

await test("keyline：环境/模型/服务/策略四项 + 状态 class", () => {
    const entry = makeEntry("funasr", {
        status: {
            environment: "ready",
            process: processState.stopped(),
            service: "degraded",
            model: "downloading",
            available: false,
        },
        preferences: makePreferences({auto_start: true}),
    });
    const items = computeKeyline(entry, t);
    assert.equal(items.length, 4);
    assert.equal(items[0].label, "环境");
    assert.equal(items[0].value, "已安装");
    assert.equal(items[0].cls, "status-available");
    assert.equal(items[1].value, "下载中");
    assert.equal(items[1].cls, "status-warning");
    assert.equal(items[2].value, "降级");
    assert.equal(items[2].cls, "status-warning");
    assert.equal(items[3].value, "自动启动");
    assert.equal(items[3].cls, "le-keyline-policy");
});

await test("keyline：无 status 时显示暂无数据", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    const items = computeKeyline(state.get("funasr"), t);
    assert.equal(items.length, 1);
    assert.ok(items[0].label.includes("暂无状态数据"));
});

// ── 汇总 ──────────────────────────────────────────────────────────────────────

console.log(`\n${passCount}/${testCount} tests passed`);
if (passCount !== testCount) {
    process.exit(1);
}
console.log("local-engine-summary tests passed");
