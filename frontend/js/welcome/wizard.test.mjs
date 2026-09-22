/**
 * 引导向导纯逻辑测试（0.22.13）。
 *
 * 测试覆盖：
 * 1. 步骤状态机边界：clamp / 前进 / 后退 / 末步判定
 * 2. 非法步骤输入收敛到合法范围
 * 3. OCR 就绪判定：引擎级状态 environment=ready（模型目录不注册 OCR，不可用）
 * 4. 引擎状态列表按 engine_id 取项，缺项/非法输入安全返回 null
 * 5. install-stage 分类：终态 vs 进行中，未知 stage 不误报失败
 * 6. chord 全局快捷键开关：chord_bindings.global 字段级更新（开启/关闭/幂等/空安全）
 *
 * 下载进度纯函数测试在 ../shared/download-progress.test.mjs（0.22.14 提取共用）。
 */

import assert from "node:assert/strict";
import {
    activeOperationId,
    applyChordGlobalToBindings,
    canGoBack,
    canGoNext,
    clampStep,
    classifyInstallStage,
    installStageTextKey,
    isChordToggleRevisionValid,
    isLastStep,
    isOcrReady,
    nextStep,
    pickEngineStatus,
    prevStep,
    rollbackChordToggles,
    shouldAcceptInstallEvent,
    STEP_COUNT,
} from "./wizard.js";

// ── 步骤状态机 ────────────────────────────────────────────────────────────────

assert.equal(STEP_COUNT, 4);

assert.equal(clampStep(0), 0);
assert.equal(clampStep(2), 2);
assert.equal(clampStep(-1), 0);
assert.equal(clampStep(99), 3);
assert.equal(clampStep("1"), 1);
assert.equal(clampStep("x"), 0);

assert.equal(canGoBack(0), false);
assert.equal(canGoBack(1), true);
assert.equal(canGoNext(2), true);
assert.equal(canGoNext(3), false);
assert.equal(isLastStep(3), true);
assert.equal(isLastStep(2), false);

assert.equal(nextStep(0), 1);
assert.equal(nextStep(3), 3, "末步前进保持原地");
assert.equal(prevStep(3), 2);
assert.equal(prevStep(0), 0, "首步后退保持原地");

// ── OCR 就绪判定（引擎级状态）────────────────────────────────────────────────

assert.equal(isOcrReady({status: {environment: "ready"}}), true);
assert.equal(isOcrReady({status: {environment: "missing"}}), false);
assert.equal(isOcrReady({status: {environment: "broken"}}), false);
assert.equal(isOcrReady({status: null}), false);
assert.equal(isOcrReady(null), false);
assert.equal(isOcrReady(undefined), false);
assert.equal(isOcrReady("garbage"), false);

// ── 引擎状态取项 ─────────────────────────────────────────────────────────────

const list = [
    {engine_id: "funasr", status: {environment: "ready"}},
    {engine_id: "paddleocr", status: {environment: "ready"}},
];
assert.equal(pickEngineStatus(list, "paddleocr")?.engine_id, "paddleocr");
assert.equal(pickEngineStatus(list, "paddleocr").status.environment, "ready");
assert.equal(pickEngineStatus([], "paddleocr"), null);
assert.equal(pickEngineStatus(undefined, "paddleocr"), null);

assert.equal(activeOperationId({
    status: {operation: {kind: "installing", stage: "downloading", operation_id: "op-1"}},
}), "op-1");
assert.equal(activeOperationId({
    status: {operation: {kind: "installing", stage: "completed", operation_id: "op-old"}},
}), null, "终态 operation 不得重新绑定");
assert.equal(activeOperationId({
    status: {operation: {kind: "idle", stage: "pending", operation_id: ""}},
}), null);

// ── install-stage 分类 ───────────────────────────────────────────────────────

assert.equal(classifyInstallStage("downloading"), "active");
assert.equal(classifyInstallStage("preparing"), "active");
assert.equal(classifyInstallStage("validating"), "active");
assert.equal(classifyInstallStage("completed"), "done");
assert.equal(classifyInstallStage("failed"), "failed");
assert.equal(classifyInstallStage("cancelled"), "failed");
assert.equal(classifyInstallStage("mysterious-new-stage"), "active", "未知 stage 不误报失败");

assert.equal(
    installStageTextKey("downloading"),
    "local_engine.operation.stage.downloading",
);

console.log("welcome/wizard.test.mjs: all assertions passed");

// ── Chord toggles 竞态防护 ─────────────────────────────────────────────────────

assert.equal(isChordToggleRevisionValid(1, 1), true, "相同 revision 有效");
assert.equal(isChordToggleRevisionValid(1, 2), false, "旧 revision 已过期");
assert.equal(isChordToggleRevisionValid(2, 1), false, "新 revision 尚未提交");
assert.equal(isChordToggleRevisionValid(0, 0), true, "初始 revision 有效");

// ── Chord toggles 失败回滚 ─────────────────────────────────────────────────────

assert.deepEqual(
    rollbackChordToggles({chord_enabled: true, chord_hint_visible: false}),
    {chord_enabled: true, chord_hint_visible: false},
    "回滚到已确认值",
);
assert.deepEqual(
    rollbackChordToggles({chord_enabled: false, chord_hint_visible: true}),
    {chord_enabled: false, chord_hint_visible: true},
    "回滚到已确认值（全 false→hint 默认 true）",
);
assert.deepEqual(
    rollbackChordToggles(null),
    {chord_enabled: false, chord_hint_visible: true},
    "null 安全回滚到默认值",
);
assert.deepEqual(
    rollbackChordToggles(undefined),
    {chord_enabled: false, chord_hint_visible: true},
    "undefined 安全回滚到默认值",
);

