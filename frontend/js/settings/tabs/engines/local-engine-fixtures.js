/**
 * Mock fixtures for local engine state tests.
 *
 * 字段名严格遵循 H1/H2 DTO 契约（dto.rs），不发明另一套。
 * 供 local-engine-state.test.mjs 和开发期 mock 使用。
 */

// ── Catalog item (EngineCatalogItem) ────────────────────────────────────────

export const funasrCatalog = {
    engine_id: "funasr",
    display_name: "FunASR",
    description: "本地语音识别",
    icon: "mic",
    version: "0.1.0",
    capability_kind: "stt",
    runtime_kind: "python_venv",
    lifecycle: "manual",
    model_id: "iic/SenseVoiceSmall",
    model_revision: "v1",
    resource_budget: {
        estimated_env_disk_mb: 3000,
        estimated_model_disk_mb: 234,
        estimated_stable_ram_mb: 500,
        estimated_peak_ram_mb: 1500,
    },
    compute_options: [
        {preference: "cpu", profile_id: "cpu-x64", backend: "cpu", compatible: true, disabled_reason: null},
        {preference: "cuda", profile_id: "cuda12", backend: "cuda", compatible: false, disabled_reason: "本机无 CUDA GPU"},
    ],
    current_compute_preference: "cpu",
};

export const paddleocrCatalog = {
    engine_id: "paddleocr",
    display_name: "PP-OCRv6",
    description: "本地文字识别 (ONNX Runtime)",
    icon: "scan-text",
    version: "0.22.8",
    capability_kind: "ocr",
    // 0.22.8: 从 python_venv 改为 onnx_runtime
    runtime_kind: "onnx_runtime",
    lifecycle: "on_demand",
    model_id: "PP-OCRv6",
    model_revision: "ppocrv6-tiny",
    resource_budget: {
        // ORT DLL ~8MB + det/rec/dict ~10MB
        estimated_env_disk_mb: 10,
        estimated_model_disk_mb: 10,
        estimated_stable_ram_mb: 410,
        estimated_peak_ram_mb: 1140,
    },
    compute_options: [
        {preference: "cpu", profile_id: "cpu-x64", backend: "cpu", compatible: true, disabled_reason: null},
    ],
    current_compute_preference: "cpu",
};

export function makeCatalog() {
    return [structuredClone(funasrCatalog), structuredClone(paddleocrCatalog)];
}

// ── Status snapshot (EngineStatusDto) ─────────────────────────────────────────

/**
 * 构建 status DTO。
 * wire shape: { engine_id, service_epoch, revision, status: EngineStatusWire }
 * EngineStatusWire: { desired, operation, environment, process, service, model, available, backend, last_error }
 *
 * **process 使用 ProcessStateDto shape**：
 *   { state: "stopped" | "starting" | "running" | "stopping" | "exited", pid?: number, reason?: string }
 *
 * `available` 是后端推导的一键可用性（desired=running && service 可用 && model ready），
 * 前端只消费不推导。
 */
export function makeStatus(overrides = {}) {
    const base = {
        engine_id: "funasr",
        service_epoch: "epoch-0011223344556677",
        revision: "1",
        status: {
            desired: "stopped",
            operation: {kind: "idle", operation_id: "", stage: "pending", cancellable: false},
            environment: "missing",
            process: {state: "stopped"},
            service: "unknown",
            model: "unknown",
            available: false,
            backend: {
                requested_preference: "cpu",
                resolved_profile: null,
                backend_verification: {
                    state: "pending",
                    expected_backend: "cpu",
                    actual_backend: null,
                    device_name: null,
                    mismatch_reason: null,
                },
                fallback_reasons: [],
            },
            last_error: null,
        },
    };
    return deepMerge(base, overrides);
}

// ── ProcessStateDto 便捷工厂 ─────────────────────────────────────────────────

/** 进程状态 DTO 工厂——与后端 dto.rs project_process_state 输出 shape 一致。 */
export const processState = {
    stopped: () => ({state: "stopped"}),
    starting: () => ({state: "starting"}),
    running: (pid = 1234) => ({state: "running", pid}),
    stopping: () => ({state: "stopping"}),
    exited: (reason = "exit code 1") => ({state: "exited", reason}),
};

// ── Log entry (EngineLogDto) ──────────────────────────────────────────────────

export function makeLog(engineId, instanceId, seq, text, overrides = {}) {
    return deepMerge({
        engine_id: engineId,
        instance_id: instanceId,
        operation_id: null,
        seq: String(seq),
        timestamp: "2026-08-26T00:00:00Z",
        level: "info",
        text: text,
    }, overrides);
}

// ── Storage (EngineStorageDto) ────────────────────────────────────────────────

export function makeStorage(engineId, overrides = {}) {
    return deepMerge({
        engine_id: engineId,
        targets: [
            {
                target_id: "environment:slot-a",
                kind: "engine_environment",
                engine_id: engineId,
                label_key: "local_engine.storage.engine_environment",
                label_fallback: "当前环境",
                size_bytes: 3000 * 1024 * 1024,
                current: true,
                removable: false,
                shared: false,
                requires_separate_confirmation: false,
                blocked_reason: "current_environment",
            },
            {
                target_id: "environment:slot-b",
                kind: "engine_environment",
                engine_id: engineId,
                label_key: "local_engine.storage.engine_environment",
                label_fallback: "残留环境",
                size_bytes: 2900 * 1024 * 1024,
                current: false,
                removable: true,
                shared: false,
                requires_separate_confirmation: false,
                blocked_reason: null,
            },
        ],
        total_size_bytes: 5900 * 1024 * 1024,
        releasable_size_bytes: 2900 * 1024 * 1024,
    }, overrides);
}

