/**
 * 云端 STT 供应商预设（0.29.1 从 voice.js 迁出 + 整理）。
 *
 * 预设 = kind（协议）+ base_url + 默认 model_id 的打包；协议由预设自动决定，
 * 「协议类型」下拉仅在自定义预设时露出（不再与预设并列成两个维度）。
 *
 * 与 Rust 侧单一真源对齐铁则：
 * - `KIND_DEFAULT_BASE_URL` 与 `crates/blink-domain-stt/src/cloud.rs::default_base_url`
 *   保持一致（改一处必须同步另一处）；
 * - kind 的合法值域与 `SttCloudProvider.kind` 一致（openai/groq/mimo/mimo_plan/custom）。
 */

/**
 * 供应商预设。custom 为 null——kind/base_url/model_id 全部由用户手填。
 * @readonly
 */
export const CLOUD_STT_PRESETS = {
    openai: {kind: "openai", base_url: "https://api.openai.com/v1", model_id: "whisper-1"},
    groq: {kind: "groq", base_url: "https://api.groq.com/openai/v1", model_id: "whisper-large-v3"},
    mimo: {kind: "mimo", base_url: "https://api.xiaomimimo.com/v1", model_id: ""},
    mimo_plan: {
        kind: "mimo_plan",
        base_url: "https://token-plan-cn.xiaomimimo.com/v1",
        model_id: "",
    },
    custom: null,
};

/**
 * kind（协议）→ 供应商默认 base_url。与 Rust `cloud::default_base_url` 同步维护。
 * `custom` 无默认（等价 openai 协议，URL 完全由用户指定）。
 * @readonly
 */
export const KIND_DEFAULT_BASE_URL = {
    openai: "https://api.openai.com/v1",
    groq: "https://api.groq.com/openai/v1",
    mimo: "https://api.xiaomimimo.com/v1",
    mimo_plan: "https://token-plan-cn.xiaomimimo.com/v1",
    custom: "",
};

const stripTrailingSlash = (url) => (url || "").trim().replace(/\/+$/, "");

/**
 * 当前 cloud_provider 命中的预设 id；未命中返回 "custom"。
 *
 * 匹配规则：kind 一致，且 **effective base_url**（显式 base_url，缺失时回落
 * kind 默认值）与预设 URL 相同——修复旧实现"base_url=null（用默认值）时
 * 被误判为自定义"的回显 bug。
 *
 * @param {{kind?: string, base_url?: string|null}|null} cloudProvider
 * @returns {string} 预设 id（"custom" 表示自定义）
 */
export function matchCloudPreset(cloudProvider) {
    if (!cloudProvider) return "custom";
    const kind = cloudProvider.kind || "openai";
    const effective = stripTrailingSlash(
        cloudProvider.base_url || KIND_DEFAULT_BASE_URL[kind] || "",
    );
    for (const [id, preset] of Object.entries(CLOUD_STT_PRESETS)) {
        if (!preset) continue;
        if (preset.kind !== kind) continue;
        if (stripTrailingSlash(preset.base_url) === effective) return id;
    }
    return "custom";
}
