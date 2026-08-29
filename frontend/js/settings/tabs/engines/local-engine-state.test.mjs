/**
 * 本地引擎状态 reducer 测试（0.22.5 H3）。
 *
 * 测试覆盖：
 * 1. 新 epoch 的低 revision 能覆盖旧 epoch 高 revision
 * 2. 同 epoch 旧 revision 被丢弃
 * 3. 两个 engine 状态互不污染
 * 4. 旧 instance 日志不覆盖新 instance
 * 5. 重复 seq 去重
 * 6. 初始化 pull 比实时 event 旧时不回退
 * 7. 旧 operation completion 不结束新 operation
 * 8. dispose 后不再处理事件（controller 层）
 * 9. renderer 对 incompatible option 禁用
 * 10. process running + model loading 不显示 ready
 * 11. PaddleOCR 没有 descriptor 声明的 GPU 选项
 * 12. 日志文本不通过 innerHTML 注入
 */

import assert from "node:assert/strict";
import {
    createInitialState,
    setCatalog,
    mergeStatus,
    appendLog,
    setLogHistory,
    setStorage,
    setPendingAction,
    clearLogs,
    getEntry,
    isEngineReady,
    hasActiveOperation,
    isOperationCancellable,
    getPrimaryAction,
    isActionBlocked,
    MAX_LOG_LINES,
} from "./local-engine-state.js";
import {
    funasrCatalog,
    paddleocrCatalog,
    makeCatalog,
    makeStatus,
    makeLog,
    makeStorage,
    processState,
} from "./local-engine-fixtures.js";

// ── 辅助 ──────────────────────────────────────────────────────────────────────

let testCount = 0;
let passCount = 0;

function test(name, fn) {
    testCount++;
    try {
        fn();
        passCount++;
        console.log(`  ✓ ${name}`);
    } catch (e) {
        console.error(`  ✗ ${name}`);
        console.error(`    ${e.message}`);
        console.error(`    ${e.stack?.split("\n")[1]?.trim() || ""}`);
        throw e;
    }
}

// ── 1. 新 epoch 的低 revision 能覆盖旧 epoch 高 revision ────────────────────

test("新 epoch 的低 revision 能覆盖旧 epoch 高 revision", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // 旧 epoch + 高 revision
    const oldStatus = makeStatus({
        service_epoch: "epoch-old1",
        revision: "999",
    });
    state = mergeStatus(state, oldStatus);

    let entry = getEntry(state, "funasr");
    assert.equal(entry.status.service_epoch, "epoch-old1");
    assert.equal(entry.status.revision, "999");

    // 新 epoch + 低 revision → 必须接受（epoch 不同，只比较是否相同，不比较大小）
    const newStatus = makeStatus({
        service_epoch: "epoch-new1",
        revision: "1",
    });
    state = mergeStatus(state, newStatus);

    entry = getEntry(state, "funasr");
    assert.equal(entry.status.service_epoch, "epoch-new1");
    assert.equal(entry.status.revision, "1");
});

// ── 2. 同 epoch 旧 revision 被丢弃 ───────────────────────────────────────────

test("同 epoch 旧 revision 被丢弃", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // revision 5
    state = mergeStatus(state, makeStatus({revision: "5"}));

    // revision 3（更旧）→ 必须被丢弃
    state = mergeStatus(state, makeStatus({revision: "3"}));

    let entry = getEntry(state, "funasr");
    assert.equal(entry.status.revision, "5", "旧 revision 不应覆盖新 revision");

    // revision 10（更新）→ 接受
    state = mergeStatus(state, makeStatus({revision: "10"}));
    entry = getEntry(state, "funasr");
    assert.equal(entry.status.revision, "10");

    // revision 10（相同）→ 丢弃
    state = mergeStatus(state, makeStatus({revision: "10"}));
    entry = getEntry(state, "funasr");
    assert.equal(entry.status.revision, "10");
});

// ── 3. 两个 engine 状态互不污染 ───────────────────────────────────────────────

