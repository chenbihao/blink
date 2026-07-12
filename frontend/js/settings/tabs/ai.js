/**
 * AI Tab 模块
 * 包含：AI 总开关 / 供应商配置 / 模型管理（表格+编辑 modal+拉取 popover）/ tier 路由 / preset 目录
 *
 * 主体搬自原 settings.js 2582–4169（0.9.5 拆分时只留了简化框架 + openAIProviderModal
 * 的 TODO 占位，导致无法新增/编辑供应商、模型、tier 全缺；0.9.5.1 整体还原）。
 *
 * 数据流：
 *   loadAIConfig() 拉后端 → 渲染 UI + 记 currentAIConfig
 *   用户改字段 → 写 currentAIConfig → saveAIConfig() → invoke set_config('ai_config')
 *   新增供应商 → modal → save_ai_secret(写 CM) → 更新 providers → saveAIConfig() →
 *     弹 toast 询问总开关（§5.3 严格 opt-in）
 *   删除供应商 → 确认 → delete_ai_secret（幂等）→ 移除 entry → 清悬空 tier → saveAIConfig()
 * **不发密钥回前端**：has_ai_secret 只返 bool
 */
import { invoke, confirmDialog } from "../../tauri.js";
import { t } from "../../i18n/index.js";
import { saveConfig } from "../../config-keys.js";

/** AI 提供商类型标签 */
const AI_KIND_LABEL = {
  openai_compatible: "OpenAI Compatible",
  anthropic_messages: "Anthropic",
  gemini_generate_content: "Gemini",
};

/** 当前 AI 配置（loadAIConfig 拉取后持有，各函数读写） */
let currentAIConfig = null;
/** 密钥存在性映射 provider_id → boolean（不发明文回前端） */
let hasSecretMap = new Map();

// 模型编辑 modal 草稿变量
let _modelEditProviderId = null;
let _modelEditOriginalId = null;
let _modelEditDraft = null;
let _modelSavedToastTimer = null;
/** 拉取模型 popover 缓存 { models, error, loading } 或 null */
let _modelFetchCache = null;

// 供应商 modal 内模型多选状态
let _providerModelCache = null;       // { models, error, loading }
let _providerSelectedModels = [];     // string[]
let _editOriginalModelIds = [];       // 编辑模式：打开时已有的 model id

/**
 * 初始化 AI Tab
 */
export function initAITab() {
  loadAIConfig();
}

/**
 * 加载 AI 配置
 */
async function loadAIConfig() {
  try {
    const cfg = await invoke("get_config_section", { key: "app.ai" });
    currentAIConfig = cfg && typeof cfg === "object" ? cfg : defaultAIConfig();
  } catch (e) {
    console.error("get_config_section app.ai failed:", e);
    currentAIConfig = defaultAIConfig();
  }
  // 密钥存在性并行查询
  hasSecretMap = new Map();
  const providers = currentAIConfig.providers || [];
  await Promise.all(
    providers.map(async (p) => {
      try {
        const has = await invoke("has_ai_secret", { providerId: p.id });
        hasSecretMap.set(p.id, !!has);
      } catch {
        hasSecretMap.set(p.id, false);
      }
    }),
  );
  applyAIConfigToUI();
  bindAIEvents();
}

/**
 * 默认 AI 配置
 */
function defaultAIConfig() {
  return {
    enabled: false,
    allow_intent_routing: false,
    min_query_len: 4,
    require_whitespace: true,
    exclude_pure_numeric: true,
    respect_awareness_url_path: true,
    providers: [],
    tier_router: null,
    tier_light: null,
    tier_main: null,
    direct_execute_safe_actions: false,
    streaming: true,
    slo_hard_timeout_ms: null,
  };
}

/**
 * 应用 AI 配置到 UI
 */
function applyAIConfigToUI() {
  const c = currentAIConfig;
  const $ = (id) => document.getElementById(id);
  if ($("ai-enabled")) $("ai-enabled").checked = !!c.enabled;
  if ($("ai-allow-routing")) $("ai-allow-routing").checked = !!c.allow_intent_routing;
  if ($("ai-min-query-len")) $("ai-min-query-len").value = c.min_query_len ?? 4;
  if ($("ai-require-whitespace")) $("ai-require-whitespace").checked = c.require_whitespace !== false;
  if ($("ai-exclude-pure-numeric")) $("ai-exclude-pure-numeric").checked = c.exclude_pure_numeric !== false;
  if ($("ai-respect-awareness-url-path")) $("ai-respect-awareness-url-path").checked = c.respect_awareness_url_path !== false;
  if ($("ai-streaming")) $("ai-streaming").checked = c.streaming !== false;
  if ($("ai-direct-safe")) $("ai-direct-safe").checked = !!c.direct_execute_safe_actions;
  if ($("ai-timeout-ms")) $("ai-timeout-ms").value = c.slo_hard_timeout_ms ?? 2500;

  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
}

/**
 * 渲染 AI 供应商列表（含模型表格 + accordion + tint monogram）
 */
