/**
 * 0.22.8-E PaddleOCR ONNX 原位适配测试。
 *
 * 覆盖（E 包交付报告约定的场景）：
 * 1. catalog runtime_kind 改为 onnx_runtime，model_revision 为 ppocrv6-tiny
 * 2. deployment identity 投影：desired/loaded/pending_restart
 * 3. DLL identity 变化才显示 pending_restart；只切模型 generation 不触发
 * 4. legacy deployment 存在时可查询
 * 5. 安装失败不乐观显示 Ready（desired != loaded 时文案说真话）
 * 6. computeEngineSummary：pending_restart 时不说"运行中"
 * 7. computeFeedback：pending_restart 显示"重启后生效"
 * 8. computeFeedback：legacy 存在时提示可清理
 * 9. computeKeyline：PaddleOCR 有 ONNX Runtime 运行时行
 * 10. ONNX 诊断 fixture 结构正确
 */

import assert from "node:assert/strict";
import {
    createInitialState,
    setCatalog,
    mergeStatus,
    getDesiredDeployment,
    getLoadedDeployment,
    isPendingRestart,
    getLegacyDeployment,
    hasDeploymentMismatch,
    getEntry,
} from "./local-engine-state.js";
import {
    paddleocrCatalog,
    makeStatus,
    makeOnnxDiagnostics,
    makeOnnxDeploymentBackend,
    processState,
} from "./local-engine-fixtures.js";
import {
    computeEngineSummary,
    computeFeedback,
    computeKeyline,
} from "./local-engine-summary.js";

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
 * 构建 PaddleOCR 状态条目。
 */
function makePaddleOcrEntry(statusOverrides = {}, catalogOverrides = {}) {
    let state = createInitialState();
    state = setCatalog(state, [{...paddleocrCatalog, ...catalogOverrides}]);
    const statusDto = makeStatus({
        engine_id: "paddleocr",
        service_epoch: "epoch-paddleocr-001",
        revision: "1",
        status: {
            ...READY_BASE,
            operation: idleOperation(),
            ...statusOverrides,
        },
    });
    state = mergeStatus(state, statusDto);
    return getEntry(state, "paddleocr");
}

// ── 1. catalog 基本字段 ─────────────────────────────────────────────────────

await test("catalog: paddleocr runtime_kind = onnx_runtime, model_revision = ppocrv6-tiny", () => {
    assert.equal(paddleocrCatalog.runtime_kind, "onnx_runtime",
        "runtime_kind 必须为 onnx_runtime");
    assert.equal(paddleocrCatalog.model_revision, "ppocrv6-tiny",
        "model_revision 必须为 ppocrv6-tiny");
    assert.equal(paddleocrCatalog.engine_id, "paddleocr",
        "engine_id 不变，不注册 paddleocr-onnx");
    assert.equal(paddleocrCatalog.capability_kind, "ocr");
});

// ── 2. deployment identity 投影 ─────────────────────────────────────────────

await test("getDesiredDeployment: 从 backend.desired_deployment 提取", () => {
    const entry = makePaddleOcrEntry({
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0-abc",
                model_revision: "ppocrv6-tiny",
            },
        }),
    });
    const desired = getDesiredDeployment(entry);
    assert.ok(desired, "desired_deployment 不应为 null");
    assert.equal(desired.runtime_kind, "onnx_runtime");
    assert.equal(desired.dll_identity, "onnxruntime-1.20.0-abc");
    assert.equal(desired.model_revision, "ppocrv6-tiny");
});

await test("getLoadedDeployment: null 表示未初始化 ORT", () => {
    const entry = makePaddleOcrEntry({
        backend: makeOnnxDeploymentBackend({
            loaded_deployment: null,
        }),
    });
    assert.equal(getLoadedDeployment(entry), null,
        "首次安装后 loaded 为 null（未初始化 ORT）");
});

await test("getLoadedDeployment: 有值时正确提取", () => {
    const entry = makePaddleOcrEntry({
        backend: makeOnnxDeploymentBackend({
            loaded_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0-abc",
                model_revision: "ppocrv6-tiny",
            },
        }),
    });
    const loaded = getLoadedDeployment(entry);
    assert.ok(loaded);
    assert.equal(loaded.dll_identity, "onnxruntime-1.20.0-abc");
});

// ── 3. pending_restart：DLL identity 变化才显示 ────────────────────────────