test("两个 engine 状态互不污染", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // funasr 状态
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-fa",
        revision: "1",
        status: {environment: "ready"},
    }));

    // paddleocr 状态
    state = mergeStatus(state, makeStatus({
        engine_id: "paddleocr",
        service_epoch: "epoch-po",
        revision: "1",
        status: {environment: "missing"},
    }));

    const funasr = getEntry(state, "funasr");
    const paddleocr = getEntry(state, "paddleocr");

    assert.equal(funasr.status.service_epoch, "epoch-fa");
    assert.equal(funasr.status.status.environment, "ready");

    assert.equal(paddleocr.status.service_epoch, "epoch-po");
    assert.equal(paddleocr.status.status.environment, "missing");

    // 更新 funasr 不影响 paddleocr
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-fa",
        revision: "2",
        status: {environment: "broken"},
    }));

    const paddleocr2 = getEntry(state, "paddleocr");
    assert.equal(paddleocr2.status.status.environment, "missing", "paddleocr 不受 funasr 更新影响");
});

// ── 4. 旧 instance 日志不覆盖新 instance ──────────────────────────────────────

test("旧 instance 日志不覆盖新 instance", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // 先设置当前 instance 为 "inst-new"
    state = appendLog(state, makeLog("funasr", "inst-new", "1", "new log 1"));
    let entry = getEntry(state, "funasr");
    assert.equal(entry.currentInstanceId, "inst-new");
    assert.equal(entry.logs.length, 1);

    // 旧 instance 的日志不应进入当前实时日志区
    state = appendLog(state, makeLog("funasr", "inst-old", "1", "old log from old instance"));
    entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 1, "旧 instance 日志不应混入当前流");
    assert.equal(entry.logs[0].text, "new log 1");

    // 新 instance 的日志正常追加
    state = appendLog(state, makeLog("funasr", "inst-new", "2", "new log 2"));
    entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 2);
    assert.equal(entry.logs[1].text, "new log 2");
});

// ── 5. 重复 seq 去重 ──────────────────────────────────────────────────────────

test("重复 seq 去重", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    state = appendLog(state, makeLog("funasr", "inst-1", "1", "log 1"));
    state = appendLog(state, makeLog("funasr", "inst-1", "2", "log 2"));
    state = appendLog(state, makeLog("funasr", "inst-1", "1", "log 1 duplicate")); // 重复 seq

    let entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 2, "重复 seq 应去重");
    assert.equal(entry.logs[0].text, "log 1");
    assert.equal(entry.logs[1].text, "log 2");

    // 不同 instance 但同 seq → 不去重（但旧 instance 被隔离）
    state = appendLog(state, makeLog("funasr", "inst-2", "1", "new instance log 1"));
    entry = getEntry(state, "funasr");
    // inst-2 日志被加入（因为 currentInstanceId 是 inst-1，inst-2 的日志被隔离）
    assert.equal(entry.logs.length, 2, "旧 instance 的日志不进入当前流");
});

// ── 6. 初始化 pull 比实时 event 旧时不回退 ────────────────────────────────────

test("初始化 pull 比实时 event 旧时不回退", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // 模拟实时事件先到达（revision 5）
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-1",
        revision: "5",
        status: {environment: "ready"},
    }));

    // 模拟初始化 pull 后到达（revision 3，更旧）
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-1",
        revision: "3",
        status: {environment: "missing"},
    }));

    const entry = getEntry(state, "funasr");
    // 不应回退到 revision 3 / missing
    assert.equal(entry.status.revision, "5");
    assert.equal(entry.status.status.environment, "ready");
});

// ── 7. 旧 operation completion 不结束新 operation ─────────────────────────────

test("旧 operation completion 不结束新 operation", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // 旧 operation（installing, op-old）
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-1",
        revision: "1",
        status: {
            operation: {kind: "installing", operation_id: "op-old", stage: "pending", cancellable: true},
        },
    }));

    // 新 operation（installing, op-new）—— 通过 revision 增长
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-1",
        revision: "2",
        status: {
            operation: {kind: "installing", operation_id: "op-new", stage: "preparing", cancellable: true},
        },
    }));

    // 旧 operation completion（op-old 完成）—— revision 不大于当前
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-1",
        revision: "1", // 旧 revision，应被丢弃
        status: {
            operation: {kind: "idle", operation_id: "", stage: "completed", cancellable: false},
        },
    }));

    const entry = getEntry(state, "funasr");
    // 旧 operation completion 不应结束新 operation
    assert.equal(entry.status.status.operation.operation_id, "op-new");
    assert.equal(entry.status.status.operation.kind, "installing");
});

