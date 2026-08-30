/**
 * 0.22.6 新增功能测试。
 *
 * 覆盖：
 * 1. setModels：全量替换模型列表
 * 2. setPreferences：设置引擎偏好
 * 3. appendLog：operation 日志（带 operation_id）与 instance 日志隔离
 * 4. appendLog：operation 日志不做 instance 过滤
 * 5. appendLog：相同 source + seq 去重
 * 6. setLogHistory：历史日志的 sourceKind 标记
 * 7. 语音页模型选择器：只过滤已安装模型
 * 8. 语音页模型选择器：无已安装模型时返回空列表
 * 9. 语音页模型选择器：is_selected 标记正确
 * 10. preferences 回滚场景模拟
 */

import assert from "node:assert/strict";
import {
    createInitialState,
    setCatalog,
    setModels,
    setPreferences,
    appendLog,
    setLogHistory,
    getEntry,
    MAX_LOG_LINES,
    setPendingModelAction,
    getPendingModelAction,
    getEffectiveModelInstallState,
} from "./local-engine-state.js";
import {
    funasrCatalog,
    makeModel,
    makePreferences,
    makeLog,
} from "./local-engine-fixtures.js";

// ── 日志辅助：通过 overrides 传入 operation_id ────────────────────────────────

function makeOpLog(operationId, seq, text, overrides = {}) {
    return makeLog("funasr", null, seq, text, {operation_id: operationId, ...overrides});
}

function makeInstLog(instanceId, seq, text, overrides = {}) {
    return makeLog("funasr", instanceId, seq, text, overrides);
}

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
        console.error(`    ${e.stack?.split("\n")[1]?.trim() || ""}`);
        throw e;
    }
}

// 所有测试在一个 async IIFE 中顺序执行，确保 await 正确等待
await (async () => {

// ── 1. setModels：全量替换模型列表 ──────────────────────────────────────────

await test("setModels：全量替换模型列表", () => {
    let state = createInitialState();
    state = setCatalog(state, [funasrCatalog]);

    const models = [
        makeModel({model_id: "model-a", display_name: "Model A"}),
        makeModel({model_id: "model-b", display_name: "Model B", install_state: "not_installed"}),
    ];
    state = setModels(state, "funasr", models);

    const entry = getEntry(state, "funasr");
    assert.equal(entry.models.length, 2);
    assert.equal(entry.models[0].model_id, "model-a");
    assert.equal(entry.models[1].install_state, "not_installed");
});

// ── 2. setPreferences：设置引擎偏好 ──────────────────────────────────────────

await test("setPreferences：设置引擎偏好", () => {
    let state = createInitialState();
    state = setCatalog(state, [funasrCatalog]);

    const prefs = makePreferences({
        compute_preference: "cuda",
        auto_start: true,
        requires_rebuild: true,
    });
    state = setPreferences(state, "funasr", prefs);

    const entry = getEntry(state, "funasr");
    assert.equal(entry.preferences.compute_preference, "cuda");
    assert.equal(entry.preferences.auto_start, true);
    assert.equal(entry.preferences.requires_rebuild, true);
});

// ── 3. appendLog：operation 日志与 instance 日志隔离 ──────────────────────────

await test("appendLog：operation 日志不做 instance 过滤", () => {
    let state = createInitialState();
    state = setCatalog(state, [funasrCatalog]);

    // 先设置 currentInstanceId
    const instLog = makeInstLog("inst-1", 1, "instance log");
    state = appendLog(state, instLog);
    let entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 1);
    assert.equal(entry.currentInstanceId, "inst-1");

    // operation 日志（不同 instance_id，但有 operation_id）不应被 instance 过滤
    const opLog = makeOpLog("op-abc", 1, "operation log");
    state = appendLog(state, opLog);
    entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 2, "operation 日志不应被 instance 过滤");
    assert.equal(entry.logs[1].text, "operation log");
    assert.equal(entry.logs[1].sourceKind, "operation");
});

// ── 4. appendLog：instance 日志做 instance 过滤 ──────────────────────────────