function renderAIProviders() {
  const container = document.getElementById("ai-providers-container");
  if (!container) return;
  const providers = currentAIConfig.providers || [];
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
      const hasKey = hasSecretMap.get(p.id) === true;
      const statusCls = hasKey ? "" : "no-key";
      const statusText = hasKey ? t("ai.provider.configured") : t("ai.provider.not_configured");
      const presetKey = guessPresetForProvider(p.kind, p.base_url);
      const preset = AI_PRESET_CATALOG[presetKey] || AI_PRESET_CATALOG.custom;
      // monogram：preset 有值用 preset，否则从 display_name 取前 2 字符
      const rawMono = preset.monogram || (p.display_name || "").slice(0, 2);
      const monogram = rawMono || "?";
      const isCJK = /[一-鿿]/.test(monogram);
      const monoCjkCls = isCJK ? " ai-provider-mono--cjk" : "";
      const modelsTable = models.length > 0
        ? `<table class="ai-models-table">
            <colgroup>
              <col class="col-id" />
              <col class="col-name" />
              <col class="col-enabled" />
              <col class="col-actions" />
            </colgroup>
            <thead><tr><th>Model ID</th><th>${escapeHtml(t("ai.model.col.name"))}</th><th>${escapeHtml(t("ai.model.col.enabled"))}</th><th>${escapeHtml(t("ai.model.col.actions"))}</th></tr></thead>
            <tbody>${models.map((m) => {
              const enabled = m.enabled !== false;
              const hasParams = m.temperature != null || m.max_tokens != null || (m.custom_parameters && m.custom_parameters.length > 0);
              const paramsBadge = hasParams
                ? `<span class="ai-model-params-badge" title="${escapeAttr(t("ai.model.params_badge.title"))}">${escapeHtml(t("ai.model.params_badge"))}</span>`
                : "";
              return `<tr data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}">
                <td class="ai-models-table-id" title="${escapeAttr(m.id)}">${escapeHtml(m.id)}${paramsBadge}</td>
                <td title="${escapeAttr(m.display_name || m.id)}">${escapeHtml(m.display_name || m.id)}</td>
                <td>
                  <label class="switch switch-sm">
                    <input type="checkbox" class="ai-model-toggle" data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}" ${enabled ? "checked" : ""} />
                    <span class="slider"></span>
                  </label>
                </td>
                <td class="ai-models-table-actions">
                  <button class="ai-model-edit-btn" data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.model.edit"))}">✎</button>
                  <button class="ai-model-delete" data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.model.delete"))}">✕</button>
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
            <span class="ai-provider-chevron">▸</span>
            <span class="ai-provider-mono${monoCjkCls}" data-tint="${preset.tint}">${escapeHtml(monogram)}</span>
            <div class="ai-provider-info">
              <div class="ai-provider-title">${escapeHtml(p.display_name)}</div>
              <div class="ai-provider-meta">${escapeHtml(kindLabel)} · ${escapeHtml(modelSummary || "(no model)")}</div>
            </div>
            <span class="ai-provider-status ${statusCls}">${escapeHtml(statusText)}</span>
            <button class="ai-provider-edit" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.provider.edit"))}">✎</button>
            <button class="ai-provider-delete" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.provider.delete"))}">✕</button>
          </div>
          <div class="ai-provider-models" style="display:none;">${modelsTable}</div>
        </div>`;
    })
    .join("");
  container.innerHTML =
    cards + `<button class="ai-providers-add" id="ai-add-provider">${escapeHtml(t("ai.providers.add"))}</button>`;

  document.getElementById("ai-add-provider")?.addEventListener("click", () => openAIProviderModal());
  // accordion 展开/折叠
  container.querySelectorAll(".ai-provider-header").forEach((header) => {
    header.addEventListener("click", (e) => {
      if (e.target.closest(".ai-provider-edit") || e.target.closest(".ai-provider-delete")) return;
      const card = header.closest(".ai-provider-card");
      const modelsDiv = card.querySelector(".ai-provider-models");
      const chevron = header.querySelector(".ai-provider-chevron");
      const isOpen = modelsDiv.style.display !== "none";
      modelsDiv.style.display = isOpen ? "none" : "";
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
}

/** 切换模型启用状态 */
function toggleModelEnabled(providerId, modelId, enabled) {
  const providers = currentAIConfig.providers || [];
  const provider = providers.find((p) => p.id === providerId);
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

/**
 * 保存 provider 后引导用户添加模型（展开 accordion + 高亮"添加模型"按钮）
 */
function guideAddModelForProvider(providerId) {
  const tryGuide = (retries) => {
    const card = document.querySelector(`.ai-provider-card[data-provider-id="${CSS.escape(providerId)}"]`);
    if (!card) {
      if (retries > 0) setTimeout(() => tryGuide(retries - 1), 80);
      return;
    }
    const modelsDiv = card.querySelector(".ai-provider-models");
    const chevron = card.querySelector(".ai-provider-chevron");
    if (modelsDiv && modelsDiv.style.display === "none") {
      modelsDiv.style.display = "";
      if (chevron) chevron.textContent = "▾";
    }
    const addBtn = card.querySelector(".ai-model-add-inline");
    if (addBtn) {
      addBtn.classList.add("ai-model-add-inline--pulse");
      addBtn.scrollIntoView({ behavior: "smooth", block: "center" });
      setTimeout(() => addBtn.classList.remove("ai-model-add-inline--pulse"), 3000);
    }
  };
  setTimeout(() => tryGuide(3), 20);
}

/** 从 provider 中删除单个模型（始终 confirm，tier 引用时提示更严重后果） */
async function deleteModelFromProvider(providerId, modelId) {
  const refs = [];
  ["router", "light", "main"].forEach((tier) => {
    const a = currentAIConfig[`tier_${tier}`];
    if (a && a.provider_id === providerId && a.model_id === modelId) refs.push(t(`ai.tier.${tier}`));
  });
  const msg = refs.length > 0
    ? t("ai.model.delete.referenced", { model: modelId, tiers: refs.join("、") })
    : t("ai.model.delete.confirm", { model: modelId });
  const ok = await confirmDialog(msg, {
    title: t("ai.model.delete"),
    kind: refs.length > 0 ? "warning" : "info",
  });
  if (!ok) return;

  const providers = currentAIConfig.providers || [];
  const provider = providers.find((p) => p.id === providerId);
  if (!provider) return;
  provider.models = (provider.models || []).filter((m) => m.id !== modelId);
  ["router", "light", "main"].forEach((tier) => {
    const a = currentAIConfig[`tier_${tier}`];
    if (a && a.provider_id === providerId && a.model_id === modelId) {
      currentAIConfig[`tier_${tier}`] = null;
    }
  });
  saveAIConfig()
    .then(() => {
      renderAIProviders();
      renderAITierSelects();
      renderAITierBanner();
    })
    .catch((e) => console.error("[ai] delete model failed:", e));
}

// ── 模型编辑 modal ────────────────────────────────────────────────────────────

/**
 * 打开模型编辑 modal。modelId 为 null 时是"新增"模式。
 */
function openAIModelEditModal(providerId, modelId) {
  const provider = (currentAIConfig.providers || []).find((p) => p.id === providerId);
  if (!provider) return;
  const isEdit = modelId != null;
  const existing = isEdit ? (provider.models || []).find((m) => m.id === modelId) : null;
  if (isEdit && !existing) return;

  _modelEditProviderId = providerId;
  _modelEditOriginalId = isEdit ? existing.id : null;
  _modelEditDraft = {
    id: existing?.id ?? "",
    display_name: existing?.display_name ?? "",
    enabled: existing?.enabled !== false,
    temperature: existing?.temperature ?? null,
    max_tokens: existing?.max_tokens ?? null,
    custom_parameters: (existing?.custom_parameters || []).map((cp) => ({ ...cp })),
  };

  const $ = (id) => document.getElementById(id);
  $("ai-model-edit-title").textContent = t(isEdit ? "ai.model_modal.title.edit" : "ai.model_modal.title.add");
  $("ai-model-edit-provider-info").textContent = t("ai.model_modal.provider_info", { name: provider.display_name });

  // 新增模式：隐藏调用参数 & 高级段；编辑模式：显示全部
  const paramsSection = document.getElementById("ai-model-param-temperature")?.closest(".ai-modal-section");
  if (paramsSection) {
    const allSections = paramsSection.parentElement?.querySelectorAll(".ai-modal-section");
    // 调用参数段 & 高级段是 modal 内第 2、3 个 section（第 1 个是基础）
    if (allSections && allSections.length >= 3) {
      allSections[1].style.display = isEdit ? "" : "none";
      allSections[2].style.display = isEdit ? "" : "none";
    }
  }
  // 按钮：新增显示"保存并继续"+"完成"，编辑显示"保存"+"取消"
  const continueBtn = document.getElementById("ai-model-edit-continue");
  const saveBtn = document.getElementById("ai-model-edit-save");
  const cancelBtn = document.getElementById("ai-model-edit-cancel");
  if (continueBtn) {
    continueBtn.style.display = isEdit ? "none" : "";
    continueBtn.textContent = t("ai.model_modal.save_continue");
  }
  if (saveBtn) {
    saveBtn.textContent = isEdit ? t("ai.model_modal.save") : t("ai.model_modal.done");
  }
  if (cancelBtn) cancelBtn.textContent = t("ai.model_modal.cancel");

  const idInput = $("ai-model-edit-id");
  idInput.value = _modelEditDraft.id;
  idInput.readOnly = isEdit;
  idInput.classList.toggle("input-readonly", isEdit);
  $("ai-model-edit-display-name").value = _modelEditDraft.display_name;

  closeModelFetchDropdown();
  _modelFetchCache = null;

  setupModelParamRow("temperature", _modelEditDraft.temperature, 0.7);
  setupModelParamRow("max-tokens", _modelEditDraft.max_tokens, 4096);

  renderCustomParams();

  $("ai-model-edit-error").textContent = "";
  const overlay = $("ai-model-edit-overlay");
  overlay.style.display = "flex";
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
  body.style.display = enabled ? "" : "none";
  const shown = enabled ? currentValue : fallbackValue;
  range.value = shown;
  num.value = shown;
}

/** 渲染自定义参数键值对列表 */
function renderCustomParams() {
  const container = document.getElementById("ai-model-edit-custom-params");
  if (!container) return;
  const params = _modelEditDraft.custom_parameters || [];
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
      <button type="button" class="ai-model-custom-param-del" data-idx="${idx}" title="${escapeAttr(t("common.delete"))}">✕</button>
    </div>`;
  }).join("");
  container.querySelectorAll(".ai-model-custom-param-key").forEach((el) => {
    el.addEventListener("input", () => {
      const i = Number(el.dataset.idx);
      _modelEditDraft.custom_parameters[i].key = el.value;
    });
  });
  container.querySelectorAll(".ai-model-custom-param-val").forEach((el) => {
    el.addEventListener("input", () => {
      const i = Number(el.dataset.idx);
      _modelEditDraft.custom_parameters[i].value = coerceCustomParamValue(el.value);
    });
  });
  container.querySelectorAll(".ai-model-custom-param-del").forEach((btn) => {
    btn.addEventListener("click", () => {
      const i = Number(btn.dataset.idx);
      _modelEditDraft.custom_parameters.splice(i, 1);
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
function closeAIModelEditModal() {
  const overlay = document.getElementById("ai-model-edit-overlay");
  if (overlay) overlay.style.display = "none";
  closeModelFetchDropdown();
  _modelEditProviderId = null;
  _modelEditOriginalId = null;
  _modelEditDraft = null;
  _modelFetchCache = null;
}

// ── 拉取模型 popover ──────────────────────────────────────────────────────────

/** 聚焦触发——首次拉取，后续直接显示 */
async function openModelFetchDropdown() {
  const dropdown = document.getElementById("ai-model-edit-fetch-dropdown");
  if (!dropdown) return;
  dropdown.style.display = "";
  if (!_modelFetchCache) {
    await performModelFetch();
  }
  renderModelFetchList(document.getElementById("ai-model-edit-id")?.value || "");
}

/** 关闭下拉但保留缓存（下次打开秒开） */
function closeModelFetchDropdown() {
  const dropdown = document.getElementById("ai-model-edit-fetch-dropdown");
  if (dropdown) dropdown.style.display = "none";
}

/** 执行拉取——从当前 modal 关联的 provider 抓 model 列表 */
async function performModelFetch() {
  const providerId = _modelEditProviderId;
  const provider = (currentAIConfig.providers || []).find((p) => p.id === providerId);
  if (!provider) {
    _modelFetchCache = { models: [], error: t("ai.model_modal.err.provider_gone"), loading: false };
    return;
  }
  _modelFetchCache = { models: [], error: null, loading: true };
  renderModelFetchList("");
  try {
    const models = await fetchAvailableModelsFor(provider.kind, provider.base_url, providerId);
    _modelFetchCache = { models: models || [], error: null, loading: false };
  } catch (e) {
    _modelFetchCache = { models: [], error: String(e.message || e), loading: false };
  }
}

/** 渲染下拉列表——按 filter 过滤；已存在的 model 灰化"已添加" */
function renderModelFetchList(filter) {
  const dropdown = document.getElementById("ai-model-edit-fetch-dropdown");
  if (!dropdown) return;
  const cache = _modelFetchCache;
  if (!cache || cache.loading) {
    dropdown.innerHTML = `<div class="ai-model-edit-fetch-dropdown-empty"><span class="ai-spinner"></span> ${escapeHtml(t("ai.model_modal.fetch.loading"))}</div>`;
    return;
  }
  if (cache.error) {
    dropdown.innerHTML = `<div class="ai-model-edit-fetch-dropdown-empty">${escapeHtml(t("ai.model_modal.fetch.failed", { err: cache.error }))}</div>`;
    return;
  }
  const provider = (currentAIConfig.providers || []).find((p) => p.id === _modelEditProviderId);
  const existingIds = new Set((provider?.models || []).map((m) => m.id));
  const q = (filter || "").trim().toLowerCase();
  const filtered = cache.models
    .filter((m) => (q ? m.toLowerCase().includes(q) : true))
    .slice(0, 100);
  if (filtered.length === 0) {
    const msg = cache.models.length === 0
      ? t("ai.model_modal.fetch.empty")
      : t("ai.model_modal.fetch.no_match");
    dropdown.innerHTML = `<div class="ai-model-edit-fetch-dropdown-empty">${escapeHtml(msg)}</div>`;
    return;
  }
  dropdown.innerHTML = filtered.map((m) => {
    const isExisting = existingIds.has(m);
    return `<div class="ai-model-edit-fetch-dropdown-item${isExisting ? " is-added" : ""}" data-model-id="${escapeAttr(m)}">
      <span>${escapeHtml(m)}</span>
      ${isExisting ? `<span class="added-tag">${escapeHtml(t("ai.modal.badge.added"))}</span>` : ""}
    </div>`;
  }).join("");
  dropdown.querySelectorAll(".ai-model-edit-fetch-dropdown-item:not(.is-added)").forEach((item) => {
    item.addEventListener("mousedown", (e) => {
      e.preventDefault();
      const id = item.dataset.modelId;
      const idInput = document.getElementById("ai-model-edit-id");
      const dispInput = document.getElementById("ai-model-edit-display-name");
      if (idInput) idInput.value = id;
      if (dispInput && !dispInput.value.trim()) dispInput.value = id;
      closeModelFetchDropdown();
    });
  });
}

/**
 * 校验并保存模型——共用逻辑，返回 true 成功 / false 失败（错误已写入 UI）
 */
async function validateAndSaveModel() {
  const $ = (id) => document.getElementById(id);
  const errorEl = $("ai-model-edit-error");
  errorEl.textContent = "";

  const providerId = _modelEditProviderId;
  const provider = (currentAIConfig.providers || []).find((p) => p.id === providerId);
  if (!provider) {
    errorEl.textContent = t("ai.model_modal.err.provider_gone");
    return false;
  }
  const isEdit = _modelEditOriginalId != null;

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

  const cleanedCustom = (_modelEditDraft.custom_parameters || []).filter((cp) => (cp.key || "").trim().length > 0);

  const newModel = {
    id,
    display_name: displayName || id,
    enabled: isEdit ? _modelEditDraft.enabled : true,
    context_window: null,
    input_price_per_million: null,
    output_price_per_million: null,
    temperature: tempToggle ? tempVal : null,
    max_tokens: maxToggle ? Math.floor(maxVal) : null,
    custom_parameters: cleanedCustom,
  };

  if (isEdit) {
    const idx = (provider.models || []).findIndex((m) => m.id === _modelEditOriginalId);
    if (idx < 0) {
      errorEl.textContent = t("ai.model_modal.err.model_gone");
      return false;
    }
    const old = provider.models[idx];
    newModel.enabled = old.enabled !== false;
    newModel.context_window = old.context_window ?? null;
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
    errorEl.textContent = t("ai.error.save_failed", { err: String(e) });
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
  renderAIProviders();
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
  _modelFetchCache = null;
  setTimeout(() => $("ai-model-edit-id").focus(), 40);
  // toast
  showModelSavedToast();
  // 刷新列表（让用户看到刚加的模型出现在 tier select 等处）
  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
}

function showModelSavedToast() {
  const toast = document.getElementById("ai-model-saved-toast");
  if (!toast) return;
  toast.style.display = "flex";
  clearTimeout(_modelSavedToastTimer);
  _modelSavedToastTimer = setTimeout(() => { toast.style.display = "none"; }, 2500);
}

/** 模型 modal 事件绑定——由 bindAIEvents 调一次 */
function bindAIModelEditModalEvents() {
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
      body.style.display = toggle.checked ? "" : "none";
    });
    range.addEventListener("input", () => { num.value = range.value; });
    num.addEventListener("input", () => {
      const v = Number(num.value);
      if (Number.isFinite(v)) range.value = v;
    });
  });

  $("ai-model-edit-custom-params-add")?.addEventListener("click", () => {
    if (!_modelEditDraft) return;
    _modelEditDraft.custom_parameters.push({ key: "", value: "" });
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

  // 聚焦 Model ID 输入框时自动拉取模型列表
  const idInput = $("ai-model-edit-id");
  if (idInput) {
    idInput.addEventListener("focus", () => openModelFetchDropdown());
    idInput.addEventListener("input", () => {
      const dropdown = $("ai-model-edit-fetch-dropdown");
      if (dropdown && dropdown.style.display !== "none") {
        renderModelFetchList(idInput.value);
      }
    });
    idInput.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        const dropdown = $("ai-model-edit-fetch-dropdown");
        if (dropdown && dropdown.style.display !== "none") {
          e.stopPropagation();
          closeModelFetchDropdown();
        }
      }
    });
  }
  // 点击 model select 区域外关闭下拉（但不关 modal）
  const idWrap = $("ai-model-edit-id-wrap");
  if (idWrap) {
    idWrap.addEventListener("mousedown", (e) => {
      if (e.target.closest(".ai-model-edit-fetch-dropdown")) return;
      // 点击 input 本身不关闭
    });
  }

  let downOnOverlay = false;
  overlay.addEventListener("mousedown", (e) => {
    downOnOverlay = e.target.id === "ai-model-edit-overlay";
    // 点击下拉外部时关闭下拉
    const dropdown = $("ai-model-edit-fetch-dropdown");
    if (dropdown && dropdown.style.display !== "none") {
      if (!e.target.closest("#ai-model-edit-id-wrap")) {
        closeModelFetchDropdown();
      }
    }
  });
  overlay.addEventListener("mouseup", (e) => {
    if (downOnOverlay && e.target.id === "ai-model-edit-overlay") closeAIModelEditModal();
    downOnOverlay = false;
  });

  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && overlay.style.display !== "none") {
      closeAIModelEditModal();
    }
  });
}

