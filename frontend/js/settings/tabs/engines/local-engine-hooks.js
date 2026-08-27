/**
 * 受限 adapter hooks —— FunASR 与 PaddleOCR 的引擎专属配置入口（0.22.6）。
 *
 * 通用 renderer 只处理生命周期（install/start/stop/repair/cleanup/cancel）。
 * FunASR/PaddleOCR 的配置保存通过按 engine_id 注册的受限 hook 完成，
 * 但不能复制 card lifecycle。
 *
 * ## 受限边界
 *
 * - FunASR compute preference 映射现有 SttConfig。
 * - PaddleOCR compute preference/lifecycle 映射 OcrConfig。
 * - PaddleOCR 只显示后端 catalog 声明的 auto/cpu。
 * - renderer 绝不允许任意 engine id 动态注入 HTML、command 或字段路径。
 *
 * ## 0.22.6 改进
 *
 * - 初始化必须读取 preferences（get_local_engine_preferences）
 * - 保存走闭合命令 set_local_engine_preferences
 * - 失败回滚（select 恢复原值）
 * - requires_rebuild 提示
 * - FunASR auto_start 开关
 *
 * @module local-engine-hooks
 */

import {registerAdapterHook, unregisterAdapterHook} from "./local-engine-card.js";
import {invoke} from "../../../shared/tauri.js";
import {t} from "../../../i18n/index.js";

// ── FunASR hook ───────────────────────────────────────────────────────────────

/**
 * 注册 FunASR 受限 adapter hook。
 *
 * FunASR 的 compute preference 映射现有 SttConfig.local_engine.device。
 * 配置保存走闭合命令 `set_local_engine_preferences`，不通过泛型 `set_config`。
 * 失败时回滚 select 到 preferences 中的原值。
 */
function registerFunasrHook() {
    registerAdapterHook("funasr", {
        /**
         * 渲染 FunASR 专属配置区。
         * 展示 compute preference 选择器 + auto_start 开关。
         * 初始化读取 preferences（entry.preferences）。
         */
        renderConfig(container, entry, controller) {
            if (!container) return;
            container.textContent = "";

            const catalog = entry.catalog;
            if (!catalog) return;

            const prefs = entry.preferences;
            const currentCompute = prefs?.compute_preference || catalog.current_compute_preference || "cpu";
            const autoStart = prefs?.auto_start ?? false;
            const requiresRebuild = prefs?.requires_rebuild === true;

            // ── compute preference 选择器 ──────────────────────────────
            const computeRow = document.createElement("div");
            computeRow.className = "le-config-row";

            const computeLabel = document.createElement("label");
            computeLabel.className = "le-config-label";
            computeLabel.textContent = t("local_engine.config.compute_preference");
            computeRow.appendChild(computeLabel);

            const select = document.createElement("select");
            select.className = "le-config-select";

            for (const opt of catalog.compute_options) {
                const option = document.createElement("option");
                option.value = opt.preference;
                option.textContent = t(`local_engine.compute.${opt.preference}`, opt.preference);
                option.disabled = !opt.compatible;
                if (opt.disabled_reason) {
                    option.title = opt.disabled_reason;
                }
                if (opt.preference === currentCompute) {
                    option.selected = true;
                }
                select.appendChild(option);
            }

            // 失败回滚：保存失败时恢复 select 原值
            select.addEventListener("change", async (e) => {
                const newPref = e.target.value;
                const oldPref = currentCompute;
                try {
                    const result = await invoke("set_local_engine_preferences", {
                        engineId: "funasr",
                        patch: {compute_preference: newPref},
                    });
                    // 如果需要重建，提示用户
                    if (result?.requires_rebuild) {
                        console.info("[funasr-hook] compute profile changed, needs rebuild");
                    }
                    // 刷新 status（environment 可能变为 needs_rebuild）
                    if (controller?.isMounted()) {
                        controller.refreshStatus().catch(() => {});
                    }
                } catch (err) {
                    console.error("[funasr-hook] save compute preference failed:", err);
                    // 回滚 select
                    select.value = oldPref;
                }
            });

            computeRow.appendChild(select);
            container.appendChild(computeRow);

            // ── auto_start 开关 ────────────────────────────────────────
            const autoStartRow = document.createElement("div");
            autoStartRow.className = "le-config-row";

            const autoStartLabel = document.createElement("label");
            autoStartLabel.className = "le-config-label";
            autoStartLabel.textContent = t("local_engine.config.auto_start");
            autoStartRow.appendChild(autoStartLabel);

            const autoStartToggle = document.createElement("input");
            autoStartToggle.type = "checkbox";
            autoStartToggle.className = "le-config-checkbox";
            autoStartToggle.checked = autoStart;
            autoStartToggle.id = "le-funasr-auto-start";

            autoStartToggle.addEventListener("change", async () => {
                const newVal = autoStartToggle.checked;
                try {
                    await invoke("set_local_engine_preferences", {
                        engineId: "funasr",
                        patch: {auto_start: newVal},
                    });
                } catch (err) {
                    console.error("[funasr-hook] save auto_start failed:", err);
                    // 回滚
                    autoStartToggle.checked = !newVal;
                }
            });

            const autoStartHint = document.createElement("span");
            autoStartHint.className = "le-config-hint";
            autoStartHint.textContent = t("local_engine.config.auto_start_hint");
            autoStartHint.style.fontSize = "var(--text-xs)";
            autoStartHint.style.color = "var(--text-dim)";
            autoStartHint.style.fontStyle = "normal";

            autoStartRow.appendChild(autoStartToggle);
            autoStartRow.appendChild(autoStartHint);
            container.appendChild(autoStartRow);

            // ── requires_rebuild 提示 ──────────────────────────────────
            if (requiresRebuild) {
                const rebuildHint = document.createElement("div");
                rebuildHint.className = "le-config-rebuild-hint";
                rebuildHint.style.cssText =
                    "padding:0.375rem 0.625rem;border-radius:var(--radius-sm,4px);" +
                    "background:var(--accent-bg);border:1px solid var(--accent);" +
                    "font-size:var(--text-xs);color:var(--accent);font-style:normal;margin-top:0.25rem;";
                rebuildHint.textContent = t("local_engine.config.requires_rebuild_hint");
                container.appendChild(rebuildHint);
            }

            // ── 配置模型 vs 实际加载模型 ───────────────────────────────
            renderModelConfigRow(container, entry, "funasr");
        },

        onComputePreferenceChange(engineId, preference) {
            console.debug(`[funasr-hook] compute preference changed: ${engineId} → ${preference}`);
        },
    });
}

