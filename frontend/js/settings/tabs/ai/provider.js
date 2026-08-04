//! AI Tab 供应商渲染 + modal + preset 目录 + 模型多选（0.14.6 §4.2 拆分）。
//!
//! 包含：
//! - renderAIProviders — 供应商列表（accordion + 模型表格 + 拖动排序）
//! - getExpandedProviderIds / restoreExpandedProviderIds — accordion 状态保存/恢复
//! - toggleModelEnabled / guideAddModelForProvider / deleteModelFromProvider — 模型操作
//! - AI_PRESET_CATALOG / guessPresetForProvider / AI_PRESET_GROUPS / renderPresetList / applyAIPresetToModal — preset 目录
//! - uniqueDisplayName — 唯一名生成
//! - openAIProviderModal / closeAIProviderModal / saveNewProviderFromModal / saveNewProvider / saveEditedProvider / deleteAIProvider — 供应商 modal
//! - clearProviderModelSelect / triggerProviderModelFetch / filterProviderModels / bindManualAddHandlers / toggleProviderModel / renderProviderModelTags — 模型多选

import { aiState, AI_KIND_LABEL, saveAIConfig, fetchAvailableModelsFor, escapeHtml, escapeAttr } from "./state.js";
import { renderAITierSelects, renderAITierBanner } from "./tier.js";
import { openAIModelEditModal } from "./model-edit.js";
import { invoke, confirmDialog } from "../../../shared/tauri.js";
import { t } from "../../../i18n/index.js";
import { iconHTML } from "../../../shared/icon.js";

// ════════════════════════════════════════════════════════════
//  供应商列表渲染
// ════════════════════════════════════════════════════════════