// ── tier 路由 ─────────────────────────────────────────────────────────────────

function renderAITierSelects() {
  const providers = currentAIConfig.providers || [];
  const options = [`<option value="">${escapeHtml(t("ai.tier.unassigned"))}</option>`];
  providers.forEach((p) => {
    (p.models || []).forEach((m) => {
      const val = `${p.id}::${m.id}`;
      const isEnabled = m.enabled !== false;
      const label = isEnabled
        ? `${p.display_name} / ${m.id}`
        : `${p.display_name} / ${m.id} ${t("ai.tier.model_disabled")}`;
      const disabledAttr = isEnabled ? "" : " disabled";
      options.push(`<option value="${escapeAttr(val)}"${disabledAttr}>${escapeHtml(label)}</option>`);
    });
  });
  const html = options.join("");
  ["router", "light", "main"].forEach((tier) => {
    const sel = document.getElementById(`ai-tier-${tier}`);
    if (!sel) return;
    sel.innerHTML = html;
    const assign = currentAIConfig[`tier_${tier}`];
    sel.value = assign ? `${assign.provider_id}::${assign.model_id}` : "";
  });
  renderAITierDegrade();
}

function renderAITierDegrade() {
  const cfg = currentAIConfig;
  const chain = { router: ["light", "main"], light: ["main"], main: [] };
  const isUsable = (a) => {
    if (!a) return false;
    const provider = (cfg.providers || []).find((p) => p.id === a.provider_id);
    if (!provider) return false;
    const model = (provider.models || []).find((m) => m.id === a.model_id);
    return !!model && model.enabled !== false;
  };
  const findAssignmentDetail = (a) => {
    const provider = (cfg.providers || []).find((p) => p.id === a.provider_id);
    const model = provider && (provider.models || []).find((m) => m.id === a.model_id);
    return { provider, model };
  };
  ["router", "light", "main"].forEach((tier) => {
    const el = document.getElementById(`ai-tier-${tier}-degrade`);
    if (!el) return;
    const assign = cfg[`tier_${tier}`];
    if (assign) {
      const { provider, model } = findAssignmentDetail(assign);
      if (!provider || !model) {
        el.textContent = t("ai.tier.no_provider");
        el.className = "ai-tier-degrade error";
      } else if (model.enabled === false) {
        el.textContent = t("ai.tier.model_disabled_warn", { model: model.id });
        el.className = "ai-tier-degrade error";
      } else {
        el.textContent = "";
        el.className = "ai-tier-degrade";
      }
      return;
    }
    let target = null;
    for (const next of chain[tier]) {
      const a = cfg[`tier_${next}`];
      if (!isUsable(a)) continue;
      const provider = (cfg.providers || []).find((p) => p.id === a.provider_id);
      target = { tier: next, label: provider.display_name };
      break;
    }
    if (target) {
      el.textContent = t("ai.tier.degrade_to", { tier: t(`ai.tier.${target.tier}`) });
      el.className = "ai-tier-degrade warning";
    } else {
      el.textContent = tier === "main" && cfg.enabled ? t("ai.tier.no_provider") : "";
      el.className = tier === "main" && cfg.enabled ? "ai-tier-degrade error" : "ai-tier-degrade";
    }
  });
}

