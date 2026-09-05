/**
 * 受限 adapter hooks —— FunASR 与 PaddleOCR 的引擎专属配置入口（0.22.6，中密度重设计）。
 *
 * 通用 renderer 只处理生命周期（install/start/stop/repair/cleanup/cancel）。
 * FunASR/PaddleOCR 的配置保存通过按 engine_id 注册的受限 hook 完成，
 * 但不能复制 card lifecycle。
 *
 * ## 受限边界
 *
 * - FunASR compute preference / auto_start 映射现有受限 preferences command。
 * - PaddleOCR compute preference / ocr_backend / lifecycle 映射同一 command。
 * - 单一可用 compute profile 渲染静态文本，不显示虚假可选下拉。
 * - 当前模型为只读展示（selected/active 三身份不混淆）——模型切换归
 *   语音页/引擎页模型列表的受限选择路径，本区不复制第二套真源。
 * - renderer 绝不允许任意 engine id 动态注入 HTML、command 或字段路径。
 *
 * ## 布局约定
 *
 * 配置区由若干 `.le-config-group`（label + control）横向换行排列；
 * renderer 只在控件结构变化时整体渲染；preferences / selected / active
 * 通过 syncConfig 原位同步，避免替换当前焦点控件。
 *
 * @module local-engine-hooks
 */

import {registerAdapterHook, unregisterAdapterHook} from "./local-engine-card.js";
import {t} from "../../../i18n/index.js";
import {computeOptionsDisplayMode} from "./local-engine-state.js";
import {
    isPendingRestart,
    getDesiredDeployment,
    getLoadedDeployment,
    getLegacyDeployment,
} from "./local-engine-state.js";
import {getSelection} from "./local-engine-selection.js";

// ── 公共构造 ──────────────────────────────────────────────────────────────────

/** 构造一个配置组（label + control 横排）。 */
function makeGroup(labelText) {
    const group = document.createElement("div");
    group.className = "le-config-group";

    const label = document.createElement("span");
    label.className = "le-config-label";
    label.textContent = labelText;
    group.appendChild(label);
    return group;
}

/** compute preference 显示名。 */
function computeLabel(preference) {
    return t(`local_engine.compute.${preference}`, preference);
}

/**
 * active implementation wire 值 → 只读诊断文案。
 *
 * 真源是 `status.status.active_implementation`（0.22.9 只读字段，
 * start 冻结快照提交）。null/缺省 = 未运行。前端只做展示映射，
 * 不提供任何选择/提交入口。
 */
export function activeImplementationLabel(entry) {
    const impl = entry?.status?.status?.active_implementation;
    if (!impl) {
        const label = t("local_engine.implementation.none");
        return label !== "local_engine.implementation.none" ? label : "未运行";
    }
    const key = `local_engine.implementation.${impl}`;
    const label = t(key);
    if (label !== key) return label;
    const map = {
        funasr_gguf_worker: "GGUF worker（常驻）",
        paddleocr_onnx_in_process: "ONNX in-process",
    };
    return map[impl] || impl;
}

/**
 * FunASR 运行时诊断只读组（0.22.9 Handoff 09）。
 *
 * 展示 runtime 种类（引擎 descriptor 声明）与 active implementation
 * （start 冻结）。均只读——用户不选择技术底座，卡片不提供
 * GGUF/ONNX 或 True/Pseudo 技术开关。
 */
function appendFunasrRuntimeGroup(container, entry) {
    const runtimeLabel = t("local_engine.config.runtime_diag");
    const group = makeGroup(runtimeLabel !== "local_engine.config.runtime_diag"
        ? runtimeLabel : "运行时");
    group.className += " le-config-group-runtime";

    const value = document.createElement("span");
    value.className = "le-config-static le-runtime-diag";
    value.textContent = activeImplementationLabel(entry);
    group.appendChild(value);
    container.appendChild(group);
}

/** 原位同步 FunASR 运行时诊断组。 */
function syncFunasrRuntimeGroup(container, entry) {
    const el = container.querySelector(".le-runtime-diag");
    const next = activeImplementationLabel(entry);
    if (el && el.textContent !== next) el.textContent = next;
}

/**
 * 渲染"当前模型"只读组 + selected≠active 待重启提示。
 * 当前模型的切换入口在模型列表/语音页，本区不复制选择命令。
 */
