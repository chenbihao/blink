//! AI Tab 共享状态（0.14.6 §4.2 拆分）。
//!
//! ai.js 拆分为多个子模块后，所有共享状态统一存在此对象中。
//! 各子模块通过 `import { aiState } from './state.js'` 访问和修改状态。

import {saveConfig} from "../../../shared/config-keys.js";
import {invoke} from "../../../shared/tauri.js";

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
        min_query_len: 4,
        providers: [],
        tier_ultra_light: null,
        tier_light: null,
        tier_main: null,
        slo_hard_timeout_ms: 20_000,
        chat_config: {
            agent_mode: "full",
            main_window_model: "light",
            title_model: "ultra_light",
            auto_title: false,
            memory_config: {
                mode: "token_aware",
                window_size: 20,
                trigger_ratio: 0.8,
                compress_ratio: 0.7,
                recall_enabled: true,
                recall_top_k: 3,
                summary_enabled: false,
                recall_scope: "this_conversation",
            },
            skill_config: {enabled: true, source_blink: true, source_claude: true, source_zcode: false}
        },
    };
}

// ── 0.28.28：陈旧整份保存防护（字段补丁合并）─────────────────────────────
//
// 同步/WebDAV/其他窗口可能在本页两次保存之间写入 app.ai。整份上传旧对象会
// 覆盖刚同步的供应商。保存前：diff 本地对象与最近一次加载快照 → 只把本页
// 实际改过的顶层字段合并到后端最新值上（字段补丁），未触碰的字段取后端权威值；
// 保存成功后本页状态收敛为已保存载荷（后续编辑以此为基线 diff）。
// 保存经 promise 链串行化，避免并发保存互相读到中间态。
let lastLoadedRaw = null;
let saveChain = Promise.resolve();

/** 记录最近一次加载的配置快照（loadAIConfig 成功后调）。 */
export function snapshotLoadedAIConfig(cfg) {
    lastLoadedRaw = cfg;
}

function topLevelChangedKeys(local, baseline) {
    const changed = new Set();
    if (!baseline) {
        // 无基线（首载失败等）：退化为整份保存（与旧行为一致）
        return null;
    }
    const keys = new Set([...Object.keys(local || {}), ...Object.keys(baseline)]);
    for (const key of keys) {
        if (JSON.stringify(local?.[key]) !== JSON.stringify(baseline[key])) {
            changed.add(key);
        }
    }
    return changed;
}

/**
 * 保存 AI 配置到后端。
 * 0.28.28：先取后端最新值，把本页有改动的顶层字段合并上去再保存——
 * 本页未触碰的字段（如刚同步进来的供应商）不会被旧对象覆盖；
 * 本地无任何改动时保存最新值（幂等）。
 * 读取失败保留现有内容并抛错（不降级为默认空配置继续保存）。
 */
export async function saveAIConfig() {
    const run = saveChain.then(doSaveAIConfig);
    // 队列不因单次失败断裂
    saveChain = run.catch(() => {});
    return run;
}

async function doSaveAIConfig() {
    const local = aiState.currentAIConfig;
    if (!local || typeof local !== "object") {
        throw new Error("AI 配置未加载，已取消保存（避免空配置覆盖）");
    }
    let payload = local;
    try {
        const latest = await invoke("get_config_section", {key: "app.ai"});
        if (latest && typeof latest === "object") {
            const changed = topLevelChangedKeys(local, lastLoadedRaw);
            if (changed === null) {
                payload = {...latest, ...local};
            } else if (changed.size > 0) {
                payload = JSON.parse(JSON.stringify(latest));
                for (const key of changed) {
                    payload[key] = local[key];
                }
            } else {
                payload = latest;  // 本页无改动：保存最新值（幂等，不覆盖）
            }
        }
    } catch (e) {
        // 后端预读失败：保留本地内容继续保存（与旧行为一致），记录可诊断日志
        console.warn("saveAIConfig: 预读最新配置失败，按本地对象保存", e);
    }
    await saveConfig("ai_config", payload);
    // 收敛：本页状态与基线 = 已保存载荷（后续编辑以此为 diff 基线）
    aiState.currentAIConfig = payload;
    snapshotLoadedAIConfig(payload);
}

// ── HTML 转义 helpers ──────────────────────────────────────

/** HTML 转义 */
export function escapeHtml(s) {
    return String(s ?? "").replace(/[&<>"']/g, (c) => ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;"
    }[c]));
}

/** 属性转义 */
export function escapeAttr(s) {
    return escapeHtml(s);
}

/** 从后端拉取供应商可用模型列表（供应商 modal 和模型编辑 popover 共用）
 * 0.21.21: 返回 ModelMeta 富结构（含 context_window）
 * @returns {Promise<Array<{id: string, context_window?: number}>|null>}
 */
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