// ── PaddleOCR hook ────────────────────────────────────────────────────────────

/**
 * 注册 PaddleOCR 受限 adapter hook。
 *
 * PaddleOCR 的 compute preference/lifecycle 映射 OcrConfig。
 * PaddleOCR 只显示后端 catalog 声明的 auto/cpu。
 * 配置保存走闭合命令 `set_local_engine_preferences`。
 * 失败回滚。
 */
function registerPaddleOcrHook() {
    registerAdapterHook("paddleocr", {
        /**
         * 渲染 PaddleOCR 专属配置区。
         * 展示 catalog 声明的 compute options（auto/cpu）+ lifecycle 选择器。
         * 初始化读取 preferences。
         */
        renderConfig(container, entry, controller) {
            if (!container) return;
            container.textContent = "";

            const catalog = entry.catalog;
            if (!catalog) return;

            const prefs = entry.preferences;
            const currentCompute = prefs?.compute_preference || catalog.current_compute_preference || "auto";
            const currentLifecycle = prefs?.lifecycle || catalog.lifecycle || "on_demand";
            const requiresRebuild = prefs?.requires_rebuild === true;

            // ── compute preference 选择器 ──────────────────────────────
            const computeRow = document.createElement("div");
            computeRow.className = "le-config-row";

            const computeLabel = document.createElement("label");
            computeLabel.className = "le-config-label";
            computeLabel.textContent = t("local_engine.config.compute_preference");
            computeRow.appendChild(computeLabel);

            const select = document.createElement("select");
            select.className = "le-config-select";

            for (const opt of catalog.compute_options) {
                const option = document.createElement("option");
                option.value = opt.preference;
                option.textContent = t(`local_engine.compute.${opt.preference}`, opt.preference);
                option.disabled = !opt.compatible;
                if (opt.disabled_reason) {
                    option.title = opt.disabled_reason;
                }
                if (opt.preference === currentCompute) {
                    option.selected = true;
                }
                select.appendChild(option);
            }

            select.addEventListener("change", async (e) => {
                const newPref = e.target.value;
                const oldPref = currentCompute;
                try {
                    await invoke("set_local_engine_preferences", {
                        engineId: "paddleocr",
                        patch: {compute_preference: newPref},
                    });
                    if (controller?.isMounted()) {
                        controller.refreshStatus().catch(() => {});
                    }
                } catch (err) {
                    console.error("[paddleocr-hook] save compute preference failed:", err);
                    select.value = oldPref;
                }
            });

            computeRow.appendChild(select);
            container.appendChild(computeRow);

            // ── lifecycle 选择器 ───────────────────────────────────────
            const lifecycleRow = document.createElement("div");
            lifecycleRow.className = "le-config-row";

            const lifecycleLabel = document.createElement("label");
            lifecycleLabel.className = "le-config-label";
            lifecycleLabel.textContent = t("local_engine.config.lifecycle");
            lifecycleRow.appendChild(lifecycleLabel);

            const lifecycleSelect = document.createElement("select");
            lifecycleSelect.className = "le-config-select";

            const lifecycleOptions = ["on_demand", "keep_running", "stop_after_use"];
            for (const opt of lifecycleOptions) {
                const option = document.createElement("option");
                option.value = opt;
                option.textContent = t(`local_engine.lifecycle.${opt}`, opt);
                if (currentLifecycle === opt) {
                    option.selected = true;
                }
                lifecycleSelect.appendChild(option);
            }

            lifecycleSelect.addEventListener("change", async (e) => {
                const newVal = e.target.value;
                const oldVal = currentLifecycle;
                try {
                    await invoke("set_local_engine_preferences", {
                        engineId: "paddleocr",
                        patch: {lifecycle: newVal},
                    });
                } catch (err) {
                    console.error("[paddleocr-hook] save lifecycle failed:", err);
                    lifecycleSelect.value = oldVal;
                }
            });

            lifecycleRow.appendChild(lifecycleSelect);
            container.appendChild(lifecycleRow);

            // ── requires_rebuild 提示 ──────────────────────────────────
            if (requiresRebuild) {
                const rebuildHint = document.createElement("div");
                rebuildHint.className = "le-config-rebuild-hint";
                rebuildHint.style.cssText =
                    "padding:0.375rem 0.625rem;border-radius:var(--radius-sm,4px);" +
                    "background:var(--accent-bg);border:1px solid var(--accent);" +
                    "font-size:var(--text-xs);color:var(--accent);font-style:normal;margin-top:0.25rem;";
                rebuildHint.textContent = t("local_engine.config.requires_rebuild_hint");
                container.appendChild(rebuildHint);
            }

            // ── 配置模型 vs 实际加载模型 ───────────────────────────────
            renderModelConfigRow(container, entry, "paddleocr");
        },

        onComputePreferenceChange(engineId, preference) {
            console.debug(`[paddleocr-hook] compute preference changed: ${engineId} → ${preference}`);
        },
    });
}

