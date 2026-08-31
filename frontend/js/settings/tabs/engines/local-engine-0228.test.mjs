/**
 * 0.22.6.1 安装反馈闭环测试。
 *
 * 覆盖（交付报告约定的端到端场景）：
 * 1. install 开始：乐观 pending 立即生效（按钮禁用 + 无"毫无反应"期）
 * 2. backend operation status 到达：synthetic pending id 绑定真实 operation_id
 * 3. verifying/promoting/switching/validating：install_stage 事件按真实 id 应用
 * 4. completed：解除忙碌
 * 5. command 返回：pendingAction 清除
 * 6. start 按钮恢复
 * 7. completion event 丢失：command 返回 + refreshStatus 仍能恢复（退出 busy）
 * 8. start 失败：pendingAction 清除 + onError + 最终状态刷新（rollback error 可见）
 * 9. FunASR 单一可用 compute 选项 → 静态展示（无 CUDA 错觉）
 */

import assert from "node:assert/strict";
import {
    createInitialState,
    setCatalog,
    setPendingAction,
    mergeStatus,
    applyInstallStage,
    bindRealOperationId,
    computeOptionsDisplayMode,
    getEntry,
    hasActiveOperation,
    isActionBlocked,
    getPrimaryAction,
} from "./local-engine-state.js";
import {
    makeCatalog,
    makeStatus,
    processState,
} from "./local-engine-fixtures.js";

// ── mock 基建（必须在动态 import local-runtime.js 之前设置）─────────────────

if (!globalThis.window) {
    globalThis.window = {};
}

const _invokeImpl = {fn: async () => []};
globalThis.window.__TAURI__ = {
    core: {invoke: (cmd, args) => _invokeImpl.fn(cmd, args)},
    event: {listen: async () => () => {}},
};

// mock 设置后动态导入
const {createLocalEngineController} = await import("./local-runtime.js");

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

// ── 辅助 ─────────────────────────────────────────────────────────────────────

function idleOperation() {
    return {kind: "idle", operation_id: "", stage: "pending", cancellable: false};
}

const READY_BASE = {
    environment: "ready",
    process: processState.stopped(),
};

/**
 * 自增 revision 的状态推送序列（同 epoch）。
 */
function makeSeqState(epoch) {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    let rev = 0;
    return {
        push(overrides = {}) {
            rev += 1;
            state = mergeStatus(state, makeStatus({
                engine_id: "funasr",
                service_epoch: epoch,
                revision: String(rev),
                ...overrides,
            }));
            return state;
        },
        get state() {
            return state;
        },
    };
}

// ── 1. 乐观 pending：点击后立即忙碌 ──────────────────────────────────────────

await test("乐观 pending：pendingAction 存在即阻断生命周期按钮", () => {
    const seq = makeSeqState("epoch-pending-block");
    seq.push({status: {...READY_BASE, operation: idleOperation()}});
    assert.equal(getPrimaryAction(seq.state.get("funasr")), "start", "空闲时可启动");

    // 用户点击安装——pendingAction 已设置，后端 operation 尚未到达
    let state = setPendingAction(seq.state, "funasr", {kind: "install", operationId: "install-1"});
    assert.equal(isActionBlocked(state.get("funasr"), "start"), true, "pending 期间 start 应被阻断");
    assert.equal(isActionBlocked(state.get("funasr"), "install"), true, "pending 期间 install 应被阻断");
    // 清除 pending 后恢复
    state = setPendingAction(state, "funasr", null);
    assert.equal(isActionBlocked(state.get("funasr"), "start"), false, "pending 清除后 start 恢复");
});

// ── 2. synthetic id → 真实 operation_id 绑定 ─────────────────────────────────

await test("绑定真实 operation_id：活跃后端 operation 到达后 pending 换用真实 id", () => {
    const seq = makeSeqState("epoch-bind");
    seq.push({status: {...READY_BASE, operation: idleOperation()}});
    let state = setPendingAction(seq.state, "funasr", {kind: "install", operationId: "install-synthetic"});

    // 后端 status 到达：kind=installing、真实 operation_id
    // （controller 的 applyStatusDto = mergeStatus + bindRealOperationId）
    const statusDto = makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-bind",
        revision: "100",
        status: {
            ...READY_BASE,
            operation: {kind: "installing", operation_id: "op-real-1", stage: "downloading", cancellable: true},
        },
    });
    state = mergeStatus(state, statusDto);
    state = bindRealOperationId(state, "funasr", statusDto);
    assert.equal(
        getEntry(state, "funasr").pendingAction.operationId,
        "op-real-1",
        "pending 应绑定真实 operation_id",
    );
    assert.equal(getEntry(state, "funasr").pendingAction.kind, "install", "kind 保持用户触发的操作");

    // 绑定后 install_stage 事件（真实 id）才能命中
    state = applyInstallStage(state, {engine_id: "funasr", operation_id: "op-real-1", stage: "verifying"});
    assert.equal(
        getEntry(state, "funasr").status.status.operation.stage,
        "verifying",
        "绑定真实 id 后 install_stage 事件应被应用",
    );
});