export function renderAIProviders() {
  const cfg = aiState.currentAIConfig;
  const container = document.getElementById("ai-providers-container");
  if (!container) return;
  const providers = cfg.providers || [];
  if (providers.length === 0) {
    container.innerHTML =
      `<div class="ai-providers-empty">${escapeHtml(t("ai.providers.empty"))}</div>
       <button class="ai-providers-add" id="ai-add-provider">${escapeHtml(t("ai.providers.add"))}</button>`;
    document.getElementById("ai-add-provider")?.addEventListener("click", () => openAIProviderModal());
    return;
  }
  const cards = providers
    .map((p) => {
      const kindLabel = AI_KIND_LABEL[p.kind] || p.kind;
      const models = p.models || [];
      const modelSummary = models.map((m) => m.id).join(", ");
      const isLocalKind = p.kind === "ollama_http";
      const hasKey = isLocalKind || aiState.hasSecretMap.get(p.id) === true;
      const statusCls = hasKey ? "" : "no-key";
      const statusText = hasKey ? t("ai.provider.configured") : t("ai.provider.not_configured");
      const presetKey = guessPresetForProvider(p.kind, p.base_url);
      const preset = AI_PRESET_CATALOG[presetKey] || AI_PRESET_CATALOG.custom;
      const rawMono = preset.monogram || (p.display_name || "").slice(0, 2);
      const monogram = rawMono || "?";
      const isCJK = /[一-鿿]/.test(monogram);
      const monoCjkCls = isCJK ? " ai-provider-mono--cjk" : "";
      const modelsTable = models.length > 0
        ? `<table class="ai-models-table">
            <colgroup>
              <col class="col-id" />
              <col class="col-name" />
              <col class="col-caps" />
              <col class="col-enabled" />
              <col class="col-actions" />
            </colgroup>
            <thead><tr><th>Model ID</th><th>${escapeHtml(t("ai.model.col.name"))}</th><th>${escapeHtml(t("ai.model.col.capabilities"))}</th><th>${escapeHtml(t("ai.model.col.enabled"))}</th><th>${escapeHtml(t("ai.model.col.actions"))}</th></tr></thead>
            <tbody>${models.map((m) => {
              const enabled = m.enabled !== false;
              const hasParams = m.temperature != null || m.max_tokens != null || (m.custom_parameters && m.custom_parameters.length > 0);
              const paramsBadge = hasParams
                ? `<span class="ai-model-params-badge" title="${escapeAttr(t("ai.model.params_badge.title"))}">${escapeHtml(t("ai.model.params_badge"))}</span>`
                : "";
              const caps = (m.capabilities || ["chat"]);
              const capsHtml = caps.map((cap) => `<span class="ai-cap-badge ai-cap-${escapeAttr(cap)}">${escapeHtml(t("ai.cap." + cap))}</span>`).join("");
              return `<tr data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}">
                <td class="ai-models-table-id" title="${escapeAttr(m.id)}">${escapeHtml(m.id)}${paramsBadge}</td>
                <td title="${escapeAttr(m.display_name || m.id)}">${escapeHtml(m.display_name || m.id)}</td>
                <td class="ai-models-table-caps">${capsHtml}</td>
                <td>
                  <label class="switch switch-sm">
                    <input type="checkbox" class="ai-model-toggle" data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}" ${enabled ? "checked" : ""} />
                    <span class="slider"></span>
                  </label>
                </td>
                <td class="ai-models-table-actions">
                  <button class="ai-model-edit-btn" data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.model.edit"))}">${iconHTML("pencil")}</button>
                  <button class="ai-model-delete" data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.model.delete"))}">${iconHTML("x")}</button>
                </td>
              </tr>`;
            }).join("")}</tbody>
          </table>
          <button class="ai-model-add-inline" data-provider-id="${escapeAttr(p.id)}">${escapeHtml(t("ai.model.add"))}</button>`
        : `<div class="ai-models-table-empty">${escapeHtml(t("ai.model.empty"))}</div>
           <button class="ai-model-add-inline" data-provider-id="${escapeAttr(p.id)}">${escapeHtml(t("ai.model.add"))}</button>`;
      return `
        <div class="ai-provider-card" data-provider-id="${escapeAttr(p.id)}">
          <div class="ai-provider-header" data-provider-id="${escapeAttr(p.id)}">
            <span class="ai-provider-drag-handle" draggable="true" title="拖动排序">⋮⋮</span>
            <span class="ai-provider-chevron">▸</span>
            <span class="ai-provider-mono${monoCjkCls}" data-tint="${preset.tint}">${escapeHtml(monogram)}</span>
            <div class="ai-provider-info">
              <div class="ai-provider-title">${escapeHtml(p.display_name)}</div>
              <div class="ai-provider-meta">${escapeHtml(kindLabel)} · ${escapeHtml(modelSummary || "(no model)")}</div>
            </div>
            <span class="ai-provider-status ${statusCls}">${escapeHtml(statusText)}</span>
<label class="switch switch-sm" title="${escapeAttr(t("ai.provider.enable_toggle"))}">
  <input type="checkbox" class="ai-provider-toggle" data-provider-id="${escapeAttr(p.id)}" ${p.enabled !== false ? "checked" : ""} />
  <span class="slider"></span>
</label>
<button class="ai-provider-edit" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.provider.edit"))}">${iconHTML("pencil")}</button>
<button class="ai-provider-delete" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.provider.delete"))}">${iconHTML("x")}</button>
          </div>
          <div class="ai-provider-models hidden">${modelsTable}</div>
        </div>`;
    })
    .join("");
  container.innerHTML =
    cards + `<button class="ai-providers-add" id="ai-add-provider">${escapeHtml(t("ai.providers.add"))}</button>`;

  document.getElementById("ai-add-provider")?.addEventListener("click", () => openAIProviderModal());
  // accordion 展开/折叠
  container.querySelectorAll(".ai-provider-header").forEach((header) => {
    header.addEventListener("click", (e) => {
      if (e.target.closest(".ai-provider-edit") || e.target.closest(".ai-provider-delete") || e.target.closest(".ai-provider-drag-handle") || e.target.closest(".ai-provider-toggle")) return;
      const card = header.closest(".ai-provider-card");
      const modelsDiv = card.querySelector(".ai-provider-models");
      const chevron = header.querySelector(".ai-provider-chevron");
      const isOpen = !modelsDiv.classList.contains('hidden');
      modelsDiv.classList.toggle('hidden', isOpen);
      chevron.textContent = isOpen ? "▸" : "▾";
    });
  });
  container.querySelectorAll(".ai-provider-edit").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      openAIProviderModal(btn.dataset.providerId);
    });
  });
  container.querySelectorAll(".ai-provider-delete").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      deleteAIProvider(btn.dataset.providerId);
    });
  });
  container.querySelectorAll(".ai-model-toggle").forEach((toggle) => {
    toggle.addEventListener("change", (e) => {
      e.stopPropagation();
      const { providerId, modelId } = toggle.dataset;
      toggleModelEnabled(providerId, modelId, toggle.checked);
    });
  });
  // 0.16.0: Provider 启用/禁用开关
  container.querySelectorAll(".ai-provider-toggle").forEach((toggle) => {
    toggle.addEventListener("change", (e) => {
      e.stopPropagation();
      toggleProviderEnabled(toggle.dataset.providerId, toggle.checked);
    });
  });
  container.querySelectorAll(".ai-model-delete").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const { providerId, modelId } = btn.dataset;
      deleteModelFromProvider(providerId, modelId);
    });
  });
  container.querySelectorAll(".ai-model-edit-btn").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const { providerId, modelId } = btn.dataset;
      openAIModelEditModal(providerId, modelId);
    });
  });
  container.querySelectorAll(".ai-model-add-inline").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      openAIModelEditModal(btn.dataset.providerId, null);
    });
  });

  // ── 供应商拖动排序 ──
  let dragState = null;
  container.querySelectorAll(".ai-provider-card").forEach((card) => {
    const handle = card.querySelector(".ai-provider-drag-handle");
    if (!handle) return;
    handle.addEventListener("mousedown", (e) => {
      e.preventDefault();
      e.stopPropagation();
      dragState = { card, startY: e.clientY, started: false };
      document.addEventListener("mousemove", onDragMove);
      document.addEventListener("mouseup", onDragEnd);
    });
  });

  function onDragMove(e) {
    if (!dragState) return;
    if (!dragState.started && Math.abs(e.clientY - dragState.startY) > 4) {
      dragState.started = true;
      dragState.card.classList.add("dragging");
    }
    if (!dragState.started) return;

    const cards = Array.from(container.querySelectorAll(".ai-provider-card"));
    const draggedCard = dragState.card;

    for (const c of cards) {
      if (c === draggedCard) continue;
      const rect = c.getBoundingClientRect();
      if (e.clientY >= rect.top && e.clientY <= rect.bottom) {
        const midY = rect.top + rect.height / 2;
        if (e.clientY < midY) {
          container.insertBefore(draggedCard, c);
        } else {
          const next = c.nextElementSibling;
          if (next && next !== draggedCard) {
            container.insertBefore(draggedCard, next);
          } else if (!next) {
            container.appendChild(draggedCard);
          }
        }
        break;
      }
    }
  }

  function onDragEnd() {
    document.removeEventListener("mousemove", onDragMove);
    document.removeEventListener("mouseup", onDragEnd);
    if (!dragState) return;
    dragState.card.classList.remove("dragging");

    if (dragState.started) {
      const newOrder = Array.from(container.querySelectorAll(".ai-provider-card"))
        .map((c) => c.dataset.providerId)
        .filter(Boolean);
      const providers = aiState.currentAIConfig.providers || [];
      const reordered = newOrder.map((id) => providers.find((p) => p.id === id)).filter(Boolean);
      if (reordered.length === providers.length) {
        aiState.currentAIConfig.providers = reordered;
        saveAIConfig()
          .then(() => renderAITierSelects())
          .catch((e) => console.error("[ai] reorder provider failed:", e));
      }
    }
    dragState = null;
  }
}