await test("isPendingRestart: DLL identity 变化 → true", () => {
    const entry = makePaddleOcrEntry({
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.21.0-new",
                model_revision: "ppocrv6-tiny",
            },
            loaded_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0-old",
                model_revision: "ppocrv6-tiny",
            },
            pending_restart: true,
        }),
    });
    assert.equal(isPendingRestart(entry), true,
        "DLL identity 变化时 pending_restart 为 true");
});

await test("isPendingRestart: 只切模型 generation → false", () => {
    const entry = makePaddleOcrEntry({
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0-same",
                model_revision: "ppocrv6-tiny-v2",
            },
            loaded_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0-same",
                model_revision: "ppocrv6-tiny-v1",
            },
            pending_restart: false,
        }),
    });
    assert.equal(isPendingRestart(entry), false,
        "DLL identity 不变时 pending_restart 为 false（只切模型不要求重启）");
});

await test("hasDeploymentMismatch: DLL 不一致 → true", () => {
    const entry = makePaddleOcrEntry({
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.21.0",
                model_revision: "ppocrv6-tiny",
            },
            loaded_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0",
                model_revision: "ppocrv6-tiny",
            },
        }),
    });
    assert.equal(hasDeploymentMismatch(entry), true,
        "DLL identity 不一致时 mismatch 为 true");
});

await test("hasDeploymentMismatch: 只切模型 generation → false", () => {
    const entry = makePaddleOcrEntry({
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0-same",
                model_revision: "ppocrv6-tiny-v2",
            },
            loaded_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0-same",
                model_revision: "ppocrv6-tiny-v1",
            },
        }),
    });
    assert.equal(hasDeploymentMismatch(entry), false,
        "只切模型 generation 不算 mismatch");
});

await test("hasDeploymentMismatch: loaded=null → false（首次安装未加载不算 mismatch）", () => {
    const entry = makePaddleOcrEntry({
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0",
                model_revision: "ppocrv6-tiny",
            },
            loaded_deployment: null,
        }),
    });
    assert.equal(hasDeploymentMismatch(entry), false,
        "首次安装 loaded=null 不算 mismatch");
});

// ── 4. legacy deployment ───────────────────────────────────────────────────

await test("getLegacyDeployment: 有旧版 Python 环境时可查询", () => {
    const entry = makePaddleOcrEntry({
        backend: makeOnnxDeploymentBackend({
            legacy_deployment: {
                runtime_kind: "python_venv",
                path_display: "C:\\Users\\test\\.blink\\paddleocr-venv",
                size_bytes: 3 * 1024 * 1024 * 1024,
            },
        }),
    });
    const legacy = getLegacyDeployment(entry);
    assert.ok(legacy, "legacy_deployment 不应为 null");
    assert.equal(legacy.runtime_kind, "python_venv");
    assert.equal(legacy.size_bytes, 3 * 1024 * 1024 * 1024);
});

await test("getLegacyDeployment: 无 legacy 时返回 null", () => {
    const entry = makePaddleOcrEntry({
        backend: makeOnnxDeploymentBackend({
            legacy_deployment: null,
        }),
    });
    assert.equal(getLegacyDeployment(entry), null);
});

// ── 5. 安装失败不乐观显示 Ready ─────────────────────────────────────────────

await test("computeEngineSummary: desired != null 但 loaded = null → 不显示 Ready/运行中", () => {
    const entry = makePaddleOcrEntry({
        environment: "ready",
        process: processState.stopped(),
        available: false,
        model: "not_loaded",
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0",
                model_revision: "ppocrv6-tiny",
            },
            loaded_deployment: null,
            pending_restart: false,
        }),
    });
    const summary = computeEngineSummary(entry, null);
    // 不应是 "ok" tone（不乐观显示 Ready）
    assert.notEqual(summary.tone, "ok", "loaded=null 时不应乐观显示运行中");
    assert.ok(
        !summary.text.includes("运行中"),
        `summary 不应包含"运行中"，实际: ${summary.text}`,
    );
});

await test("computeEngineSummary: pending_restart → 显示待重启", () => {
    const entry = makePaddleOcrEntry({
        environment: "ready",
        process: processState.stopped(),
        available: false,
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.21.0",
                model_revision: "ppocrv6-tiny",
            },
            loaded_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0",
                model_revision: "ppocrv6-tiny",
            },
            pending_restart: true,
        }),
    });
    const summary = computeEngineSummary(entry, null);
    assert.equal(summary.tone, "warn", "pending_restart 时 tone 应为 warn");
    assert.ok(
        summary.text.includes("待重启"),
        `summary 应包含"待重启"，实际: ${summary.text}`,
    );
});