function renderAITierBanner() {
  const banner = document.getElementById("ai-tier-banner");
  if (!banner) return;
  const cfg = currentAIConfig;
  if (!cfg.enabled) {
    banner.style.display = "none";
    return;
  }
  const hasIssue = ["router", "light", "main"].some((tier) => {
    const assign = cfg[`tier_${tier}`];
    if (!assign) return tier !== "main";
    const provider = (cfg.providers || []).find((p) => p.id === assign.provider_id);
    const model = provider && (provider.models || []).find((m) => m.id === assign.model_id);
    return !provider || !model || model.enabled === false;
  });
  const mainAssign = cfg.tier_main;
  const mainMissing = !mainAssign || !(cfg.providers || []).some((p) =>
    p.id === mainAssign.provider_id &&
    (p.models || []).some((m) => m.id === mainAssign.model_id && m.enabled !== false),
  );
  if (!hasIssue && !mainMissing) {
    banner.style.display = "none";
    return;
  }
  banner.style.display = "block";
  banner.textContent = mainMissing
    ? t("ai.tier.no_provider").replace(/^→\s*/, "⚠️ ")
    : "⚠️ " + t("ai.tier.degrade_to", { tier: t("ai.tier.main") });
}

// ── AI 事件绑定（幂等）──────────────────────────────────────────────────────

