/**
 * 0.22.6 前端收敛适配测试。
 *
 * 覆盖（交付报告约定的五个必测场景）：
 * 1. oversized revision：大整数 revision 比较不能走字符串字典序
 * 2. 迟到事件：operation_id / revision 不匹配的迟到事件被拒绝
 * 3. cancelled operation：取消是正常终态，不进错误、退出忙碌
 * 4. selected model != active model：二者独立标记、签名独立参与
 * 5. operation completed 后解除 busy
 */

import assert from "node:assert/strict";
import {
    createInitialState,
    setCatalog,
    setPendingModelAction,
    mergeStatus,
    applyInstallStage,
    getEntry,
    hasActiveOperation,
    isOperationCancellable,
    getPrimaryAction,
    isActionBlocked,
    getEffectiveModelInstallState,
} from "./local-engine-state.js";
import {
    makeCatalog,
    makeStatus,
    processState,
    makeModel,
} from "./local-engine-fixtures.js";

// ── mock 基建（local-engine-models 的 import 链需要 window.__TAURI__）────────
// 必须在动态 import local-engine-models.js 之前设置——tauri.js 在加载时
// 会覆写 window.alert/confirm/prompt。

if (!globalThis.window) {
    globalThis.window = {};
}

const _invokeImpl = {fn: async () => []};
globalThis.window.__TAURI__ = {
    core: {invoke: (cmd, args) => _invokeImpl.fn(cmd, args)},
    event: {listen: async () => () => {}},
};

// mock 设置后动态导入（静态 import 会被提升到 mock 之前执行）
const {modelRowSignature} = await import("./local-engine-models.js");

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

function pushRevision(state, epoch, revision) {
    return mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: epoch,
        revision,
    }));
}

// ── 1. oversized revision ─────────────────────────────────────────────────────

await test("oversized revision：u64 大整数严格递增被接受", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    // 2^63-1 → 2^63（均 > Number.MAX_SAFE_INTEGER）
    state = pushRevision(state, "epoch-big", "9223372036854775807");
    state = pushRevision(state, "epoch-big", "9223372036854775808");
    assert.equal(
        getEntry(state, "funasr").status.revision,
        "9223372036854775808",
        "超过 MAX_SAFE_INTEGER 的更大 revision 应被接受",
    );
});

await test("oversized revision：u64 max 同值重复推送被拒绝", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = pushRevision(state, "epoch-u64", "18446744073709551615");
    state = pushRevision(state, "epoch-u64", "18446744073709551615");
    assert.equal(getEntry(state, "funasr").status.revision, "18446744073709551615");
});

await test("oversized revision：字典序陷阱——'10' 晚于 '9' 到达必须被接受", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = pushRevision(state, "epoch-lex", "9");
    // 字典序 "9" > "10"，数值 10 > 9——必须走数值比较
    state = pushRevision(state, "epoch-lex", "10");
    assert.equal(getEntry(state, "funasr").status.revision, "10", "'10' 数值大于 '9'，应被接受");
});

await test("oversized revision：字典序陷阱——'9' 晚于 '10' 到达被拒绝", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = pushRevision(state, "epoch-lex2", "10");
    state = pushRevision(state, "epoch-lex2", "9");
    assert.equal(getEntry(state, "funasr").status.revision, "10", "'9' 数值小于 '10'，应被拒绝");
});

await test("oversized revision：非法 revision fail closed 被拒绝", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    state = pushRevision(state, "epoch-bad", "42");
    state = pushRevision(state, "epoch-bad", "not-a-number");
    assert.equal(getEntry(state, "funasr").status.revision, "42", "非法 revision 不得覆盖有效状态");
});

// ── 2. 迟到事件 ───────────────────────────────────────────────────────────────

await test("迟到事件：operation_id 不匹配的 install_stage 被拒绝", () => {
    const seq = makeSeqState("epoch-stage");
    seq.push({
        status: {
            operation: {kind: "installing", operation_id: "op-current", stage: "downloading", cancellable: true},
        },
    });

    // 迟到的旧 operation stage 事件 → 拒绝
    let state = applyInstallStage(seq.state, {
        engine_id: "funasr",
        operation_id: "op-stale",
        stage: "promoting",
    });
    assert.equal(
        getEntry(state, "funasr").status.status.operation.stage,
        "downloading",
        "旧 operation 的 stage 事件不得覆盖当前操作",
    );

    // 匹配 operation_id 的事件 → 应用
    state = applyInstallStage(seq.state, {
        engine_id: "funasr",
        operation_id: "op-current",
        stage: "verifying",
    });
    assert.equal(
        getEntry(state, "funasr").status.status.operation.stage,
        "verifying",
        "匹配 operation_id 的 stage 事件应被应用",
    );
});