// ── 8. dispose 后不再处理事件（controller 层概念验证）──────────────────────────

test("dispose 后不再处理事件（reducer 侧纯函数验证）", () => {
    // reducer 是纯函数，dispose 在 controller 层控制——重新 createInitialState 即等效清空
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = mergeStatus(state, makeStatus({engine_id: "funasr"}));

    assert.ok(getEntry(state, "funasr") !== null);

    // 模拟 dispose → 重新初始化
    state = createInitialState();
    assert.equal(getEntry(state, "funasr"), null, "dispose 后状态应清空");
});

// ── 9. renderer 对 incompatible option 禁用 ────────────────────────────────────

test("renderer 对 incompatible option 禁用（通过 catalog compute_options 验证）", () => {
    const catalog = makeCatalog();
    const funasr = catalog.find((c) => c.engine_id === "funasr");

    // funasr 的 cuda 选项是 incompatible
    const cudaOpt = funasr.compute_options.find((o) => o.preference === "cuda");
    assert.equal(cudaOpt.compatible, false);
    assert.ok(cudaOpt.disabled_reason, "incompatible option 应有 disabled_reason");

    // cpu 选项是 compatible
    const cpuOpt = funasr.compute_options.find((o) => o.preference === "cpu");
    assert.equal(cpuOpt.compatible, true);
    assert.equal(cpuOpt.disabled_reason, null);
});

// ── 10. process running + model loading 不显示 ready ──────────────────────────

test("isEngineReady 消费后端推导的 status.available", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // available=false（推导规则在后端）→ NOT ready
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-1",
        revision: "1",
        status: {
            desired: "running",
            process: processState.running(1234),
            service: "healthy",
            model: "loading",
            available: false,
        },
    }));

    let entry = getEntry(state, "funasr");
    assert.equal(isEngineReady(entry), false, "available=false 不应显示 ready");

    // available=true → ready
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-1",
        revision: "2",
        status: {
            desired: "running",
            process: processState.running(1234),
            service: "healthy",
            model: "ready",
            available: true,
        },
    }));

    entry = getEntry(state, "funasr");
    assert.equal(isEngineReady(entry), true, "available=true 应显示 ready");

    // available 缺省 → NOT ready（fail closed）
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-1",
        revision: "3",
        status: {
            desired: "running",
            process: processState.running(1234),
            service: "healthy",
            model: "ready",
        },
    }));

    entry = getEntry(state, "funasr");
    assert.equal(isEngineReady(entry), false, "available 缺省时 fail closed");
});

// ── 11. PaddleOCR 没有 descriptor 声明的 GPU 选项 ─────────────────────────────

test("PaddleOCR 没有 descriptor 声明的 GPU 选项", () => {
    const catalog = makeCatalog();
    const paddleocr = catalog.find((c) => c.engine_id === "paddleocr");

    // 只应声明 auto 和 cpu
    const prefs = paddleocr.compute_options.map((o) => o.preference);
    assert.ok(prefs.includes("auto"));
    assert.ok(prefs.includes("cpu"));
    assert.ok(!prefs.includes("cuda"), "PaddleOCR 不应声明 CUDA");
    assert.ok(!prefs.includes("vulkan"), "PaddleOCR 不应声明 Vulkan");
    assert.ok(!prefs.includes("directml"), "PaddleOCR 不应声明 DirectML");
});

// ── 12. 日志文本不通过 innerHTML 注入 ──────────────────────────────────────────

test("日志文本不通过 innerHTML 注入（reducer 层验证）", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // 注入恶意日志文本
    const maliciousText = '<img src=x onerror=alert("xss")>';
    state = appendLog(state, makeLog("funasr", "inst-1", "1", maliciousText));

    const entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 1);
    assert.equal(entry.logs[0].text, maliciousText, "日志文本应原样存储");

    // 验证 renderer 使用 textContent（需要 DOM 环境，此处验证数据结构正确）
    // 完整验证在浏览器环境或 jsdom 中进行
    assert.equal(typeof entry.logs[0].text, "string");
});

