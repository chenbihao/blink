/**
 * 0.17.3 首次启动引导窗口
 *
 * 展示主快捷键 + Chord 快捷键两组 + "开始使用"按钮。
 * 点击"开始使用"或关闭窗口 -> first_run=false，后续启动不再弹引导。
 *
 * 优化：将 Alt+Space（全局主快捷键）与 Alt+Q/A/C/E/S（Chord 快捷键）
 * 分开展示，避免用户误解 Chord 快捷键为全局快捷键。
 */

import {getCurrentWindow, invoke} from "./shared/tauri.js";
import {applyI18nFromConfig, onLangChange, t} from "./i18n/index.js";
import {renderCombo} from "./shared/kbd.js";

// ── 快捷键数据 ────────────────────────────────────────────────────────────────

// 主快捷键：全局热键，任何地方按下即可触发
const MAIN_SHORTCUT = {
    combo: "Alt+Space",
    labelKey: "welcome.shortcut.voice_input",
    hintKey: "welcome.main.hint",
};

// Chord 快捷键：仅在主窗口可见时按住 Alt + 字母键触发
const CHORD_SHORTCUTS = [
    {combo: "Alt+Q", labelKey: "welcome.shortcut.chat"},
    {combo: "Alt+A", labelKey: "welcome.shortcut.screenshot"},
    {combo: "Alt+C", labelKey: "welcome.shortcut.clipboard_history"},
    {combo: "Alt+E", labelKey: "welcome.shortcut.edit"},
    {combo: "Alt+S", labelKey: "welcome.shortcut.sticky"},
];

// ── 渲染 ────────────────────────────────────────────────────────────────────

function renderShortcuts() {
    const container = document.getElementById("shortcut-list");
    if (!container) return;

    container.innerHTML = "";

    // ── 主快捷键 ──
    const mainSection = document.createElement("div");
    mainSection.className = "welcome-section welcome-section--main";

    const mainLabel = document.createElement("span");
    mainLabel.className = "welcome-shortcut-label";
    mainLabel.textContent = t(MAIN_SHORTCUT.labelKey);

    const mainKeys = document.createElement("span");
    mainKeys.className = "welcome-shortcut-keys";
    mainKeys.appendChild(renderCombo(MAIN_SHORTCUT.combo));

    const mainRow = document.createElement("div");
    mainRow.className = "welcome-shortcut-row welcome-shortcut-row--main";
    mainRow.appendChild(mainLabel);
    mainRow.appendChild(mainKeys);
    mainSection.appendChild(mainRow);

    // 主快捷键说明
    const mainHint = document.createElement("p");
    mainHint.className = "welcome-section-hint";
    mainHint.textContent = t(MAIN_SHORTCUT.hintKey);
    mainSection.appendChild(mainHint);

    container.appendChild(mainSection);

    // ── Chord 快捷键 ──
    const chordSection = document.createElement("div");
    chordSection.className = "welcome-section welcome-section--chord";

    // Chord 分组标题
    const chordTitle = document.createElement("div");
    chordTitle.className = "welcome-section-title";
    chordTitle.textContent = t("welcome.chord.title");
    chordSection.appendChild(chordTitle);

    // Chord 说明
    const chordDesc = document.createElement("p");
    chordDesc.className = "welcome-section-desc";
    chordDesc.textContent = t("welcome.chord.desc");
    chordSection.appendChild(chordDesc);

    // Chord 快捷键列表
    const chordList = document.createElement("div");
    chordList.className = "welcome-chord-list";
    for (const {combo, labelKey} of CHORD_SHORTCUTS) {
        const row = document.createElement("div");
        row.className = "welcome-shortcut-row";

        const label = document.createElement("span");
        label.className = "welcome-shortcut-label";
        label.textContent = t(labelKey);

        const keys = document.createElement("span");
        keys.className = "welcome-shortcut-keys";
        keys.appendChild(renderCombo(combo));

        row.appendChild(label);
        row.appendChild(keys);
        chordList.appendChild(row);
    }
    chordSection.appendChild(chordList);

    container.appendChild(chordSection);
}

// ── 初始化 ────────────────────────────────────────────────────────────────────

async function init() {
    // 加载语言配置 + 应用 i18n
    await applyI18nFromConfig();

    // 渲染快捷键列表
    renderShortcuts();

    // 语言切换时刷新快捷键标签
    onLangChange(() => renderShortcuts());

    // "开始使用"按钮
    const startBtn = document.getElementById("start-btn");
    if (startBtn) {
        startBtn.addEventListener("click", async () => {
            try {
                await invoke("set_config", {key: "first_run", value: false});
            } catch (e) {
                console.error("welcome: set_config first_run failed:", e);
            }
            getCurrentWindow()?.close();
        });
    }
}

init().catch((e) => console.error("welcome init failed:", e));
