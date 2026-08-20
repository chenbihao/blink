/**
 * 模型选择器（0.12.2 §4.4 / 0.21.23 从 main.js 下沉）。
 *
 * 结构：快捷档（主档/轻量档）+ 分隔线 + 启用供应商列表
 * （hover 右侧展开该供应商模型 flyout——position: fixed，越界翻转/钳制）。
 * 依赖方向：本模块单向依赖 thinking-selector（互斥收起 + 思考能力同步）；
 * thinking-selector 经 beforeOpen 回调反向收起本模块下拉，不形成 import 环。
 */

import * as state from "./state.js";
import * as ipc from "./ipc.js";
import {escapeAttr, escapeText} from "./utils.js";
import {hideThinkingDropdown, setThinkingModelId, syncThinkingControl} from "./thinking-selector.js";

/** @type {HTMLElement} 模型触发器按钮 */
let modelTrigger = null;
/** @type {HTMLElement} 下拉容器 */
let modelDropdown = null;
/** @type {HTMLElement} 供应商模型 flyout（0.21.18：hover 供应商行右侧弹出） */
let modelFlyout = null;
/** @type {HTMLElement|null} 当前展开 flyout 的供应商行 */
let flyoutOwnerRow = null;
/** @type {number|null} flyout 关闭延迟 timer（§5.3 hover 缓冲） */
let flyoutCloseTimer = null;
/** @type {Map<string, Array>|null} provider_name → 模型列表（render 时构建） */
let providerModels = null;

/**
 * 更新模型触发器标签，只显示模型名称；供应商归属在下拉内表达。
 * 禁止 innerHTML 拼接（XSS）——用 textContent 赋值。
 * @param {object} status ChatStatus（provider_name, model_name）
 */
export function updateProviderLabel(status) {
    const label = document.getElementById("chat-provider-label");
    if (!label) return;

    const modelName = status.model_name && String(status.model_name).trim();
    label.textContent = modelName || "未配置模型";
}

/** 拉取模型列表 + 状态，刷新下拉和标签 */
export async function refreshModelSelector() {
    try {
        const [models, status] = await Promise.all([
            ipc.getChatModels(),
            ipc.getChatStatus(),
        ]);
        state.setProviderConfigured(status.provider_configured);
        // 0.21.17：从 status 恢复当前生效模型的思考能力（selected 优先，Main 回落）
        state.setThinkingCapability({
            effort: status.reasoning_effort ?? null,
            supportsEffort: status.supports_effort_levels ?? false,
        });
        // 记录当前模型 id（"{provider}:{model}"），供思考强度持久化定位
        setThinkingModelId(models.find((m) => m.is_selected)?.id ?? null);
        renderModelDropdown(models);
        updateProviderLabel(status);
        syncThinkingControl();
    } catch (e) {
        console.error("[chat] 刷新模型选择器失败:", e);
    }
}

/**
 * 渲染模型选择器下拉。
 * 结构：快捷档（主档/轻量档）+ 分隔线 + 启用供应商列表（hover 右侧展开该供应商模型）。
 * @param {Array<{id, provider_name, model_name, is_main, is_light, is_selected}>} models
 */
