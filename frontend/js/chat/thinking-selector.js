/**
 * 思考控件（0.21.17 / 0.21.23 从 main.js 下沉；0.27.15 能力化）。
 *
 * 0.27.15：按后端能力快照（status.thinking_capability）诚实渲染——
 * - `levels`：等级下拉（目录声明档位；可关时含「关」，可开·默认时含「开」）
 * - `unknown`：能力未知——保留全量下拉（默认/关/预设档/自定义）+ 提示
 * - `switch`：简单开关（开 = 默认强度/预算）；不可关模型显示「始终开启」
 * - `unsupported`：控件隐藏
 *
 * 选中档位经 set_model_reasoning_effort 持久化到 per-model 配置，
 * 空字符串 = omit（跟随服务默认），"none" = 显式关闭，"default_on" = 开·默认强度。
 *
 * 互斥约定（0.27.7 §九）：composer 弹层互斥统一走 composer-popups 仲裁——
 * 打开思考下拉前 openExclusive("thinking") 收掉模型下拉 / 命令面板等。
 * 键盘导航（0.27.15 §6.5）：↑/↓ 高亮、Enter/Tab 采纳、Esc 关闭；
 * IME 组合中不处理（flyout-keynav 统一守卫）。
 */

import * as state from "./state.js";
import * as ipc from "./ipc.js";
import {setThinkingState} from "./composer.js";
import {escapeAttr} from "./utils.js";
import {EFFORT_LEVELS} from "../shared/effort-levels.js";
import {openExclusive, registerPopup} from "./composer-popups.js";
import {clearFlyoutKeyActive, registerFlyoutKeynav} from "./flyout-keynav.js";

/** 当前选中模型 id（"{provider}:{model}"），供持久化思考强度定位 */
let currentThinkingModelId = null;
let effortRevision = 0;

/** @type {HTMLElement} 思考控件触发器按钮 */
let thinkingTrigger = null;

/** @type {HTMLElement} 思考等级下拉容器 */
let thinkingDropdown = null;

/**
 * 初始化思考控件交互。
 */
