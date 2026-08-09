//! AI Tab 共享状态（0.14.6 §4.2 拆分）。
//!
//! ai.js 拆分为多个子模块后，所有共享状态统一存在此对象中。
//! 各子模块通过 `import { aiState } from './state.js'` 访问和修改状态。

import { saveConfig } from "../../../shared/config-keys.js";
import { invoke } from "../../../shared/tauri.js";

/** AI 提供商类型标签 */
export const AI_KIND_LABEL = {
  openai_compatible: "OpenAI Compatible",
  anthropic_messages: "Anthropic",
  gemini_generate_content: "Gemini",
};

/** 当前 AI 配置（loadAIConfig 拉取后持有，各函数读写） */
export const aiState = {
  currentAIConfig: null,
  /** 密钥存在性映射 provider_id → boolean（不发明文回前端） */
  hasSecretMap: new Map(),
  /** 密钥掩码提示映射 provider_id → string|null（如 "sk-a••••cdef"），仅 hasSecretMap 为 true 时有值 */
  secretHintMap: new Map(),

  // ── 模型编辑 modal 草稿变量 ──────────────────────────────
  _modelEditProviderId: null,
  _modelEditOriginalId: null,
  _modelEditDraft: null,
  _modelSavedToastTimer: null,
  /** 拉取模型 popover 缓存 { models, error, loading } 或 null */
  _modelFetchCache: null,

  // ── 供应商 modal 内模型多选状态 ──────────────────────────
  _providerModelCache: null,       // { models, error, loading }
  _providerSelectedModels: [],     // string[]
  _editOriginalModelIds: [],       // 编辑模式：打开时已有的 model id

  // ── Skill 导入面板状态 ──────────────────────────────────
  _skillImportSourcesCache: null,
  _customImportDir: null,

  // ── 跨模块回调（编排层注册，避免循环依赖）──────────────────
  _renderAIProviders: null,             // 重渲染供应商列表
  _getExpandedProviderIds: null,        // 获取当前展开的供应商 ID
  _restoreExpandedProviderIds: null,    // 恢复展开状态
};

/** 默认 AI 配置 */
export function defaultAIConfig() {
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
    slo_hard_timeout_ms: 20_000,
    chat_config: { pure_chat: false, auto_title: false, title_tier: "light", memory_config: { mode: "token_aware", window_size: 20, trigger_ratio: 0.8, compress_ratio: 0.7, recall_enabled: true, recall_top_k: 3 }, skill_config: { enabled: true, source_blink: true, source_claude: true, source_zcode: false } },
  };
}

/** 保存 AI 配置到后端 */
export async function saveAIConfig() {
  try {
    await saveConfig("ai_config", aiState.currentAIConfig);
  } catch (e) {
    console.error("save ai_config failed:", e);
    throw e;
  }
}

// ── HTML 转义 helpers ──────────────────────────────────────

/** HTML 转义 */
export function escapeHtml(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

/** 属性转义 */
export function escapeAttr(s) {
  return escapeHtml(s);
}

/** 从后端拉取供应商可用模型列表（供应商 modal 和模型编辑 popover 共用） */
export async function fetchAvailableModelsFor(kind, baseUrl, providerId) {
  const apiKey = document.getElementById("ai-modal-api-key")?.value?.trim() || null;
  const models = await invoke("fetch_ai_models", {
    kind,
    baseUrl: baseUrl || null,
    apiKey: apiKey || null,
    providerId: providerId || null,
  });
  return models || [];
}
