import {renderKey} from "../shared/kbd.js";

/** 补全文字和采纳键同处输入框 overlay；完整触发词也保留轻量采纳提示。 */
export function renderCompletionGhost(query, suggestion, typedEl, suggestEl, tabKey) {
    typedEl.textContent = suggestion ? query : "";
    suggestEl.replaceChildren();
    if (!suggestion) return false;
    if (suggestion.display) {
        const text = document.createElement("span");
        text.textContent = ` → ${suggestion.display}`;
        suggestEl.appendChild(text);
    }
    suggestEl.appendChild(renderKey(tabKey, {quiet: true}));
    return true;
}