function appendCurrentModelGroup(container, entry) {
    const models = entry.models;
    const selected = Array.isArray(models) ? models.find((m) => m.is_selected) : null;
    const active = Array.isArray(models) ? models.find((m) => m.is_active) : null;
    const name = (selected && (selected.display_name || selected.model_id))
        || entry.catalog?.model_id
        || "—";

    const group = makeGroup(t("local_engine.config.current_model"));
    group.className += " le-config-group-model";
    const value = document.createElement("span");
    value.className = "le-config-static le-current-model-value";
    value.textContent = name;
    group.appendChild(value);
    container.appendChild(group);

    // 节点始终保留，状态变化只切 hidden，避免重建整个配置区。
    const hint = document.createElement("span");
    hint.className = "le-config-mismatch";
    hint.textContent = t("local_engine.model.mismatch_hint");
    hint.hidden = !(selected && active && selected.model_id !== active.model_id);
    container.appendChild(hint);
}

/** 渲染 requires_rebuild 提示（保存 compute 偏好后环境待重建）。 */
function appendRebuildHint(container, prefs) {
    const hint = document.createElement("span");
    hint.className = "le-config-rebuild-hint";
    hint.textContent = t("local_engine.config.requires_rebuild_hint");
    hint.hidden = prefs?.requires_rebuild !== true;
    container.appendChild(hint);
}

/** 原位同步所有引擎共用的模型、compute 与待重建状态。 */
function syncCommonConfig(container, entry) {
    const models = entry.models;
    const selected = Array.isArray(models) ? models.find((m) => m.is_selected) : null;
    const active = Array.isArray(models) ? models.find((m) => m.is_active) : null;
    const modelValue = container.querySelector(".le-current-model-value");
    if (modelValue) {
        modelValue.textContent = (selected && (selected.display_name || selected.model_id))
            || entry.catalog?.model_id
            || "—";
    }
    const mismatch = container.querySelector(".le-config-mismatch");
    if (mismatch) mismatch.hidden = !(selected && active && selected.model_id !== active.model_id);

    const preference = entry.preferences?.compute_preference
        || entry.catalog?.current_compute_preference
        || "auto";
    const computeSelect = container.querySelector(".le-compute-select");
    if (computeSelect) {
        if (computeSelect.value !== preference) computeSelect.value = preference;
        computeSelect.dataset.savedValue = preference;
    }
    const computeStatic = container.querySelector(".le-compute-static");
    if (computeStatic) computeStatic.textContent = computeLabel(preference);

    const rebuildHint = container.querySelector(".le-config-rebuild-hint");
    if (rebuildHint) rebuildHint.hidden = entry.preferences?.requires_rebuild !== true;
}

/**
 * 渲染 compute preference 组：单一可用 profile → 静态文本；
 * 两个及以上真实可用候选 → select（保存走受限 command，失败回滚）。
 */
function appendComputeGroup(container, entry, engineId, controller) {
    const catalog = entry.catalog;
    const prefs = entry.preferences;
    const current = prefs?.compute_preference || catalog.current_compute_preference || "auto";
    const mode = computeOptionsDisplayMode(catalog.compute_options);

    const group = makeGroup(t("local_engine.config.compute_preference"));

    if (mode === "static") {
        // 单一可用选项：只读展示，避免制造"可以选择 CUDA"的错觉
        const staticValue = document.createElement("span");
        staticValue.className = "le-config-static le-compute-static";
        staticValue.textContent = computeLabel(current);
        group.appendChild(staticValue);
        container.appendChild(group);
        return;
    }

    const select = document.createElement("select");
    select.className = "le-config-select le-compute-select";
    select.dataset.savedValue = current;
    for (const opt of catalog.compute_options) {
        const option = document.createElement("option");
        option.value = opt.preference;
        option.textContent = computeLabel(opt.preference);
        option.disabled = !opt.compatible;
        if (opt.disabled_reason) option.title = opt.disabled_reason;
        if (opt.preference === current) option.selected = true;
        select.appendChild(option);
    }
    select.addEventListener("change", async (e) => {
        const next = e.target.value;
        const prev = select.dataset.savedValue || current;
        select.disabled = true;
        try {
            const saved = await controller.savePreferences(engineId, {compute_preference: next});
            const accepted = saved?.compute_preference || next;
            select.dataset.savedValue = accepted;
            select.value = accepted;
            if (controller?.isMounted()) {
                controller.refreshStatus().catch(() => {});
            }
        } catch (err) {
            console.error(`[${engineId}-hook] save compute preference failed:`, err);
            select.value = prev;
        } finally {
            select.disabled = false;
        }
    });
    group.appendChild(select);
    container.appendChild(group);
}

