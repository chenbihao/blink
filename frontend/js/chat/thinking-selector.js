/**
 * 思考控件（0.21.17 / 0.21.23 从 main.js 下沉）。
 *
 * 支持等级的模型（OpenAI 兼容且非 DeepSeek 底座）显示「思考 · 档位」下拉；
 * 其余退化为简单开关。选中档位经 set_model_reasoning_effort 持久化到
 * per-model 配置，空字符串 = omit（不发送，用模型默认档）。
 *
 * 互斥约定：两下拉（模型 / 思考）不同时展开——打开思考下拉前经
 * `beforeOpen` 回调收掉模型下拉（由 main.js 注入，避免模块间循环依赖）。
 */

import * as state from "./state.js";
import * as ipc from "./ipc.js";
import {setThinkingState} from "./composer.js";
import {escapeAttr} from "./utils.js";
import {EFFORT_LEVELS} from "../shared/effort-levels.js";

/** 当前选中模型 id（"{provider}:{model}"），供持久化思考强度定位 */
let currentThinkingModelId = null;

/** @type {HTMLElement} 思考控件触发器按钮 */
let thinkingTrigger = null;

/** @type {HTMLElement} 思考等级下拉容器 */
let thinkingDropdown = null;

/** 打开下拉前收掉另一侧下拉（main.js 注入 hideModelDropdown） */
let beforeOpen = null;

/**
 * 初始化思考控件交互。
 * @param {{beforeOpen: () => void}} opts
 */
export function initThinkingSelector(opts = {}) {
    beforeOpen = opts.beforeOpen || null;
    thinkingTrigger = document.getElementById("chat-thinking-btn");
    thinkingDropdown = document.getElementById("chat-thinking-dropdown");
    if (!thinkingTrigger || !thinkingDropdown) return;

    thinkingTrigger.addEventListener("click", (e) => {
        e.stopPropagation();
        beforeOpen?.(); // 0.21.18：先收掉模型下拉，两下拉互斥不同时展开
        if (state.supportsEffort) {
            toggleThinkingDropdown();
        } else {
            // 简单开关（DeepSeek/Anthropic/Ollama 等无等级概念）
            state.setThinkingEnabled(!state.thinkingEnabled);
            syncThinkingControl();
        }
    });

    thinkingDropdown.addEventListener("click", async (e) => {
        e.stopPropagation();
        const opt = e.target.closest(".chat-thinking-option");
        if (opt && opt.dataset.effort !== undefined) {
            hideThinkingDropdown();
            await applyThinkingEffort(opt.dataset.effort);
            return;
        }
        if (e.target.closest("#chat-thinking-custom-apply")) {
            const input = document.getElementById("chat-thinking-custom-input");
            const effort = input ? input.value.trim() : "";
            hideThinkingDropdown();
            await applyThinkingEffort(effort);
        }
    });

    // 点击外部关闭下拉
    document.addEventListener("click", (e) => {
        if (!thinkingDropdown.hidden && !thinkingTrigger.contains(e.target)) {
            hideThinkingDropdown();
        }
    });
}

/**
 * 记录当前生效模型 id（refreshModelSelector 拉到 is_selected 后调用）。
 * @param {string|null} id "{provider}:{model}"
 */
export function setThinkingModelId(id) {
    currentThinkingModelId = id;
}

/** 思考强度线值 → 按钮标签文案（null/"" = 默认档：不主动给模型打 patch）。 */
function effortLabel(effort) {
    if (effort === null || effort === undefined || effort === "") return "思考 · 默认";
    if (effort === "none") return "思考关";
    if (EFFORT_LEVELS.includes(effort)) return "思考 · " + effort;
    return "思考 · 自定义";
}

/** 思考是否实际开启（供 payload + thinking chunk 丢弃判断）。 */
export function effectiveThinkingEnabled() {
    if (state.supportsEffort) return state.thinkingEffort !== "none";
    return state.thinkingEnabled;
}

/** 同步思考控件视觉：支持等级的模型显示「思考 · 档位」，否则退化为简单开关。 */
export function syncThinkingControl() {
    // 无等级概念（简单开关）时隐藏 caret，避免"像有下拉却只是开关"的误导
    thinkingTrigger?.classList.toggle("no-effort", !state.supportsEffort);
    if (state.supportsEffort) {
        setThinkingState({
            enabled: effectiveThinkingEnabled(),
            label: effortLabel(state.thinkingEffort),
        });
    } else {
        setThinkingState({enabled: state.thinkingEnabled, label: "深度思考"});
    }
}

/** 渲染思考等级下拉（默认档 + 关闭 + 预设档位 + 自定义输入 + 提示）。 */
function renderThinkingDropdown() {
    if (!thinkingDropdown) return;
    const current = state.thinkingEffort ?? ""; // null 与 "" 都视为默认档
    const isCustom = current !== "" && current !== "none" && !EFFORT_LEVELS.includes(current);
    let html = "";

    // 默认档（不主动打 patch）+ 关闭——均为中性态，选中时不点亮（仅真实档位/自定义高亮）
    html += '<div class="chat-thinking-group">思考</div>';
    html += '<div class="chat-thinking-option" data-effort="">';
    html += '<div class="chat-thinking-option-text">';
    html += '<span class="chat-thinking-option-title">默认</span>';
    html += '<span class="chat-thinking-option-sub">不主动给模型打 patch</span>';
    html += "</div></div>";
    html += '<div class="chat-thinking-option" data-effort="none">思考关</div>';

    // 预设档位（原文本展示，避免中文翻译对不上供应商档位）
    html += '<div class="chat-thinking-group">思考级别</div>';
    for (const level of EFFORT_LEVELS) {
        const selected = current === level;
        html += `<div class="chat-thinking-option${selected ? " chat-thinking-option-selected" : ""}" data-effort="${level}">${level}</div>`;
    }

    // 自定义（当前为自定义值时预填）
    html += '<div class="chat-thinking-group">自定义</div>';
    html += '<div class="chat-thinking-custom-row">';
    html += `<input type="text" id="chat-thinking-custom-input" placeholder="留空 = 不发送（模型默认档）" value="${isCustom ? escapeAttr(current) : ""}">`;
    html += '<button class="chat-thinking-custom-apply" id="chat-thinking-custom-apply">应用</button>';
    html += "</div>";

    html += '<div class="chat-thinking-hint">该模型支持档位未知：若请求报错，请在此选择或输入一个支持的档位。</div>';

    thinkingDropdown.innerHTML = html;
}

/** 持久化思考强度并刷新控件（空字符串 = omit 不发送）。 */
async function applyThinkingEffort(effort) {
    if (!currentThinkingModelId) return;
    try {
        const ok = await ipc.setModelReasoningEffort(currentThinkingModelId, effort);
        if (ok) {
            state.setThinkingEffort(effort);
            syncThinkingControl();
        } else {
            console.error("[chat] 设置思考强度失败：模型不存在或已禁用");
        }
    } catch (err) {
        console.error("[chat] 设置思考强度失败:", err);
    }
}

function toggleThinkingDropdown() {
    if (thinkingDropdown.hidden) {
        renderThinkingDropdown();
        thinkingDropdown.hidden = false;
        thinkingTrigger.classList.add("active");
    } else {
        hideThinkingDropdown();
    }
}

export function hideThinkingDropdown() {
    if (!thinkingDropdown) return;
    thinkingDropdown.hidden = true;
    if (thinkingTrigger) thinkingTrigger.classList.remove("active");
}