// ── Chord 全局快捷键开关（chord_bindings.global 字段级更新）──────────────────

// 开启：写入 follow_chord，保留本条目其他字段，不动其他动作
{
    const src = {
        chat: {key: "q", modifiers: ["alt"]},
        screenshot: {
            key: "",
            modifiers: ["alt"],
            global: {mode: "custom", modifiers: ["ctrl", "alt"], key: "a"},
        },
    };
    const next = applyChordGlobalToBindings(src, "chat", true);
    assert.deepEqual(next.chat.global, {mode: "follow_chord"}, "开启写入 follow_chord");
    assert.equal(next.chat.key, "q", "保留触发键字段");
    assert.equal(src.chat.global, undefined, "不改原对象");
    assert.deepEqual(
        next.screenshot.global,
        {mode: "custom", modifiers: ["ctrl", "alt"], key: "a"},
        "不动其他动作的 global",
    );
}

// 关闭：删除 global，保留 key/modifiers 等其他字段
{
    const src = {chat: {key: "q", modifiers: ["alt"], global: {mode: "follow_chord"}}};
    const next = applyChordGlobalToBindings(src, "chat", false);
    assert.equal(next.chat.global, undefined, "关闭删除 global");
    assert.equal(next.chat.key, "q", "保留触发键字段");
    assert.deepEqual(next.chat.modifiers, ["alt"], "保留修饰键字段");
}

// 关闭已无 global 的条目：幂等（不报错、不丢字段）
{
    const next = applyChordGlobalToBindings({edit: {key: "e", modifiers: ["alt"]}}, "edit", false);
    assert.deepEqual(next.edit, {key: "e", modifiers: ["alt"]});
}

// 动作条目不存在：开启时创建空触发键条目（后端按 default_key 解析生效键）
{
    const next = applyChordGlobalToBindings({}, "sticky", true);
    assert.deepEqual(next.sticky, {
        key: "",
        modifiers: ["alt"],
        global: {mode: "follow_chord"},
    });
}

// null / undefined bindings 安全
{
    assert.deepEqual(
        applyChordGlobalToBindings(null, "chat", true).chat,
        {key: "", modifiers: ["alt"], global: {mode: "follow_chord"}},
    );
    assert.equal(applyChordGlobalToBindings(undefined, "chat", true).chat.key, "");
}

// ── 安装进度事件 operation_id 隔离 ─────────────────────────────────────────────

// **铁则：operation_id 不从事件绑定**
// operation_id 必须从后端命令返回值或 get_local_engine_status 获取。
// shouldAcceptInstallEvent 只做校验，不做绑定。

// 未绑定 + 事件有 operation_id：无法证明归属，拒绝
{
    const r = shouldAcceptInstallEvent(null, "op-1");
    assert.equal(r.accept, false, "未绑定时必须 fail-closed");
    assert.equal(r.newOpId, null, "不从事件绑定：newOpId 保持 null");
}

// 已绑定 + 匹配：接受
{
    const r = shouldAcceptInstallEvent("op-1", "op-1");
    assert.equal(r.accept, true, "匹配：接受");
    assert.equal(r.newOpId, "op-1", "匹配：newOpId 不变");
}

// 已绑定 + 不匹配：拒绝（旧操作迟到）
{
    const r = shouldAcceptInstallEvent("op-2", "op-1");
    assert.equal(r.accept, false, "旧操作迟到：拒绝");
    assert.equal(r.newOpId, "op-2", "旧操作迟到：newOpId 不变");
}

// 已绑定 + 事件无 operation_id：无法核对身份，拒绝
{
    const r = shouldAcceptInstallEvent("op-1", null);
    assert.equal(r.accept, false, "事件无 operation_id：拒绝");
    assert.equal(r.newOpId, "op-1", "事件无 operation_id：newOpId 不变");
}

// 双方无 operation_id：拒绝
{
    const r = shouldAcceptInstallEvent(null, undefined);
    assert.equal(r.accept, false, "双方无 operation_id：拒绝");
    assert.equal(r.newOpId, null, "双方无 operation_id：newOpId null");
}

// ── 并发/连续 operation 事件交错场景 ───────────────────────────────────────────

// 模拟：操作 A (op-a) 进行中，操作 B (op-b) 发起，op-a 的迟到事件到达
{
    // 1. op-a 从后端获取（不从事件绑定）
    let current = "op-a";

    // 2. op-a 事件匹配
    let r = shouldAcceptInstallEvent(current, "op-a");
    assert.equal(r.accept, true);
    current = r.newOpId;

    // 3. 新操作 op-b 发起（installOcr 重置 currentOpId = null）
    current = null;

    // 4. op-b 事件到达（未绑定，拒绝且不绑定）
    r = shouldAcceptInstallEvent(current, "op-b");
    assert.equal(r.accept, false);
    current = r.newOpId; // 仍为 null
    assert.equal(current, null, "不从事件绑定");

    // 5. 状态查询绑定 op-b 后，op-a 的迟到事件必须拒绝
    current = "op-b";
    r = shouldAcceptInstallEvent(current, "op-a");
    assert.equal(r.accept, false, "op-a 迟到事件必须拒绝");
    assert.equal(r.newOpId, "op-b", "current 保持 op-b");
}

// 模拟：操作 A 终态后清理；操作 B 在状态查询重新绑定前不接受事件
{
    let current = "op-a";
    // 终态事件到达（终态处理清理 currentOpId）
    current = null;
    // 操作 B 事件（未绑定，拒绝）
    const r = shouldAcceptInstallEvent(current, "op-b");
    assert.equal(r.accept, false, "重新绑定前必须拒绝新操作事件");
    assert.equal(r.newOpId, null, "不从事件绑定");
}