function bindAIEvents() {
  const root = document.getElementById("ai");
  if (!root || root.dataset.eventsBound === "1") return;
  root.dataset.eventsBound = "1";

  const $ = (id) => document.getElementById(id);

  $("ai-enabled")?.addEventListener("change", (e) => {
    currentAIConfig.enabled = e.target.checked;
    renderAITierBanner();
    saveAIConfig();
  });
  $("ai-allow-routing")?.addEventListener("change", (e) => {
    currentAIConfig.allow_intent_routing = e.target.checked;
    saveAIConfig();
  });
  $("ai-min-query-len")?.addEventListener("change", (e) => {
    const v = parseInt(e.target.value, 10);
    currentAIConfig.min_query_len = isNaN(v) ? 4 : Math.max(1, Math.min(20, v));
    e.target.value = currentAIConfig.min_query_len;
    saveAIConfig();
  });
  $("ai-require-whitespace")?.addEventListener("change", (e) => {
    currentAIConfig.require_whitespace = e.target.checked;
    saveAIConfig();
  });
  $("ai-exclude-pure-numeric")?.addEventListener("change", (e) => {
    currentAIConfig.exclude_pure_numeric = e.target.checked;
    saveAIConfig();
  });
  $("ai-respect-awareness-url-path")?.addEventListener("change", (e) => {
    currentAIConfig.respect_awareness_url_path = e.target.checked;
    saveAIConfig();
  });
  $("ai-streaming")?.addEventListener("change", (e) => {
    currentAIConfig.streaming = e.target.checked;
    saveAIConfig();
  });
  $("ai-direct-safe")?.addEventListener("change", (e) => {
    currentAIConfig.direct_execute_safe_actions = e.target.checked;
    saveAIConfig();
  });
  $("ai-timeout-ms")?.addEventListener("change", (e) => {
    const v = parseInt(e.target.value, 10);
    currentAIConfig.slo_hard_timeout_ms = isNaN(v) ? null : Math.max(500, Math.min(30000, v));
    e.target.value = currentAIConfig.slo_hard_timeout_ms ?? 2500;
    saveAIConfig();
  });

  ["router", "light", "main"].forEach((tier) => {
    $(`ai-tier-${tier}`)?.addEventListener("change", (e) => {
      const val = e.target.value;
      if (!val) {
        currentAIConfig[`tier_${tier}`] = null;
      } else {
        const sep = val.indexOf("::");
        const providerId = val.slice(0, sep);
        const modelId = val.slice(sep + 2);
        currentAIConfig[`tier_${tier}`] = { provider_id: providerId, model_id: modelId };
      }
      renderAITierDegrade();
      renderAITierBanner();
      saveAIConfig();
    });
  });

  // Provider modal 事件
  $("ai-modal-cancel")?.addEventListener("click", closeAIProviderModal);
  $("ai-modal-save")?.addEventListener("click", saveNewProviderFromModal);
  // 供应商 modal：不绑点击外部关闭（避免配置中途误触丢失表单）
  $("ai-modal-kind")?.addEventListener("change", () => {
    $("ai-modal-preset").value = "custom";
  });
  $("ai-modal-base-url")?.addEventListener("input", () => {
    const bu = $("ai-modal-base-url").value.trim();
    const kind = $("ai-modal-kind").value;
    $("ai-modal-preset").value = guessPresetForProvider(kind, bu);
  });

  // 供应商 modal 模型多选：聚焦拉取 + 实时搜索 + Enter 快速添加
  $("ai-provider-model-input")?.addEventListener("focus", () => triggerProviderModelFetch());
  $("ai-provider-model-input")?.addEventListener("input", (e) => filterProviderModels(e.target.value));
  $("ai-provider-model-input")?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      const val = e.target.value.trim();
      if (val && !_providerSelectedModels.includes(val)) {
        _providerSelectedModels.push(val);
        renderProviderModelTags();
        e.target.value = "";
        filterProviderModels("");
      }
    }
  });
  // 点击 modal 外部区域关闭下拉（但不关 modal）
  $("ai-modal-overlay")?.addEventListener("click", (e) => {
    const dropdown = $("ai-provider-model-dropdown");
    if (!dropdown || dropdown.style.display === "none") return;
    if (e.target.closest("#ai-provider-model-select")) return;
    dropdown.style.display = "none";
  });

  $("ai-modal-test")?.addEventListener("click", async () => {
    const btn = $("ai-modal-test");
    const resultEl = $("ai-modal-test-result");
    const kind = $("ai-modal-kind").value;
    const baseUrl = $("ai-modal-base-url").value.trim() || null;
    const apiKey = $("ai-modal-api-key").value.trim();
    const overlay = $("ai-modal-overlay");
    const providerId = overlay.dataset.editProviderId || null;

    if (!apiKey && !providerId) {
      resultEl.textContent = t("ai.modal.test.empty_key");
      resultEl.className = "ai-test-result error";
      resultEl.style.display = "";
      return;
    }

    btn.classList.add("testing");
    btn.textContent = t("ai.modal.test.testing");
    resultEl.style.display = "none";
    try {
      const msg = await invoke("test_ai_provider", {
        kind, baseUrl, apiKey: apiKey || "", providerId: providerId || null,
      });
      resultEl.textContent = `✅ ${msg}`;
      resultEl.className = "ai-test-result success";
      resultEl.style.display = "";
    } catch (e) {
      resultEl.textContent = `❌ ${e}`;
      resultEl.className = "ai-test-result error";
      resultEl.style.display = "";
    } finally {
      btn.classList.remove("testing");
      btn.textContent = t("ai.modal.test");
    }
  });

  $("ai-toast-enable")?.addEventListener("click", () => {
    currentAIConfig.enabled = true;
    $("ai-enabled").checked = true;
    hideAIEnableToast();
    renderAITierBanner();
    saveAIConfig();
  });
  $("ai-toast-later")?.addEventListener("click", hideAIEnableToast);

  bindAIModelEditModalEvents();
}

