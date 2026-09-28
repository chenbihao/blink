//! SuggestionBar（0.24 §3.2 / §5.5）：主窗口双槽建议条。
//!
//! **只渲染非 Completion 槽位**——Completion 是输入延伸走 ghost 影子，同一建议
//! 不在两个视觉区重复出现；secondary 为 Completion 时属罕见排序结果（翻译 0.92 >
//! 低分补全），ghost 只画 primary 影子无法承载，为避免"不可见却可 Shift+Tab 采纳"
//! 的悬空建议，该槽在 bar 内按补全语义渲染（见 §3.2"键随槽走"）。
//!
//! **键随槽走不随视觉区走**（§3.2）：Tab 恒采纳 primary、Shift+Tab 恒采纳
//! secondary；primary 为 Completion 时 Tab 作用于 ghost（keyboard.js 分派），
//! bar 行点击与键盘共用 suggestion-accept.js 的 acceptance。
//!
//! **粘性槽位**（§5.5）：建议首次出现时长高，query 非空期间槽位保持（min-height
//! 预占，连续输入不逐键抖动）；仅在空态↔非空态/ESC（reset）转换时释放并 resync。
//!
//! 数据流：search.js 三处消费点调 `update(query, set)`；set 为 SuggestionSet wire
//! 形状 `{revision, primary?, secondary?}`（camelCase，secondary 可缺省）。

import {t} from "../i18n/index.js";
import {renderCombo, renderKey} from "../shared/kbd.js";
import {iconHTML} from "../shared/icon.js";
import * as autosuggestConfig from "./autosuggest-config.js";
import {acceptSuggestion} from "./suggestion-accept.js";
import {syncWindowSize, resyncWindowSize} from "./window-size.js";

let barEl = null;

// 当前 SuggestionSet 投影：槽位对象 + revision（0.24 §3.6 采纳时校验）。
let primary = null; // 非 Completion 的 primary 槽（Completion 走 ghost，不进 bar）
let secondary = null;
let revision = null;

// 粘性槽位状态（§5.5）：query 非空期间已出现过建议 → true，槽位高度保持。
let sticky = false;

/** 初始化：绑定 bar DOM。main.js 启动时调一次。 */
export function init() {
    barEl = document.getElementById("suggestion-bar");
}

/**
 * 某槽位是否有可渲染/可采纳建议。
 * keyboard.js 的 Shift+Tab 分派与点击采纳都读这里（含 secondary Completion）。
 */
export function hasSlot(slot) {
    return slot === "primary" ? primary !== null : secondary !== null;
}

/**
 * 采纳指定槽位（keyboard.js Shift+Tab / 行点击共用；Tab 的 primary 分派
 * 在 keyboard.js 优先走 ghost 的 Completion，无 Completion 影子时落到这里）。
 * @param {"primary"|"secondary"} slot
 * @returns {boolean} true = 已采纳（keyboard 层据此 preventDefault）
 */
export function accept(slot) {
    const sug = slot === "primary" ? primary : secondary;
    return acceptSuggestion(sug, slot, revision, {
        onAccepted: clearRows,
        onStale: clearRows,
    });
}

/**
 * 用新一轮搜索的 SuggestionSet 刷新 bar。set 为 null/无槽位时清空渲染
 * （query 非空期间粘性槽位仍保持高度）。
 * @param {string} query 本轮 query（空态转换时释放粘性槽位）
 * @param {{revision?: number, primary?: object, secondary?: object}|null} set
 */
export function update(query, set) {
    const queryEmpty = !query || !query.trim();
    if (queryEmpty && sticky) {
        // 空态转换（退格清空 / fetchContextSuggestions 空 query）：释放粘性槽位
        // 并 resync——高度变化只发生在空态↔建议态转换（§6.2 验收）。
        sticky = false;
        primary = null;
        secondary = null;
        revision = null;
        renderRows();
        resyncWindowSize();
        return;
    }

    primary = set?.primary && set.primary.kind !== "completion" ? set.primary : null;
    // secondary 是 Completion 时仍渲染（见文件头注释：避免悬空可采纳槽）
    secondary = set?.secondary ?? null;
    revision = set?.revision ?? null;

    if (!queryEmpty && (primary || secondary)) {
        sticky = true; // 首次出现即 arm；之后逐键更新不再改变槽位高度
    }
    renderRows();
    syncWindowSize();
}