function renderModelDropdown(models) {
    if (!modelDropdown) return;
    hideModelFlyout();
    if (!models || models.length === 0) {
        modelDropdown.innerHTML =
            '<div class="chat-model-empty">暂无可用模型，请先在设置中配置</div>';
        return;
    }

    const mainModel = models.find((m) => m.is_main);
    const lightModel = models.find((m) => m.is_light);

    // 按供应商分组（保持出现顺序），供 flyout 渲染与 hover 定位
    providerModels = new Map();
    for (const m of models) {
        const list = providerModels.get(m.provider_name) || [];
        list.push(m);
        providerModels.set(m.provider_name, list);
    }

    let html = "";
    // 快捷档区：主档 / 轻量档，显示「档位 · 模型显示名」+ provider 名作为副标题
    html += '<div class="chat-model-group">';
    if (mainModel) {
        html += renderModelOption(
            mainModel.id,
            mainModel.model_name,
            mainModel.provider_name,
            mainModel.is_selected,
            "main"
        );
    } else {
        // Main 档未配置：给出占位提示
        html += renderModelOption(null, "主档未配置", "", false, "main");
    }
    if (lightModel) {
        html += renderModelOption(
            lightModel.id,
            lightModel.model_name,
            lightModel.provider_name,
            lightModel.is_selected,
            "light"
        );
    }
    html += "</div>";

    // 供应商列表：hover 右侧弹出该供应商全部模型
    html += '<div class="chat-model-separator"></div>';
    html += '<div class="chat-model-group">';
    html += '<div class="chat-model-group-title">供应商</div>';
    for (const [provider, list] of providerModels) {
        const selected = list.some((model) => model.is_selected);
        html += `<div class="chat-model-provider-row${selected ? " is-selected" : ""}" data-provider="${escapeAttr(provider)}">`;
        html += `<span class="chat-model-provider-name">${escapeText(provider)}</span>`;
        html += `<span class="chat-model-provider-count">${list.length}</span>`;
        html += '<svg class="chat-model-provider-chevron" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="2" viewBox="0 0 24 24"><path d="m9 18 6-6-6-6"/></svg>';
        html += "</div>";
    }
    html += "</div>";

    modelDropdown.innerHTML = html;

    // hover 展开 flyout（行每次重渲染，事件绑定在新元素上）
    modelDropdown.querySelectorAll(".chat-model-provider-row").forEach((row) => {
        row.addEventListener("mouseenter", () => openModelFlyout(row));
        row.addEventListener("mouseleave", scheduleCloseModelFlyout);
    });
}

/** 在供应商行右侧弹出该供应商的模型 flyout（position: fixed，越界翻转到左侧/钳制上下）。 */
function openModelFlyout(row) {
    if (!modelFlyout || !providerModels) return;
    const provider = row.dataset.provider;
    const list = provider ? providerModels.get(provider) : null;
    if (!list || list.length === 0) return;

    cancelCloseModelFlyout();
    if (flyoutOwnerRow && flyoutOwnerRow !== row) {
        flyoutOwnerRow.classList.remove("flyout-open");
    }
    flyoutOwnerRow = row;
    row.classList.add("flyout-open");

    // 供应商名已在触发行显示，flyout 贴着该行展开，无需重复标题（0.21.18）
    let html = "";
    for (const m of list) {
        html += renderModelOption(m.id, m.model_name, "", m.is_selected, "");
    }
    modelFlyout.innerHTML = html;

    // 先显示再量尺寸（隐藏元素 offsetWidth/Height 为 0）
    modelFlyout.hidden = false;
    const rect = row.getBoundingClientRect();
    const fw = modelFlyout.offsetWidth;
    const fh = modelFlyout.offsetHeight;
    let left = rect.right + 4;
    if (left + fw > window.innerWidth - 8) left = Math.max(8, rect.left - 4 - fw);
    let top = rect.top;
    if (top + fh > window.innerHeight - 8) top = Math.max(8, window.innerHeight - 8 - fh);
    modelFlyout.style.left = left + "px";
    modelFlyout.style.top = top + "px";
}

/** 鼠标移出供应商行/ flyout 后延迟关闭（§5.3 hover 缓冲，防跨越间隙误关）。 */
function scheduleCloseModelFlyout() {
    if (flyoutCloseTimer) clearTimeout(flyoutCloseTimer);
    flyoutCloseTimer = setTimeout(() => {
        flyoutCloseTimer = null;
        hideModelFlyout();
    }, 120);
}

function cancelCloseModelFlyout() {
    if (flyoutCloseTimer) {
        clearTimeout(flyoutCloseTimer);
        flyoutCloseTimer = null;
    }
}

/** 隐藏 flyout 并清理供应商行高亮。 */
function hideModelFlyout() {
    cancelCloseModelFlyout();
    if (modelFlyout) modelFlyout.hidden = true;
    if (flyoutOwnerRow) {
        flyoutOwnerRow.classList.remove("flyout-open");
        flyoutOwnerRow = null;
    }
}