// ── preset 目录 ───────────────────────────────────────────────────────────────

/**
 * AI 供应商预设目录——按厂商展开成"协议 + base_url + 视觉信息"。
 * base_url = null：该协议不需要用户填（Anthropic / Gemini 走 rig 默认）。
 * kind 必须与后端 ProviderKind serde rename 值一致。
 */
const AI_PRESET_CATALOG = {
  "openai":            { kind: "openai_compatible",     base_url: "https://api.openai.com/v1",                          display_name_default: "OpenAI",               monogram: "OA",   tint: "green",  category: "main" },
  "anthropic":         { kind: "anthropic_messages",    base_url: null,                                                 display_name_default: "Anthropic",            monogram: "An",   tint: "amber",  category: "main" },
  "gemini":            { kind: "gemini_generate_content",base_url: null,                                                display_name_default: "Google Gemini",        monogram: "Ge",   tint: "teal",   category: "main" },
  "deepseek":          { kind: "openai_compatible",     base_url: "https://api.deepseek.com/v1",                        display_name_default: "DeepSeek",             monogram: "深度",  tint: "blue",   category: "cn" },
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
  "ollama":            { kind: "openai_compatible",     base_url: "http://localhost:11434/v1",                          display_name_default: "Ollama",              monogram: "Ol",   tint: "slate",  category: "local" },
  "lm-studio":         { kind: "openai_compatible",     base_url: "http://localhost:1234/v1",                          display_name_default: "LM Studio",           monogram: "LM",   tint: "slate",  category: "local" },
  "custom":            { kind: null,                    base_url: null,                                                 display_name_default: null,                  monogram: null,   tint: "ink",    category: "custom" },
};

/** 猜测 provider 编辑时该回填到哪个 preset。完全匹配 kind + base_url → 命中；否则 "custom" */
function guessPresetForProvider(kind, baseUrl) {
  const bu = (baseUrl || "").trim().replace(/\/$/, "");
  for (const [key, preset] of Object.entries(AI_PRESET_CATALOG)) {
    if (key === "custom") continue;
    if (preset.kind !== kind) continue;
    const presetBu = (preset.base_url || "").replace(/\/$/, "");
    if (presetBu === bu) return key;
  }
  return "custom";
}

/** 预设分组顺序与翻译 key */
const AI_PRESET_GROUPS = [
  { category: "main",   i18nKey: "ai.preset.group.main" },
  { category: "cn",     i18nKey: "ai.preset.group.cn" },
  { category: "gw",     i18nKey: "ai.preset.group.gw" },
  { category: "local",  i18nKey: "ai.preset.group.local" },
  { category: "custom", i18nKey: "ai.preset.group.custom" },
];

/**
 * 渲染预设列表（按 category 分组，组内按 name 排序）
 */