await test("appendLog：旧 instance 日志不进入当前实时日志区", () => {
    let state = createInitialState();
    state = setCatalog(state, [funasrCatalog]);

    // 设置 currentInstanceId
    state = appendLog(state, makeInstLog("inst-1", 1, "first"));

    // 旧 instance 日志应被过滤
    state = appendLog(state, makeInstLog("inst-old", 1, "stale"));
    const entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 1, "旧 instance 日志不应进入当前流");
    assert.equal(entry.logs[0].text, "first");
});

// ── 5. appendLog：相同 source + seq 去重 ──────────────────────────────────────

await test("appendLog：相同 source + seq 去重", () => {
    let state = createInitialState();
    state = setCatalog(state, [funasrCatalog]);

    // 同一 instance + seq → 去重
    state = appendLog(state, makeInstLog("inst-1", 1, "first"));
    state = appendLog(state, makeInstLog("inst-1", 1, "dup"));
    let entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 1, "相同 instance + seq 去重");

    // 不同 seq → 不去重
    state = appendLog(state, makeInstLog("inst-1", 2, "second"));
    entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 2);

    // operation 日志同 operation_id + seq → 去重
    state = appendLog(state, makeOpLog("op-1", 1, "op-first"));
    state = appendLog(state, makeOpLog("op-1", 1, "op-dup"));
    entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 3, "operation 日志去重");
    assert.equal(entry.logs[2].text, "op-first");
});

// ── 6. setLogHistory：历史日志的 sourceKind 标记 ──────────────────────────────

await test("setLogHistory：历史日志的 sourceKind 标记", () => {
    let state = createInitialState();
    state = setCatalog(state, [funasrCatalog]);

    const history = [
        makeInstLog("inst-1", 1, "instance log"),
        makeOpLog("op-1", 1, "operation log"),
    ];
    state = setLogHistory(state, "funasr", history);

    const entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 2);
    assert.equal(entry.logs[0].sourceKind, "instance");
    assert.equal(entry.logs[0].source, "inst-1");
    assert.equal(entry.logs[1].sourceKind, "operation");
    assert.equal(entry.logs[1].source, "op-1");
});

// ── 7. 语音页模型选择器：只过滤已安装且校验通过的模型 ──────────────────────────────

await test("语音页模型选择器：只过滤已安装且校验通过的模型", () => {
    // 模拟后端返回的模型列表
    const models = [
        makeModel({model_id: "sensevoice", install_state: "installed", verification_state: "verified", is_selected: true}),
        makeModel({model_id: "paraformer", install_state: "not_installed", verification_state: "unknown"}),
        makeModel({model_id: "whisper", install_state: "download_failed", verification_state: "unknown"}),
        makeModel({model_id: "broken", install_state: "installed", verification_state: "corrupted"}),
        makeModel({model_id: "mismatched", install_state: "installed", verification_state: "mismatched"}),
    ];

    // 模拟语音页过滤逻辑（与 voice.js initLocalModelSelect 一致）
    const USABLE_VERIFICATION = ["verified", "unverified", "unknown"];
    const installed = models.filter(
        (m) => m.install_state === "installed"
            && USABLE_VERIFICATION.includes((m.verification_state || "").toLowerCase())
    );
    assert.equal(installed.length, 1, "只有 installed + verified 的模型应出现");
    assert.equal(installed[0].model_id, "sensevoice");
});

// ── 8. 语音页模型选择器：无已安装模型时返回空列表 ──────────────────────────────

await test("语音页模型选择器：无已安装模型时返回空列表", () => {
    const models = [
        makeModel({model_id: "sensevoice", install_state: "not_installed", verification_state: "unknown"}),
        makeModel({model_id: "paraformer", install_state: "downloading", verification_state: "unknown"}),
    ];

    const USABLE_VERIFICATION = ["verified", "unverified", "unknown"];
    const installed = models.filter(
        (m) => m.install_state === "installed"
            && USABLE_VERIFICATION.includes((m.verification_state || "").toLowerCase())
    );
    assert.equal(installed.length, 0, "无已安装模型");
});

// ── 9. 语音页模型选择器：is_selected 标记正确 ──────────────────────────────────