await test("computeEngineSummary: available=true 且无 pending → 显示运行中", () => {
    const entry = makePaddleOcrEntry({
        environment: "ready",
        process: processState.running(1234),
        available: true,
        service: "healthy",
        model: "ready",
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0",
                model_revision: "ppocrv6-tiny",
            },
            loaded_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0",
                model_revision: "ppocrv6-tiny",
            },
            pending_restart: false,
        }),
    });
    const summary = computeEngineSummary(entry, null);
    assert.equal(summary.tone, "ok");
    assert.ok(
        summary.text.includes("运行中"),
        `summary 应包含"运行中"，实际: ${summary.text}`,
    );
});

// ── 6. pending_restart 时不说"运行中" ───────────────────────────────────────

await test("computeEngineSummary: available=true 但 pending_restart → 不说运行中", () => {
    // 即使 available=true，pending_restart 优先级高于"运行中"
    const entry = makePaddleOcrEntry({
        environment: "ready",
        process: processState.running(1234),
        available: true,
        service: "healthy",
        model: "ready",
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.21.0",
                model_revision: "ppocrv6-tiny",
            },
            loaded_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0",
                model_revision: "ppocrv6-tiny",
            },
            pending_restart: true,
        }),
    });
    const summary = computeEngineSummary(entry, null);
    assert.ok(
        !summary.text.includes("运行中"),
        `pending_restart 时不应显示"运行中"，实际: ${summary.text}`,
    );
    assert.equal(summary.tone, "warn");
});

// ── 7. computeFeedback: pending_restart 显示"重启后生效" ──────────────────

await test("computeFeedback: pending_restart → 显示重启后生效", () => {
    const entry = makePaddleOcrEntry({
        environment: "ready",
        process: processState.stopped(),
        available: false,
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.21.0",
                model_revision: "ppocrv6-tiny",
            },
            loaded_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0",
                model_revision: "ppocrv6-tiny",
            },
            pending_restart: true,
        }),
    });
    const feedback = computeFeedback(entry, null);
    assert.equal(feedback.tone, "warn");
    assert.ok(
        feedback.text.includes("重启"),
        `feedback 应包含"重启"，实际: ${feedback.text}`,
    );
});

await test("computeFeedback: 只切模型 generation → 不显示重启提示", () => {
    const entry = makePaddleOcrEntry({
        environment: "ready",
        process: processState.stopped(),
        available: false,
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0-same",
                model_revision: "ppocrv6-tiny-v2",
            },
            loaded_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0-same",
                model_revision: "ppocrv6-tiny-v1",
            },
            pending_restart: false,
        }),
    });
    const feedback = computeFeedback(entry, null);
    assert.notEqual(feedback.tone, "warn",
        "只切模型 generation 时 feedback 不应为 warn（不提示重启）");
    assert.ok(
        !feedback.text.includes("重启"),
        `只切模型不应提示重启，实际: ${feedback.text}`,
    );
});

// ── 8. computeFeedback: legacy 存在时提示可清理 ────────────────────────────

await test("computeFeedback: legacy 存在且环境 ready → 提示旧版环境可清理", () => {
    const entry = makePaddleOcrEntry({
        environment: "ready",
        process: processState.stopped(),
        available: false,
        backend: makeOnnxDeploymentBackend({
            desired_deployment: {
                runtime_kind: "onnx_runtime",
                dll_identity: "onnxruntime-1.20.0",
                model_revision: "ppocrv6-tiny",
            },
            loaded_deployment: null,
            pending_restart: false,
            legacy_deployment: {
                runtime_kind: "python_venv",
                size_bytes: 2 * 1024 * 1024 * 1024,
            },
        }),
    });
    const feedback = computeFeedback(entry, null);
    assert.ok(
        feedback.text.includes("旧版") || feedback.text.includes("Python"),
        `feedback 应提示旧版 Python 环境，实际: ${feedback.text}`,
    );
});