export function initThinkingSelector() {
    thinkingTrigger = document.getElementById("chat-thinking-btn");
    thinkingDropdown = document.getElementById("chat-thinking-dropdown");
    if (!thinkingTrigger || !thinkingDropdown) return;
    // 0.27.7 接入弹层互斥仲裁（§九）
    registerPopup("thinking", hideThinkingDropdown);
    // 0.27.15：键盘导航（IME 组合中 flyout-keynav 不抢键盘）
    registerFlyoutKeynav("thinking", {
        container: () => thinkingDropdown,
        isActive: () => !thinkingDropdown.hidden,
        adopt: (el) => applyThinkingEffort(el.dataset.effort ?? ""),
        close: hideThinkingDropdown,
    });

    thinkingTrigger.addEventListener("click", (e) => {
        e.stopPropagation();
        const cap = state.thinkingCapability;
        // 0.27.15：不支持思考的模型控件已隐藏（防御——正常路径 sync 已隐藏）
        if (cap?.shape === "unsupported") return;
        if (cap?.shape === "switch") {
            if (cap.canDisable === false) return; // 始终开启——开关不可用
            // 简单开关（Anthropic 预算型 / Ollama / DeepSeek 未知 / Moonshot k2.6）
            state.setThinkingEnabled(!state.thinkingEnabled);
            syncThinkingControl();
            return;
        }
        toggleThinkingDropdown();
    });

    thinkingDropdown.addEventListener("click", async (e) => {
        e.stopPropagation();
        const opt = e.target.closest(".chat-thinking-option");
        if (opt && opt.dataset.effort !== undefined && !opt.classList.contains("flyout-nav-item-disabled")) {
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
    if (id !== currentThinkingModelId) effortRevision++;
    currentThinkingModelId = id;
}

/** 思考强度线值 → 按钮标签文案（null/"" = 默认档：不主动给模型打 patch）。 */
function effortLabel(effort) {
    if (effort === null || effort === undefined || effort === "") return "思考 · 默认";
    if (effort === "none") return "思考关";
    if (effort === "default_on") return "思考 · 开";
    if (EFFORT_LEVELS.includes(effort)) return "思考 · " + effort;
    return "思考 · 自定义";
}

/** 思考是否实际开启（供 payload + thinking chunk 丢弃判断）。
 *
 * 0.27.15：「默认」（omit）不等于关闭——FollowDefault 可能产出思考块，
 * 一律按展示处理；仅显式 none 关闭时丢弃。开关型沿用 thinkingEnabled。
 */
export function effectiveThinkingEnabled() {
    if (state.supportsEffort) {
        return state.thinkingEffort !== "none";
    }
    return state.thinkingEnabled;
}

/** 同步思考控件视觉：按能力渲染形态，隐藏不支持模型的控件。 */
export function syncThinkingControl() {
    const cap = state.thinkingCapability;
    if (!thinkingTrigger) return;
    // 0.27.15：不支持思考参数的模型隐藏控件（不显示虚假开关）
    if (cap?.shape === "unsupported") {
        thinkingTrigger.classList.add("hidden");
        return;
    }
    thinkingTrigger.classList.remove("hidden");
    const dropdownShape = !cap || cap.shape === "levels" || cap.shape === "unknown";
    if (!dropdownShape) {
        // 开关型：隐藏 caret，避免"像有下拉却只是开关"的误导
        thinkingTrigger.classList.add("no-effort");
        if (cap && cap.canDisable === false) {
            // 始终开启——诚实呈现，不可交互关闭
            setThinkingState({enabled: true, label: "思考 · 始终开启"});
            return;
        }
        setThinkingState({enabled: state.thinkingEnabled, label: "深度思考"});
        return;
    }
    thinkingTrigger.classList.remove("no-effort");
    setThinkingState({
        enabled: effectiveThinkingEnabled(),
        label: effortLabel(state.thinkingEffort),
    });
}

/** 渲染思考等级下拉（0.27.15 按能力分组渲染）。 */
function renderThinkingDropdown() {
    if (!thinkingDropdown) return;
    const cap = state.thinkingCapability;
    const current = state.thinkingEffort ?? ""; // null 与 "" 都视为默认档
    let html = "";

    const option = (effort, title, sub, {selected = false} = {}) => {
        const cls =
            "chat-thinking-option flyout-nav-item" +
            (selected ? " chat-thinking-option-selected" : "");
        const subHtml = sub ? `<span class="chat-thinking-option-sub">${sub}</span>` : "";
        return `<div class="${cls}" data-effort="${escapeAttr(effort)}">
<div class="chat-thinking-option-text">
<span class="chat-thinking-option-title">${title}</span>
${subHtml}
</div></div>`;
    };

    if (!cap || cap.shape === "unknown") {
        // 能力未知：现状全量下拉（默认/关/预设档/自定义）+ 提示
        html += '<div class="chat-thinking-group">思考</div>';
        html += option("", "默认", "不主动给模型打 patch", {selected: current === ""});
        html += option("none", "思考关", null, {selected: current === "none"});
        html += '<div class="chat-thinking-group">思考级别</div>';
        for (const level of EFFORT_LEVELS) {
            html += option(level, level, null, {selected: current === level});
        }
        html += '<div class="chat-thinking-group">自定义</div>';
        const isCustom = current !== "" && current !== "none" && current !== "default_on" && !EFFORT_LEVELS.includes(current);
        html += '<div class="chat-thinking-custom-row">';
        html += `<input type="text" id="chat-thinking-custom-input" placeholder="留空 = 不发送（模型默认档）" value="${isCustom ? escapeAttr(current) : ""}">`;
        html += '<button class="chat-thinking-custom-apply" id="chat-thinking-custom-apply">应用</button>';
        html += "</div>";
        html += '<div class="chat-thinking-hint">该模型支持档位未知：若请求报错，请在此选择或输入一个支持的档位。</div>';
    } else {
        // 能力已知（levels）：目录声明档位 + 默认/关/开·默认
        html += '<div class="chat-thinking-group">思考</div>';
        html += option("", "默认", "跟随服务默认（不发送思考参数）", {selected: current === ""});
        if (cap.canDisable) {
            html += option("none", "关", null, {selected: current === "none"});
        }
        if (cap.canEnableDefault) {
            html += option("default_on", "开", "默认强度 / 默认预算", {selected: current === "default_on"});
        }
        if (cap.levels && cap.levels.length > 0) {
            html += '<div class="chat-thinking-group">思考级别</div>';
            for (const level of cap.levels) {
                const isDefault = cap.defaultEffort === level;
                html += option(level, level, isDefault ? "目录默认档" : null, {selected: current === level});
            }
        }
    }

    thinkingDropdown.innerHTML = html;
}

/** 持久化思考强度并刷新控件（空字符串 = omit 不发送）。 */
export async function applyThinkingEffort(effort) {
    if (!currentThinkingModelId) return;
    const modelId = currentThinkingModelId;
    const revision = ++effortRevision;
    try {
        const ok = await ipc.setModelReasoningEffort(modelId, effort);
        if (modelId !== currentThinkingModelId || revision !== effortRevision) return;
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
        openExclusive("thinking"); // 0.27.7：互斥仲裁收掉其余弹层（含模型下拉）
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
    clearFlyoutKeyActive(thinkingDropdown);
    if (thinkingTrigger) thinkingTrigger.classList.remove("active");
}