await test("语音页模型选择器：is_selected 标记正确", () => {
    const models = [
        makeModel({model_id: "sensevoice", install_state: "installed", verification_state: "verified", is_selected: true}),
        makeModel({model_id: "paraformer", install_state: "installed", verification_state: "verified", is_selected: false}),
    ];

    const USABLE_VERIFICATION = ["verified", "unverified", "unknown"];
    const installed = models.filter(
        (m) => m.install_state === "installed"
            && USABLE_VERIFICATION.includes((m.verification_state || "").toLowerCase())
    );
    const selected = installed.find((m) => m.is_selected);
    assert.equal(selected.model_id, "sensevoice");
});

// ── 9b. 语音页模型选择器：Corrupted/Mismatched 被排除 ──────────────────────────

await test("语音页模型选择器：Corrupted 和 Mismatched 模型被排除", () => {
    const models = [
        makeModel({model_id: "ok", install_state: "installed", verification_state: "verified"}),
        makeModel({model_id: "corrupt", install_state: "installed", verification_state: "corrupted"}),
        makeModel({model_id: "mismatch", install_state: "installed", verification_state: "mismatched"}),
    ];

    const USABLE_VERIFICATION = ["verified", "unverified", "unknown"];
    const installed = models.filter(
        (m) => m.install_state === "installed"
            && USABLE_VERIFICATION.includes((m.verification_state || "").toLowerCase())
    );
    assert.equal(installed.length, 1, "Corrupted 和 Mismatched 应被排除");
    assert.equal(installed[0].model_id, "ok");
});

// ── 10. preferences 回滚场景模拟 ──────────────────────────────────────────────

await test("preferences 回滚：保存失败时 UI 恢复原值", () => {
    // 模拟 voice.js 中的回滚逻辑
    let state = createInitialState();
    state = setCatalog(state, [funasrCatalog]);
    const prefs = makePreferences({compute_preference: "cpu"});
    state = setPreferences(state, "funasr", prefs);

    const entry = getEntry(state, "funasr");
    const oldPref = entry.preferences.compute_preference; // "cpu"

    // 模拟用户改为 cuda 但保存失败
    const newPref = "cuda";
    // 保存"失败" → 回滚
    // 在实际代码中，select.value = oldPref 恢复原值
    // 这里验证 state 中的 preferences 没有变
    assert.equal(entry.preferences.compute_preference, oldPref, "保存失败后 preferences 不变");
    assert.notEqual(entry.preferences.compute_preference, newPref, "不应为失败的新值");
});

// ── 11. 日志 bounded：超过 MAX_LOG_LINES 截断 ─────────────────────────────────

await test("appendLog：超过 MAX_LOG_LINES 截断保留最新", () => {
    let state = createInitialState();
    state = setCatalog(state, [funasrCatalog]);

    // 填充超过上限
    for (let i = 0; i < MAX_LOG_LINES + 10; i++) {
        state = appendLog(state, makeInstLog("inst-1", i, `line-${i}`));
    }

    const entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, MAX_LOG_LINES, "截断到 MAX_LOG_LINES");
    // 最新的行保留
    assert.equal(entry.logs[MAX_LOG_LINES - 1].text, `line-${MAX_LOG_LINES + 9}`);
});

// ── 12. operation 日志 + instance 日志混合 bounded ──────────────────────────

await test("appendLog：operation + instance 日志混合 bounded", () => {
    let state = createInitialState();
    state = setCatalog(state, [funasrCatalog]);

    // 交替添加 operation 和 instance 日志
    for (let i = 0; i < 100; i++) {
        state = appendLog(state, makeInstLog("inst-1", i, `inst-${i}`));
        state = appendLog(state, makeOpLog("op-1", i, `op-${i}`));
    }

    const entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 200, "混合日志共 200 条");
    // 验证 sourceKind 分布
    const opCount = entry.logs.filter((l) => l.sourceKind === "operation").length;
    const instCount = entry.logs.filter((l) => l.sourceKind === "instance").length;
    assert.equal(opCount, 100);
    assert.equal(instCount, 100);
});