/** 获取当前展开的供应商 ID 列表（用于 model 保存后恢复展开态） */
export function getExpandedProviderIds() {
  const container = document.getElementById("ai-providers-container");
  if (!container) return [];
  return Array.from(container.querySelectorAll(".ai-provider-card"))
    .filter((card) => card.querySelector(".ai-provider-chevron")?.textContent === "▾")
    .map((card) => card.dataset.providerId);
}

/** 恢复展开状态 */
export function restoreExpandedProviderIds(ids) {
  const container = document.getElementById("ai-providers-container");
  if (!container) return;
  ids.forEach((id) => {
    const card = container.querySelector(`.ai-provider-card[data-provider-id="${CSS.escape(id)}"]`);
    if (card) {
      const modelsDiv = card.querySelector(".ai-provider-models");
      const chevron = card.querySelector(".ai-provider-chevron");
      if (modelsDiv && chevron) {
        modelsDiv.classList.remove('hidden');
        chevron.textContent = "▾";
      }
    }
  });
}

function toggleModelEnabled(providerId, modelId, enabled) {
  const cfg = aiState.currentAIConfig;
  const provider = (cfg.providers || []).find((p) => p.id === providerId);
  if (!provider) return;
  const model = (provider.models || []).find((m) => m.id === modelId);
  if (!model) return;
  model.enabled = enabled;
  saveAIConfig()
    .then(() => {
      renderAITierSelects();
      renderAITierBanner();
    })
    .catch((e) => console.error("[ai] toggle model enabled failed:", e));
}

// 0.16.0: Provider 启用/禁用
function toggleProviderEnabled(providerId, enabled) {
  const cfg = aiState.currentAIConfig;
  const provider = (cfg.providers || []).find((p) => p.id === providerId);
  if (!provider) return;
  provider.enabled = enabled;
  saveAIConfig()
    .then(() => {
      renderAITierSelects();
      renderAITierBanner();
    })
    .catch((e) => console.error("[ai] toggle provider enabled failed:", e));
}

function guideAddModelForProvider(providerId) {
  const card = document.querySelector(`.ai-provider-card[data-provider-id="${CSS.escape(providerId)}"]`);
  if (card) {
    const modelsDiv = card.querySelector(".ai-provider-models");
    if (modelsDiv) modelsDiv.classList.remove('hidden');
    const chevron = card.querySelector(".ai-provider-chevron");
    if (chevron) chevron.textContent = "▾";
  }
  openAIModelEditModal(providerId, null);
}

async function deleteModelFromProvider(providerId, modelId) {
  const cfg = aiState.currentAIConfig;
  const provider = (cfg.providers || []).find((p) => p.id === providerId);
  if (!provider) return;
  const model = (provider.models || []).find((m) => m.id === modelId);
  if (!model) return;
  const ok = await confirmDialog(
    t("ai.model.delete.confirm", { id: modelId, name: model.display_name || modelId }),
    { title: t("ai.model.delete"), kind: "warning" },
  );
  if (!ok) return;
  provider.models = provider.models.filter((m) => m.id !== modelId);
  // 清悬空 tier
  ["router", "light", "main"].forEach((tier) => {
    const a = cfg[`tier_${tier}`];
    if (a && a.provider_id === providerId && a.model_id === modelId) {
      cfg[`tier_${tier}`] = null;
    }
  });
  try {
    await saveAIConfig();
  } catch (e) {
    console.error("[ai] delete model failed:", e);
    return;
  }
  const expandedIds = getExpandedProviderIds();
  renderAIProviders();
  restoreExpandedProviderIds(expandedIds);
  renderAITierSelects();
  renderAITierBanner();
}

// ════════════════════════════════════════════════════════════
//  Preset 目录
// ════════════════════════════════════════════════════════════

