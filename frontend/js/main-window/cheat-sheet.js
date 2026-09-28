//! Chord Cheat Sheet（0.24 §5.5）：按住 Alt 时的完整键帽表。
//!
//! Chord 提示不再在输入框 overlay（旧 .ghost-chord）与 statusbar 副行
//! （旧 .hint-secondary）之间迁移——完整键帽表只在 Alt 按住（body.chord-visible）
//! 时以独立覆盖层出现；空 query 闲置态的一行精简提示归 ResultFooter（statusbar.js）。
//!
//! 覆盖层绝对定位在窗口底部、不占布局——Alt 按下/抬起不触发窗口 resize
//! （§6.2"连续输入过程中窗口高度不逐键变化"）。可用动作集与旧两处一致：
//! chord.getActions()（query 非空时只含 available_with_query 动作）。

import * as chord from "./chord.js";
import {chordKeyLabel} from "./chord.js";
import {renderCombo} from "../shared/kbd.js";
import {queryEl} from "./dom.js";

let el = null;

/** 初始化：绑定 DOM + 订阅可见性/输入变化。main.js 启动时调一次。 */
export function init() {
    el = document.getElementById("chord-cheat-sheet");
    // Alt 按下/抬起（input-state 投影 chord-visible）→ 重投影
    chord.onVisibilityChange(render);
    // query 变化改变可用动作集（available_with_query 过滤）→ 重投影
    queryEl.addEventListener("input", render);
    render();
}

function render() {
    if (!el) return;
    el.replaceChildren();

    // 可见性三重与门（与旧 .ghost-chord 显示条件一致）：
    // 1. body.chord-visible（Alt 按下 && chord 启用 && 非独占模式）
    // 2. hint_visible 配置（关闭后用户仍可触发，只是不显示提示）
    // 3. 有可显示动作
    // voice-active / clipboard-mode-active 由 CSS 隐藏（独占交互态不叠 Cheat Sheet）。
    if (!document.body.classList.contains("chord-visible")) {
        el.classList.add("hidden");
        return;
    }
    const actions = chord.getActions();
    if (!actions.length) {
        el.classList.add("hidden");
        return;
    }

    const list = document.createElement("div");
    list.className = "cheat-sheet-list";
    actions.forEach((a) => {
        const row = document.createElement("div");
        row.className = "cheat-sheet-row";
        const combo = document.createElement("span");
        combo.className = "cheat-sheet-keys";
        // 完整组合键形式（Alt 修饰键显式写出）——Cheat Sheet 是参考卡，
        // 与旧"单键帽省略 Alt"的紧凑提示不同
        combo.appendChild(renderCombo(`Alt+${chordKeyLabel(a.key)}`));
        row.appendChild(combo);
        const label = document.createElement("span");
        label.className = "cheat-sheet-label";
        label.textContent = a.label;
        row.appendChild(label);
        list.appendChild(row);
    });
    el.appendChild(list);
    el.classList.remove("hidden");
}