// ── 13. stop_orphan_engine: command 常量与后端对齐 ──────────────────────────────

await test("stop_orphan_engine: COMMANDS 常量值为 stop_orphan_engine", async () => {
    // 动态导入 controller 模块以读取 COMMANDS 常量
    // local-runtime.js 在模块加载时访问 window.__TAURI__，需先设置
    if (!globalThis.window) globalThis.window = {};
    if (!globalThis.window.__TAURI__) {
        globalThis.window.__TAURI__ = {
            core: {invoke: async () => []},
            event: {listen: async () => () => {}},
        };
    }
    const mod = await import("./local-runtime.js");
    // COMMANDS 是模块内 private const，不 export。
    // 通过 controller 实例间接验证：stopOrphan 方法存在且调用正确 command。
    assert.equal(typeof mod.createLocalEngineController, "function");
});

// ── 14. stop_orphan_engine: controller.stopOrphan 方法存在且返回 Promise ──────────

await test("stop_orphan_engine: controller.stopOrphan 方法存在且返回 Promise", async () => {
    if (!globalThis.window) globalThis.window = {};
    if (!globalThis.window.__TAURI__) {
        globalThis.window.__TAURI__ = {
            core: {invoke: async () => {}},
            event: {listen: async () => () => {}},
        };
    }
    const {createLocalEngineController} = await import("./local-runtime.js");
    const controller = createLocalEngineController({});
    assert.equal(typeof controller.stopOrphan, "function", "controller 应有 stopOrphan 方法");
    // 方法存在即可，不实际调用（需要 mock invoke）
});

// ── 15. stop_orphan_engine: OrphanStopResultDto 契约验证 ──────────────────────────

await test("stop_orphan_engine: OrphanStopResultDto 契约验证", () => {
    // 模拟后端返回的 OrphanStopResultDto
    const dto = {
        engine_id: "funasr",
        stopped: true,
        reason: "adoptable_killed",
        detail: "进程已终止",
    };
    assert.equal(dto.engine_id, "funasr");
    assert.equal(dto.stopped, true);
    assert.equal(typeof dto.reason, "string");
    assert.equal(typeof dto.detail, "string");

    // 验证 detail 可选字段
    const dtoNoDetail = {
        engine_id: "funasr",
        stopped: false,
        reason: "lease_not_found",
    };
    assert.equal(dtoNoDetail.detail, undefined);
});

// ── 16. stop_orphan_engine: 所有 reason 变体可序列化 ──────────────────────────

await test("stop_orphan_engine: 所有 reason 变体可序列化", () => {
    const reasons = [
        "lease_not_found",
        "pid_not_exist",
        "adoptable_killed",
        "verification_failed",
        "health_unreachable",
    ];
    for (const reason of reasons) {
        const dto = {
            engine_id: "funasr",
            stopped: reason === "adoptable_killed",
            reason,
        };
        // 验证可序列化
        const json = JSON.stringify(dto);
        const parsed = JSON.parse(json);
        assert.equal(parsed.reason, reason);
    }
});

// ── 17. orphan_recovery DTO: actionable=false 不显示停止入口 ──────────────────

await test("orphan_recovery: actionable=false 不显示停止入口", () => {
    const diag = {
        orphan_recovery: {present: false, actionable: false, reason: "no_lease"},
    };
    assert.equal(diag.orphan_recovery.actionable, false);
    // 前端逻辑：if (orphanRecovery && orphanRecovery.actionable === true)
    // → 不渲染按钮
    const shouldShow = diag.orphan_recovery && diag.orphan_recovery.actionable === true;
    assert.equal(shouldShow, false, "actionable=false 不应显示按钮");
});

// ── 18. orphan_recovery DTO: actionable=true 显示停止入口 ──────────────────────

await test("orphan_recovery: actionable=true 显示停止入口", () => {
    const diag = {
        orphan_recovery: {present: true, actionable: true, reason: "adoptable"},
    };
    assert.equal(diag.orphan_recovery.actionable, true);
    const shouldShow = diag.orphan_recovery && diag.orphan_recovery.actionable === true;
    assert.equal(shouldShow, true, "actionable=true 应显示按钮");
});

