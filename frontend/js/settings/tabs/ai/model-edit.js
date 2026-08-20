//! AI Tab 模型编辑 modal + 拉取 popover（0.14.6 §4.2 拆分）。
//!
//! 包含：
//! - openAIModelEditModal / closeAIModelEditModal — modal 生命周期
//! - setupModelParamRow / renderCustomParams / coerceCustomParamValue — 参数编辑
//! - openModelFetchDropdown / closeModelFetchDropdown / performModelFetch / renderModelFetchList — 拉取 popover
//! - validateAndSaveModel / saveModelEdit / saveAndContinueModelEdit — 保存逻辑
//! - showModelSavedToast / bindAIModelEditModalEvents — UI 辅助

import {aiState, escapeAttr, escapeHtml, fetchAvailableModelsFor, saveAIConfig} from "./state.js";
import {renderAITierBanner, renderAITierSelects} from "./tier.js";
import {t} from "../../../i18n/index.js";
import {iconHTML} from "../../../shared/icon.js";

/**
 * 打开模型编辑 modal。modelId 为 null 时是"新增"模式。
 */
export function openAIModelEditModal(providerId, modelId) {
    const cfg = aiState.currentAIConfig;
    const provider = (cfg.providers || []).find((p) => p.id === providerId);
    if (!provider) return;
    const isEdit = modelId != null;
    const existing = isEdit ? (provider.models || []).find((m) => m.id === modelId) : null;
    if (isEdit && !existing) return;

    aiState._modelEditProviderId = providerId;
    aiState._modelEditOriginalId = isEdit ? existing.id : null;
    aiState._modelEditDraft = {
        id: existing?.id ?? "",
        display_name: existing?.display_name ?? "",
        enabled: existing?.enabled !== false,
        context_window: existing?.context_window ?? null,
        temperature: existing?.temperature ?? null,
        max_tokens: existing?.max_tokens ?? null,
        custom_parameters: (existing?.custom_parameters || []).map((cp) => ({...cp})),
        capabilities: existing?.capabilities ? [...existing.capabilities] : ["chat"],
        reasoning_effort: existing?.reasoning_effort ?? null,
    };

    // 能力复选框回显
    ["chat", "embedding"].forEach((cap) => {
        const cb = document.getElementById(`ai-model-edit-cap-${cap}`);
        if (cb) cb.checked = aiState._modelEditDraft.capabilities.includes(cap);
    });

    const $ = (id) => document.getElementById(id);
    $("ai-model-edit-title").textContent = t(isEdit ? "ai.model_modal.title.edit" : "ai.model_modal.title.add");
    $("ai-model-edit-provider-info").textContent = t("ai.model_modal.provider_info", {name: provider.display_name});

    // 新增模式：隐藏调用参数 & 高级段；编辑模式：显示全部
    const paramsSection = document.getElementById("ai-model-param-temperature")?.closest(".ai-modal-section");
    if (paramsSection) {
        const allSections = paramsSection.parentElement?.querySelectorAll(".ai-modal-section");
        if (allSections && allSections.length >= 3) {
            allSections[1].classList.toggle('hidden', !isEdit);
            allSections[2].classList.toggle('hidden', !isEdit);
        }
    }
    // 按钮：新增显示"保存并继续"+"完成"，编辑显示"保存"+"取消"
    const continueBtn = document.getElementById("ai-model-edit-continue");
    const saveBtn = document.getElementById("ai-model-edit-save");
    const cancelBtn = document.getElementById("ai-model-edit-cancel");
    if (continueBtn) {
        continueBtn.classList.toggle('hidden', isEdit);
        continueBtn.textContent = t("ai.model_modal.save_continue");
    }
    if (saveBtn) {
        saveBtn.textContent = isEdit ? t("ai.model_modal.save") : t("ai.model_modal.done");
    }
    if (cancelBtn) cancelBtn.textContent = t("ai.model_modal.cancel");

    const idInput = $("ai-model-edit-id");
    idInput.value = aiState._modelEditDraft.id;
    idInput.readOnly = isEdit;
    idInput.classList.toggle("input-readonly", isEdit);
    $("ai-model-edit-display-name").value = aiState._modelEditDraft.display_name;

    closeModelFetchDropdown();
    aiState._modelFetchCache = null;

    setupModelParamRow("temperature", aiState._modelEditDraft.temperature, 0.7);
    setupModelParamRow("max-tokens", aiState._modelEditDraft.max_tokens, 4096);
    setupContextWindowRow(aiState._modelEditDraft.context_window);
    setupReasoningEffortRow(aiState._modelEditDraft.reasoning_effort);

    renderCustomParams();

    $("ai-model-edit-error").textContent = "";
    const overlay = $("ai-model-edit-overlay");
    overlay.classList.remove('hidden');
    setTimeout(() => idInput.focus(), 40);
}