/**
 * 渲染单个下拉选项 HTML。
 * 0.12.3 重新设计：供应商名 + 模型名两行布局，左对齐，供应商用弱色小字区分。
 * @param {string|null} id
 * @param {string} label 模型显示名
 * @param {string} providerName 供应商名
 * @param {boolean} selected
 * @param {string} badge "main"/"light"/""
 */
function renderModelOption(id, label, providerName, selected, badge) {
    const badgeHtml = badge
        ? `<span class="chat-model-badge chat-model-badge-${badge}">${badge === "main" ? "主" : "轻"}</span>`
        : '<span class="chat-model-badge-placeholder"></span>';
    const providerHtml = providerName
        ? `<span class="chat-model-option-provider">${escapeText(providerName)}</span>`
        : '';
    return `<div class="chat-model-option${selected ? " chat-model-option-selected" : ""}" data-model-id="${escapeAttr(id ?? "")}" title="${escapeAttr(providerName ? providerName + ' · ' + label : label)}">
    ${badgeHtml}
    <div class="chat-model-option-text">
      <span class="chat-model-option-name">${escapeText(label)}</span>
      ${providerHtml}
    </div>
    ${selected ? '<span class="chat-model-check">✓</span>' : '<span class="chat-model-check-placeholder"></span>'}
  </div>`;
}

/** 绑定模型选择器交互（触发器 toggle + 选项点击 + 供应商 flyout + 外部关闭） */
export function bindModelSelector() {
    modelTrigger = document.getElementById("chat-model-trigger");
    modelDropdown = document.getElementById("chat-model-dropdown");
    modelFlyout = document.getElementById("chat-model-flyout");
    if (!modelTrigger || !modelDropdown) return;

    // 触发器点击 toggle 下拉（0.21.18：先收掉思考下拉，两下拉互斥不同时展开）
    modelTrigger.addEventListener("click", (e) => {
        e.stopPropagation();
        hideThinkingDropdown();
        toggleDropdown();
    });

    // 下拉项点击（事件委托，因 innerHTML 重渲染）
    // 0.12.4 §6.2：加 stopPropagation 阻止事件冒泡到 trigger，避免 hideDropdown 后被 toggle 重开
    modelDropdown.addEventListener("click", (e) => {
        e.stopPropagation();
        const opt = e.target.closest(".chat-model-option");
        if (opt) selectChatModel(opt.dataset.modelId || null);
    });

    // flyout 内模型点击（position: fixed，独立于下拉的委托）
    modelFlyout.addEventListener("click", (e) => {
        e.stopPropagation();
        const opt = e.target.closest(".chat-model-option");
        if (opt) selectChatModel(opt.dataset.modelId || null);
    });

    // hover 缓冲：flyout 悬停取消关闭（§5.3）
    modelFlyout.addEventListener("mouseenter", cancelCloseModelFlyout);
    modelFlyout.addEventListener("mouseleave", scheduleCloseModelFlyout);

    // 点击外部关闭下拉（同时关闭 flyout）
    document.addEventListener("click", (e) => {
        if (!modelDropdown.hidden && !modelTrigger.contains(e.target)) {
            hideModelDropdown();
        }
    });
}

/** 切换 chat 运行时选中模型并刷新选择器。 */
async function selectChatModel(id) {
    hideModelDropdown();
    try {
        const ok = await ipc.selectChatModel(id);
        if (ok) {
            await refreshModelSelector();
        }
    } catch (err) {
        console.error("[chat] 切换模型失败:", err);
    }
}

function toggleDropdown() {
    if (modelDropdown.hidden) {
        modelDropdown.hidden = false;
        modelTrigger.classList.add("active");
    } else {
        hideModelDropdown();
    }
}

export function hideModelDropdown() {
    if (!modelDropdown) return;
    modelDropdown.hidden = true;
    if (modelTrigger) modelTrigger.classList.remove("active");
    hideModelFlyout();
}
