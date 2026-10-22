/**
 * 编辑器预设菜单（0.27.6 §3.9 / §5.6.3）
 *
 * 数据源：提示词库 scope=editor 且未隐藏条目（`prompt_library_list`），
 * 缓存 + `blink://prompt-library-changed` 失效（设置页改预设即热生效）。
 * 互译内置（builtin.editor.translate）展开为三项：自动方向 / 译为中文 /
 * 译为英文（§3.9 混合或不明确时允许显式选择目标）。
 *
 * 无 bundler 铁则：纯 ES module；菜单为动态 DOM，样式走 content-editor.css。
 */

import {invoke, listen} from "../shared/tauri.js";
import {EVENTS} from "../shared/event-names.js";
import {t} from "../i18n/index.js";

/** 编辑器预设缓存（null = 未加载；[] = 已加载但为空）。 */
let presetCache = null;

/** 变更事件到达即失效（下次 openPresetMenu 重新拉取）。 */
listen(EVENTS.PROMPT_LIBRARY_CHANGED, () => {
    presetCache = null;
});

/**
 * 拉取有效编辑器预设（未隐藏、scope=editor）。
 * @returns {Promise<Array<{id: string, name: string, builtin: boolean}>>}
 */
export async function fetchEditorPresets() {
    if (presetCache) return presetCache;
    try {
        const rows = await invoke("prompt_library_list", {scope: "editor"});
        presetCache = (Array.isArray(rows) ? rows : [])
            .filter((row) => !row.hidden)
            .map((row) => ({id: row.id, name: row.name, builtin: !!row.builtin}));
    } catch (e) {
        console.error("prompt_library_list(editor) failed:", e);
        presetCache = [];
    }
    return presetCache;
}

/** 互译内置稳定 ID（与后端 builtin.editor.translate 对齐）。 */
export const TRANSLATE_PRESET_ID = "builtin.editor.translate";

/**
 * 预设 → 菜单项投影（纯函数）：
 * - 互译内置展开三项（自动 / 译中 / 译英）；
 * - 其余预设一项（preset 传 id，translateTarget 为 null）。
 * 库为空时回落单条「整理」（默认预设兜底，与后端 None 语义一致）。
 *
 * @returns {Array<{preset: string, label: string, translateTarget: "zh"|"en"|null}>}
 */
export function presetMenuItems(presets) {
    if (!presets || presets.length === 0) {
        return [{preset: "builtin.editor.tidy", label: t("editor.preset.tidy"), translateTarget: null}];
    }
    const items = [];
    for (const preset of presets) {
        if (preset.id === TRANSLATE_PRESET_ID) {
            items.push({preset: preset.id, label: t("editor.preset.translate.auto"), translateTarget: null});
            items.push({preset: preset.id, label: t("editor.preset.translate.toZh"), translateTarget: "zh"});
            items.push({preset: preset.id, label: t("editor.preset.translate.toEn"), translateTarget: "en"});
        } else {
            items.push({preset: preset.id, label: preset.name, translateTarget: null});
        }
    }
    return items;
}

/**
 * 打开预设菜单（anchor 元素下方弹出；点击项 → onPick；Esc/点击外部关闭）。
 * @param {HTMLElement} anchorEl - 触发 chip（定位锚点）
 * @param {(item: {preset: string, label: string, translateTarget: string|null}) => void} onPick
 */
export async function openPresetMenu(anchorEl, onPick) {
    closePresetMenu();
    const presets = await fetchEditorPresets();
    const items = presetMenuItems(presets);

    const menu = document.createElement("div");
    menu.className = "editor-preset-menu";
    menu.setAttribute("role", "menu");
    for (const item of items) {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "editor-preset-menu-item";
        button.setAttribute("role", "menuitem");
        button.textContent = item.label;
        button.addEventListener("click", () => {
            closePresetMenu();
            onPick(item);
        });
        menu.appendChild(button);
    }
    document.body.appendChild(menu);

    // 定位：chip 下方左对齐；越界钳制到视口内（§5.6 视觉尺寸钳制）
    const rect = anchorEl.getBoundingClientRect();
    menu.style.left = `${Math.max(8, Math.min(rect.left, window.innerWidth - menu.offsetWidth - 8))}px`;
    menu.style.top = `${Math.min(rect.bottom + 6, window.innerHeight - menu.offsetHeight - 8)}px`;

    // 关闭：外部按下 / Esc（0.27.7 前的独立轻量实现；composer 面板协调属 Chat 侧）
    setTimeout(() => {
        document.addEventListener("pointerdown", onOutside, {capture: true});
        document.addEventListener("keydown", onEsc);
    }, 0);

    function onOutside(event) {
        if (!menu.contains(event.target)) closePresetMenu();
    }
    function onEsc(event) {
        if (event.key === "Escape") closePresetMenu();
    }
    function cleanup() {
        document.removeEventListener("pointerdown", onOutside, {capture: true});
        document.removeEventListener("keydown", onEsc);
    }
    currentMenu = {menu, cleanup};
}

/** 当前打开的菜单（单例；新开先关旧的）。 */
let currentMenu = null;

/** 关闭菜单（幂等）。 */
export function closePresetMenu() {
    if (!currentMenu) return;
    currentMenu.cleanup();
    currentMenu.menu.remove();
    currentMenu = null;
}

/**
 * 候选卡/按钮的动作标注（纯函数）：预设名 + 互译方向。
 * @param {string|null} presetName - 完成事件 presetName
 * @param {"zh"|"en"|null} translateTarget - 完成事件 translateTarget
 * @returns {string|null} 无预设信息时 null（回落旧 scope 标题）
 */
export function composeActionLabel(presetName, translateTarget) {
    if (!presetName) return null;
    if (translateTarget === "zh") {
        return `${presetName} · ${t("editor.preset.translate.toZh")}`;
    }
    if (translateTarget === "en") {
        return `${presetName} · ${t("editor.preset.translate.toEn")}`;
    }
    return presetName;
}