/** 参数行（temperature / max-tokens）通用设置：同步覆盖 toggle 与两个联动 input */
function setupModelParamRow(key, currentValue, fallbackValue) {
    const $ = (id) => document.getElementById(id);
    const toggle = $(`ai-model-edit-${key}-toggle`);
    const body = $(`ai-model-param-${key}-body`);
    const range = $(`ai-model-edit-${key}-range`);
    const num = $(`ai-model-edit-${key}-num`);
    const enabled = currentValue != null;
    toggle.checked = enabled;
    body.classList.toggle('hidden', !enabled);
    const shown = enabled ? currentValue : fallbackValue;
    range.value = shown;
    num.value = shown;
}

/** 上下文窗口行（0.21.21）：null = 自动估算（toggle 关）；Some(n) = 手动指定 */
function setupContextWindowRow(currentValue) {
    const $ = (id) => document.getElementById(id);
    const toggle = $("ai-model-edit-context-window-toggle");
    const body = $("ai-model-param-context-window-body");
    const num = $("ai-model-edit-context-window-num");
    if (!toggle || !body || !num) return;
    const enabled = currentValue != null;
    toggle.checked = enabled;
    body.classList.toggle("hidden", !enabled);
    num.value = enabled ? currentValue : 8192;
}

/** 思考强度行（0.21.17 + 0.21.18）：null = auto（toggle 关）；"" = 默认（不发送）；否则回显预设/自定义 */
function setupReasoningEffortRow(effort) {
    const $ = (id) => document.getElementById(id);
    const toggle = $("ai-model-edit-reasoning-effort-toggle");
    const body = $("ai-model-param-reasoning-effort-body");
    const select = $("ai-model-edit-reasoning-effort-select");
    const custom = $("ai-model-edit-reasoning-effort-custom");
    if (!toggle || !body || !select || !custom) return;

    const enabled = effort != null;
    toggle.checked = enabled;
    body.classList.toggle("hidden", !enabled);
    if (!enabled) return;

    const presets = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    if (effort === "" || presets.includes(effort)) {
        select.value = effort; // "" = 默认（不发送，用模型默认档）
        custom.value = "";
    } else {
        select.value = "custom";
        custom.value = effort; // 自定义值（留空 = omit 不发送）
    }
    custom.hidden = select.value !== "custom";
}

