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
 * 注册 PaddleOCR 受限 adapter hook。
 *
 * 配置组：当前模型（只读）/ OCR 后端 / 计算设备 / 运行策略（生命周期）。
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

            // 计算设备（catalog 声明 auto/cpu 双候选 → select）
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