/** 清空建议（reset / 窗口隐藏 / 独占模式进入时调）。释放粘性槽位。 */
export function clear() {
    if (!primary && !secondary && !sticky) return;
    primary = null;
    secondary = null;
    revision = null;
    sticky = false;
    renderRows();
}

// ── 渲染 ─────────────────────────────────────────────────────────────────────

/** 清空行 DOM 并卸载粘性槽位高度（保持元素本身，避免 #results 布局跳动）。 */
function clearRows() {
    if (!barEl) return;
    barEl.replaceChildren();
    barEl.classList.remove("slot-armed");
}

function renderRows() {
    if (!barEl) return;
    barEl.replaceChildren();
    // 粘性槽位：query 非空期间保持一行高度，建议消失/重现不逐键抖动（§5.5）
    barEl.classList.toggle("slot-armed", sticky);

    if (primary) barEl.appendChild(buildRow(primary, "primary"));
    if (secondary) barEl.appendChild(buildRow(secondary, "secondary"));
    if (primary || secondary) barEl.classList.add("visible");
    else barEl.classList.remove("visible");
}

/**
 * 构建单个建议行：图标 + 文案（+ 来源）+ 采纳键帽。
 * @param {object} sug Suggestion wire 对象
 * @param {"primary"|"secondary"} slot
 */
function buildRow(sug, slot) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = `suggestion-row slot-${slot}`;
    // 防止点击把焦点从输入框移到 button（主链路铁则：焦点留在 #query，
    // 采纳后继续输入）。click 仍正常触发。
    row.addEventListener("mousedown", (e) => e.preventDefault());
    row.addEventListener("click", () => accept(slot));

    const icon = document.createElement("span");
    icon.className = "suggestion-icon";
    icon.innerHTML = iconHTML(iconFor(sug));
    row.appendChild(icon);

    const text = document.createElement("span");
    text.className = "suggestion-text";
    text.appendChild(document.createTextNode(rowText(sug)));
    if (sug.origin) {
        const originKey = `suggestion.origin.${sug.origin}`;
        const originText = t(originKey);
        // 降级保护：t() 未命中返回 key 本身时不显示字面串
        if (originText && originText !== originKey) {
            const origin = document.createElement("span");
            origin.className = "suggestion-origin";
            origin.textContent = originText;
            text.appendChild(document.createTextNode(" · "));
            text.appendChild(origin);
        }
    }
    row.appendChild(text);

    // 采纳键帽：primary 读 autosuggest_tab_key 配置（ArrowRight 遗留别名保留），
    // secondary 恒 Shift+Tab（§5.6）。单键走 renderKey、组合键走 renderCombo（kbd 铁则）
    const keyEl = slot === "primary"
        ? renderKey(autosuggestConfig.getTabKey())
        : renderCombo("Shift+Tab");
    keyEl.classList.add("suggestion-key");
    row.appendChild(keyEl);
    return row;
}

/**
 * 行文案：display 之外按 kind 覆盖——AskAi 的后端 display 硬编码"按 Tab 问 AI"，
 * ArrowRight 配置用户会看到错误键名；bar 自带键帽 chip，文案不应重复键名。
 */
function rowText(sug) {
    if (sug.kind === "askAi") return t("suggestionbar.ask_ai");
    if (sug.kind === "completion") return t("suggestionbar.completion_to", {target: sug.display});
    return sug.display || "";
}

/** kind → Lucide 图标（图标用包禁 emoji，spec-frontend §4.2；只可用 sprite 内已有图标）。 */
function iconFor(sug) {
    if (sug.kind === "translate") return "languages";
    if (sug.kind === "askAi") return "sparkles";
    return "zap";
}