/** 渲染自定义参数键值对列表 */
function renderCustomParams() {
    const container = document.getElementById("ai-model-edit-custom-params");
    if (!container) return;
    const params = aiState._modelEditDraft.custom_parameters || [];
    if (params.length === 0) {
        container.innerHTML = `<div class="ai-model-custom-params-empty">${escapeHtml(t("ai.model_modal.custom_params.empty"))}</div>`;
        return;
    }
    container.innerHTML = params.map((p, idx) => {
        const valDisplay = p.value == null
            ? ""
            : (typeof p.value === "string" ? p.value : JSON.stringify(p.value));
        return `<div class="ai-model-custom-param-row" data-idx="${idx}">
      <input type="text" class="ai-model-custom-param-key" data-idx="${idx}" value="${escapeAttr(p.key || "")}" placeholder="${escapeAttr(t("ai.model_modal.custom_params.key.ph"))}" />
      <input type="text" class="ai-model-custom-param-val" data-idx="${idx}" value="${escapeAttr(valDisplay)}" placeholder="${escapeAttr(t("ai.model_modal.custom_params.val.ph"))}" />
      <button type="button" class="ai-model-custom-param-del" data-idx="${idx}" title="${escapeAttr(t("common.delete"))}">${iconHTML("x")}</button>
    </div>`;
    }).join("");
    container.querySelectorAll(".ai-model-custom-param-key").forEach((el) => {
        el.addEventListener("input", () => {
            const i = Number(el.dataset.idx);
            aiState._modelEditDraft.custom_parameters[i].key = el.value;
        });
    });
    container.querySelectorAll(".ai-model-custom-param-val").forEach((el) => {
        el.addEventListener("input", () => {
            const i = Number(el.dataset.idx);
            aiState._modelEditDraft.custom_parameters[i].value = coerceCustomParamValue(el.value);
        });
    });
    container.querySelectorAll(".ai-model-custom-param-del").forEach((btn) => {
        btn.addEventListener("click", () => {
            const i = Number(btn.dataset.idx);
            aiState._modelEditDraft.custom_parameters.splice(i, 1);
            renderCustomParams();
        });
    });
}