// ── FunASR hook ───────────────────────────────────────────────────────────────

/**
 * 注册 FunASR 受限 adapter hook。
 *
 * 配置组：当前模型（只读）/ 计算设备 / 自动启动开关。
 * 保存走闭合命令 `set_local_engine_preferences`，失败回滚。
 */
function registerFunasrHook() {
    registerAdapterHook("funasr", {
        renderConfig(container, entry, controller) {
            if (!container) return;
            container.textContent = "";
            const catalog = entry.catalog;
            if (!catalog) return;

            const prefs = entry.preferences;
            const autoStart = prefs?.auto_start ?? false;

            // 当前模型（只读）+ 待重启提示
            appendCurrentModelGroup(container, entry);

            // 运行时诊断（只读：runtime 种类 + active implementation）
            appendFunasrRuntimeGroup(container, entry);

            // 计算设备（FunASR descriptor 当前只声明 CPU → 静态文本）
            appendComputeGroup(container, entry, "funasr", controller);

            // 自动启动开关
            const autoGroup = makeGroup(t("local_engine.config.auto_start"));
            const switchLabel = document.createElement("label");
            switchLabel.className = "le-switch";
            switchLabel.title = t("local_engine.config.auto_start_hint");

            const toggle = document.createElement("input");
            toggle.type = "checkbox";
            toggle.className = "le-switch-input";
            toggle.checked = autoStart;
            toggle.dataset.savedValue = String(autoStart);
            // checkbox 视觉隐藏（le-switch-input），用 aria-label 提供可访问名称
            toggle.setAttribute("aria-label", t("local_engine.config.auto_start"));

            const track = document.createElement("span");
            track.className = "le-switch-track";
            track.setAttribute("aria-hidden", "true");

            switchLabel.appendChild(toggle);
            switchLabel.appendChild(track);
            autoGroup.appendChild(switchLabel);
            container.appendChild(autoGroup);

            toggle.addEventListener("change", async () => {
                const next = toggle.checked;
                const prev = toggle.dataset.savedValue === "true";
                toggle.disabled = true;
                try {
                    const saved = await controller.savePreferences("funasr", {auto_start: next});
                    const accepted = saved?.auto_start ?? next;
                    toggle.dataset.savedValue = String(accepted);
                    toggle.checked = accepted;
                } catch (err) {
                    console.error("[funasr-hook] save auto_start failed:", err);
                    toggle.checked = prev;
                } finally {
                    toggle.disabled = false;
                }
            });

            appendRebuildHint(container, prefs);
        },
        syncConfig(container, entry) {
            syncCommonConfig(container, entry);
            syncFunasrRuntimeGroup(container, entry);
            const toggle = container.querySelector(".le-switch-input");
            const next = entry.preferences?.auto_start ?? false;
            if (toggle) {
                if (toggle.checked !== next) toggle.checked = next;
                toggle.dataset.savedValue = String(next);
            }
        },
    });
}

// ── PaddleOCR hook ────────────────────────────────────────────────────────────

/**
 * 渲染 ONNX 资产状态组（ORT DLL / det / rec / dictionary 大小）。
 *
 * 0.22.8-E: 原位展示 CPU ONNX runtime 和资产真实大小。
 * 资产大小从 storage DTO 的 targets 中提取，
 * catalog.resource_budget 提供预算估计值。
 */
