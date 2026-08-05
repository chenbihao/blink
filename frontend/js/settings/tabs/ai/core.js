//! AI Tab 核心：init + config 加载 + UI 应用 + 事件绑定（0.14.6 §4.2 拆分）。
//!
//! 本模块是 AI Tab 的入口和协调层：
//! - initAITab() — 对外入口（由 ai.js re-export）
//! - loadAIConfig() — 从后端加载配置
//! - applyAIConfigToUI() — 将配置应用到 UI
//! - bindAIEvents() — 绑定所有 AI 相关事件（幂等）
//!
//! 依赖所有子模块：state / tier / provider / model-edit / skill

import { aiState, defaultAIConfig, saveAIConfig } from "./state.js";
import { renderAITierSelects, renderAITierDegrade, renderAITierBanner } from "./tier.js";
import { renderAIProviders, closeAIProviderModal, saveNewProviderFromModal, guessPresetForProvider, clearProviderModelSelect, triggerProviderModelFetch, filterProviderModels, renderProviderModelTags } from "./provider.js";
import { bindAIModelEditModalEvents } from "./model-edit.js";
import { loadSkillList, showSkillImportPanel, initSkillImportHandlers } from "./skill.js";
import { invoke } from "../../../shared/tauri.js";
import { t, onLangChange } from "../../../i18n/index.js";

// 0.17.8: AI 权限记忆配置的默认值
const defaultPermissionConfig = { memory_enabled: true, memory_days: 7 };
// 运行时副本，事件回调中读写
let permissionConfig = { ...defaultPermissionConfig };

/**
 * 初始化 AI Tab
 */
export function initAITab() {
  loadAIConfig();
  onLangChange(() => {
    updateMemoryContextSizeDisplay();
  });
}

/**
 * 加载 AI 配置
 */
async function loadAIConfig() {
  try {
    const cfg = await invoke("get_config_section", { key: "app.ai" });
    aiState.currentAIConfig = cfg && typeof cfg === "object" ? cfg : defaultAIConfig();
  } catch (e) {
    console.error("get_config_section app.ai failed:", e);
    aiState.currentAIConfig = defaultAIConfig();
  }
  // 密钥存在性 + 掩码提示并行查询
  aiState.hasSecretMap = new Map();
  aiState.secretHintMap = new Map();
  const providers = aiState.currentAIConfig.providers || [];
  await Promise.all(
    providers.map(async (p) => {
      try {
        const has = await invoke("has_ai_secret", { providerId: p.id });
        aiState.hasSecretMap.set(p.id, !!has);
        if (has) {
          try {
            const hint = await invoke("get_ai_secret_hint", { providerId: p.id });
            aiState.secretHintMap.set(p.id, hint || null);
          } catch {
            aiState.secretHintMap.set(p.id, null);
          }
        }
      } catch {
        aiState.hasSecretMap.set(p.id, false);
      }
    }),
  );
  // 0.17.8: 加载 AI 权限记忆配置（独立分片 app.ai_permission）
  try {
    const perm = await invoke("get_config_section", { key: "app.ai_permission" });
    permissionConfig = perm && typeof perm === "object" ? { ...defaultPermissionConfig, ...perm } : { ...defaultPermissionConfig };
  } catch (e) {
    console.warn("get_config_section app.ai_permission failed:", e);
    permissionConfig = { ...defaultPermissionConfig };
  }

  applyAIConfigToUI();
  bindAIEvents();
}

/**
 * 应用 AI 配置到 UI
 */