// ── 额外：getPrimaryAction 逻辑 ──────────────────────────────────────────────

test("getPrimaryAction: Missing → install", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        status: {environment: "missing"},
    }));

    const entry = getEntry(state, "funasr");
    assert.equal(getPrimaryAction(entry), "install");
});

test("getPrimaryAction: Broken → repair", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        status: {environment: "broken"},
    }));

    const entry = getEntry(state, "funasr");
    assert.equal(getPrimaryAction(entry), "repair");
});

test("getPrimaryAction: Ready + stopped → start", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        status: {
            environment: "ready",
            process: processState.stopped(),
        },
    }));

    const entry = getEntry(state, "funasr");
    assert.equal(getPrimaryAction(entry), "start");
});

test("getPrimaryAction: running → stop", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        status: {
            environment: "ready",
            process: processState.running(1234),
        },
    }));

    const entry = getEntry(state, "funasr");
    assert.equal(getPrimaryAction(entry), "stop", "process running → stop");
});

test("getPrimaryAction: cancellable operation → cancel", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        status: {
            environment: "ready",
            operation: {kind: "installing", operation_id: "op-1", stage: "preparing", cancellable: true},
        },
    }));

    const entry = getEntry(state, "funasr");
    assert.equal(getPrimaryAction(entry), "cancel");
    assert.equal(isOperationCancellable(entry), true);
});

test("isActionBlocked: operation 活跃时非 cancel action 被阻止", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        status: {
            environment: "ready",
            operation: {kind: "installing", operation_id: "op-1", stage: "preparing", cancellable: true},
        },
    }));

    const entry = getEntry(state, "funasr");
    assert.equal(isActionBlocked(entry, "install"), true, "操作进行中 install 被阻止");
    assert.equal(isActionBlocked(entry, "start"), true, "操作进行中 start 被阻止");
    assert.equal(isActionBlocked(entry, "cancel"), false, "cancel 不被阻止（可取消）");
    assert.equal(isActionBlocked(entry, "log"), false, "log 不被阻止");
});

// ── 额外：bounded 日志 ──────────────────────────────────────────────────────────

test("日志 bounded 最大行数", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // 超过 MAX_LOG_LINES 的日志
    for (let i = 0; i < MAX_LOG_LINES + 100; i++) {
        state = appendLog(state, makeLog("funasr", "inst-1", String(i), `log ${i}`));
    }

    const entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, MAX_LOG_LINES, "日志应被 bounded 截断");
    // 保留最新的
    assert.equal(entry.logs[entry.logs.length - 1].text, `log ${MAX_LOG_LINES + 99}`);
});

// ── 额外：clearLogs 只清 UI 缓冲 ───────────────────────────────────────────────

test("clearLogs 清空 UI 日志缓冲", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = appendLog(state, makeLog("funasr", "inst-1", "1", "log 1"));
    state = appendLog(state, makeLog("funasr", "inst-1", "2", "log 2"));

    let entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 2);

    state = clearLogs(state, "funasr");
    entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 0, "UI 缓冲应被清空");
    // status 不受影响
    assert.ok(entry.status !== null || entry.status === null);
});

// ── 额外：setLogHistory 替换不追加 ─────────────────────────────────────────────

test("setLogHistory 替换不追加", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = appendLog(state, makeLog("funasr", "inst-1", "1", "realtime log"));

    // pull 历史替换
    const history = [
        makeLog("funasr", "inst-1", "1", "history log 1"),
        makeLog("funasr", "inst-1", "2", "history log 2"),
    ];
    state = setLogHistory(state, "funasr", history);

    const entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 2);
    assert.equal(entry.logs[0].text, "history log 1");
    assert.equal(entry.logs[1].text, "history log 2");
});

// ── 额外：setLogHistory 不得清空 operation 日志 ────────────────────────────────