await test("computeFeedback: 无 legacy → 不提示旧版环境", () => {
    const entry = makePaddleOcrEntry({
        environment: "ready",
        process: processState.stopped(),
        available: false,
        backend: makeOnnxDeploymentBackend({
            legacy_deployment: null,
        }),
    });
    const feedback = computeFeedback(entry, null);
    assert.ok(
        !feedback.text.includes("旧版") && !feedback.text.includes("Python"),
        `无 legacy 时不应提示旧版环境，实际: ${feedback.text}`,
    );
});

// ── 9. computeKeyline: PaddleOCR 有 ONNX Runtime 运行时行 ───────────────────

await test("computeKeyline: PaddleOCR 包含 ONNX Runtime 运行时行", () => {
    const entry = makePaddleOcrEntry();
    const items = computeKeyline(entry, null);
    const runtimeItem = items.find(
        (i) => i.label === "运行时" || i.label === "Runtime",
    );
    assert.ok(runtimeItem, "PaddleOCR keyline 应包含运行时行");
    assert.ok(
        runtimeItem.value.includes("ONNX") || runtimeItem.value.includes("onnx"),
        `运行时值应包含 ONNX，实际: ${runtimeItem.value}`,
    );
});

await test("computeKeyline: 非 PaddleOCR 引擎不包含运行时行", () => {
    let state = createInitialState();
    state = setCatalog(state, [{...paddleocrCatalog, engine_id: "funasr", runtime_kind: "python_venv"}]);
    state = mergeStatus(state, makeStatus({
        engine_id: "funasr",
        service_epoch: "epoch-funasr",
        revision: "1",
        status: {...READY_BASE, operation: idleOperation()},
    }));
    const entry = getEntry(state, "funasr");
    const items = computeKeyline(entry, null);
    const runtimeItem = items.find(
        (i) => i.label === "运行时" || i.label === "Runtime",
    );
    assert.equal(runtimeItem, undefined, "非 PaddleOCR 引擎不应有运行时行");
});

// ── 10. ONNX 诊断 fixture 结构正确 ──────────────────────────────────────────

await test("makeOnnxDiagnostics: 包含所有 E 包要求的状态字段", () => {
    const diag = makeOnnxDiagnostics();
    assert.equal(diag.engine_id, "paddleocr");

    // 检查 adapter_diagnostics 包含所有要求的字段
    const keys = diag.adapter_diagnostics.map((d) => d.key);
    assert.ok(keys.includes("paddleocr_installed"), "应包含 paddleocr_installed");
    assert.ok(keys.includes("paddleocr_service_state"), "应包含 paddleocr_service_state");
    assert.ok(keys.includes("paddleocr_model_state"), "应包含 paddleocr_model_state");
    assert.ok(keys.includes("paddleocr_model_id"), "应包含 paddleocr_model_id");
    assert.ok(keys.includes("paddleocr_model_revision"), "应包含 paddleocr_model_revision");
    assert.ok(keys.includes("paddleocr_instance_id"), "应包含 paddleocr_instance_id");
    assert.ok(keys.includes("paddleocr_actual_backend"), "应包含 paddleocr_actual_backend");

    // 验证值
    const getId = (key) => diag.adapter_diagnostics.find((d) => d.key === key)?.value;
    assert.equal(getId("paddleocr_model_id"), "PP-OCRv6");
    assert.equal(getId("paddleocr_model_revision"), "ppocrv6-tiny");
    assert.equal(getId("paddleocr_actual_backend"), "onnx-ocr");
    assert.equal(getId("paddleocr_instance_id"), "—");
});

await test("makeOnnxDeploymentBackend: 默认 desired != null, loaded = null, pending_restart = false", () => {
    const backend = makeOnnxDeploymentBackend();
    assert.ok(backend.desired_deployment, "默认应有 desired_deployment");
    assert.equal(backend.loaded_deployment, null, "默认 loaded 应为 null");
    assert.equal(backend.pending_restart, false, "默认 pending_restart 应为 false");
    assert.equal(backend.legacy_deployment, null, "默认 legacy 应为 null");
    assert.equal(backend.desired_deployment.runtime_kind, "onnx_runtime");
    assert.equal(backend.desired_deployment.model_revision, "ppocrv6-tiny");
});

// ── 汇总 ─────────────────────────────────────────────────────────────────────

console.log(`\n${passCount}/${testCount} tests passed.`);
if (passCount !== testCount) {
    process.exit(1);
}
console.log("local-engine-0228e tests passed");