export const AI_PRESET_CATALOG = {
  "openai":            { kind: "openai_compatible",     base_url: "https://api.openai.com/v1",                          display_name_default: "OpenAI",               monogram: "OA",   tint: "green",  category: "main" },
  "anthropic":         { kind: "anthropic_messages",    base_url: null,                                                 display_name_default: "Anthropic",            monogram: "An",   tint: "amber",  category: "main" },
  "gemini":            { kind: "gemini_generate_content",base_url: null,                                                display_name_default: "Google Gemini",        monogram: "Ge",   tint: "teal",   category: "main" },
  "deepseek":          { kind: "openai_compatible",     base_url: "https://api.deepseek.com/v1",                        display_name_default: "DeepSeek",             monogram: "深度",  tint: "blue",   category: "cn" },
  "deepseek-anthropic":{ kind: "anthropic_messages",    base_url: "https://api.deepseek.com/anthropic",                display_name_default: "DeepSeek (Anthropic)", monogram: "深度",  tint: "blue",   category: "cn" },
  "siliconflow":       { kind: "openai_compatible",     base_url: "https://api.siliconflow.cn/v1",                      display_name_default: "SiliconFlow",          monogram: "硅基",  tint: "blue",   category: "cn" },
  "moonshot":          { kind: "openai_compatible",     base_url: "https://api.moonshot.cn/v1",                         display_name_default: "Moonshot",             monogram: "Ki",   tint: "purple", category: "cn" },
  "zhipu":             { kind: "openai_compatible",     base_url: "https://open.bigmodel.cn/api/paas/v4",               display_name_default: "Zhipu",                monogram: "智谱",  tint: "indigo", category: "cn" },
  "zhipu-anthropic":   { kind: "anthropic_messages",    base_url: "https://open.bigmodel.cn/api/anthropic",             display_name_default: "Zhipu (Anthropic)",    monogram: "智谱",  tint: "indigo", category: "cn" },
  "doubao":            { kind: "openai_compatible",     base_url: "https://ark.cn-beijing.volces.com/api/v3",           display_name_default: "Doubao",               monogram: "豆包",  tint: "rose",   category: "cn" },
  "volcengine-anthropic":{ kind: "anthropic_messages",  base_url: "https://ark.cn-beijing.volces.com/api/coding",       display_name_default: "Volcengine (Anthropic)",monogram: "火山",  tint: "rose",   category: "cn" },
  "aliyun":            { kind: "openai_compatible",     base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",  display_name_default: "Aliyun",               monogram: "阿里",  tint: "orange", category: "cn" },
  "stepfun":           { kind: "openai_compatible",     base_url: "https://api.stepfun.com/v1",                        display_name_default: "StepFun",             monogram: "阶跃",  tint: "purple", category: "cn" },
  "minimax":           { kind: "openai_compatible",     base_url: "https://api.minimax.chat/v1",                       display_name_default: "MiniMax",             monogram: "MM",   tint: "pink",   category: "cn" },
  "hunyuan":           { kind: "openai_compatible",     base_url: "https://api.hunyuan.cloud.tencent.com/v1",          display_name_default: "Hunyuan",             monogram: "混元",  tint: "teal",   category: "cn" },
  "xiaomimimo":        { kind: "openai_compatible",     base_url: "https://token-plan-cn.xiaomimimo.com/v1",           display_name_default: "Xiaomi MiMo",         monogram: "小米", tint: "orange", category: "cn" },
  "xiaomimimo-anthropic":{ kind: "anthropic_messages",  base_url: "https://token-plan-cn.xiaomimimo.com/anthropic",    display_name_default: "Xiaomi MiMo (Anthropic)",monogram: "小米", tint: "orange", category: "cn" },
  "groq":              { kind: "openai_compatible",     base_url: "https://api.groq.com/openai/v1",                    display_name_default: "Groq",                monogram: "Gq",   tint: "orange", category: "gw" },
  "openrouter":        { kind: "openai_compatible",     base_url: "https://openrouter.ai/api/v1",                      display_name_default: "OpenRouter",          monogram: "OR",   tint: "purple", category: "gw" },
  "mistral":           { kind: "openai_compatible",     base_url: "https://api.mistral.ai/v1",                         display_name_default: "Mistral",             monogram: "Mi",   tint: "orange", category: "gw" },
  "xai":               { kind: "openai_compatible",     base_url: "https://api.x.ai/v1",                               display_name_default: "xAI",                 monogram: "xA",   tint: "slate",  category: "gw" },
  "together":          { kind: "openai_compatible",     base_url: "https://api.together.xyz/v1",                       display_name_default: "Together",            monogram: "Tg",   tint: "green",  category: "gw" },
  "perplexity":        { kind: "openai_compatible",     base_url: "https://api.perplexity.ai",                         display_name_default: "Perplexity",          monogram: "Px",   tint: "teal",   category: "gw" },
  "huggingface":       { kind: "openai_compatible",     base_url: "https://api-inference.huggingface.co/v1",           display_name_default: "Hugging Face",        monogram: "HF",   tint: "amber",  category: "gw" },
  "nvidia":            { kind: "openai_compatible",     base_url: "https://integrate.api.nvidia.com/v1",               display_name_default: "NVIDIA",              monogram: "NV",   tint: "green",  category: "gw" },
  "agnes-ai":          { kind: "openai_compatible",     base_url: "https://apihub.agnes-ai.com/v1",                    display_name_default: "Agnes AI",            monogram: "Ag",   tint: "rose",   category: "gw" },
  "ollama":            { kind: "ollama_http",           base_url: "http://localhost:11434",                             display_name_default: "Ollama",              monogram: "Ol",   tint: "slate",  category: "local" },
  "lm-studio":         { kind: "openai_compatible",     base_url: "http://localhost:1234/v1",                          display_name_default: "LM Studio",           monogram: "LM",   tint: "slate",  category: "local" },
  "custom":            { kind: null,                    base_url: null,                                                 display_name_default: null,                  monogram: null,   tint: "ink",    category: "custom" },
};

export function guessPresetForProvider(kind, baseUrl) {
  const bu = (baseUrl || "").trim().replace(/\/$/, "");
  for (const [key, preset] of Object.entries(AI_PRESET_CATALOG)) {
    if (key === "custom") continue;
    if (preset.kind !== kind) continue;
    const presetBu = (preset.base_url || "").replace(/\/$/, "");
    if (presetBu === bu) return key;
  }
  return "custom";
}

export const AI_PRESET_GROUPS = [
  { category: "main",   i18nKey: "ai.preset.group.main" },
  { category: "cn",     i18nKey: "ai.preset.group.cn" },
  { category: "gw",     i18nKey: "ai.preset.group.gw" },
  { category: "local",  i18nKey: "ai.preset.group.local" },
  { category: "custom", i18nKey: "ai.preset.group.custom" },
];

export function renderPresetList(selectedKey, isEdit) {
  const list = document.getElementById("ai-preset-list");
  if (!list) return;
  const addedKinds = new Set(
    (aiState.currentAIConfig.providers || []).map((p) => {
      const key = guessPresetForProvider(p.kind, p.base_url);
      return key !== "custom" ? key : null;
    }).filter(Boolean),
  );

  const grouped = new Map();
  for (const key of Object.keys(AI_PRESET_CATALOG)) {
    const cat = AI_PRESET_CATALOG[key].category || "custom";
    if (!grouped.has(cat)) grouped.set(cat, []);
    grouped.get(cat).push(key);
  }
  for (const [cat, keys] of grouped) {
    if (cat === "custom") continue;
    keys.sort((a, b) => {
      const na = AI_PRESET_CATALOG[a].display_name_default || "";
      const nb = AI_PRESET_CATALOG[b].display_name_default || "";
      return na.localeCompare(nb);
    });
  }

  let html = "";
  for (const { category, i18nKey } of AI_PRESET_GROUPS) {
    const keys = grouped.get(category);
    if (!keys || keys.length === 0) continue;
    html += `<div class="ai-preset-group">`;
    html += `<div class="ai-preset-group-label">${escapeHtml(t(i18nKey))}</div>`;
    html += `<div class="ai-preset-group-items">`;
    for (const key of keys) {
      const preset = AI_PRESET_CATALOG[key];
      const isSelected = key === selectedKey;
      const isCustom = key === "custom";
      const name = preset.display_name_default || t("ai.modal.preset.custom");
      const monogram = preset.monogram || "?";
      const isCJK = /[一-鿿]/.test(monogram);
      const cjkCls = isCJK ? " ai-preset-item-mono--cjk" : "";
      const customCls = isCustom ? " ai-preset-item--custom" : "";
      const selectedCls = isSelected ? " selected" : "";
      const addedAttr = addedKinds.has(key) ? ' data-added="1"' : "";
      const addedTitle = addedKinds.has(key) ? ` title="${escapeAttr(t("ai.modal.badge.added"))}"` : "";
      html += `<button type="button" class="ai-preset-item${selectedCls}${customCls}" data-preset="${key}"${addedAttr}${addedTitle}>
        <span class="ai-preset-item-mono${cjkCls}" data-tint="${preset.tint}">${escapeHtml(monogram)}</span>
        <span class="ai-preset-item-name">${escapeHtml(name)}</span>
      </button>`;
    }
    html += `</div></div>`;
  }
  list.innerHTML = html;

  list.querySelectorAll(".ai-preset-item").forEach((item) => {
    item.addEventListener("click", () => {
      const key = item.dataset.preset;
      list.querySelectorAll(".ai-preset-item").forEach((i) => i.classList.remove("selected"));
      item.classList.add("selected");
      document.getElementById("ai-modal-preset").value = key;
      applyAIPresetToModal(key, isEdit);
    });
  });
}

export function applyAIPresetToModal(presetKey, isEdit) {
  const $ = (id) => document.getElementById(id);
  const preset = AI_PRESET_CATALOG[presetKey];
  if (!preset || presetKey === "custom") return;
  if (preset.kind) $("ai-modal-kind").value = preset.kind;
  $("ai-modal-base-url").value = preset.base_url || "";
  if (!isEdit && preset.display_name_default) {
    $("ai-modal-display-name").value = uniqueDisplayName(preset.display_name_default);
  }
  const testResult = $("ai-modal-test-result");
  if (testResult) { testResult.classList.add('hidden'); testResult.textContent = ""; }
  clearProviderModelSelect();
}

/** 生成不与现有 provider 重名的 display_name */
export function uniqueDisplayName(base) {
  const existing = new Set(
    (aiState.currentAIConfig.providers || []).map((p) => (p.display_name || "").trim()),
  );
  if (!existing.has(base)) return base;
  for (let i = 2; i < 100; i++) {
    const candidate = `${base} (${i})`;
    if (!existing.has(candidate)) return candidate;
  }
  return base;
}

// ════════════════════════════════════════════════════════════
//  供应商 modal
// ════════════════════════════════════════════════════════════

export function openAIProviderModal(editProviderId) {
  const $ = (id) => document.getElementById(id);
  const overlay = $("ai-modal-overlay");
  if (!overlay) return;
  const cfg = aiState.currentAIConfig;
  const isEdit = typeof editProviderId === "string" && editProviderId.length > 0;

  if (isEdit) {
    const p = (cfg.providers || []).find((x) => x.id === editProviderId);
    if (!p) {
      console.warn("[ai] 编辑模式找不到 provider", editProviderId);
      return;
    }
    overlay.dataset.editProviderId = editProviderId;
    $("ai-modal-title").textContent = t("ai.modal.title.edit");
    $("ai-modal-kind").value = p.kind;
    $("ai-modal-preset").value = guessPresetForProvider(p.kind, p.base_url);
    $("ai-modal-display-name").value = p.display_name || "";
    $("ai-modal-base-url").value = p.base_url || "";
    $("ai-modal-api-key").value = "";
    $("ai-modal-api-key").placeholder = t("ai.modal.api_key.ph.edit");
    $("ai-modal-api-key").classList.remove("has-secret-hint");
    $("ai-modal-api-key-hint").textContent = t("ai.modal.api_key.hint.edit");
    invoke("get_ai_secret_hint", { providerId: editProviderId }).then((masked) => {
      if (masked && $("ai-modal-api-key").value === "") {
        $("ai-modal-api-key").placeholder = masked + " — " + t("ai.modal.api_key.ph.edit");
        $("ai-modal-api-key").classList.add("has-secret-hint");
      }
    }).catch(() => { });
    $("ai-modal-kind-row").classList.remove('hidden');
    $("ai-modal-preset-row").classList.remove('hidden');
    renderPresetList(guessPresetForProvider(p.kind, p.base_url), true);
    $("ai-modal-kind").disabled = false;
    $("ai-modal-model-section").classList.remove('hidden');
    clearProviderModelSelect();
    aiState._providerSelectedModels = (p.models || []).map((m) => m.id);
    aiState._editOriginalModelIds = [...aiState._providerSelectedModels];
    renderProviderModelTags();
  } else {
    delete overlay.dataset.editProviderId;
    $("ai-modal-title").textContent = t("ai.modal.title");
    $("ai-modal-kind-row").classList.remove('hidden');
    $("ai-modal-preset-row").classList.remove('hidden');
    $("ai-modal-preset").value = "openai";
    $("ai-modal-kind").value = "openai_compatible";
    $("ai-modal-display-name").value = "";
    $("ai-modal-base-url").value = "";
    $("ai-modal-api-key").value = "";
    $("ai-modal-api-key").placeholder = t("ai.modal.api_key.ph");
    $("ai-modal-api-key").classList.remove("has-secret-hint");
    $("ai-modal-api-key-hint").textContent = t("ai.modal.api_key.hint");
    $("ai-modal-kind").disabled = false;
    $("ai-modal-model-section").classList.remove('hidden');
    renderPresetList("openai", false);
    applyAIPresetToModal("openai", false);
    clearProviderModelSelect();
  }
  const testResult = $("ai-modal-test-result");
  if (testResult) { testResult.classList.add('hidden'); testResult.textContent = ""; }
  const errEl = $("ai-modal-error");
  if (errEl) errEl.textContent = "";
  overlay.classList.remove('hidden');
  setTimeout(() => $("ai-modal-display-name")?.focus(), 40);
}

export function closeAIProviderModal() {
  const overlay = document.getElementById("ai-modal-overlay");
  if (!overlay) return;
  overlay.classList.add('hidden');
  const keyInput = document.getElementById("ai-modal-api-key");
  if (keyInput) keyInput.value = "";
  delete overlay.dataset.editProviderId;
}

export async function saveNewProviderFromModal() {
  const $ = (id) => document.getElementById(id);
  const errEl = $("ai-modal-error");
  if (errEl) errEl.textContent = "";

  const displayName = $("ai-modal-display-name").value.trim();
  const kind = $("ai-modal-kind").value;
  const baseUrl = $("ai-modal-base-url").value.trim();
  const apiKey = $("ai-modal-api-key").value.trim();
  const overlay = $("ai-modal-overlay");
  const editProviderId = overlay?.dataset?.editProviderId || null;

  if (!displayName) {
    if (errEl) errEl.textContent = t("ai.modal.err.name");
    return;
  }
  if (kind !== "ollama_http" && !apiKey && !editProviderId) {
    if (errEl) errEl.textContent = t("ai.modal.err.api_key");
    return;
  }

  if (editProviderId) {
    await saveEditedProvider(editProviderId, { displayName, baseUrl, apiKey, errEl, selectedModels: aiState._providerSelectedModels });
  } else {
    await saveNewProvider({ kind, displayName, baseUrl, apiKey, errEl, selectedModels: aiState._providerSelectedModels });
  }
}

async function saveNewProvider({ kind, displayName, baseUrl, apiKey, errEl, selectedModels }) {
  const providerId = displayName.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "") || `provider-${Date.now()}`;
  const cfg = aiState.currentAIConfig;
  if ((cfg.providers || []).some((p) => p.id === providerId)) {
    if (errEl) errEl.textContent = t("ai.modal.err.duplicate_id");
    return;
  }
  if (apiKey) {
    try {
      await invoke("save_ai_secret", { providerId, secret: apiKey });
    } catch (e) {
      if (errEl) errEl.textContent = t("ai.error.save_failed", { err: String(e) });
      return;
    }
  }
  const newModels = (selectedModels || []).map((modelId) => ({
    id: modelId,
    display_name: modelId,
    enabled: true,
    context_window: null,
    input_price_per_million: null,
    output_price_per_million: null,
    temperature: null,
    max_tokens: null,
    custom_parameters: [],
    capabilities: ["chat"],
  }));
  const newProvider = {
    id: providerId,
    kind,
    display_name: displayName,
    base_url: baseUrl || null,
    secret_ref: `blink/${providerId}/key`,
    models: newModels,
    enabled: true, // 0.16.0: 新建 provider 默认启用
    created_at: Math.floor(Date.now() / 1000),
  };
  cfg.providers = [...(cfg.providers || []), newProvider];
  aiState.hasSecretMap.set(providerId, !!apiKey || kind === "ollama_http");

  try {
    await saveAIConfig();
  } catch (e) {
    cfg.providers = cfg.providers.filter((p) => p.id !== providerId);
    if (errEl) errEl.textContent = t("ai.error.save_failed", { err: String(e) });
    return;
  }

  closeAIProviderModal();
  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
}