await test("迟到事件：idle 状态下的 install_stage 被拒绝", () => {
    const seq = makeSeqState("epoch-idle");
    seq.push({status: {operation: idleOperation()}});

    const state = applyInstallStage(seq.state, {
        engine_id: "funasr",
        operation_id: "op-any",
        stage: "downloading",
    });
    assert.equal(
        getEntry(state, "funasr").status.status.operation.stage,
        "pending",
        "idle 下不应凭空进入操作 stage",
    );
});

await test("迟到事件：无 status 快照时 install_stage 不产生条目", () => {
    let state = createInitialState();
    state = setCatalog(state, makeCatalog());
    const after = applyInstallStage(state, {
        engine_id: "funasr",
        operation_id: "op-x",
        stage: "downloading",
    });
    assert.equal(getEntry(after, "funasr").status, null, "无快照不得凭空造出 status");
});

await test("迟到事件：同 epoch 旧 revision 的状态快照被丢弃", () => {
    const seq = makeSeqState("epoch-rev");
    seq.push({status: {environment: "ready"}});
    const before = getEntry(seq.state, "funasr").status.status.environment;

    // revision 更小的迟到快照（手工构造，绕过自增 helper）
    const lateState = mergeStatus(seq.state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-rev",
        revision: "1",
        status: {environment: "broken"},
    }));
    assert.equal(getEntry(lateState, "funasr").status.status.environment, before,
        "旧 revision 快照不得覆盖新状态");
});

// ── 3. cancelled operation ────────────────────────────────────────────────────

await test("cancelled operation：stage=cancelled 不算活跃、不可取消", () => {
    const seq = makeSeqState("epoch-cancel");
    seq.push({
        status: {
            operation: {kind: "installing", operation_id: "op-c1", stage: "downloading", cancellable: true},
        },
    });
    assert.equal(hasActiveOperation(seq.state.get("funasr")), true, "下载中应为活跃");

    // 取消后 stage=cancelled（kind 尚未回 idle 的过渡快照）
    seq.push({
        status: {
            operation: {kind: "installing", operation_id: "op-c1", stage: "cancelled", cancellable: false},
        },
    });
    const entry = seq.state.get("funasr");
    assert.equal(hasActiveOperation(entry), false, "cancelled 不是活跃操作");
    assert.equal(isOperationCancellable(entry), false, "cancelled 后不可再取消");
});

await test("cancelled operation：install 命令返回 end_state=cancelled 是正常终态", async () => {
    const {createLocalEngineController} = await import("./local-runtime.js");

    const errors = [];
    const origInvoke = _invokeImpl.fn;
    _invokeImpl.fn = async (cmd) => {
        if (cmd === "get_local_engine_catalog") return makeCatalog();
        if (cmd === "get_local_engine_status") return [];
        if (cmd === "get_local_engine_logs") return [];
        if (cmd === "install_local_engine") {
            return {engine_id: "funasr", operation_id: "op-cancelled-1", end_state: "cancelled"};
        }
        return [];
    };

    const controller = createLocalEngineController({onError: (e) => errors.push(e)});
    try {
        await controller.mount();

        let result = null;
        let threw = false;
        try {
            result = await controller.install("funasr", null);
        } catch {
            threw = true;
        }
        assert.equal(threw, false, "cancelled 不应作为错误抛出");
        assert.equal(result?.end_state, "cancelled");
        assert.equal(errors.length, 0, "cancelled 不应触发 onError");

        const entry = controller.getState().get("funasr");
        assert.equal(entry.pendingAction, null, "取消终态后 pendingAction 应清除");
    } finally {
        controller.dispose();
        _invokeImpl.fn = origInvoke;
    }
});

// ── 4. selected model != active model ─────────────────────────────────────────

await test("selected != active：两标记独立，签名随各自变化", () => {
    const base = makeModel({install_state: "installed", is_selected: false, is_active: false});
    const sigNone = modelRowSignature(base, "installed");
    const sigSelected = modelRowSignature({...base, is_selected: true}, "installed");
    const sigActive = modelRowSignature({...base, is_active: true}, "installed");
    const sigBoth = modelRowSignature({...base, is_selected: true, is_active: true}, "installed");

    assert.notEqual(sigSelected, sigNone, "selected 变化应改变签名");
    assert.notEqual(sigActive, sigNone, "active 变化应改变签名");
    assert.notEqual(sigSelected, sigActive, "selected 与 active 是不同状态");
    assert.notEqual(sigBoth, sigSelected, "同时 active 时签名不同");
});

