/**
 * 受限 adapter hooks —— FunASR 与 PaddleOCR 的引擎专属配置入口（0.22.5 H3）。
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
 * ## 使用方式
 *
 * ```js
 * import {registerLocalEngineHooks} from "./local-engine-hooks.js";
 * // 在 settings/index.js mountLocalRuntime 时调用
 * registerLocalEngineHooks();
 * ```
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
 * 不在此注入 command 或字段路径——renderer 只展示生命周期，hook 只做配置保存。
 *
 * 配置保存走闭合命令 `set_local_engine_preferences`，不通过泛型 `set_config`。
 */
function registerFunasrHook() {
    registerAdapterHook("funasr", {
        /**
         * 渲染 FunASR 专属配置区。
         * 只展示 compute preference 选择器（cpu/cuda），
         * 变更时通过 `set_local_engine_preferences` 保存。
         */
        renderConfig(container, entry, controller) {
            if (!container) return;
            container.textContent = "";

            const catalog = entry.catalog;
            if (!catalog) return;

            // compute preference 选择器
            const row = document.createElement("div");
            row.className = "le-config-row";

            const label = document.createElement("label");
            label.className = "le-config-label";
            label.textContent = t("local_engine.config.compute_preference");
            row.appendChild(label);

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
                if (opt.preference === catalog.current_compute_preference) {
                    option.selected = true;
                }
                select.appendChild(option);
            }

            select.addEventListener("change", async (e) => {
                const pref = e.target.value;
                try {
                    // 走闭合命令 set_local_engine_preferences
                    const result = await invoke("set_local_engine_preferences", {
                        engineId: "funasr",
                        patch: {compute_preference: pref},
                    });
                    // 如果需要重建，提示用户
                    if (result?.requires_rebuild) {
                        console.info("[funasr-hook] compute profile changed, needs rebuild");
                    }
                } catch (err) {
                    console.error("[funasr-hook] save compute preference failed:", err);
                }
            });

            row.appendChild(select);
            container.appendChild(row);
        },

        /**
         * compute preference 变更回调。
         */
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
 *
 * 配置保存走闭合命令 `set_local_engine_preferences`。
 */
function registerPaddleOcrHook() {
    registerAdapterHook("paddleocr", {
        /**
         * 渲染 PaddleOCR 专属配置区。
         * 只展示 catalog 声明的 compute options（auto/cpu），
         * 不显示 catalog 未声明的 GPU 选项。
         */
        renderConfig(container, entry, controller) {
            if (!container) return;
            container.textContent = "";

            const catalog = entry.catalog;
            if (!catalog) return;

            // compute preference 选择器
            const row = document.createElement("div");
            row.className = "le-config-row";

            const label = document.createElement("label");
            label.className = "le-config-label";
            label.textContent = t("local_engine.config.compute_preference");
            row.appendChild(label);

            const select = document.createElement("select");
            select.className = "le-config-select";

            // 只显示 catalog 声明的选项——不显示未声明的 GPU 选项
            for (const opt of catalog.compute_options) {
                const option = document.createElement("option");
                option.value = opt.preference;
                option.textContent = t(`local_engine.compute.${opt.preference}`, opt.preference);
                option.disabled = !opt.compatible;
                if (opt.disabled_reason) {
                    option.title = opt.disabled_reason;
                }
                if (opt.preference === catalog.current_compute_preference) {
                    option.selected = true;
                }
                select.appendChild(option);
            }

            select.addEventListener("change", async (e) => {
                const pref = e.target.value;
                try {
                    await invoke("set_local_engine_preferences", {
                        engineId: "paddleocr",
                        patch: {compute_preference: pref},
                    });
                } catch (err) {
                    console.error("[paddleocr-hook] save compute preference failed:", err);
                }
            });

            row.appendChild(select);
            container.appendChild(row);

            // lifecycle 选择器
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
                // 选中当前值（从 catalog 推断）
                if (catalog.lifecycle === opt) {
                    option.selected = true;
                }
                lifecycleSelect.appendChild(option);
            }

            lifecycleSelect.addEventListener("change", async (e) => {
                const val = e.target.value;
                try {
                    await invoke("set_local_engine_preferences", {
                        engineId: "paddleocr",
                        patch: {lifecycle: val},
                    });
                } catch (err) {
                    console.error("[paddleocr-hook] save lifecycle failed:", err);
                }
            });

            lifecycleRow.appendChild(lifecycleSelect);
            container.appendChild(lifecycleRow);
        },

        onComputePreferenceChange(engineId, preference) {
            console.debug(`[paddleocr-hook] compute preference changed: ${engineId} → ${preference}`);
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