async function saveEditedProvider(providerId, { displayName, baseUrl, apiKey, errEl, selectedModels }) {
  const cfg = aiState.currentAIConfig;
  const idx = (cfg.providers || []).findIndex((p) => p.id === providerId);
  if (idx < 0) {
    errEl.textContent = t("ai.error.save_failed", { err: "provider not found" });
    return;
  }
  const old = cfg.providers[idx];

  const changingKey = apiKey.length > 0;
  if (changingKey) {
    try {
      await invoke("save_ai_secret", { providerId, secret: apiKey });
    } catch (e) {
      errEl.textContent = t("ai.error.save_failed", { err: String(e) });
      return;
    }
  }

  const existingIds = new Set((old.models || []).map((m) => m.id));
  const newModels = (selectedModels || [])
    .filter((modelId) => !existingIds.has(modelId))
    .map((modelId) => ({
      id: modelId,
      display_name: modelId,
      enabled: true,
      context_window: null,
      input_price_per_million: null,
      output_price_per_million: null,
      temperature: null,
      max_tokens: null,
      custom_parameters: [],
      capabilities: ["chat"],
    }));
  const mergedModels = [...(old.models || []), ...newModels];

  const updated = { ...old, display_name: displayName, base_url: baseUrl || null, models: mergedModels };
  cfg.providers = [
    ...cfg.providers.slice(0, idx),
    updated,
    ...cfg.providers.slice(idx + 1),
  ];
  if (changingKey) aiState.hasSecretMap.set(providerId, true);

  try {
    await saveAIConfig();
  } catch (e) {
    cfg.providers[idx] = old;
    errEl.textContent = t("ai.error.save_failed", { err: String(e) });
    return;
  }

  closeAIProviderModal();
  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
}