// ── ONNX OCR 诊断字段 fixture (0.22.8-E) ─────────────────────────────────────

/**
 * 构造 PaddleOCR ONNX 诊断 DTO mock。
 *
 * 诊断 DTO 来自 get_engine_diagnostics command，在 0.22.8-D 后 paddleocr
 * 引擎的 adapter_diagnostics 包含 ONNX executor 投影的状态字段。
 *
 * 关键字段（来自 D 包回报）：
 * - paddleocr_installed: executor 存在即已安装
 * - paddleocr_service_state: executor 状态字符串（Idle/Starting/Ready/Stopping/Failed/NotInstalled）
 * - paddleocr_model_state: 模型状态（Ready/Loading/Failed/Idle/NotInstalled）
 * - paddleocr_model_id: "PP-OCRv6"
 * - paddleocr_model_revision: "ppocrv6-tiny"
 * - paddleocr_instance_id: None（in-process，无 PID）
 * - paddleocr_actual_backend: "onnx-ocr"
 */
export function makeOnnxDiagnostics(overrides = {}) {
    return deepMerge({
        engine_id: "paddleocr",
        environment: "ready",
        process: {state: "stopped"},
        service: "healthy",
        model: "not_loaded",
        adapter_diagnostics: [
            {key: "paddleocr_installed", value: "true", label: "info"},
            {key: "paddleocr_service_state", value: "Idle", label: "info"},
            {key: "paddleocr_model_state", value: "NotInstalled", label: "info"},
            {key: "paddleocr_model_id", value: "PP-OCRv6", label: "info"},
            {key: "paddleocr_model_revision", value: "ppocrv6-tiny", label: "info"},
            {key: "paddleocr_instance_id", value: "—", label: "info"},
            {key: "paddleocr_actual_backend", value: "onnx-ocr", label: "info"},
        ],
        recent_logs: [],
        orphan_recovery: {present: false, actionable: false, reason: ""},
    }, overrides);
}

// ── ONNX deployment identity fixture (0.22.8-E) ───────────────────────────────

/**
 * 构造 ONNX deployment identity mock（附加在 status.backend 中）。
 *
 * 0.22.8-D desired/loaded deployment identity 通过 status.backend 字段传递：
 * - desired_deployment: { runtime_kind, dll_identity, model_revision }
 * - loaded_deployment: { runtime_kind, dll_identity, model_revision } | null
 * - pending_restart: boolean（DLL identity 变化时为 true）
 * - legacy_deployment: { runtime_kind: "python_venv", path_display?, size_bytes? } | null
 *
 * 只切模型 generation 不触发 pending_restart（DLL 不变）。
 */
export function makeOnnxDeploymentBackend(overrides = {}) {
    return deepMerge({
        requested_preference: "cpu",
        resolved_profile: {
            profile_id: "cpu-x64",
            backend: "cpu",
            artifact_id: "onnxruntime-1.20.0",
            priority: 0,
        },
        backend_verification: {
            state: "pending",
            expected_backend: "cpu",
            actual_backend: null,
            device_name: null,
            mismatch_reason: null,
        },
        fallback_reasons: [],
        // 0.22.8-E: deployment identity（desired/loaded/pending_restart/legacy）
        desired_deployment: {
            runtime_kind: "onnx_runtime",
            dll_identity: "onnxruntime-1.20.0-sha256",
            model_revision: "ppocrv6-tiny",
        },
        loaded_deployment: null,
        pending_restart: false,
        legacy_deployment: null,
    }, overrides);
}

// ── helpers ───────────────────────────────────────────────────────────────────

/**
 * 构造 ModelCatalogItemDto mock。
 * @param {Object} overrides
 */
export function makeModel(overrides = {}) {
    return deepMerge({
        engine_id: "funasr",
        model_id: "iic/SenseVoiceSmall",
        display_name: "SenseVoiceSmall",
        estimated_size_mb: 234,
        install_state: "installed",
        verification_state: "verified",
        compatibility: "compatible",
        is_selected: false,
        is_active: false,
        model_revision: "v1",
        descriptor_model_id: "iic/SenseVoiceSmall",
    }, overrides);
}

/**
 * 构造 EnginePreferencesDto mock。
 * @param {Object} overrides
 */
export function makePreferences(overrides = {}) {
    return deepMerge({
        engine_id: "funasr",
        compute_preference: "cpu",
        auto_start: false,
        lifecycle: null,
        requires_rebuild: false,
    }, overrides);
}

function deepMerge(target, source) {
    // null/undefined → 直接替换
    if (source === null || source === undefined) {
        return structuredClone(source);
    }
    // 数组 → 替换，不逐元素合并
    if (Array.isArray(source)) {
        return structuredClone(source);
    }
    // 基本类型 → 直接替换
    if (typeof source !== "object") {
        return source;
    }
    // 对象：如果 target 是对象则逐 key 合并，否则直接替换
    if (typeof target === "object" && target !== null && !Array.isArray(target)) {
        const targetKeys = new Set(Object.keys(target));
        const sourceKeys = Object.keys(source);
        // 如果 keys 完全不重叠 → 互斥 enum wire shape → 完全替换
        const hasOverlap = sourceKeys.some((k) => targetKeys.has(k));
        if (!hasOverlap && sourceKeys.length > 0) {
            return structuredClone(source);
        }
        const result = structuredClone(target);
        for (const key of sourceKeys) {
            if (key in target) {
                result[key] = deepMerge(target[key], source[key]);
            } else {
                result[key] = structuredClone(source[key]);
            }
        }
        return result;
    }
    return structuredClone(source);
}
