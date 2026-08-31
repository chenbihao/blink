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
 * renderer 在 preferences / selected / active 签名变化时整体重渲染，
 * 控件值始终来自后端真源，失败时回滚显示。
 *
 * @module local-engine-hooks
 */

import {registerAdapterHook, unregisterAdapterHook} from "./local-engine-card.js";
import {invoke} from "../../../shared/tauri.js";
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
    value.className = "le-config-static";
    value.textContent = name;
    group.appendChild(value);
    container.appendChild(group);

    // selected ≠ active → 待重启/未生效（不得静默声称切换完成）
    if (selected && active && selected.model_id !== active.model_id) {
        const hint = document.createElement("span");
        hint.className = "le-config-mismatch";
        hint.textContent = t("local_engine.model.mismatch_hint");
        container.appendChild(hint);
    }
}

/** 渲染 requires_rebuild 提示（保存 compute 偏好后环境待重建）。 */
function appendRebuildHint(container, prefs) {
    if (prefs?.requires_rebuild !== true) return;
    const hint = document.createElement("span");
    hint.className = "le-config-rebuild-hint";
    hint.textContent = t("local_engine.config.requires_rebuild_hint");
    container.appendChild(hint);
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
        staticValue.className = "le-config-static";
        staticValue.textContent = computeLabel(current);
        group.appendChild(staticValue);
        container.appendChild(group);
        return;
    }

    const select = document.createElement("select");
    select.className = "le-config-select";
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
        const prev = current;
        try {
            await invoke("set_local_engine_preferences", {
                engineId,
                patch: {compute_preference: next},
            });
            if (controller?.isMounted()) {
                controller.refreshStatus().catch(() => {});
            }
        } catch (err) {
            console.error(`[${engineId}-hook] save compute preference failed:`, err);
            select.value = prev;
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
                try {
                    await invoke("set_local_engine_preferences", {
                        engineId: "funasr",
                        patch: {auto_start: next},
                    });
                } catch (err) {
                    console.error("[funasr-hook] save auto_start failed:", err);
                    toggle.checked = !next;
                }
            });

            appendRebuildHint(container, prefs);
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
            backendSelect.className = "le-config-select";
            for (const backend of ["windows", "paddleocr", "auto"]) {
                const option = document.createElement("option");
                option.value = backend;
                option.textContent = t(`local_engine.ocr_backend.${backend}`, backend);
                option.selected = backend === currentBackend;
                backendSelect.appendChild(option);
            }
            backendSelect.addEventListener("change", async (event) => {
                const next = event.target.value;
                try {
                    await invoke("set_local_engine_preferences", {
                        engineId: "paddleocr",
                        patch: {ocr_backend: next},
                    });
                } catch (err) {
                    console.error("[paddleocr-hook] save OCR backend failed:", err);
                    backendSelect.value = currentBackend;
                }
            });
            backendGroup.appendChild(backendSelect);
            container.appendChild(backendGroup);

            // 计算设备（catalog 声明 auto/cpu 双候选 → select）
            appendComputeGroup(container, entry, "paddleocr", controller);

            // 运行策略（生命周期）
            const lifecycleGroup = makeGroup(t("local_engine.config.lifecycle"));
            const lifecycleSelect = document.createElement("select");
            lifecycleSelect.className = "le-config-select";
            for (const opt of ["on_demand", "keep_running", "stop_after_use"]) {
                const option = document.createElement("option");
                option.value = opt;
                option.textContent = t(`local_engine.lifecycle.${opt}`, opt);
                if (currentLifecycle === opt) option.selected = true;
                lifecycleSelect.appendChild(option);
            }
            lifecycleSelect.addEventListener("change", async (event) => {
                const next = event.target.value;
                try {
                    await invoke("set_local_engine_preferences", {
                        engineId: "paddleocr",
                        patch: {lifecycle: next},
                    });
                } catch (err) {
                    console.error("[paddleocr-hook] save lifecycle failed:", err);
                    lifecycleSelect.value = currentLifecycle;
                }
            });
            lifecycleGroup.appendChild(lifecycleSelect);
            container.appendChild(lifecycleGroup);

            appendRebuildHint(container, prefs);
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