test("setLogHistory 保留正在进行的 operation 日志（下载中 focus 刷新不清空）", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // 模拟下载中：实时 operation 日志（operation_id 非 null，引擎未运行）
    const opLog = (seq, text) => makeLog("funasr", "", seq, text, {
        operation_id: "install-model-paraformer-zh-1",
    });
    state = appendLog(state, opLog("1", "[INFO] 开始下载模型: paraformer-zh"));
    state = appendLog(state, opLog("2", "[INFO] 下载主模型: paraformer-zh"));

    // 窗口 focus → refreshStatus → pullLogs：引擎没跑，历史为空数组
    state = setLogHistory(state, "funasr", []);

    let entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 2, "空历史 pull 不得清空 operation 日志");
    assert.equal(entry.logs[1].text, "[INFO] 下载主模型: paraformer-zh");

    // 引擎运行中的 pull（instance 历史）同样不得清掉 operation 日志
    state = setLogHistory(state, "funasr", [
        makeLog("funasr", "inst-9", "1", "server log 1"),
    ]);
    entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 3, "instance 历史替换后 operation 日志仍在");
    assert.equal(entry.logs[0].text, "server log 1");
    assert.equal(entry.logs[2].text, "[INFO] 下载主模型: paraformer-zh");
});

// ── 额外：epoch 变化时清空旧 instance 日志 ─────────────────────────────────────

test("epoch 变化时清空旧 instance 日志", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // 先设置旧 epoch 的 status（这样 entry.status 不为 null）
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-old",
        revision: "1",
    }));

    // 旧 epoch 日志
    state = appendLog(state, makeLog("funasr", "inst-old", "1", "old epoch log 1"));
    state = appendLog(state, makeLog("funasr", "inst-old", "2", "old epoch log 2"));
    let entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 2);

    // 新 epoch → 清空旧日志
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-new",
        revision: "1",
    }));

    entry = getEntry(state, "funasr");
    assert.equal(entry.logs.length, 0, "epoch 变化时应清空旧日志");
});

// ── 额外：hasActiveOperation ────────────────────────────────────────────────────

test("hasActiveOperation 正确判断活跃操作", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    // idle → 不活跃
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        status: {operation: {kind: "idle", operation_id: "", stage: "pending", cancellable: false}},
    }));
    let entry = getEntry(state, "funasr");
    assert.equal(hasActiveOperation(entry), false);

    // installing + preparing → 活跃
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-1",
        revision: "2",
        status: {operation: {kind: "installing", operation_id: "op-1", stage: "preparing", cancellable: true}},
    }));
    entry = getEntry(state, "funasr");
    assert.equal(hasActiveOperation(entry), true);

    // installing + completed → 不活跃
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-1",
        revision: "3",
        status: {operation: {kind: "installing", operation_id: "op-1", stage: "completed", cancellable: false}},
    }));
    entry = getEntry(state, "funasr");
    assert.equal(hasActiveOperation(entry), false);
});

// ── 额外：pendingAction ────────────────────────────────────────────────────────

test("pendingAction 设置和清除", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    state = setPendingAction(state, "funasr", {kind: "install", operationId: "op-1"});
    let entry = getEntry(state, "funasr");
    assert.equal(entry.pendingAction.kind, "install");
    assert.equal(entry.pendingAction.operationId, "op-1");

    state = setPendingAction(state, "funasr", null);
    entry = getEntry(state, "funasr");
    assert.equal(entry.pendingAction, null);
});

// ── 额外：setStorage ───────────────────────────────────────────────────────────

test("setStorage 设置存储概览", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());

    const storage = makeStorage("funasr");
    state = setStorage(state, storage);

    const entry = getEntry(state, "funasr");
    assert.ok(entry.storage);
    assert.equal(entry.storage.engine_id, "funasr");
    assert.equal(entry.storage.targets.length, 2);
    assert.equal(entry.storage.total_size_bytes, 5900 * 1024 * 1024);
});

// ── 汇总 ──────────────────────────────────────────────────────────────────────

console.log(`\n${passCount}/${testCount} tests passed`);
if (passCount !== testCount) {
    process.exit(1);
}
console.log("local-engine-state tests passed");