function applyAIConfigToUI() {
  const c = aiState.currentAIConfig;
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
  // 对话配置
  const chatCfg = c.chat_config || { auto_title: false, title_tier: "light" };
  if ($("ai-chat-auto-title")) $("ai-chat-auto-title").checked = !!chatCfg.auto_title;
  if ($("ai-chat-title-tier")) $("ai-chat-title-tier").value = chatCfg.title_tier || "light";
  // 记忆策略配置
  const memCfg = chatCfg.memory_config || { mode: "token_aware", window_size: 20, trigger_ratio: 0.8, compress_ratio: 0.7, recall_enabled: true, recall_top_k: 3 };
  if ($("ai-memory-mode")) $("ai-memory-mode").value = memCfg.mode || "token_aware";
  if ($("ai-memory-window-size")) $("ai-memory-window-size").value = memCfg.window_size ?? 20;
  if ($("ai-memory-trigger-ratio")) {
    $("ai-memory-trigger-ratio").value = Math.round((memCfg.trigger_ratio ?? 0.8) * 100);
  }
  if ($("ai-memory-compress-ratio")) {
    $("ai-memory-compress-ratio").value = Math.round((memCfg.compress_ratio ?? 0.7) * 100);
  }
  if ($("ai-memory-recall-enabled")) $("ai-memory-recall-enabled").checked = memCfg.recall_enabled !== false;
  if ($("ai-memory-recall-top-k")) $("ai-memory-recall-top-k").value = memCfg.recall_top_k ?? 3;
  updateMemoryConfigVisibility(memCfg.mode || "token_aware");
  updateMemoryContextSizeDisplay();
  // Skill 配置
  const skillCfg = chatCfg.skill_config || { enabled: true };
  if ($("ai-skill-enabled")) $("ai-skill-enabled").checked = skillCfg.enabled !== false;
  loadSkillList();
  // 0.17.8: 权限记忆配置
  if ($("ai-perm-memory-enabled")) $("ai-perm-memory-enabled").checked = permissionConfig.memory_enabled !== false;
  if ($("ai-perm-memory-days")) $("ai-perm-memory-days").value = permissionConfig.memory_days ?? 7;

  // 工具结果回流 AI 三态分段按钮
  const feedbackValue = c.ai_tool_result_feedback ?? "on";
  setSegControlValue("ai-tool-feedback", feedbackValue);

  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
  loadSystemPromptInfo();
}

/**
 * 加载并展示 system prompt token 信息
 */
async function loadSystemPromptInfo() {
  const tokensEl = document.getElementById("ai-prompt-tokens");
  const metaEl = document.getElementById("ai-prompt-meta");
  if (!tokensEl || !metaEl) return;

  try {
    const info = await invoke("get_system_prompt_info");
    const tokens = info.tokens ?? 0;
    const threshold = info.threshold ?? 1500;
    const toolsCount = info.tools_count ?? 0;

    tokensEl.textContent = `${tokens} / ${threshold}`;
    if (tokens > threshold) {
      tokensEl.className = "ai-prompt-tokens ai-prompt-tokens--over";
    } else if (tokens > threshold * 0.8) {
      tokensEl.className = "ai-prompt-tokens ai-prompt-tokens--warn";
    } else {
      tokensEl.className = "ai-prompt-tokens";
    }
    metaEl.textContent = `· ${toolsCount} 个工具`;
  } catch (e) {
    tokensEl.textContent = "—";
    tokensEl.className = "ai-prompt-tokens";
    metaEl.textContent = "";
    console.warn("[ai] get_system_prompt_info failed:", e);
  }
}

// ── UI helpers ──────────────────────────────────────────────

function setSegControlValue(id, value) {
  const container = document.getElementById(id);
  if (!container) return;
  container.querySelectorAll(".seg-btn").forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.value === value);
  });
}

function updateMemoryConfigVisibility(mode) {
  const body = document.getElementById("ai-memory-config-body");
  if (!body) return;
  body.classList.toggle('hidden', mode === "off");
}

function updateMemoryContextSizeDisplay() {
  const el = document.getElementById("ai-memory-context-size");
  if (!el) return;
  const cfg = aiState.currentAIConfig;
  if (!cfg) return;
  const mainAssign = cfg.tier_main;
  if (!mainAssign) {
    el.textContent = t("ai.memory.context_size.unconfigured") || "未配置";
    return;
  }
  const provider = (cfg.providers || []).find((p) => p.id === mainAssign.provider_id);
  const model = provider && (provider.models || []).find((m) => m.id === mainAssign.model_id);
  const ctxWindow = model?.context_window;
  if (!ctxWindow) {
    el.textContent = t("ai.memory.context_size.unknown") || "未知";
    return;
  }
  const memCfg = cfg.chat_config?.memory_config || {};
  const windowSize = memCfg.window_size ?? 20;
  const estimated = Math.round(ctxWindow * 0.6);
  el.textContent = `${estimated} tokens (≈${windowSize} 轮)`;
}

// ── Toast ───────────────────────────────────────────────────