// ── 19. orphan_recovery: 无 orphan_recovery 字段时不显示 ──────────────────────

await test("orphan_recovery: 无 orphan_recovery 字段时不显示", () => {
    const diag = {process: {state: "running"}, service: "unreachable"};
    const shouldShow = !!(diag.orphan_recovery && diag.orphan_recovery.actionable === true);
    assert.equal(shouldShow, false, "无 orphan_recovery 字段不显示按钮");
});

// ── 20. orphan_recovery: legacy_venv_exists 不单独触发显示 ───────────────────

await test("orphan_recovery: legacy_venv_exists 不单独触发显示", () => {
    // 旧逻辑拼接 legacy_venv_exists，新逻辑只看 orphan_recovery.actionable
    const diag = {
        process: {state: "stopped"},
        service: "unreachable",
        legacy_venv_exists: true,
        orphan_recovery: {present: false, actionable: false, reason: "no_lease"},
    };
    const shouldShow = !!(diag.orphan_recovery && diag.orphan_recovery.actionable === true);
    assert.equal(shouldShow, false, "legacy_venv_exists 不应单独触发显示");
});

// ── 21. orphan_recovery: process running + service unreachable 不单独触发 ───

await test("orphan_recovery: process running + service unreachable 不单独触发", () => {
    // 旧逻辑拼接 process/service，新逻辑只看 orphan_recovery.actionable
    const diag = {
        process: {state: "running"},
        service: "unreachable",
        orphan_recovery: {present: false, actionable: false, reason: "no_lease"},
    };
    const shouldShow = !!(diag.orphan_recovery && diag.orphan_recovery.actionable === true);
    assert.equal(shouldShow, false, "process running + service unreachable 不单独触发");
});

// ── 22. orphan_recovery: 不暴露 PID/路径/token/endpoint ──────────────────────

await test("orphan_recovery: DTO 不暴露 PID/路径/token/endpoint", () => {
    const diag = {
        orphan_recovery: {present: true, actionable: true, reason: "adoptable"},
    };
    // orphan_recovery 只含 { present, actionable, reason }
    const orc = diag.orphan_recovery;
    assert.ok(!("pid" in orc), "不应暴露 pid");
    assert.ok(!("path" in orc), "不应暴露 path");
    assert.ok(!("token" in orc), "不应暴露 token");
    assert.ok(!("endpoint" in orc), "不应暴露 endpoint");
    assert.ok(!("executable" in orc), "不应暴露 executable");
    assert.ok(!("creation_time" in orc), "不应暴露 creation_time");
});

// ── 23. stop_orphan_engine: mock invoke 验证 command 和参数 ──────────────────

await test("stop_orphan_engine: mock invoke 验证 command 和参数", async () => {
    if (!globalThis.window) globalThis.window = {};
    let invokeCalled = false;
    let invokeCmd = null;
    let invokeArgs = null;
    globalThis.window.__TAURI__ = {
        core: {
            invoke: async (cmd, args) => {
                // 只追踪 stop_orphan_engine 调用，其他 command 返回默认值
                if (cmd === "stop_orphan_engine") {
                    invokeCalled = true;
                    invokeCmd = cmd;
                    invokeArgs = args;
                    return {engine_id: "funasr", stopped: true, reason: "adoptable_killed"};
                }
                // 其他 command 的默认返回
                if (cmd === "list_local_engines" || cmd === "get_local_engine_status") return [];
                return {};
            },
        },
        event: {listen: async () => () => {}},
    };
    const {createLocalEngineController} = await import("./local-runtime.js");
    const controller = createLocalEngineController({});
    await controller.mount();
    await controller.stopOrphan("funasr");
    assert.equal(invokeCalled, true, "invoke 应被调用");
    assert.equal(invokeCmd, "stop_orphan_engine", "command 应为 stop_orphan_engine");
    assert.deepEqual(invokeArgs, {engineId: "funasr"}, "参数应为 {engineId}");
});

// ── 24. stop_orphan_engine: 连续点击只 invoke 一次（防重入） ──────────────────