export async function deleteAIProvider(providerId) {
  const cfg = aiState.currentAIConfig;
  const provider = (cfg.providers || []).find((p) => p.id === providerId);
  if (!provider) return;

  const referenced = [];
  ["router", "light", "main"].forEach((tier) => {
    const a = cfg[`tier_${tier}`];
    if (a && a.provider_id === providerId) referenced.push(t(`ai.tier.${tier}`));
  });
  const modelCount = (provider.models || []).length;
  const providerName = provider.display_name || provider.id;
  let msg = t("ai.provider.delete.confirm", { name: providerName, models: modelCount });
  if (referenced.length > 0) {
    msg = `${t("ai.provider.referenced", { tiers: referenced.join("、") })}\n\n${msg}`;
  }
  const ok = await confirmDialog(msg, {
    title: t("ai.provider.delete"),
    kind: referenced.length > 0 ? "warning" : "info",
  });
  if (!ok) return;

  try {
    await invoke("delete_ai_secret", { providerId });
  } catch (e) {
    console.error("delete_ai_secret failed:", e);
  }

  cfg.providers = (cfg.providers || []).filter((p) => p.id !== providerId);
  aiState.hasSecretMap.delete(providerId);
  ["router", "light", "main"].forEach((tier) => {
    const a = cfg[`tier_${tier}`];
    if (a && a.provider_id === providerId) {
      cfg[`tier_${tier}`] = null;
    }
  });

  if (cfg.providers.length === 0 && cfg.enabled) {
    cfg.enabled = false;
    const enabledEl = document.getElementById("ai-enabled");
    if (enabledEl) enabledEl.checked = false;
  }

  await saveAIConfig();
  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
}

