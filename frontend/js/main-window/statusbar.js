//! 底部提示栏：显示当前选中项的动作提示（左）+ 翻页提示（右）。
//! 无结果时整条隐藏（不占窗口高度）。
//!
//! **0.8.1 优化**：接入 ghost 状态——有 hint 时左侧显示"[Tab] 接受补全 → fanyi"，
//! 视觉与 overlay 的"影子灰字"分开：overlay 只画 ghost text，键帽 chip 走 statusbar。
//! 这也是 Raycast / Warp 的做法（active UI hint 归 statusbar，passive suggestion 归 inline overlay）。
//!
//! **文案键帽化**：所有含键位的提示（"↑↓ 选择" / "Enter 打开" / "PgUp/PgDn 翻页"）
//! 全部改用 i18n 模板 `{{key:X}}` 占位符 + `renderHint` 渲染出 <kbd> DOM。
//!
//! **0.10.8 §11.2 方案 2 — 双行 stack**：
//! Ghost + chord 同时存在时不再左右两端撕裂。左侧改为垂直 stack：
//! - `.hint-primary`：主行（ghost 提示 / 常规态导航提示）
//! - `.hint-secondary`：副行（chord 键帽，仅 hasHint + chord-visible + actions 存在时插入）
//! 右侧翻页保留不动。无 chord 场景保持单行观感（primary 占满，secondary 不插入）。
//! `#statusbar { min-height }` 稳定基线高度，避免双行切换时窗口抖动。

import {actionHint} from "./hints.js";
import {t} from "../i18n/index.js";
import * as ghost from "./ghost.js";
import * as chord from "./chord.js";
import * as autosuggestConfig from "./autosuggest-config.js";
import {renderHint, renderKey} from "../shared/kbd.js";
import {syncWindowSize} from "./window-size.js";
import {queryEl} from "./dom.js";

const el = document.getElementById("statusbar");

/** 缓存最近一次 update() 的入参，供 ghost onChange 回调时重绘用。 */
let lastActive = null;
let lastPaging = {page: 1, pageCount: 1};
/** 0.21.x: 是否剪贴板模式（影响 Enter 动作提示文案：上屏 vs 复制）。 */
let lastClipboardMode = false;

/** 初始化：订阅 ghost + chord 状态。main.js 在 ghost.init() 之后调一次。 */
export function init() {
    ghost.onChange(() => render());
    // chord-visible 变化 → 重绘 + 窗口 resize（statusbar 高度可能变化）
    chord.onVisibilityChange(() => {
        render();
        syncWindowSize();
    });
}

/**
 * 刷新提示栏。
 * 0.16.1: active.actions 数组，取 actions[0] 的 kind/hint 生成提示。
 * 0.21.x: clipboardMode 参数供 Enter 提示文案区分（上屏 / 复制）。
 * @param {{actions?: Array<{kind: string, hint?: string}>}|null} active 当前选中项
 * @param {{page: number, pageCount: number}} paging 翻页信息
 * @param {boolean} [clipboardMode] 是否处于剪贴板模式
 */
export function update(active, paging, clipboardMode) {
    lastActive = active;
    lastPaging = paging || {page: 1, pageCount: 1};
    lastClipboardMode = !!clipboardMode;
    render();
}

function render() {
    // 隐藏条件：无候选 AND 无 ghost hint。有 ghost hint 时即使无候选也要显示
    // （用户输入 `fy` 时可能还没有本地结果，仍要提示可 Tab 接受）。
    const hasHint = ghost.hasHint();
    if (!lastActive && !hasHint) {
        el.classList.remove("visible");
        el.replaceChildren();
        return;
    }

    el.replaceChildren();
    el.appendChild(buildLeft(lastActive, hasHint));
    const right = buildRight(lastPaging, hasHint);
    if (right) el.appendChild(right);
    el.classList.add("visible");
}

/** 左侧：`.hint-primary`（ghost 或常规态）+ 可选 `.hint-secondary`（chord 副行）。 */
function buildLeft(active, hasHint) {
    const left = document.createElement("div");
    left.className = "hint-left";

    const primary = document.createElement("div");
    primary.className = "hint-primary";
    fillPrimary(primary, active, hasHint);
    left.appendChild(primary);

    // 副行：输入框有文本 + chord-visible + chord 有动作 -> 追加 chord 键帽副行
    // 0.16.1：显示条件从 hasHint 改为"输入框有文本"--chord overlay 与 statusbar 副行互斥。
    const secondary = buildSecondary();
    if (secondary) left.appendChild(secondary);

    return left;
}