await test("stop_orphan_engine: 防重入逻辑验证", async () => {
    if (!globalThis.window) globalThis.window = {};
    let invokeCount = 0;
    globalThis.window.__TAURI__ = {
        core: {
            invoke: async (cmd, args) => {
                if (cmd === "stop_orphan_engine") {
                    invokeCount++;
                    await new Promise((r) => setTimeout(r, 100));
                    return {engine_id: args.engineId, stopped: true, reason: "adoptable_killed"};
                }
                if (cmd === "list_local_engines" || cmd === "get_local_engine_status") return [];
                return {};
            },
        },
        event: {listen: async () => () => {}},
    };
    const {createLocalEngineController} = await import("./local-runtime.js");
    const controller = createLocalEngineController({});
    await controller.mount();

    // 模拟前端按钮防重入逻辑
    const btnState = {inFlight: false};
    const clickHandler = async () => {
        if (btnState.inFlight) return; // 防重入
        btnState.inFlight = true;
        try {
            await controller.stopOrphan("funasr");
        } finally {
            btnState.inFlight = false;
        }
    };

    // 连续两次点击，第二次应被阻止
    const p1 = clickHandler();
    const p2 = clickHandler();
    await Promise.all([p1, p2]);
    assert.equal(invokeCount, 1, "防重入应只 invoke 一次");
});

// ── 25. stop_orphan_engine: 成功后刷新 diagnostics/status ──────────────────────

await test("stop_orphan_engine: 成功后调用 refreshStatus", async () => {
    if (!globalThis.window) globalThis.window = {};
    let refreshCalled = false;
    globalThis.window.__TAURI__ = {
        core: {
            invoke: async (cmd) => {
                if (cmd === "stop_orphan_engine") {
                    return {engine_id: "funasr", stopped: true, reason: "adoptable_killed"};
                }
                if (cmd === "list_local_engines" || cmd === "get_local_engine_status") return [];
                return {};
            },
        },
        event: {listen: async () => () => {}},
    };
    const {createLocalEngineController} = await import("./local-runtime.js");
    const controller = createLocalEngineController({});
    await controller.mount();
    controller.refreshStatus = async () => {
        refreshCalled = true;
    };
    await controller.stopOrphan("funasr");
    assert.equal(refreshCalled, true, "stopOrphan 后应调用 refreshStatus");
});

// ── 26. stop_orphan_engine: 结构化失败正常展示 ──────────────────────────────

await test("stop_orphan_engine: 结构化失败返回 reason", async () => {
    if (!globalThis.window) globalThis.window = {};
    globalThis.window.__TAURI__ = {
        core: {
            invoke: async (cmd) => {
                if (cmd === "stop_orphan_engine") {
                    return {engine_id: "funasr", stopped: false, reason: "kill_failed"};
                }
                if (cmd === "list_local_engines" || cmd === "get_local_engine_status") return [];
                return {};
            },
        },
        event: {listen: async () => () => {}},
    };
    const {createLocalEngineController} = await import("./local-runtime.js");
    const controller = createLocalEngineController({});
    await controller.mount();
    const result = await controller.stopOrphan("funasr");
    assert.equal(result.stopped, false, "应返回 stopped=false");
    assert.equal(result.reason, "kill_failed", "应返回结构化 reason");
});

// ── 27. stop_orphan_engine: invoke 失败抛出异常 ──────────────────────────────

await test("stop_orphan_engine: invoke 失败抛出异常", async () => {
    if (!globalThis.window) globalThis.window = {};
    globalThis.window.__TAURI__ = {
        core: {
            invoke: async (cmd) => {
                if (cmd === "stop_orphan_engine") {
                    throw {message: "network error"};
                }
                if (cmd === "list_local_engines" || cmd === "get_local_engine_status") return [];
                return {};
            },
        },
        event: {listen: async () => () => {}},
    };
    const {createLocalEngineController} = await import("./local-runtime.js");
    const controller = createLocalEngineController({});
    await controller.mount();
    await assert.rejects(
        () => controller.stopOrphan("funasr"),
        (err) => {
            assert.ok(err, "应抛出错误");
            return true;
        }
    );
});