// ════════════════════════════════════════════════════════════
//  供应商 modal 模型多选
// ════════════════════════════════════════════════════════════

/** 重置模型多选状态 */
export function clearProviderModelSelect() {
  aiState._providerModelCache = null;
  aiState._providerSelectedModels = [];
  aiState._editOriginalModelIds = [];
  const input = document.getElementById("ai-provider-model-input");
  if (input) input.value = "";
  const dropdown = document.getElementById("ai-provider-model-dropdown");
  if (dropdown) { dropdown.classList.add('hidden'); dropdown.innerHTML = ""; }
  renderProviderModelTags();
}

/** 聚焦输入框时触发拉取（首次拉取，后续切换显示） */
export async function triggerProviderModelFetch() {
  if (aiState._providerModelCache && !aiState._providerModelCache.loading && !aiState._providerModelCache.error) {
    filterProviderModels(document.getElementById("ai-provider-model-input")?.value || "");
    return;
  }
  const dropdown = document.getElementById("ai-provider-model-dropdown");
  if (!dropdown) return;
  dropdown.classList.remove('hidden');
  aiState._providerModelCache = { models: [], error: null, loading: true };
  filterProviderModels("");

  const $ = (id) => document.getElementById(id);
  const kind = $("ai-modal-kind").value;
  const baseUrl = $("ai-modal-base-url").value.trim();
  const apiKey = $("ai-modal-api-key").value.trim();
  const overlay = $("ai-modal-overlay");
  const providerId = overlay?.dataset?.editProviderId || null;

  if (!apiKey && !providerId && kind !== "ollama_http") {
    aiState._providerModelCache = { models: [], error: t("ai.modal.test.empty_key"), loading: false };
    filterProviderModels("");
    return;
  }

  try {
    const models = await fetchAvailableModelsFor(kind, baseUrl, providerId);
    aiState._providerModelCache = { models: models || [], error: null, loading: false };
  } catch (e) {
    aiState._providerModelCache = { models: [], error: String(e.message || e), loading: false };
  }
  filterProviderModels(document.getElementById("ai-provider-model-input")?.value || "");
}