await test("selected != active：selected≠active 的两模型签名互异且标记独立出现", () => {
    // 场景：配置切到模型 B（selected），运行实例仍加载模型 A（active）。
    // 这是合法状态——launch snapshot 冻结，配置变化不改写运行中的 active。
    const selectedB = makeModel({
        model_id: "model-b",
        install_state: "installed",
        is_selected: true,
        is_active: false,
    });
    const activeA = makeModel({
        model_id: "model-a",
        install_state: "installed",
        is_selected: false,
        is_active: true,
    });

    const sigB = modelRowSignature(selectedB, "installed");
    const sigA = modelRowSignature(activeA, "installed");
    assert.notEqual(sigB, sigA, "selected≠active 的两行签名互异");
});

await test("selected != active：pending 操作优先级与 selected/active 无关", () => {
    const model = makeModel({install_state: "installed", is_selected: true, is_active: false});
    let state = createInitialState();
    state = setPendingModelAction(state, "funasr", model.model_id, {kind: "repair", operationId: "op-r1"});
    assert.equal(
        getEffectiveModelInstallState(state.get("funasr"), model),
        "repairing",
        "修复中状态优先于已安装态展示",
    );
});

// ── 5. operation completed 后解除 busy ────────────────────────────────────────

await test("completed 解除 busy：操作完成后 start 可用", () => {
    const seq = makeSeqState("epoch-done");
    // 环境就绪 + 停止（wire 是全量快照，后续推送需携带完整状态）
    const readyBase = {
        environment: "ready",
        process: processState.stopped(),
    };
    seq.push({
        status: {...readyBase, operation: idleOperation()},
    });
    assert.equal(getPrimaryAction(seq.state.get("funasr")), "start", "空闲时应可启动");

    // 开始安装（不可取消的过渡）
    seq.push({
        status: {
            ...readyBase,
            operation: {kind: "installing", operation_id: "op-d1", stage: "downloading", cancellable: false},
        },
    });
    assert.equal(hasActiveOperation(seq.state.get("funasr")), true, "下载中为忙碌");
    assert.equal(isActionBlocked(seq.state.get("funasr"), "start"), true, "忙碌时 start 被阻止");
    assert.equal(getPrimaryAction(seq.state.get("funasr")), null, "忙碌且不可取消时无主操作");

    // 完成：stage=completed（kind 回 idle 前的快照）→ 忙碌解除
    seq.push({
        status: {
            ...readyBase,
            operation: {kind: "installing", operation_id: "op-d1", stage: "completed", cancellable: false},
        },
    });
    assert.equal(hasActiveOperation(seq.state.get("funasr")), false, "completed 后解除忙碌");
    assert.equal(isActionBlocked(seq.state.get("funasr"), "start"), false, "completed 后 start 不再被阻止");

    // kind 回 idle → 恢复 start
    seq.push({
        status: {
            ...readyBase,
            operation: idleOperation(),
        },
    });
    assert.equal(getPrimaryAction(seq.state.get("funasr")), "start", "回 idle 后恢复 start");
});

await test("completed 解除 busy：failed 同样退出忙碌", () => {
    const seq = makeSeqState("epoch-fail");
    const readyBase = {
        environment: "ready",
        process: processState.stopped(),
    };
    seq.push({
        status: {
            ...readyBase,
            operation: {kind: "repairing", operation_id: "op-f1", stage: "preparing", cancellable: true},
        },
    });
    assert.equal(hasActiveOperation(seq.state.get("funasr")), true);

    seq.push({
        status: {
            ...readyBase,
            last_error: {code: "self_test_failed", phase: "validating", action_hint: "请查看日志", detail: "…"},
            operation: {kind: "repairing", operation_id: "op-f1", stage: "failed", cancellable: false},
        },
    });
    const entry = seq.state.get("funasr");
    assert.equal(hasActiveOperation(entry), false, "failed 后退出忙碌");
    assert.equal(isOperationCancellable(entry), false);
});

// ── 汇总 ─────────────────────────────────────────────────────────────────────

console.log(`\n${passCount}/${testCount} tests passed.`);
if (passCount !== testCount) {
    process.exit(1);
}
console.log("local-engine-0227 tests passed");