/** value 自动推断类型：数字 / 布尔 / JSON / 字符串 */
function coerceCustomParamValue(raw) {
    if (typeof raw !== "string") return raw;
    const trimmed = raw.trim();
    if (trimmed === "") return "";
    if (trimmed === "true") return true;
    if (trimmed === "false") return false;
    if (trimmed === "null") return null;
    if (/^-?\d+(\.\d+)?$/.test(trimmed)) {
        const n = Number(trimmed);
        if (Number.isFinite(n)) return n;
    }
    if (/^[[{"]/.test(trimmed)) {
        try {
            return JSON.parse(trimmed);
        } catch {
            // JSON 解析失败，回落字符串
        }
    }
    return raw;
}

/** 关闭模型编辑 modal */
export function closeAIModelEditModal() {
    const overlay = document.getElementById("ai-model-edit-overlay");
    if (overlay) overlay.classList.add('hidden');
    closeModelFetchDropdown();
    aiState._modelEditProviderId = null;
    aiState._modelEditOriginalId = null;
    aiState._modelEditDraft = null;
    aiState._modelFetchCache = null;
}

// ── 拉取模型 popover ──────────────────────────────────────────────────────────

/** 聚焦触发——首次拉取，后续直接显示 */
async function openModelFetchDropdown() {
    const dropdown = document.getElementById("ai-model-edit-fetch-dropdown");
    if (!dropdown) return;
    dropdown.classList.remove('hidden');
    if (!aiState._modelFetchCache) {
        await performModelFetch();
    }
    renderModelFetchList(document.getElementById("ai-model-edit-id")?.value || "");
}

/** 关闭下拉但保留缓存（下次打开秒开） */
function closeModelFetchDropdown() {
    const dropdown = document.getElementById("ai-model-edit-fetch-dropdown");
    if (dropdown) dropdown.classList.add('hidden');
}

/** 执行拉取——从当前 modal 关联的 provider 抓 model 列表 */
async function performModelFetch() {
    const cfg = aiState.currentAIConfig;
    const providerId = aiState._modelEditProviderId;
    const provider = (cfg.providers || []).find((p) => p.id === providerId);
    if (!provider) {
        aiState._modelFetchCache = {models: [], error: t("ai.model_modal.err.provider_gone"), loading: false};
        return;
    }
    aiState._modelFetchCache = {models: [], error: null, loading: true};
    renderModelFetchList("");
    try {
        // 0.21.21: models 现在是 ModelMeta 数组 [{id, context_window?}]
        const models = await fetchAvailableModelsFor(provider.kind, provider.base_url, providerId);
        aiState._modelFetchCache = {models: models || [], error: null, loading: false};
    } catch (e) {
        aiState._modelFetchCache = {models: [], error: String(e.message || e), loading: false};
    }
}

function renderModelFetchList(filter) {
    const dropdown = document.getElementById("ai-model-edit-fetch-dropdown");
    if (!dropdown) return;
    const cache = aiState._modelFetchCache;
    if (!cache) return;

    if (cache.loading) {
        dropdown.innerHTML = `<div class="ai-model-fetch-loading"><span class="ai-spinner"></span> ${escapeHtml(t("ai.model_modal.fetch.loading"))}</div>`;
        return;
    }
    if (cache.error) {
        const q = (filter || "").trim();
        let html = `<div class="ai-model-fetch-error">${escapeHtml(t("ai.model_modal.fetch.failed", {err: cache.error}))}</div>`;
        if (q) {
            html += `<div class="ai-model-fetch-item ai-manual-add" data-model-id="${escapeAttr(q)}">
        <span>+ ${escapeHtml(t("ai.model_modal.manual_add", {id: q}))}</span>
      </div>`;
        }
        dropdown.innerHTML = html;
        bindManualAddHandlers(dropdown);
        return;
    }

    const q = (filter || "").trim();
    const qLower = q.toLowerCase();
    // 0.21.21: models 是 ModelMeta 数组 {id, context_window?}
    const modelItems = (cache.models || []).map(m => typeof m === "string" ? {id: m} : m);
    const filtered = modelItems
        .filter((m) => (q ? m.id.toLowerCase().includes(qLower) : true))
        .slice(0, 100);

    let itemsHtml = filtered.map((m) => {
        // 0.21.21: 显示 context_window 标注
        const cwLabel = m.context_window != null ? ` <span class="ai-model-fetch-cw" title="${t("ai.model_modal.context_window.fetched")}">${(m.context_window / 1024).toFixed(0)}K</span>` : "";
        return `<div class="ai-model-fetch-item" data-model-id="${escapeAttr(m.id)}" data-context-window="${m.context_window != null ? m.context_window : ""}">${escapeHtml(m.id)}${cwLabel}</div>`;
    }).join("");

    // 无匹配 + 有输入 → 显示手动添加选项
    if (filtered.length === 0 && q) {
        itemsHtml = `<div class="ai-model-fetch-item ai-manual-add" data-model-id="${escapeAttr(q)}">
      <span>+ ${escapeHtml(t("ai.model_modal.manual_add", {id: q}))}</span>
    </div>`;
    } else if (filtered.length === 0) {
        itemsHtml = `<div class="ai-model-fetch-empty">${escapeHtml(t("ai.model_modal.fetch.empty"))}</div>`;
    }

    // 有匹配但输入文本本身不在列表中 → 也显示手动添加
    if (q && filtered.length > 0 && !modelItems.some((m) => m.id.toLowerCase() === qLower)) {
        itemsHtml += `<div class="ai-model-fetch-item ai-manual-add" data-model-id="${escapeAttr(q)}">
      <span>+ ${escapeHtml(t("ai.model_modal.manual_add", {id: q}))}</span>
    </div>`;
    }

    dropdown.innerHTML = itemsHtml;
    dropdown.querySelectorAll(".ai-model-fetch-item:not(.ai-manual-add)").forEach((item) => {
        item.addEventListener("click", () => {
            const input = document.getElementById("ai-model-edit-id");
            if (input) input.value = item.dataset.modelId;
            // 0.21.21: 选中下拉项时预填 context_window
            const cwVal = item.dataset.contextWindow;
            if (cwVal && cwVal !== "") {
                const cwInput = document.getElementById("ai-model-edit-context-window-num");
                const cwToggle = document.getElementById("ai-model-edit-context-window-toggle");
                if (cwInput && cwToggle) {
                    cwInput.value = cwVal;
                    cwToggle.checked = true;
                    cwInput.closest(".ai-model-param-body")?.classList.remove("hidden");
                }
            }
            closeModelFetchDropdown();
        });
    });
    bindManualAddHandlers(dropdown);
}

function bindManualAddHandlers(container) {
    container.querySelectorAll(".ai-manual-add").forEach((el) => {
        el.addEventListener("click", () => {
            const modelId = el.dataset.modelId;
            const input = document.getElementById("ai-model-edit-id");
            if (input && modelId) input.value = modelId;
            closeModelFetchDropdown();
        });
    });
}

async function validateAndSaveModel() {
    const $ = (id) => document.getElementById(id);
    const errorEl = $("ai-model-edit-error");
    errorEl.textContent = "";

    const cfg = aiState.currentAIConfig;
    const providerId = aiState._modelEditProviderId;
    const provider = (cfg.providers || []).find((p) => p.id === providerId);
    if (!provider) {
        errorEl.textContent = t("ai.model_modal.err.provider_gone");
        return false;
    }
    const isEdit = aiState._modelEditOriginalId != null;

    const id = $("ai-model-edit-id").value.trim();
    const displayName = $("ai-model-edit-display-name").value.trim();
    const tempToggle = $("ai-model-edit-temperature-toggle").checked;
    const tempVal = Number($("ai-model-edit-temperature-num").value);
    const maxToggle = $("ai-model-edit-max-tokens-toggle").checked;
    const maxVal = Number($("ai-model-edit-max-tokens-num").value);

    if (!id) {
        errorEl.textContent = t("ai.model_modal.err.empty_id");
        return false;
    }
    if (!isEdit && (provider.models || []).some((m) => m.id === id)) {
        errorEl.textContent = t("ai.model_modal.err.duplicate_id");
        return false;
    }
    if (tempToggle && (!Number.isFinite(tempVal) || tempVal < 0 || tempVal > 2)) {
        errorEl.textContent = t("ai.model_modal.err.temperature_range");
        return false;
    }
    if (maxToggle && (!Number.isFinite(maxVal) || maxVal < 1)) {
        errorEl.textContent = t("ai.model_modal.err.max_tokens_range");
        return false;
    }

    const cleanedCustom = (aiState._modelEditDraft.custom_parameters || []).filter((cp) => (cp.key || "").trim().length > 0);

    // 收集能力复选框
    const selectedCaps = ["chat", "embedding"].filter((cap) => {
        const cb = document.getElementById(`ai-model-edit-cap-${cap}`);
        return cb && cb.checked;
    });
    const capabilities = selectedCaps.length > 0 ? selectedCaps : ["chat"];

    // 思考强度（0.21.17 + 0.21.18）：toggle 关 = auto（null，等同默认不发送）；开 = 默认/预设档位或自定义
    let reasoning_effort = null;
    const effortToggle = document.getElementById("ai-model-edit-reasoning-effort-toggle");
    if (effortToggle && effortToggle.checked) {
        const select = document.getElementById("ai-model-edit-reasoning-effort-select");
        const custom = document.getElementById("ai-model-edit-reasoning-effort-custom");
        reasoning_effort = select?.value === "custom"
            ? (custom ? custom.value.trim() : "")
            : (select ? select.value : "");
    }

    // 上下文窗口（0.21.21）：toggle 开 = 手动指定；关 = null（自动估算）
    const cwToggle = $("ai-model-edit-context-window-toggle").checked;
    const cwVal = Number($("ai-model-edit-context-window-num").value);
    if (cwToggle && (!Number.isFinite(cwVal) || cwVal < 1024)) {
        errorEl.textContent = t("ai.model_modal.err.context_window_range");
        return false;
    }

    const newModel = {
        id,
        display_name: displayName || id,
        enabled: isEdit ? aiState._modelEditDraft.enabled : true,
        context_window: cwToggle ? Math.floor(cwVal) : null,
        input_price_per_million: null,
        output_price_per_million: null,
        temperature: tempToggle ? tempVal : null,
        max_tokens: maxToggle ? Math.floor(maxVal) : null,
        custom_parameters: cleanedCustom,
        capabilities,
        reasoning_effort,
    };

    if (isEdit) {
        const idx = (provider.models || []).findIndex((m) => m.id === aiState._modelEditOriginalId);
        if (idx < 0) {
            errorEl.textContent = t("ai.model_modal.err.model_gone");
            return false;
        }
        const old = provider.models[idx];
        newModel.enabled = old.enabled !== false;
        // context_window 已由表单收集（0.21.21），不再用 old 覆盖
        newModel.input_price_per_million = old.input_price_per_million ?? null;
        newModel.output_price_per_million = old.output_price_per_million ?? null;
        provider.models[idx] = newModel;
    } else {
        provider.models = provider.models || [];
        provider.models.push(newModel);
    }

    try {
        await saveAIConfig();
    } catch (e) {
        // 回滚
        if (isEdit) {
            // 编辑模式回滚：无法恢复旧值（已被覆盖），只能提示
        } else {
            provider.models = provider.models.filter((m) => m.id !== id);
        }
        errorEl.textContent = t("ai.error.save_failed", {err: String(e)});
        return false;
    }
    return true;
}

/**
 * 保存并关闭（编辑模式默认行为）
 */
async function saveModelEdit() {
    const ok = await validateAndSaveModel();
    if (!ok) return;
    closeAIModelEditModal();
    // 通过回调调用 provider.js 的函数（避免循环依赖）
    const expandedIds = aiState._getExpandedProviderIds ? aiState._getExpandedProviderIds() : [];
    if (aiState._renderAIProviders) aiState._renderAIProviders();
    if (aiState._restoreExpandedProviderIds) aiState._restoreExpandedProviderIds(expandedIds);
    renderAITierSelects();
    renderAITierBanner();
}

/**
 * 保存并继续添加（新增模式）
 */
async function saveAndContinueModelEdit() {
    const ok = await validateAndSaveModel();
    if (!ok) return;
    // 重置表单，准备下一个
    const $ = (id) => document.getElementById(id);
    $("ai-model-edit-id").value = "";
    $("ai-model-edit-id").readOnly = false;
    $("ai-model-edit-id").classList.remove("input-readonly");
    $("ai-model-edit-display-name").value = "";
    $("ai-model-edit-error").textContent = "";
    closeModelFetchDropdown();
    aiState._modelFetchCache = null;
    setTimeout(() => $("ai-model-edit-id").focus(), 40);
    // toast
    showModelSavedToast();
    // 刷新列表（让用户看到刚加的模型出现在 tier select 等处）
    const expandedIds = aiState._getExpandedProviderIds ? aiState._getExpandedProviderIds() : [];
    if (aiState._renderAIProviders) aiState._renderAIProviders();
    if (aiState._restoreExpandedProviderIds) aiState._restoreExpandedProviderIds(expandedIds);
    renderAITierSelects();
    renderAITierBanner();
}

function showModelSavedToast() {
    const toast = document.getElementById("ai-model-saved-toast");
    if (!toast) return;
    toast.classList.remove('hidden');
    clearTimeout(aiState._modelSavedToastTimer);
    aiState._modelSavedToastTimer = setTimeout(() => {
        toast.classList.add('hidden');
    }, 2500);
}

/** 模型 modal 事件绑定——由 core.js 的 bindAIEvents 调一次 */
export function bindAIModelEditModalEvents() {
    const $ = (id) => document.getElementById(id);
    const overlay = $("ai-model-edit-overlay");
    if (!overlay) return;

    ["temperature", "max-tokens"].forEach((key) => {
        const toggle = $(`ai-model-edit-${key}-toggle`);
        const body = $(`ai-model-param-${key}-body`);
        const range = $(`ai-model-edit-${key}-range`);
        const num = $(`ai-model-edit-${key}-num`);
        if (!toggle || !body || !range || !num) return;
        toggle.addEventListener("change", () => {
            body.classList.toggle('hidden', !toggle.checked);
        });
        range.addEventListener("input", () => {
            num.value = range.value;
        });
        num.addEventListener("input", () => {
            const v = Number(num.value);
            if (Number.isFinite(v)) range.value = v;
        });
    });

    // 上下文窗口（0.21.21）：只有 toggle + num，没有 range slider
    const cwToggle = $("ai-model-edit-context-window-toggle");
    const cwBody = $("ai-model-param-context-window-body");
    if (cwToggle && cwBody) {
        cwToggle.addEventListener("change", () => {
            cwBody.classList.toggle('hidden', !cwToggle.checked);
        });
    }

    // 思考强度（0.21.17）：toggle 控制显隐，select 联动自定义输入
    const effortToggle = $("ai-model-edit-reasoning-effort-toggle");
    const effortBody = $("ai-model-param-reasoning-effort-body");
    const effortSelect = $("ai-model-edit-reasoning-effort-select");
    const effortCustom = $("ai-model-edit-reasoning-effort-custom");
    if (effortToggle && effortBody) {
        effortToggle.addEventListener("change", () => {
            effortBody.classList.toggle("hidden", !effortToggle.checked);
        });
    }
    if (effortSelect && effortCustom) {
        effortSelect.addEventListener("change", () => {
            effortCustom.hidden = effortSelect.value !== "custom";
        });
    }

    $("ai-model-edit-custom-params-add")?.addEventListener("click", () => {
        if (!aiState._modelEditDraft) return;
        aiState._modelEditDraft.custom_parameters.push({key: "", value: ""});
        renderCustomParams();
        setTimeout(() => {
            const rows = document.querySelectorAll(".ai-model-custom-param-key");
            const last = rows[rows.length - 1];
            if (last) last.focus();
        }, 20);
    });

    $("ai-model-edit-cancel")?.addEventListener("click", closeAIModelEditModal);
    $("ai-model-edit-save")?.addEventListener("click", saveModelEdit);
    $("ai-model-edit-continue")?.addEventListener("click", saveAndContinueModelEdit);

    // 聚焦 Model ID 输入框时自动拉取模型列表（编辑模式下 ID 只读，不弹下拉）
    const idInput = $("ai-model-edit-id");
    if (idInput) {
        idInput.addEventListener("focus", () => {
            if (!idInput.readOnly) openModelFetchDropdown();
        });
        idInput.addEventListener("input", () => {
            const dropdown = $("ai-model-edit-fetch-dropdown");
            if (dropdown && !dropdown.classList.contains('hidden')) {
                renderModelFetchList(idInput.value);
            }
        });
        idInput.addEventListener("keydown", (e) => {
            if (e.key === "Escape") {
                const dropdown = $("ai-model-edit-fetch-dropdown");
                if (dropdown && !dropdown.classList.contains('hidden')) {
                    e.stopPropagation();
                    closeModelFetchDropdown();
                }
            }
        });
    }
    // 点击 model select 区域外关闭下拉（但不关 modal）
    const idWrap = $("ai-model-edit-id-wrap");
    if (idWrap) {
        document.addEventListener("click", (e) => {
            const dropdown = $("ai-model-edit-fetch-dropdown");
            if (!dropdown || dropdown.classList.contains('hidden')) return;
            if (e.target.closest("#ai-model-edit-id-wrap")) return;
            if (e.target.closest("#ai-model-edit-fetch-dropdown")) return;
            if (e.target.closest("#ai-model-edit-overlay") && !e.target.closest("#ai-model-edit-id-wrap")) {
                closeModelFetchDropdown();
            }
        });
    }
}