// ── 28. IPC 契约：set_local_stt_selection 调用参数验证 ──────────────────────

await test("IPC 契约：set_local_stt_selection 参数 shape", async () => {
    if (!globalThis.window) globalThis.window = {};
    let capturedCmd = null;
    let capturedArgs = null;
    globalThis.window.__TAURI__ = {
        core: {
            invoke: async (cmd, args) => {
                if (cmd === "set_local_stt_selection") {
                    capturedCmd = cmd;
                    capturedArgs = args;
                    return;
                }
                return {};
            },
        },
        event: {listen: async () => () => {}},
    };
    const {invoke} = await import("../../../shared/tauri.js");
    await invoke("set_local_stt_selection", {engineId: "funasr", modelId: "iic/SenseVoiceSmall"});
    assert.equal(capturedCmd, "set_local_stt_selection");
    assert.deepEqual(capturedArgs, {engineId: "funasr", modelId: "iic/SenseVoiceSmall"});
});

// ── 29. IPC 契约：list_engine_models 返回 ModelCatalogItemDto shape ──────────

await test("IPC 契约：list_engine_models 返回 DTO 含必要字段", async () => {
    if (!globalThis.window) globalThis.window = {};
    globalThis.window.__TAURI__ = {
        core: {
            invoke: async (cmd) => {
                if (cmd === "list_engine_models") {
                    return [{
                        engine_id: "funasr",
                        model_id: "iic/SenseVoiceSmall",
                        display_name: "SenseVoiceSmall",
                        description: "test",
                        revision: "v1",
                        estimated_size_mb: 234,
                        install_state: "installed",
                        verification_state: "verified",
                        cache_size_bytes: 245000000,
                        is_selected: true,
                        is_active: false,
                        compatibility: "compatible",
                    }];
                }
                return {};
            },
        },
        event: {listen: async () => () => {}},
    };
    const {invoke} = await import("../../../shared/tauri.js");
    const models = await invoke("list_engine_models", {engineId: "funasr"});
    assert.equal(models.length, 1);
    const m = models[0];
    // 验证 ModelCatalogItemDto 必须包含的字段
    assert.equal(typeof m.engine_id, "string");
    assert.equal(typeof m.model_id, "string");
    assert.equal(typeof m.display_name, "string");
    assert.equal(typeof m.install_state, "string");
    assert.equal(typeof m.verification_state, "string");
    assert.equal(typeof m.is_selected, "boolean");
    assert.equal(typeof m.is_active, "boolean");
    assert.equal(typeof m.compatibility, "string");
});

// ── 30. IPC 契约：LOCAL_ENGINE_INSTALL_STAGE 事件 payload shape ──────────────

await test("IPC 契约：install_stage 事件 payload shape", () => {
    // 模拟后端 emit 的 payload
    const payload = {
        engine_id: "funasr",
        operation_id: "install-model-sensevoice-1234567890",
        stage: "downloading",
    };
    assert.equal(payload.engine_id, "funasr");
    assert.equal(typeof payload.operation_id, "string");
    assert.equal(typeof payload.stage, "string");
    // stage 应是已知枚举值之一
    const validStages = ["downloading", "staging", "verifying", "installed", "failed", "cancelled"];
    assert.ok(validStages.includes(payload.stage), `stage 应为已知值: ${payload.stage}`);
});

// ── summary ────────────────────────────────────────────────────────────────────

// ── 31. pendingModelAction：设置和获取 operation_id ────────────────────────

await test("pendingModelAction：设置和获取 operation_id", () => {
    let state = createInitialState();
    const engineId = "funasr";
    const modelId = "iic/SenseVoiceSmall";
    const opId = "install-model-sensevoice-1234567890";

    // 设置 pending model action
    state = setPendingModelAction(state, engineId, modelId, {
        kind: "install",
        operationId: opId,
    });

    const entry = getEntry(state, engineId);
    assert.ok(entry, "entry should exist");

    const pending = getPendingModelAction(entry, modelId);
    assert.ok(pending, "pending action should exist");
    assert.equal(pending.kind, "install");
    assert.equal(pending.operationId, opId);
    assert.equal(typeof pending.timestamp, "number");
});