function appendOnnxAssetStatusGroup(container, entry) {
    const catalog = entry?.catalog;
    const storage = entry?.storage;
    if (!catalog || catalog.runtime_kind !== "onnx_runtime") return;

    const group = makeGroup(t("local_engine.config.onnx_assets", "ONNX 资产"));
    group.className += " le-config-group-onnx-assets";

    // 从 storage targets 提取各资产大小
    const targets = storage?.targets || [];
    const parts = [];

    // ORT DLL
    const ortTarget = targets.find((s) => s.kind === "engine_environment" && s.current);
    if (ortTarget && ortTarget.size_bytes > 0) {
        parts.push(`${t("local_engine.config.ort_runtime", "ORT")}: ${formatBytes(ortTarget.size_bytes)}`);
    } else {
        const envBudget = catalog.resource_budget?.estimated_env_disk_mb;
        if (envBudget != null) {
            parts.push(`${t("local_engine.config.ort_runtime", "ORT")}: ~${formatMB(envBudget)}`);
        }
    }

    // det / rec / dictionary 模型资产
    const modelTargets = targets.filter((s) => s.kind === "installed_model");
    if (modelTargets.length > 0) {
        for (const mt of modelTargets) {
            const label = mt.label_fallback || mt.target_id || "model";
            parts.push(`${label}: ${formatBytes(mt.size_bytes)}`);
        }
    } else {
        const modelBudget = catalog.resource_budget?.estimated_model_disk_mb;
        if (modelBudget != null) {
            parts.push(`${t("local_engine.config.models", "模型")}: ~${formatMB(modelBudget)}`);
        }
    }

    if (parts.length === 0) return;

    const value = document.createElement("span");
    value.className = "le-config-static le-onnx-assets";
    value.textContent = parts.join(" · ");
    group.appendChild(value);
    container.appendChild(group);
}