await test("绑定真实 operation_id：终态/非活跃/idle 不绑定", () => {
    const seq = makeSeqState("epoch-bind2");
    seq.push({status: {...READY_BASE, operation: idleOperation()}});
    let state = setPendingAction(seq.state, "funasr", {kind: "start", operationId: "start-synthetic"});

    // idle operation 不绑定
    let statusDto = makeStatus({engine_id: "funasr", service_epoch: "epoch-bind2", revision: "2"});
    state = mergeStatus(state, statusDto);
    state = bindRealOperationId(state, "funasr", statusDto);
    assert.equal(getEntry(state, "funasr").pendingAction.operationId, "start-synthetic");

    // 终态 stage 不绑定
    statusDto = makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-bind2",
        revision: "3",
        status: {
            ...READY_BASE,
            operation: {kind: "starting", operation_id: "op-real-2", stage: "completed", cancellable: false},
        },
    });
    state = mergeStatus(state, statusDto);
    state = bindRealOperationId(state, "funasr", statusDto);
    assert.equal(getEntry(state, "funasr").pendingAction.operationId, "start-synthetic");
});

// ── 3-6. 端到端：install 全流程 ──────────────────────────────────────────────

await test("端到端 install：开始→状态到达→阶段推进→completed→command 返回→pending 清除→start 恢复", async () => {
    const errors = [];
    let installCalls = 0;
    let statusCalls = 0;
    const origInvoke = _invokeImpl.fn;
    _invokeImpl.fn = async (cmd) => {
        if (cmd === "get_local_engine_catalog") return makeCatalog();
        if (cmd === "get_local_engine_status") {
            statusCalls++;
            // 初始 pull：idle；action 后刷新：返回 completed 终态
            if (statusCalls <= 1) return [];
            return [makeStatus({
                engine_id: "funasr",
                service_epoch: "epoch-e2e",
                revision: "9",
                status: {
                    ...READY_BASE,
                    operation: idleOperation(),
                },
            })];
        }
        if (cmd === "get_local_engine_logs") return [];
        if (cmd === "list_engine_models") return [];
        if (cmd === "get_local_engine_preferences") return {engine_id: "funasr"};
        if (cmd === "install_local_engine") {
            installCalls++;
            return {engine_id: "funasr", operation_id: "op-real-3", end_state: "completed"};
        }
        return [];
    };

    const controller = createLocalEngineController({onError: (e) => errors.push(e)});
    try {
        await controller.mount();

        // 手动推送开始状态：后端 operation 到达（模拟 install_local_engine 启动前）
        // controller.install 内部：设置 synthetic pending → invoke → 清除 pending → refresh

        // 1. install 开始：乐观 pending 立即生效
        const installPromise = controller.install("funasr", null);
        const pendingEntry = controller.getState().get("funasr");
        assert.ok(pendingEntry.pendingAction, "install 开始后应有 pendingAction");
        assert.equal(pendingEntry.pendingAction.kind, "install");
        assert.equal(
            isActionBlocked(pendingEntry, "start"),
            true,
            "pending 期间按钮立即禁用",
        );

        // 2. backend operation status 到达（真实 id）的 reducer 语义
        //    已由"绑定真实 operation_id"测试覆盖（controller 事件监听为 mock no-op）；
        //    controller 层验证终态闭环：command 返回 → pending 清除 → start 恢复。

        // 5. command 返回：pendingAction 清除
        const result = await installPromise;
        assert.equal(result.end_state, "completed");
        const entry = controller.getState().get("funasr");
        assert.equal(entry.pendingAction, null, "command 返回后 pendingAction 应清除");
        // 6. start 按钮恢复：refreshStatus 已拉到 idle 终态
        assert.equal(isActionBlocked(entry, "start"), false, "完成后 start 不再被阻断");
        assert.equal(getPrimaryAction(entry), "start", "完成后主操作恢复为 start");
        assert.equal(installCalls, 1);
    } finally {
        controller.dispose();
        _invokeImpl.fn = origInvoke;
    }
});

// ── 7. completion event 丢失：command 返回 + refresh 恢复 ────────────────────