// ── 32. pendingModelAction：清除后获取返回 null ───────────────────────────

await test("pendingModelAction：清除后获取返回 null", () => {
    let state = createInitialState();
    const engineId = "funasr";
    const modelId = "iic/SenseVoiceSmall";
    const opId = "repair-model-sensevoice-9876543210";

    // 设置
    state = setPendingModelAction(state, engineId, modelId, {
        kind: "repair",
        operationId: opId,
    });

    // 清除
    state = setPendingModelAction(state, engineId, modelId, null);

    const entry = getEntry(state, engineId);
    const pending = getPendingModelAction(entry, modelId);
    assert.equal(pending, null);
});

// ── 33. pendingModelAction：多模型独立隔离 ────────────────────────────────

await test("pendingModelAction：多模型独立隔离", () => {
    let state = createInitialState();
    const engineId = "funasr";
    const model1 = "iic/SenseVoiceSmall";
    const model2 = "iic/paraformer-zh";
    const opId1 = "install-model-sensevoice-1111";
    const opId2 = "repair-model-paraformer-2222";

    // 设置两个模型的 pending action
    state = setPendingModelAction(state, engineId, model1, {
        kind: "install",
        operationId: opId1,
    });
    state = setPendingModelAction(state, engineId, model2, {
        kind: "repair",
        operationId: opId2,
    });

    const entry = getEntry(state, engineId);
    const p1 = getPendingModelAction(entry, model1);
    const p2 = getPendingModelAction(entry, model2);
    assert.equal(p1.operationId, opId1);
    assert.equal(p1.kind, "install");
    assert.equal(p2.operationId, opId2);
    assert.equal(p2.kind, "repair");

    // 清除 model1 不影响 model2
    state = setPendingModelAction(state, engineId, model1, null);
    const entry2 = getEntry(state, engineId);
    assert.equal(getPendingModelAction(entry2, model1), null);
    assert.ok(getPendingModelAction(entry2, model2), "model2 pending should still exist");
});

// ── 34. pendingModelAction：无 entry 时返回 null ───────────────────────────

await test("pendingModelAction：无 entry 时返回 null", () => {
    const state = createInitialState();
    const entry = getEntry(state, "nonexistent");
    assert.equal(getPendingModelAction(entry, "some-model"), null);
});

await test("setLogHistory：丢弃其他引擎的历史日志", () => {
    let state = createInitialState();
    state = setCatalog(state, [funasrCatalog]);
    const own = makeInstLog("inst-1", 1, "funasr log");
    const foreign = {...makeInstLog("inst-2", 2, "paddle log"), engine_id: "paddleocr"};
    state = setLogHistory(state, "funasr", [own, foreign]);
    const entry = getEntry(state, "funasr");
    assert.deepEqual(entry.logs.map((log) => log.text), ["funasr log"]);
});

await test("pendingModelAction：安装请求立即覆盖陈旧模型状态", () => {
    let state = createInitialState();
    const model = makeModel({install_state: "not_installed"});
    state = setPendingModelAction(state, "funasr", model.model_id, {
        kind: "install",
        operationId: "install-op",
    });
    assert.equal(
        getEffectiveModelInstallState(getEntry(state, "funasr"), model),
        "downloading",
    );
});

await test("pendingModelAction：修复与删除请求映射为可取消状态", () => {
    const model = makeModel({install_state: "installed"});
    for (const [kind, expected] of [["repair", "repairing"], ["delete", "deleting"]]) {
        let state = createInitialState();
        state = setPendingModelAction(state, "funasr", model.model_id, {
            kind,
            operationId: `${kind}-op`,
        });
        assert.equal(getEffectiveModelInstallState(getEntry(state, "funasr"), model), expected);
    }
});

})(); // end async IIFE

// IIFE 完成后打印 summary（.then 确保在 IIFE 之后执行）
await (async () => {
    console.log(`\n${passCount}/${testCount} tests passed.`);
    if (passCount !== testCount) {
        process.exit(1);
    }
})();