// ── 配置模型 vs 实际加载模型 ──────────────────────────────────────────────────

/**
 * 渲染"配置模型"与"实际加载模型"对比行。
 * 当 is_selected != is_active 时显示"待重启/未生效"提示。
 *
 * @param {HTMLElement} container
 * @param {Object} entry - EngineStateEntry
 * @param {string} engineId
 */
function renderModelConfigRow(container, entry, engineId) {
    const models = entry.models;
    if (!models || models.length === 0) return;

    // 找到 selected 和 active 模型
    const selected = models.find((m) => m.is_selected);
    const active = models.find((m) => m.is_active);

    // 配置模型行
    const configRow = document.createElement("div");
    configRow.className = "le-model-config-row";

    const configLabel = document.createElement("span");
    configLabel.className = "le-info-label";
    configLabel.textContent = t("local_engine.model.configured");
    configRow.appendChild(configLabel);

    const configValue = document.createElement("span");
    configValue.className = "le-info-value";
    configValue.textContent = selected?.display_name || selected?.model_id || "—";
    configRow.appendChild(configValue);
    container.appendChild(configRow);

    // 实际加载模型行
    const activeRow = document.createElement("div");
    activeRow.className = "le-model-config-row";

    const activeLabel = document.createElement("span");
    activeLabel.className = "le-info-label";
    activeLabel.textContent = t("local_engine.model.active");
    activeRow.appendChild(activeLabel);

    const activeValue = document.createElement("span");
    activeValue.className = "le-info-value";
    activeValue.textContent = active?.display_name || active?.model_id || "—";
    activeRow.appendChild(activeValue);
    container.appendChild(activeRow);

    // 不一致提示
    if (selected && active && selected.model_id !== active.model_id) {
        const mismatch = document.createElement("div");
        mismatch.className = "le-model-mismatch";
        const icon = document.createElement("svg");
        icon.setAttribute("class", "icon");
        icon.setAttribute("aria-hidden", "true");
        const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
        use.setAttribute("href", "#icon-triangle-alert");
        icon.appendChild(use);
        mismatch.appendChild(icon);
        const text = document.createElement("span");
        text.textContent = t("local_engine.model.mismatch_hint");
        mismatch.appendChild(text);
        container.appendChild(mismatch);
    }
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