/** 按关键词过滤模型列表并渲染下拉 */
export function filterProviderModels(filter) {
  const dropdown = document.getElementById("ai-provider-model-dropdown");
  if (!dropdown) return;
  dropdown.classList.remove('hidden');

  const cache = aiState._providerModelCache;
  if (!cache || cache.loading) {
    dropdown.innerHTML = `<div class="ai-provider-model-dropdown-empty"><span class="ai-spinner"></span> 正在拉取模型列表…</div>`;
    return;
  }
  if (cache.error) {
    const q = (filter || "").trim();
    let html = `<div class="ai-provider-model-dropdown-empty">❌ ${escapeHtml(cache.error)}</div>`;
    if (q && !aiState._providerSelectedModels.includes(q)) {
      html += `<div class="ai-provider-model-dropdown-item ai-manual-add" data-model-id="${escapeAttr(q)}">
        <span>+ 添加 "${escapeHtml(q)}"</span>
      </div>`;
    }
    dropdown.innerHTML = html;
    bindManualAddHandlers(dropdown);
    return;
  }

  const q = (filter || "").trim();
  const qLower = q.toLowerCase();
  const filtered = cache.models
    .filter((m) => (q ? m.toLowerCase().includes(qLower) : true))
    .slice(0, 100);

  const selectedSet = new Set(aiState._providerSelectedModels);
  const editPid = document.getElementById("ai-modal-overlay")?.dataset?.editProviderId;
  const provider = editPid ? (aiState.currentAIConfig.providers || []).find((p) => p.id === editPid) : null;
  const providerModelIds = new Set((provider?.models || []).map((m) => m.id));

  let itemsHtml = filtered.map((m) => {
    const isSelected = selectedSet.has(m);
    const isProviderExisting = providerModelIds.has(m) && !isSelected;
    if (isSelected) {
      return `<label class="ai-provider-model-dropdown-item" data-model-id="${escapeAttr(m)}">
        <input type="checkbox" checked data-model-id="${escapeAttr(m)}" />
        <span>${escapeHtml(m)}</span>
      </label>`;
    }
    if (isProviderExisting) {
      return `<label class="ai-provider-model-dropdown-item is-already" data-model-id="${escapeAttr(m)}">
        <input type="checkbox" disabled data-model-id="${escapeAttr(m)}" />
        <span>${escapeHtml(m)}</span>
        <span class="added-label">已添加</span>
      </label>`;
    }
    return `<label class="ai-provider-model-dropdown-item" data-model-id="${escapeAttr(m)}">
      <input type="checkbox" data-model-id="${escapeAttr(m)}" />
      <span>${escapeHtml(m)}</span>
    </label>`;
  }).join("");

  if (filtered.length === 0 && q) {
    if (!selectedSet.has(q) && !providerModelIds.has(q)) {
      itemsHtml += `<div class="ai-provider-model-dropdown-item ai-manual-add" data-model-id="${escapeAttr(q)}">
        <span>+ 添加 "${escapeHtml(q)}"</span>
      </div>`;
    } else {
      itemsHtml += `<div class="ai-provider-model-dropdown-empty">无匹配模型</div>`;
    }
  } else if (filtered.length === 0) {
    itemsHtml += `<div class="ai-provider-model-dropdown-empty">未返回可用模型</div>`;
  }

  if (q && filtered.length > 0 && !cache.models.some((m) => m.toLowerCase() === qLower) && !selectedSet.has(q) && !providerModelIds.has(q)) {
    itemsHtml += `<div class="ai-provider-model-dropdown-item ai-manual-add" data-model-id="${escapeAttr(q)}">
      <span>+ 添加 "${escapeHtml(q)}"</span>
    </div>`;
  }

  dropdown.innerHTML = itemsHtml;

  dropdown.querySelectorAll('input[type="checkbox"]').forEach((cb) => {
    cb.addEventListener("change", () => {
      toggleProviderModel(cb.dataset.modelId, cb.checked);
    });
  });
  bindManualAddHandlers(dropdown);
}

function bindManualAddHandlers(container) {
  container.querySelectorAll(".ai-manual-add").forEach((el) => {
    el.addEventListener("click", () => {
      const modelId = el.dataset.modelId;
      if (!modelId || aiState._providerSelectedModels.includes(modelId)) return;
      aiState._providerSelectedModels.push(modelId);
      renderProviderModelTags();
      const input = document.getElementById("ai-provider-model-input");
      if (input) input.value = "";
      filterProviderModels("");
    });
  });
}

function toggleProviderModel(modelId, selected) {
  if (selected) {
    if (!aiState._providerSelectedModels.includes(modelId)) {
      aiState._providerSelectedModels.push(modelId);
    }
  } else {
    aiState._providerSelectedModels = aiState._providerSelectedModels.filter((m) => m !== modelId);
  }
  renderProviderModelTags();
}

export function renderProviderModelTags() {
  const container = document.getElementById("ai-provider-model-tags");
  if (!container) return;
  if (aiState._providerSelectedModels.length === 0) {
    container.innerHTML = "";
    return;
  }
  container.innerHTML = aiState._providerSelectedModels.map((m) =>
    `<span class="ai-provider-model-tag">
      ${escapeHtml(m)}
      <button type="button" class="ai-provider-model-tag-remove" data-model-id="${escapeAttr(m)}">${iconHTML("x")}</button>
    </span>`
  ).join("");
  container.querySelectorAll(".ai-provider-model-tag-remove").forEach((btn) => {
    btn.addEventListener("click", () => {
      toggleProviderModel(btn.dataset.modelId, false);
      const cb = document.querySelector(`#ai-provider-model-dropdown input[type="checkbox"][data-model-id="${CSS.escape(btn.dataset.modelId)}"]`);
      if (cb) cb.checked = false;
    });
  });
}
