/**
 * 引导向导纯逻辑测试（0.22.13）。
 *
 * 测试覆盖：
 * 1. 步骤状态机边界：clamp / 前进 / 后退 / 末步判定
 * 2. 非法步骤输入收敛到合法范围
 * 3. OCR 就绪判定：引擎级状态 environment=ready（模型目录不注册 OCR，不可用）
 * 4. 引擎状态列表按 engine_id 取项，缺项/非法输入安全返回 null
 * 5. install-stage 分类：终态 vs 进行中，未知 stage 不误报失败
 *
 * 下载进度纯函数测试在 ../shared/download-progress.test.mjs（0.22.14 提取共用）。
 */

import assert from "node:assert/strict";
import {
    STEP_COUNT,
    canGoBack,
    canGoNext,
    classifyInstallStage,
    clampStep,
    installStageTextKey,
    isLastStep,
    isOcrReady,
    nextStep,
    pickEngineStatus,
    prevStep,
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