function renderPresetList(selectedKey, isEdit) {
  const list = document.getElementById("ai-preset-list");
  if (!list) return;
  const addedKinds = new Set(
    (currentAIConfig.providers || []).map((p) => {
      const key = guessPresetForProvider(p.kind, p.base_url);
      return key !== "custom" ? key : null;
    }).filter(Boolean),
  );

  // 按 category 分组收集 key
  const grouped = new Map();
  for (const key of Object.keys(AI_PRESET_CATALOG)) {
    const cat = AI_PRESET_CATALOG[key].category || "custom";
    if (!grouped.has(cat)) grouped.set(cat, []);
    grouped.get(cat).push(key);
  }
  // 组内按 display_name 排序（custom 不排）
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

/**
 * 应用 preset 到 modal 字段——kind / base_url / display_name（仅新增时空 name 才填）
 */
function applyAIPresetToModal(presetKey, isEdit) {
  const $ = (id) => document.getElementById(id);
  const preset = AI_PRESET_CATALOG[presetKey];
  if (!preset || presetKey === "custom") return;
  if (preset.kind) $("ai-modal-kind").value = preset.kind;
  $("ai-modal-base-url").value = preset.base_url || "";
  if (!isEdit && preset.display_name_default) {
    $("ai-modal-display-name").value = uniqueDisplayName(preset.display_name_default);
  }
  const testResult = $("ai-modal-test-result");
  if (testResult) { testResult.style.display = "none"; testResult.textContent = ""; }
  // 切换品牌时重置模型选择（不同供应商 API 可用模型不同）
  clearProviderModelSelect();
}


/** 生成不与现有 provider 重名的 display_name */
function uniqueDisplayName(base) {
  const existing = new Set(
    (currentAIConfig.providers || []).map((p) => (p.display_name || "").trim()),
  );
  if (!existing.has(base)) return base;
  for (let i = 2; i < 100; i++) {
    const candidate = `${base} (${i})`;
    if (!existing.has(candidate)) return candidate;
  }
  return base;
}

/**
 * 打开 AI 供应商 modal。editProviderId 传字符串进入编辑模式。
 */
function openAIProviderModal(editProviderId) {
  const $ = (id) => document.getElementById(id);
  const overlay = $("ai-modal-overlay");
  if (!overlay) return;
  const isEdit = typeof editProviderId === "string" && editProviderId.length > 0;

  if (isEdit) {
    const p = (currentAIConfig.providers || []).find((x) => x.id === editProviderId);
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
    $("ai-modal-api-key-hint").textContent = t("ai.modal.api_key.hint.edit");
    // 异步拉取首尾掩码更新 placeholder
    invoke("get_ai_secret_hint", { providerId: editProviderId }).then((masked) => {
      if (masked && $("ai-modal-api-key").value === "") {
        $("ai-modal-api-key").placeholder = masked + " — " + t("ai.modal.api_key.ph.edit");
      }
    }).catch(() => { /* 拉取失败保持原 placeholder */ });
    // 品牌 & 协议：显示但只读
    $("ai-modal-kind-row").style.display = "";
    $("ai-modal-preset-row").style.display = "";
    renderPresetList(guessPresetForProvider(p.kind, p.base_url), true);
    $("ai-modal-kind").disabled = true;
    $("ai-preset-list").querySelectorAll(".ai-preset-item").forEach((el) => {
      el.style.pointerEvents = "none";
      el.style.opacity = "0.5";
    });
    // 模型段：预填已有模型 tag，可继续追加
    $("ai-modal-model-section").style.display = "";
    clearProviderModelSelect();
    _providerSelectedModels = (p.models || []).map((m) => m.id);
    _editOriginalModelIds = [..._providerSelectedModels];
    renderProviderModelTags();
  } else {
    delete overlay.dataset.editProviderId;
    $("ai-modal-title").textContent = t("ai.modal.title");
    $("ai-modal-kind-row").style.display = "";
    $("ai-modal-preset-row").style.display = "";
    $("ai-modal-preset").value = "openai";
    $("ai-modal-kind").value = "openai_compatible";
    $("ai-modal-display-name").value = "";
    $("ai-modal-base-url").value = "";
    $("ai-modal-api-key").value = "";
    $("ai-modal-api-key").placeholder = t("ai.modal.api_key.ph");
    $("ai-modal-api-key-hint").textContent = t("ai.modal.api_key.hint");
    renderPresetList("openai", false);
    applyAIPresetToModal("openai", false);
    $("ai-modal-kind").disabled = false;
    clearProviderModelSelect();
    _editOriginalModelIds = [];
    $("ai-modal-model-section").style.display = "";
  }
  const testResult = $("ai-modal-test-result");
  if (testResult) { testResult.style.display = "none"; testResult.textContent = ""; testResult.className = "ai-test-result"; }
  $("ai-modal-error").textContent = "";
  overlay.style.display = "flex";
  setTimeout(() => $("ai-modal-display-name").focus(), 50);
}

/**
 * 拉取可用模型列表（Model modal / Provider modal 复用）。
 * 全部走后端 command：密钥优先用输入框明文，其次从 CM 读。
 */
async function fetchAvailableModelsFor(kind, baseUrl, providerId) {
  const apiKey = document.getElementById("ai-modal-api-key")?.value?.trim() || null;
  const models = await invoke("fetch_ai_models", {
    kind,
    baseUrl: baseUrl || null,
    apiKey: apiKey || null,
    providerId: providerId || null,
  });
  return models || [];
}

function closeAIProviderModal() {
  const overlay = document.getElementById("ai-modal-overlay");
  if (!overlay) return;
  overlay.style.display = "none";
  const keyInput = document.getElementById("ai-modal-api-key");
  if (keyInput) keyInput.value = "";
  delete overlay.dataset.editProviderId;
}

async function saveNewProviderFromModal() {
  const $ = (id) => document.getElementById(id);
  const errEl = $("ai-modal-error");
  errEl.textContent = "";

  const overlay = $("ai-modal-overlay");
  const editingId = overlay.dataset.editProviderId || null;
  const isEdit = !!editingId;

  const kind = $("ai-modal-kind").value;
  const displayName = $("ai-modal-display-name").value.trim();
  const baseUrl = $("ai-modal-base-url").value.trim();
  const apiKey = $("ai-modal-api-key").value.trim();

  if (!displayName) {
    errEl.textContent = t("ai.modal.save.empty_display");
    return;
  }
  if (kind === "openai_compatible" && !baseUrl) {
    errEl.textContent = t("ai.modal.save.empty_base_url");
    return;
  }
  if (!isEdit && !apiKey) {
    errEl.textContent = t("ai.modal.save.empty_key");
    return;
  }

  if (isEdit) {
    await saveEditedProvider(editingId, { kind, displayName, baseUrl, apiKey, errEl, selectedModels: _providerSelectedModels });
  } else {
    await saveNewProvider({ kind, displayName, baseUrl, apiKey, errEl, selectedModels: _providerSelectedModels });
  }
}

/** 新增 provider（models 可由 modal 内勾选批量创建） */
async function saveNewProvider({ kind, displayName, baseUrl, apiKey, errEl, selectedModels }) {
  const providerId = (crypto.randomUUID && crypto.randomUUID()) || `p-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;

  try {
    await invoke("save_ai_secret", { providerId, secret: apiKey });
  } catch (e) {
    errEl.textContent = t("ai.error.save_failed", { err: String(e) });
    return;
  }

  const initialModels = (selectedModels || []).map((modelId) => ({
    id: modelId,
    display_name: modelId,
    enabled: true,
    context_window: null,
    input_price_per_million: null,
    output_price_per_million: null,
    temperature: null,
    max_tokens: null,
    custom_parameters: [],
  }));

  const newProvider = {
    id: providerId,
    display_name: displayName,
    kind,
    base_url: baseUrl || null,
    secret_ref: `blink/${providerId}/key`,
    models: initialModels,
    created_at: Math.floor(Date.now() / 1000),
  };

  currentAIConfig.providers = [...(currentAIConfig.providers || []), newProvider];
  hasSecretMap.set(providerId, true);

  try {
    await saveAIConfig();
  } catch (e) {
    try { await invoke("delete_ai_secret", { providerId }); } catch { /* noop */ }
    currentAIConfig.providers = currentAIConfig.providers.filter((p) => p.id !== providerId);
    hasSecretMap.delete(providerId);
    errEl.textContent = t("ai.error.save_failed", { err: String(e) });
    return;
  }

  closeAIProviderModal();
  renderAIProviders();
  renderAITierSelects();
  // 没选模型才引导手动添加
  if (!selectedModels || selectedModels.length === 0) {
    guideAddModelForProvider(providerId);
  }
  if (!currentAIConfig.enabled) {
    showAIEnableToast();
  }
}

/**
 * 编辑既有 provider（kind + id + created_at 保持；apiKey 空 → 保留原密钥；模型可追加）
 */
async function saveEditedProvider(providerId, { displayName, baseUrl, apiKey, errEl, selectedModels }) {
  const idx = (currentAIConfig.providers || []).findIndex((p) => p.id === providerId);
  if (idx < 0) {
    errEl.textContent = t("ai.error.save_failed", { err: "provider not found" });
    return;
  }
  const old = currentAIConfig.providers[idx];

  const changingKey = apiKey.length > 0;
  if (changingKey) {
    try {
      await invoke("save_ai_secret", { providerId, secret: apiKey });
    } catch (e) {
      errEl.textContent = t("ai.error.save_failed", { err: String(e) });
      return;
    }
  }

  // 合并新选模型（跳过已存在的）
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
    }));
  const mergedModels = [...(old.models || []), ...newModels];

  const updated = { ...old, display_name: displayName, base_url: baseUrl || null, models: mergedModels };
  currentAIConfig.providers = [
    ...currentAIConfig.providers.slice(0, idx),
    updated,
    ...currentAIConfig.providers.slice(idx + 1),
  ];
  if (changingKey) hasSecretMap.set(providerId, true);

  try {
    await saveAIConfig();
  } catch (e) {
    currentAIConfig.providers[idx] = old;
    errEl.textContent = t("ai.error.save_failed", { err: String(e) });
    return;
  }

  closeAIProviderModal();
  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
}

async function deleteAIProvider(providerId) {
  const provider = (currentAIConfig.providers || []).find((p) => p.id === providerId);
  if (!provider) return;

  const referenced = [];
  ["router", "light", "main"].forEach((tier) => {
    const a = currentAIConfig[`tier_${tier}`];
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

  currentAIConfig.providers = (currentAIConfig.providers || []).filter((p) => p.id !== providerId);
  hasSecretMap.delete(providerId);
  ["router", "light", "main"].forEach((tier) => {
    const a = currentAIConfig[`tier_${tier}`];
    if (a && a.provider_id === providerId) {
      currentAIConfig[`tier_${tier}`] = null;
    }
  });

  if (currentAIConfig.providers.length === 0 && currentAIConfig.enabled) {
    currentAIConfig.enabled = false;
    const enabledEl = document.getElementById("ai-enabled");
    if (enabledEl) enabledEl.checked = false;
  }

  await saveAIConfig();
  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
}

async function saveAIConfig() {
  try {
    await saveConfig("ai_config", currentAIConfig);
  } catch (e) {
    console.error("save ai_config failed:", e);
    throw e;
  }
}

function showAIEnableToast() {
  const toast = document.getElementById("ai-enable-toast");
  if (!toast) return;
  toast.style.display = "flex";
  clearTimeout(showAIEnableToast._t);
  showAIEnableToast._t = setTimeout(hideAIEnableToast, 8000);
}

function hideAIEnableToast() {
  const toast = document.getElementById("ai-enable-toast");
  if (!toast) return;
  toast.style.display = "none";
  clearTimeout(showAIEnableToast._t);
}

// ── 供应商 modal 模型多选 ─────────────────────────────────────────────────────

/** 重置模型多选状态 */
function clearProviderModelSelect() {
  _providerModelCache = null;
  _providerSelectedModels = [];
  _editOriginalModelIds = [];
  const input = document.getElementById("ai-provider-model-input");
  if (input) input.value = "";
  const dropdown = document.getElementById("ai-provider-model-dropdown");
  if (dropdown) { dropdown.style.display = "none"; dropdown.innerHTML = ""; }
  renderProviderModelTags();
}

/**
 * 聚焦输入框时触发拉取（首次拉取，后续切换显示）
 */
async function triggerProviderModelFetch() {
  if (_providerModelCache && !_providerModelCache.loading && !_providerModelCache.error) {
    // 已有缓存，直接显示
    filterProviderModels(document.getElementById("ai-provider-model-input")?.value || "");
    return;
  }
  const dropdown = document.getElementById("ai-provider-model-dropdown");
  if (!dropdown) return;
  dropdown.style.display = "";
  _providerModelCache = { models: [], error: null, loading: true };
  filterProviderModels("");

  const $ = (id) => document.getElementById(id);
  const kind = $("ai-modal-kind").value;
  const baseUrl = $("ai-modal-base-url").value.trim();
  const apiKey = $("ai-modal-api-key").value.trim();
  const overlay = $("ai-modal-overlay");
  const providerId = overlay?.dataset?.editProviderId || null;

  if (!apiKey && !providerId) {
    _providerModelCache = { models: [], error: t("ai.modal.test.empty_key"), loading: false };
    filterProviderModels("");
    return;
  }

  try {
    const models = await fetchAvailableModelsFor(kind, baseUrl, providerId);
    _providerModelCache = { models: models || [], error: null, loading: false };
  } catch (e) {
    _providerModelCache = { models: [], error: String(e.message || e), loading: false };
  }
  filterProviderModels(document.getElementById("ai-provider-model-input")?.value || "");
}

/**
 * 按关键词过滤模型列表并渲染下拉
 */
function filterProviderModels(filter) {
  const dropdown = document.getElementById("ai-provider-model-dropdown");
  if (!dropdown) return;
  dropdown.style.display = "";

  const cache = _providerModelCache;
  if (!cache || cache.loading) {
    dropdown.innerHTML = `<div class="ai-provider-model-dropdown-empty"><span class="ai-spinner"></span> 正在拉取模型列表…</div>`;
    return;
  }
  if (cache.error) {
    // 拉取失败：显示错误 + 手动输入提示
    const q = (filter || "").trim();
    let html = `<div class="ai-provider-model-dropdown-empty">❌ ${escapeHtml(cache.error)}</div>`;
    if (q && !_providerSelectedModels.includes(q)) {
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

  const selectedSet = new Set(_providerSelectedModels);
  // 编辑模式：provider 已有的模型也算"已添加"
  const editPid = document.getElementById("ai-modal-overlay")?.dataset?.editProviderId;
  const provider = editPid ? (currentAIConfig.providers || []).find((p) => p.id === editPid) : null;
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

  // 无匹配 + 有输入 → 显示手动添加选项
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

  // 有匹配但输入文本本身不在列表中 → 也显示手动添加
  if (q && filtered.length > 0 && !cache.models.some((m) => m.toLowerCase() === qLower) && !selectedSet.has(q) && !providerModelIds.has(q)) {
    itemsHtml += `<div class="ai-provider-model-dropdown-item ai-manual-add" data-model-id="${escapeAttr(q)}">
      <span>+ 添加 "${escapeHtml(q)}"</span>
    </div>`;
  }

  dropdown.innerHTML = itemsHtml;

  // checkbox 事件
  dropdown.querySelectorAll('input[type="checkbox"]').forEach((cb) => {
    cb.addEventListener("change", () => {
      toggleProviderModel(cb.dataset.modelId, cb.checked);
    });
  });
  bindManualAddHandlers(dropdown);
}

/** 绑定手动添加项的点击事件 */
function bindManualAddHandlers(container) {
  container.querySelectorAll(".ai-manual-add").forEach((el) => {
    el.addEventListener("click", () => {
      const modelId = el.dataset.modelId;
      if (!modelId || _providerSelectedModels.includes(modelId)) return;
      _providerSelectedModels.push(modelId);
      renderProviderModelTags();
      // 清空输入框 & 刷新下拉
      const input = document.getElementById("ai-provider-model-input");
      if (input) input.value = "";
      filterProviderModels("");
    });
  });
}

/** 切换模型选中状态 */
function toggleProviderModel(modelId, selected) {
  if (selected) {
    if (!_providerSelectedModels.includes(modelId)) {
      _providerSelectedModels.push(modelId);
    }
  } else {
    _providerSelectedModels = _providerSelectedModels.filter((m) => m !== modelId);
  }
  renderProviderModelTags();
}

/** 渲染已选模型 tag 列表 */
function renderProviderModelTags() {
  const container = document.getElementById("ai-provider-model-tags");
  if (!container) return;
  if (_providerSelectedModels.length === 0) {
    container.innerHTML = "";
    return;
  }
  container.innerHTML = _providerSelectedModels.map((m) =>
    `<span class="ai-provider-model-tag">
      ${escapeHtml(m)}
      <button type="button" class="ai-provider-model-tag-remove" data-model-id="${escapeAttr(m)}">✕</button>
    </span>`
  ).join("");
  container.querySelectorAll(".ai-provider-model-tag-remove").forEach((btn) => {
    btn.addEventListener("click", () => {
      toggleProviderModel(btn.dataset.modelId, false);
      // 同步取消 dropdown 中的 checkbox
      const cb = document.querySelector(`#ai-provider-model-dropdown input[type="checkbox"][data-model-id="${CSS.escape(btn.dataset.modelId)}"]`);
      if (cb) cb.checked = false;
    });
  });
}

// ── helper ────────────────────────────────────────────────────────────────────

/** HTML 转义 */
function escapeHtml(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

/** 属性转义 */
function escapeAttr(s) {
  return escapeHtml(s);
}
