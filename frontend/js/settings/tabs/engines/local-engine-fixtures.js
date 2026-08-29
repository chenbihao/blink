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
    cleanup_summary: {
        owned_subdirs: ["generations", "logs"],
        has_model_cache: true,
        has_log_dir: true,
    },
};

export const paddleocrCatalog = {
    engine_id: "paddleocr",
    display_name: "PP-OCRv6",
    description: "本地文字识别",
    icon: "scan-text",
    version: "0.1.0",
    capability_kind: "ocr",
    runtime_kind: "python_venv",
    lifecycle: "on_demand",
    model_id: "PP-OCRv6",
    model_revision: "v1",
    resource_budget: {
        estimated_env_disk_mb: 2000,
        estimated_model_disk_mb: 300,
        estimated_stable_ram_mb: 600,
        estimated_peak_ram_mb: 1136,
    },
    compute_options: [
        {preference: "auto", profile_id: "cpu-auto", backend: "cpu", compatible: true, disabled_reason: null},
        {preference: "cpu", profile_id: "cpu-x64", backend: "cpu", compatible: true, disabled_reason: null},
    ],
    current_compute_preference: "auto",
    cleanup_summary: {
        owned_subdirs: ["generations", "logs"],
        has_model_cache: true,
        has_log_dir: true,
    },
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
                target_id: "gen:current",
                kind: "engine_generation",
                engine_id: engineId,
                label_key: "local_engine.storage.engine_generation",
                label_fallback: "当前环境",
                size_bytes: 3000 * 1024 * 1024,
                current: true,
                previous: false,
                removable: false,
                shared: false,
                requires_separate_confirmation: false,
                blocked_reason: "current_generation",
            },
            {
                target_id: "gen:old",
                kind: "engine_generation",
                engine_id: engineId,
                label_key: "local_engine.storage.engine_generation",
                label_fallback: "上一环境",
                size_bytes: 2900 * 1024 * 1024,
                current: false,
                previous: true,
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