/** 主行内容填充：hasHint → ghost 提示；否则 → 常规态导航。 */
function fillPrimary(primary, active, hasHint) {
    if (hasHint) {
        const display = ghost.currentDisplay();
        const params = {key: renderKey(autosuggestConfig.getTabKey())};
        const source = ghost.currentSource();

        // 0.16.1：context 类走独立文案（环境感知不是"补全"，措辞要区分）。
        // keyword/ai 类仍走 autosuggest_accept / autosuggest_enter。
        if (source === "context") {
            if (display) {
                primary.appendChild(renderHint(t("statusbar.context_accept"),
                    {...params, target: display}));
            } else {
                primary.appendChild(renderHint(t("statusbar.autosuggest_enter"), params));
            }
        } else if (display) {
            primary.appendChild(renderHint(t("statusbar.autosuggest_accept"),
                {...params, target: display}));
        } else {
            primary.appendChild(renderHint(t("statusbar.autosuggest_enter"), params));
        }
        // 0.8.3 §4.9：Context Suggestion 追加 origin 提示（· 来自划词 / · 来自剪贴板）,
        // Keyword 无 origin → currentOrigin() 返回 null,不追加。
        const origin = ghost.currentOrigin();
        if (origin) {
            const originKey = `suggestion.origin.${origin}`;
            const originText = t(originKey);
            // 降级保护：t() 未命中会返回 key 本身,不显示"suggestion.origin.selection"这种字面串
            if (originText && originText !== originKey) {
                primary.appendChild(document.createTextNode(" · "));
                const originEl = document.createElement("span");
                originEl.className = "hint-origin";
                originEl.textContent = originText;
                primary.appendChild(originEl);
            }
        }
        return;
    }

    // 常规态：导航 · Alt+数字 · 动作提示——三段用 · 分隔，各段都走 renderHint 支持键帽。
    if (active) {
        primary.appendChild(renderHint(t("hint.navigate")));
        primary.appendChild(document.createTextNode(" · "));
        primary.appendChild(renderHint(t("hint.alt_number")));
        primary.appendChild(document.createTextNode(" · "));
        const {template, params} = actionHint(active.actions?.[0], lastClipboardMode);
        primary.appendChild(renderHint(template, params));
    }
}

/** 副行：输入框有文本 + chord-visible + chord 有动作 → 返回 chord 键帽行，否则 null。
 *
 *  0.16.1：chord 提示 overlay 与 statusbar 副行互斥显示。输入框无文本时 chord 走 overlay
 *  （chord.js render 条件），有文本时走 statusbar 副行（此处）。有 ghost hint 但输入框为空
 *  （context 类）时不显示副行--overlay 的 chord 提示已足够。
 */
function buildSecondary() {
    if (!queryEl.value.trim()) return null;
    const actions = chord.getActions();
    if (!document.body.classList.contains("chord-visible") || !actions.length) return null;

    const secondary = document.createElement("div");
    secondary.className = "hint-secondary";
    actions.forEach((a, i) => {
        if (i > 0) {
            const sep = document.createElement("span");
            sep.className = "chord-sep";
            sep.textContent = "│";
            secondary.appendChild(sep);
        }
        // 紧凑格式：单 kbd 显示键名（Alt 修饰键隐含，省空间）
        const kbd = document.createElement("kbd");
        kbd.className = "kbd chord-key";
        kbd.textContent = chord.chordKeyLabel(a.key);
        secondary.appendChild(kbd);
        const label = document.createElement("span");
        label.className = "chord-label";
        label.textContent = a.label;
        secondary.appendChild(label);
    });
    return secondary;
}

/** 右侧：翻页提示（多于一屏才显示）。
 *
 * 0.10.8 之前 chord 会在 `hasHint && chord-visible` 时**降级到右侧**——现在改走左侧副行，
 * 右侧只负责翻页。`hasHint` 参数保留以后有扩展空间（例如 hint 期间隐藏翻页），暂未使用。
 */
function buildRight(paging, _hasHint) {
    if (paging.pageCount <= 1) return null;
    const right = document.createElement("span");
    right.className = "hint-right";
    right.appendChild(renderHint(t("statusbar.paging"), {
        page: paging.page,
        pageCount: paging.pageCount,
    }));
    return right;
}