/** 格式化字节数（B/KB/MB/GB）。 */
function formatBytes(bytes) {
    if (!bytes || bytes <= 0) return "0 B";
    const mb = bytes / (1024 * 1024);
    if (mb < 1) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
    if (mb < 1024) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

/** 格式化 MB 数值。 */
function formatMB(mb) {
    if (mb == null) return "—";
    if (mb < 1024) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

/**
 * 渲染 ONNX deployment identity 组（desired / loaded / pending_restart）。
 *
 * 0.22.8-E: 展示真实 deployment 状态，不乐观显示 Ready。
 * - desired: 用户最近安装/更新提交的目标
 * - loaded: 当前主进程实际加载的（null=未初始化 ORT）
 * - pending_restart: DLL identity 变化，重启后生效
 *
 * 只切模型 generation 不触发 pending_restart——不能错误提示重启。
 */
function appendDeploymentIdentityGroup(container, entry) {
    const desired = getDesiredDeployment(entry);
    const loaded = getLoadedDeployment(entry);
    const pending = isPendingRestart(entry);

    if (!desired && !loaded) return; // 无 deployment 信息时不渲染

    const group = makeGroup(t("local_engine.config.deployment", "部署状态"));
    group.className += " le-config-group-deployment";

    const parts = [];
    if (desired) {
        parts.push(`${t("local_engine.config.desired_deployment", "已提交")}: ${desired.model_revision || "—"}`);
    }
    if (loaded) {
        parts.push(`${t("local_engine.config.loaded_deployment", "已加载")}: ${loaded.model_revision || "—"}`);
    } else if (desired) {
        parts.push(t("local_engine.config.not_loaded", "未加载"));
    }
    if (pending) {
        parts.push(t("local_engine.config.pending_restart", "待重启"));
    }

    const value = document.createElement("span");
    value.className = "le-config-static le-deployment-status";
    if (pending) {
        value.className += " le-deployment-pending";
    }
    value.textContent = parts.join(" · ");
    group.appendChild(value);
    container.appendChild(group);
}

/**
 * 渲染 legacy Python deployment 警告组。
 *
 * **铁则**：legacy 清理必须明确警告——删除后旧版 Blink 无法复用该 OCR 环境。
 * legacy 不参与运行时 fallback，只在维护中提供主动清理入口。
 */
function appendLegacyWarningGroup(container, entry) {
    const legacy = getLegacyDeployment(entry);
    if (!legacy) return;

    const group = makeGroup(t("local_engine.config.legacy_deployment", "旧版 Python 环境"));
    group.className += " le-config-group-legacy";

    const value = document.createElement("span");
    value.className = "le-config-static le-legacy-warning";
    value.textContent = t("local_engine.config.legacy_warning",
        "检测到旧版 Python OCR 环境。清理后旧版 Blink 无法复用此环境。");
    group.appendChild(value);

    // 展示 legacy 大小（如果有）
    if (legacy.size_bytes != null) {
        const sizeEl = document.createElement("span");
        sizeEl.className = "le-config-static le-legacy-size";
        const mb = legacy.size_bytes / (1024 * 1024);
        const sizeText = mb < 1024
            ? `${Math.round(mb)} MB`
            : `${(mb / 1024).toFixed(1)} GB`;
        sizeEl.textContent = `· ${t("local_engine.storage.actual", "实际占用")} ${sizeText}`;
        group.appendChild(sizeEl);
    }

    container.appendChild(group);
}

/**
 * 注册 PaddleOCR 受限 adapter hook（0.22.8-E ONNX 原位适配）。
 *
 * 0.22.8 变更：
 * - 不新增 ONNX OCR 卡片，仍使用一张 PaddleOCR 卡片
 * - 不提供 GGUF/ONNX/runtime 技术底座选择器
 * - 展示 ONNX runtime / ORT 与 det/rec/dictionary 资产状态
 * - 展示 desired/loaded deployment + pending restart
 * - 展示 legacy Python 空间 + 明确清理警告
 * - 安装/更新/回滚/重启提示/修复/清理复用既有受限 IPC
 *
 * 保存走闭合命令 `set_local_engine_preferences`，失败回滚。
 */
function registerPaddleOcrHook() {
    registerAdapterHook("paddleocr", {
        renderConfig(container, entry, controller) {
            if (!container) return;
            container.textContent = "";
            const catalog = entry.catalog;
            if (!catalog) return;

            const prefs = entry.preferences;
            const currentBackend = prefs?.ocr_backend || "windows";
            const currentLifecycle = prefs?.lifecycle || catalog.lifecycle || "on_demand";

            // 当前模型（只读）+ 待重启提示
            appendCurrentModelGroup(container, entry);

            // 0.22.8-E: ONNX 资产状态（ORT / det / rec / dict 大小）
            appendOnnxAssetStatusGroup(container, entry);

            // 0.22.8-E: deployment identity（desired / loaded / pending_restart）
            appendDeploymentIdentityGroup(container, entry);

            // 0.22.8-E: legacy Python 空间 + 清理警告
            appendLegacyWarningGroup(container, entry);

            // OCR 路由后端
            const backendGroup = makeGroup(t("local_engine.config.ocr_backend"));
            const backendSelect = document.createElement("select");
            backendSelect.className = "le-config-select le-ocr-backend-select";
            backendSelect.dataset.savedValue = currentBackend;
            for (const backend of ["windows", "paddleocr", "auto"]) {
                const option = document.createElement("option");
                option.value = backend;
                option.textContent = t(`local_engine.ocr_backend.${backend}`, backend);
                option.selected = backend === currentBackend;
                backendSelect.appendChild(option);
            }
            backendSelect.addEventListener("change", async (event) => {
                const next = event.target.value;
                const prev = backendSelect.dataset.savedValue || currentBackend;
                backendSelect.disabled = true;
                try {
                    const saved = await controller.savePreferences("paddleocr", {ocr_backend: next});
                    const accepted = saved?.ocr_backend || next;
                    backendSelect.dataset.savedValue = accepted;
                    backendSelect.value = accepted;
                } catch (err) {
                    console.error("[paddleocr-hook] save OCR backend failed:", err);
                    backendSelect.value = prev;
                } finally {
                    backendSelect.disabled = false;
                }
            });
            backendGroup.appendChild(backendSelect);
            container.appendChild(backendGroup);

            // 计算设备（catalog 声明 cpu 单选项 → 静态文本）
            appendComputeGroup(container, entry, "paddleocr", controller);

            // 运行策略（生命周期）
            const lifecycleGroup = makeGroup(t("local_engine.config.lifecycle"));
            const lifecycleSelect = document.createElement("select");
            lifecycleSelect.className = "le-config-select le-lifecycle-select";
            lifecycleSelect.dataset.savedValue = currentLifecycle;
            for (const opt of ["on_demand", "keep_running", "stop_after_use"]) {
                const option = document.createElement("option");
                option.value = opt;
                option.textContent = t(`local_engine.lifecycle.${opt}`, opt);
                if (currentLifecycle === opt) option.selected = true;
                lifecycleSelect.appendChild(option);
            }
            lifecycleSelect.addEventListener("change", async (event) => {
                const next = event.target.value;
                const prev = lifecycleSelect.dataset.savedValue || currentLifecycle;
                lifecycleSelect.disabled = true;
                try {
                    const saved = await controller.savePreferences("paddleocr", {lifecycle: next});
                    const accepted = saved?.lifecycle || next;
                    lifecycleSelect.dataset.savedValue = accepted;
                    lifecycleSelect.value = accepted;
                } catch (err) {
                    console.error("[paddleocr-hook] save lifecycle failed:", err);
                    lifecycleSelect.value = prev;
                } finally {
                    lifecycleSelect.disabled = false;
                }
            });
            lifecycleGroup.appendChild(lifecycleSelect);
            container.appendChild(lifecycleGroup);

            appendRebuildHint(container, prefs);
        },
        syncConfig(container, entry) {
            syncCommonConfig(container, entry);

            // 0.22.8-E: ONNX 资产状态原位同步
            const assetsEl = container.querySelector(".le-onnx-assets");
            if (assetsEl) {
                const catalog = entry?.catalog;
                const storage = entry?.storage;
                const targets = storage?.targets || [];
                const parts = [];
                const ortTarget = targets.find((s) => s.kind === "engine_environment" && s.current);
                if (ortTarget && ortTarget.size_bytes > 0) {
                    parts.push(`${t("local_engine.config.ort_runtime", "ORT")}: ${formatBytes(ortTarget.size_bytes)}`);
                } else {
                    const envBudget = catalog?.resource_budget?.estimated_env_disk_mb;
                    if (envBudget != null) {
                        parts.push(`${t("local_engine.config.ort_runtime", "ORT")}: ~${formatMB(envBudget)}`);
                    }
                }
                const modelTargets = targets.filter((s) => s.kind === "installed_model");
                if (modelTargets.length > 0) {
                    for (const mt of modelTargets) {
                        const label = mt.label_fallback || mt.target_id || "model";
                        parts.push(`${label}: ${formatBytes(mt.size_bytes)}`);
                    }
                } else {
                    const modelBudget = catalog?.resource_budget?.estimated_model_disk_mb;
                    if (modelBudget != null) {
                        parts.push(`${t("local_engine.config.models", "模型")}: ~${formatMB(modelBudget)}`);
                    }
                }
                const newText = parts.join(" · ");
                if (assetsEl.textContent !== newText) {
                    assetsEl.textContent = newText;
                }
            }

            // 0.22.8-E: deployment identity 原位同步
            const depStatus = container.querySelector(".le-deployment-status");
            if (depStatus) {
                const desired = getDesiredDeployment(entry);
                const loaded = getLoadedDeployment(entry);
                const pending = isPendingRestart(entry);
                const parts = [];
                if (desired) {
                    parts.push(`${t("local_engine.config.desired_deployment", "已提交")}: ${desired.model_revision || "—"}`);
                }
                if (loaded) {
                    parts.push(`${t("local_engine.config.loaded_deployment", "已加载")}: ${loaded.model_revision || "—"}`);
                } else if (desired) {
                    parts.push(t("local_engine.config.not_loaded", "未加载"));
                }
                if (pending) {
                    parts.push(t("local_engine.config.pending_restart", "待重启"));
                }
                const newText = parts.join(" · ");
                if (depStatus.textContent !== newText) {
                    depStatus.textContent = newText;
                }
                if (pending) {
                    depStatus.classList.add("le-deployment-pending");
                } else {
                    depStatus.classList.remove("le-deployment-pending");
                }
            }

            // 0.22.8-E: legacy 警告原位同步
            const legacyWarn = container.querySelector(".le-legacy-warning");
            if (legacyWarn) {
                const legacy = getLegacyDeployment(entry);
                legacyWarn.parentElement.hidden = !legacy;
            }

            const backend = container.querySelector(".le-ocr-backend-select");
            const nextBackend = entry.preferences?.ocr_backend || "windows";
            if (backend) {
                if (backend.value !== nextBackend) backend.value = nextBackend;
                backend.dataset.savedValue = nextBackend;
            }
            const lifecycle = container.querySelector(".le-lifecycle-select");
            const nextLifecycle = entry.preferences?.lifecycle
                || entry.catalog?.lifecycle
                || "on_demand";
            if (lifecycle) {
                if (lifecycle.value !== nextLifecycle) lifecycle.value = nextLifecycle;
                lifecycle.dataset.savedValue = nextLifecycle;
            }
        },
    });
}

// ── 汇总注册入口 ──────────────────────────────────────────────────────────────

/**
 * 注册所有内置引擎的受限 adapter hook。
 * 在 settings/index.js mountLocalRuntime 时调用一次。
 */
export function registerLocalEngineHooks() {
    registerFunasrHook();
    registerPaddleOcrHook();
}

/**
 * 取消注册所有内置引擎的受限 adapter hook。
 * 在 dispose 时调用。
 */
export function unregisterLocalEngineHooks() {
    unregisterAdapterHook("funasr");
    unregisterAdapterHook("paddleocr");
}
