//! AI Tab 模型档位（0.14.6 §4.2 拆分）。
//!
//! 渲染 tier 下拉选项、主窗口模型选择与关键告警 banner。
//! 各档位的"将降级到「XX」"提示内联在下拉的"未指派"选项中,不再用独立提示行撑布局。

import {aiState, escapeAttr, escapeHtml} from "./state.js";
import {t} from "../../../i18n/index.js";

/// 各档位降级链:本档不可用 → 依次尝试下一档。
const TIER_CHAIN = {ultra_light: ["light", "main"], light: ["main"], main: []};

/** 该档位指派是否可用(存在且 provider/model 都启用)。 */
function tierUsable(a, cfg) {
    if (!a) return false;
    const provider = (cfg.providers || []).find((p) => p.id === a.provider_id);
    if (!provider || provider.enabled === false) return false;
    const model = (provider.models || []).find((m) => m.id === a.model_id);
    return !!model && model.enabled !== false;
}

/** 该档位可降级到的第一个可用档位名;无则 null。 */
function degradeTarget(cfg, tier) {
    for (const next of TIER_CHAIN[tier]) {
        if (tierUsable(cfg[`tier_${next}`], cfg)) return next;
    }
    return null;
}

export function renderAITierSelects() {
    const cfg = aiState.currentAIConfig;
    const providers = cfg.providers || [];
    ["ultra_light", "light", "main"].forEach((tier) => {
        const sel = document.getElementById(`ai-tier-${tier.replace('_', '-')}`);
        if (!sel) return;
        // 未指派选项内联降级提示:如"未指派（将降级到「轻量档」）"
        const target = degradeTarget(cfg, tier);
        const unassignedText = target
            ? t("ai.tier.unassigned_degrade", {tier: t(`ai.tier.${target}`)})
            : t("ai.tier.unassigned");
        const options = [`<option value="">${escapeHtml(unassignedText)}</option>`];
        providers.forEach((p) => {
            // 0.16.0: 禁用的 provider 不进入 tier 下拉
            if (p.enabled === false) return;
            (p.models || []).filter((m) => m.enabled !== false).forEach((m) => {
                const val = `${p.id}::${m.id}`;
                const label = `${p.display_name} / ${m.display_name || m.id}`;
                options.push(`<option value="${escapeAttr(val)}">${escapeHtml(label)}</option>`);
            });
        });
        sel.innerHTML = options.join("");
        const assign = cfg[`tier_${tier}`];
        sel.value = assign ? `${assign.provider_id}::${assign.model_id}` : "";
    });
    renderMainWindowModelSelect();
}

/** 关键告警 banner——只保留"主档不可用,AI 对话不可用";各档降级提示见对应下拉。 */
export function renderAITierBanner() {
    const banner = document.getElementById("ai-tier-banner");
    if (!banner) return;
    const cfg = aiState.currentAIConfig;
    if (!cfg.enabled) {
        banner.classList.add('hidden');
        return;
    }
    if (tierUsable(cfg.tier_main, cfg)) {
        banner.classList.add('hidden');
        return;
    }
    banner.classList.remove('hidden');
    banner.textContent = t("ai.tier.no_provider").replace(/^→\s*/, "");
}

/** 渲染主窗口 AI 的自选模型列表，并按配置切换可见性。 */
export function renderMainWindowModelSelect() {
    const cfg = aiState.currentAIConfig;
    const policy = cfg.chat_config?.main_window_model || "light";
    const mode = policy === "light" || policy === "main" ? policy : "custom";
    const modeSelect = document.getElementById("ai-main-window-model-mode");
    const customRow = document.getElementById("ai-main-window-custom-model-row");
    const customSelect = document.getElementById("ai-main-window-custom-model");
    if (modeSelect) modeSelect.value = mode;
    if (customRow) customRow.classList.toggle("hidden", mode !== "custom");
    if (!customSelect) return;

    const options = [];
    const values = new Set();
    for (const provider of cfg.providers || []) {
        if (provider.enabled === false) continue;
        for (const model of provider.models || []) {
            if (model.enabled === false || !(model.capabilities || ["chat"]).includes("chat")) continue;
            const value = `${provider.id}:${model.id}`;
            const label = `${provider.display_name} / ${model.display_name || model.id}`;
            options.push(`<option value="${escapeAttr(value)}">${escapeHtml(label)}</option>`);
            values.add(value);
        }
    }
    if (mode === "custom" && !values.has(policy)) {
        options.unshift(`<option value="${escapeAttr(policy)}">${escapeHtml(t("ai.chat.main_model.unavailable"))}</option>`);
    }
    customSelect.innerHTML = options.join("");
    if (mode === "custom") customSelect.value = policy;
}

/** 渲染对话窗口命名模型选择，并按配置切换自选模型行可见性。 */
export function renderTitleModelSelect() {
    const cfg = aiState.currentAIConfig;
    const policy = cfg.chat_config?.title_model || "ultra_light";
    const mode = ["ultra_light", "light", "main"].includes(policy) ? policy : "custom";
    const modeSelect = document.getElementById("ai-chat-title-model-mode");
    const customRow = document.getElementById("ai-chat-title-custom-model-row");
    const customSelect = document.getElementById("ai-chat-title-custom-model");
    if (modeSelect) modeSelect.value = mode;
    if (customRow) customRow.classList.toggle("hidden", mode !== "custom");
    if (!customSelect) return;

    const options = [];
    const values = new Set();
    for (const provider of cfg.providers || []) {
        if (provider.enabled === false) continue;
        for (const model of provider.models || []) {
            if (model.enabled === false || !(model.capabilities || ["chat"]).includes("chat")) continue;
            const value = `${provider.id}:${model.id}`;
            const label = `${provider.display_name} / ${model.display_name || model.id}`;
            options.push(`<option value="${escapeAttr(value)}">${escapeHtml(label)}</option>`);
            values.add(value);
        }
    }
    if (mode === "custom" && !values.has(policy)) {
        options.unshift(`<option value="${escapeAttr(policy)}">${escapeHtml(t("ai.chat.title_model.unavailable"))}</option>`);
    }
    customSelect.innerHTML = options.join("");
    if (mode === "custom") customSelect.value = policy;
}