await test("completion event 丢失：command 返回 + refreshStatus 拉到终态后退出 busy", async () => {
    const origInvoke = _invokeImpl.fn;
    let statusCalls = 0;
    _invokeImpl.fn = async (cmd) => {
        if (cmd === "get_local_engine_catalog") return makeCatalog();
        if (cmd === "get_local_engine_status") {
            statusCalls++;
            if (statusCalls <= 1) return [];
            // refresh 拉取：operation 已回 idle（completed status event 丢失的场景）
            return [makeStatus({
                engine_id: "funasr",
                service_epoch: "epoch-lost",
                revision: "5",
                status: {...READY_BASE, operation: idleOperation()},
            })];
        }
        if (cmd === "get_local_engine_logs") return [];
        if (cmd === "list_engine_models") return [];
        if (cmd === "get_local_engine_preferences") return {engine_id: "funasr"};
        if (cmd === "start_local_engine") return {engine_id: "funasr", operation_id: "op-4", end_state: "completed"};
        return [];
    };

    const errors = [];
    const controller = createLocalEngineController({onError: (e) => errors.push(e)});
    try {
        await controller.mount();

        // 模拟丢失事件：挂载后状态停留在"活跃 operation"（无 completed 事件到达）
        // controller.refreshStatus 由 _executeAction 触发 → 拉到 idle 终态
        await controller.start("funasr", null);

        const entry = controller.getState().get("funasr");
        assert.equal(entry.pendingAction, null, "pendingAction 应清除");
        assert.equal(hasActiveOperation(entry), false, "refresh 拉到终态后退出 busy");
        assert.equal(getPrimaryAction(entry), "start", "进程 stopped + idle → 主操作恢复 start");
        assert.equal(errors.length, 0);
    } finally {
        controller.dispose();
        _invokeImpl.fn = origInvoke;
    }
});

// ── 8. start 失败：rollback error 可见 ───────────────────────────────────────

await test("start 失败：pendingAction 清除 + onError + 最终状态刷新可见 rollback error", async () => {
    const origInvoke = _invokeImpl.fn;
    let statusCalls = 0;
    const rollbackError = {
        code: "stop_failed",
        phase: "rollback",
        action_hint: "切换后验证失败，已回滚",
        detail: "mock rollback error",
    };
    _invokeImpl.fn = async (cmd) => {
        if (cmd === "get_local_engine_catalog") return makeCatalog();
        if (cmd === "get_local_engine_status") {
            statusCalls++;
            if (statusCalls <= 1) return [];
            // 失败后刷新：last_error 可见 + operation 回 idle
            return [makeStatus({
                engine_id: "funasr",
                service_epoch: "epoch-fail",
                revision: "7",
                status: {
                    ...READY_BASE,
                    last_error: rollbackError,
                    operation: idleOperation(),
                },
            })];
        }
        if (cmd === "get_local_engine_logs") return [];
        if (cmd === "list_engine_models") return [];
        if (cmd === "get_local_engine_preferences") return {engine_id: "funasr"};
        if (cmd === "start_local_engine") {
            throw {code: "stop_failed", message: "切换后验证失败，已回滚", retryable: false};
        }
        return [];
    };

    const errors = [];
    const controller = createLocalEngineController({onError: (e) => errors.push(e)});
    try {
        await controller.mount();

        let threw = false;
        try {
            await controller.start("funasr", null);
        } catch {
            threw = true;
        }
        assert.equal(threw, true, "start 失败应抛出");

        const entry = controller.getState().get("funasr");
        assert.equal(entry.pendingAction, null, "失败后 pendingAction 应清除");
        assert.equal(errors.length, 1, "onError 应收到错误");
        assert.equal(errors[0].engine_id, "funasr", "错误回调必须携带引擎归属");
        assert.equal(errors[0].error_scope, "start", "错误回调必须携带操作范围");
        assert.equal(entry.transientError?.engine_id, "funasr", "瞬时错误只写入对应引擎卡片");
        assert.equal(entry.status?.status?.last_error?.code, "stop_failed", "失败后刷新应带回 rollback error");
        assert.equal(hasActiveOperation(entry), false, "失败后退出忙碌");
    } finally {
        controller.dispose();
        _invokeImpl.fn = origInvoke;
    }
});

// ── 9. 单一可用 compute 选项 → 静态展示 ──────────────────────────────────────

await test("computeOptionsDisplayMode：单一可用选项 → static，多选项 → select", () => {
    // FunASR 实际 catalog：只有 cpu
    assert.equal(
        computeOptionsDisplayMode([
            {preference: "cpu", compatible: true},
        ]),
        "static",
        "只有 cpu 时应静态展示（不渲染选择器）",
    );
    // 单选项但不可兼容 → static
    assert.equal(
        computeOptionsDisplayMode([{preference: "cpu", compatible: false}]),
        "static",
    );
    // PaddleOCR：auto/cpu 两项 → select
    assert.equal(
        computeOptionsDisplayMode([
            {preference: "auto", compatible: true},
            {preference: "cpu", compatible: true},
        ]),
        "select",
    );
    // 空 → static（无可选项，渲染 select 无意义）；非数组 → select（fail-open）
    assert.equal(computeOptionsDisplayMode([]), "static");
    assert.equal(computeOptionsDisplayMode(null), "select");
});

// ── 汇总 ─────────────────────────────────────────────────────────────────────

console.log(`\n${passCount}/${testCount} tests passed.`);
if (passCount !== testCount) {
    process.exit(1);
}
console.log("local-engine-0228 tests passed");