function showAIEnableToast() {
  const toast = document.getElementById("ai-enable-toast");
  if (!toast) return;
  toast.classList.remove('hidden');
  clearTimeout(showAIEnableToast._t);
  showAIEnableToast._t = setTimeout(hideAIEnableToast, 8000);
}

function hideAIEnableToast() {
  const toast = document.getElementById("ai-enable-toast");
  if (!toast) return;
  toast.classList.add('hidden');
  clearTimeout(showAIEnableToast._t);
}

// ── 事件绑定（幂等）─────────────────────────────────────────

function bindAIEvents() {
  const root = document.getElementById("ai-providers");
  if (!root || root.dataset.eventsBound === "1") return;
  root.dataset.eventsBound = "1";

  const $ = (id) => document.getElementById(id);
  const cfg = aiState.currentAIConfig;

  $("ai-enabled")?.addEventListener("change", (e) => {
    cfg.enabled = e.target.checked;
    renderAITierBanner();
    saveAIConfig();
  });
  $("ai-allow-routing")?.addEventListener("change", (e) => {
    cfg.allow_intent_routing = e.target.checked;
    saveAIConfig();
  });
  $("ai-min-query-len")?.addEventListener("change", (e) => {
    const v = parseInt(e.target.value, 10);
    cfg.min_query_len = isNaN(v) ? 4 : Math.max(1, Math.min(20, v));
    e.target.value = cfg.min_query_len;
    saveAIConfig();
  });
  $("ai-require-whitespace")?.addEventListener("change", (e) => {
    cfg.require_whitespace = e.target.checked;
    saveAIConfig();
  });
  $("ai-exclude-pure-numeric")?.addEventListener("change", (e) => {
    cfg.exclude_pure_numeric = e.target.checked;
    saveAIConfig();
  });
  $("ai-respect-awareness-url-path")?.addEventListener("change", (e) => {
    cfg.respect_awareness_url_path = e.target.checked;
    saveAIConfig();
  });
  $("ai-streaming")?.addEventListener("change", (e) => {
    cfg.streaming = e.target.checked;
    saveAIConfig();
  });
  $("ai-direct-safe")?.addEventListener("change", (e) => {
    cfg.direct_execute_safe_actions = e.target.checked;
    saveAIConfig();
  });
  $("ai-chat-auto-title")?.addEventListener("change", (e) => {
    cfg.chat_config = cfg.chat_config || { auto_title: false, title_tier: "light" };
    cfg.chat_config.auto_title = e.target.checked;
    saveAIConfig();
  });
  $("ai-chat-title-tier")?.addEventListener("change", (e) => {
    cfg.chat_config = cfg.chat_config || { auto_title: false, title_tier: "light" };
    cfg.chat_config.title_tier = e.target.value;
    saveAIConfig();
  });
  $("ai-memory-mode")?.addEventListener("change", (e) => {
    cfg.chat_config = cfg.chat_config || {};
    cfg.chat_config.memory_config = cfg.chat_config.memory_config || {};
    cfg.chat_config.memory_config.mode = e.target.value;
    updateMemoryConfigVisibility(e.target.value);
    saveAIConfig();
  });
  $("ai-memory-window-size")?.addEventListener("change", (e) => {
    const v = parseInt(e.target.value, 10);
    cfg.chat_config = cfg.chat_config || {};
    cfg.chat_config.memory_config = cfg.chat_config.memory_config || {};
    cfg.chat_config.memory_config.window_size = isNaN(v) ? 20 : Math.max(5, Math.min(100, v));
    e.target.value = cfg.chat_config.memory_config.window_size;
    saveAIConfig();
  });
  $("ai-memory-trigger-ratio")?.addEventListener("change", (e) => {
    const v = parseInt(e.target.value, 10);
    const clamped = isNaN(v) ? 80 : Math.max(50, Math.min(95, v));
    e.target.value = clamped;
    cfg.chat_config = cfg.chat_config || {};
    cfg.chat_config.memory_config = cfg.chat_config.memory_config || {};
    cfg.chat_config.memory_config.trigger_ratio = clamped / 100;
    saveAIConfig();
  });
  $("ai-memory-compress-ratio")?.addEventListener("change", (e) => {
    const v = parseInt(e.target.value, 10);
    const clamped = isNaN(v) ? 70 : Math.max(30, Math.min(90, v));
    e.target.value = clamped;
    cfg.chat_config = cfg.chat_config || {};
    cfg.chat_config.memory_config = cfg.chat_config.memory_config || {};
    cfg.chat_config.memory_config.compress_ratio = clamped / 100;
    saveAIConfig();
  });
  $("ai-skill-enabled")?.addEventListener("change", (e) => {
    cfg.chat_config = cfg.chat_config || {};
    cfg.chat_config.skill_config = cfg.chat_config.skill_config || {};
    cfg.chat_config.skill_config.enabled = e.target.checked;
    saveAIConfig();
  });
  $("ai-skill-refresh")?.addEventListener("click", async () => {
    const btn = $("ai-skill-refresh");
    if (btn) { btn.disabled = true; btn.textContent = "..."; }
    try {
      await invoke("refresh_skills");
      await loadSkillList();
      if (btn) btn.textContent = t("ai.skill.refresh") || "刷新";
    } catch (e) {
      console.error("refresh_skills failed:", e);
      if (btn) btn.textContent = t("ai.skill.refresh") || "刷新";
    }
    btn && (btn.disabled = false);
  });

  $("ai-skill-import-btn")?.addEventListener("click", () => showSkillImportPanel());
  initSkillImportHandlers();

  // Skill 编辑 modal 事件
  $("skill-edit-cancel")?.addEventListener("click", () => {
    const overlay = $("skill-edit-overlay");
    if (overlay) overlay.classList.add('hidden');
    const errorEl = $("skill-edit-error");
    if (errorEl) errorEl.textContent = "";
  });
  $("skill-edit-overlay")?.addEventListener("click", (e) => {
    if (e.target.id === "skill-edit-overlay") {
      e.target.classList.add('hidden');
    }
  });
  $("skill-edit-save")?.addEventListener("click", async () => {
    const overlay = $("skill-edit-overlay");
    if (!overlay) return;
    const content = $("skill-edit-textarea")?.value || "";
    const skillDir = overlay.dataset.skillDir;
    const errorEl = $("skill-edit-error");
    if (errorEl) errorEl.textContent = "";
    if (!skillDir) return;
    try {
      await invoke("save_skill_md", { skillDir, content });
      overlay.classList.add('hidden');
      await invoke("refresh_skills");
      await loadSkillList();
    } catch (e) {
      if (errorEl) errorEl.textContent = String(e);
      console.error("save_skill_md failed:", e);
    }
  });
  $("ai-memory-recall-enabled")?.addEventListener("change", (e) => {
    cfg.chat_config = cfg.chat_config || {};
    cfg.chat_config.memory_config = cfg.chat_config.memory_config || {};
    cfg.chat_config.memory_config.recall_enabled = e.target.checked;
    saveAIConfig();
  });
  $("ai-memory-recall-top-k")?.addEventListener("change", (e) => {
    const v = parseInt(e.target.value, 10);
    cfg.chat_config = cfg.chat_config || {};
    cfg.chat_config.memory_config = cfg.chat_config.memory_config || {};
    cfg.chat_config.memory_config.recall_top_k = isNaN(v) ? 3 : Math.max(1, Math.min(10, v));
    e.target.value = cfg.chat_config.memory_config.recall_top_k;
    saveAIConfig();
  });
  $("ai-tool-feedback")?.addEventListener("click", (e) => {
    const btn = e.target.closest(".seg-btn");
    if (!btn) return;
    const val = btn.dataset.value;
    setSegControlValue("ai-tool-feedback", val);
    cfg.ai_tool_result_feedback = val;
    saveAIConfig();
  });
  $("ai-timeout-ms")?.addEventListener("change", (e) => {
    const v = parseInt(e.target.value, 10);
    cfg.slo_hard_timeout_ms = isNaN(v) ? null : Math.max(500, Math.min(30000, v));
    e.target.value = cfg.slo_hard_timeout_ms ?? 2500;
    saveAIConfig();
  });

  ["router", "light", "main"].forEach((tier) => {
    $(`ai-tier-${tier}`)?.addEventListener("change", (e) => {
      const val = e.target.value;
      if (!val) {
        cfg[`tier_${tier}`] = null;
      } else {
        const sep = val.indexOf("::");
        const providerId = val.slice(0, sep);
        const modelId = val.slice(sep + 2);
        cfg[`tier_${tier}`] = { provider_id: providerId, model_id: modelId };
      }
      renderAITierDegrade();
      renderAITierBanner();
      if (tier === "main") updateMemoryContextSizeDisplay();
      saveAIConfig();
    });
  });

  // 0.17.8: 权限记忆配置事件
  $("ai-perm-memory-enabled")?.addEventListener("change", (e) => {
    permissionConfig.memory_enabled = e.target.checked;
    invoke("set_config", { key: "ai_permission", value: permissionConfig }).catch((e) =>
      console.error("set_config ai_permission failed:", e),
    );
  });
  $("ai-perm-memory-days")?.addEventListener("change", (e) => {
    const v = parseInt(e.target.value, 10);
    permissionConfig.memory_days = isNaN(v) ? 7 : Math.max(1, Math.min(90, v));
    e.target.value = permissionConfig.memory_days;
    invoke("set_config", { key: "ai_permission", value: permissionConfig }).catch((e) =>
      console.error("set_config ai_permission failed:", e),
    );
  });
  $("ai-perm-clear-memory")?.addEventListener("click", async () => {
    const btn = $("ai-perm-clear-memory");
    if (btn) { btn.disabled = true; }
    try {
      await invoke("clear_all_permission_memory");
    } catch (e) {
      console.error("clear_all_permission_memory failed:", e);
    } finally {
      if (btn) { btn.disabled = false; }
    }
  });

  // Provider modal 事件
  $("ai-modal-cancel")?.addEventListener("click", closeAIProviderModal);
  $("ai-modal-save")?.addEventListener("click", saveNewProviderFromModal);
  $("ai-modal-kind")?.addEventListener("change", () => {
    $("ai-modal-preset").value = "custom";
  });
  $("ai-modal-base-url")?.addEventListener("input", () => {
    const bu = $("ai-modal-base-url").value.trim();
    const kind = $("ai-modal-kind").value;
    $("ai-modal-preset").value = guessPresetForProvider(kind, bu);
  });

  // 供应商 modal 模型多选
  $("ai-provider-model-input")?.addEventListener("focus", () => triggerProviderModelFetch());
  $("ai-provider-model-input")?.addEventListener("input", (e) => filterProviderModels(e.target.value));
  $("ai-provider-model-input")?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      const val = e.target.value.trim();
      if (val && !aiState._providerSelectedModels.includes(val)) {
        aiState._providerSelectedModels.push(val);
        renderProviderModelTags();
        e.target.value = "";
        filterProviderModels("");
      }
    }
  });
  $("ai-modal-overlay")?.addEventListener("click", (e) => {
    const dropdown = $("ai-provider-model-dropdown");
    if (!dropdown || dropdown.classList.contains('hidden')) return;
    if (e.target.closest("#ai-provider-model-select")) return;
    dropdown.classList.add('hidden');
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      const modelOverlay = $("ai-model-edit-overlay");
      if (modelOverlay && !modelOverlay.classList.contains('hidden')) return;
      const overlay = $("ai-modal-overlay");
      if (overlay && !overlay.classList.contains('hidden')) {
        closeAIProviderModal();
      }
    }
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
      resultEl.classList.remove('hidden');
      return;
    }

    btn.classList.add("testing");
    btn.textContent = t("ai.modal.test.testing");
    resultEl.classList.add('hidden');
    try {
      const msg = await invoke("test_ai_provider", {
        kind, baseUrl, apiKey: apiKey || "", providerId: providerId || null,
      });
      resultEl.textContent = msg;
      resultEl.className = "ai-test-result success";
      resultEl.classList.remove('hidden');
    } catch (e) {
      resultEl.textContent = String(e);
      resultEl.className = "ai-test-result error";
      resultEl.classList.remove('hidden');
    } finally {
      btn.classList.remove("testing");
      btn.textContent = t("ai.modal.test");
    }
  });

  $("ai-toast-enable")?.addEventListener("click", () => {
    cfg.enabled = true;
    $("ai-enabled").checked = true;
    hideAIEnableToast();
    renderAITierBanner();
    saveAIConfig();
  });
  $("ai-toast-later")?.addEventListener("click", hideAIEnableToast);

  bindAIModelEditModalEvents();
}
