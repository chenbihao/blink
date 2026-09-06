/**
 * 0.22.13 首次启动引导窗口——分步向导。
 *
 * 4 步（步步可跳过/后退）：核心快捷键 → 常用开关 → 引擎增强 → 完成。
 * 任何退出路径（完成/跳过/关窗）都标记已完成当前版引导（onboarding_version）：
 * - 本页走 complete_onboarding 命令（版本常量唯一真源在后端）；
 * - 窗口 X 关闭由后端 CloseRequested 回调兜底。
 * 引导内容随版本更新：后端 ONBOARDING_VERSION 自增即会让老用户补看一次。
 *
 * 纯逻辑（步骤状态机/OCR 编排纯函数）在 ./welcome/wizard.js，本模块只做 DOM 与 invoke。
 */

import {getCurrentWindow, invoke, listen} from "./shared/tauri.js";
import {applyI18nFromConfig, onLangChange, t} from "./i18n/index.js";
import {renderCombo} from "./shared/kbd.js";
import {EVENTS} from "./shared/event-names.js";
import {
    canGoBack,
    canGoNext,
    classifyInstallStage,
    clampStep,
    estimateEtaMs,
    etaTextKeyAndParams,
    formatBytes,
    installStageTextKey,
    isLastStep,
    isOcrReady,
    nextStep,
    OCR_ENGINE_ID,
    pickEngineStatus,
    prevStep,
    progressPercent,
    pushProgressSample,
} from "./welcome/wizard.js";

// ── 快捷键数据（第 1 步）──────────────────────────────────────────────────────

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

// ── 第 2 步开关定义（0.22.13：悬浮球已随 0.11 划词 chord 移除，不再列入）────────

const TOGGLES = [
    {
        id: "auto_start",
        labelKey: "welcome.step2.auto_start",
        descKey: "welcome.step2.auto_start.desc",
    },
    {
        id: "chord_enabled",
        labelKey: "welcome.step2.chord",
        descKey: "welcome.step2.chord.desc",
    },
];

// ── 全局状态 ────────────────────────────────────────────────────────────────

let currentStep = 0;
/** 第 2 步开关当前值（get_config 一次性读入，改动即时 set_config 生效）。 */
let toggleValues = {auto_start: false, chord_enabled: true, chord_hint_visible: true};
/** OCR 引导 UI 状态：idle | checking | not-installed | installing | ready | failed | unavailable。 */
let ocrState = "idle";
/** 最近一次 install-stage 的 stage wire 值（installing 态展示对应文案；渲染时翻译）。 */
let ocrStage = "";
/** 下载进度（installing 态非 null）：{ downloaded, total, samples }。 */
let ocrProgress = null;
/** OCR 竞态防护代际：进入新检查/安装时自增，旧异步回调按代际失效。 */
let ocrGeneration = 0;

// ── 第 1 步：快捷键渲染 ──────────────────────────────────────────────────────

function renderShortcuts() {
    const container = document.getElementById("shortcut-list");
    if (!container) return;

    container.innerHTML = "";

    const mainSection = document.createElement("div");
    mainSection.className = "welcome-section welcome-section--main";

    const mainRow = document.createElement("div");
    mainRow.className = "welcome-shortcut-row welcome-shortcut-row--main";
    const mainLabel = document.createElement("span");
    mainLabel.className = "welcome-shortcut-label";
    mainLabel.textContent = t(MAIN_SHORTCUT.labelKey);
    const mainKeys = document.createElement("span");
    mainKeys.className = "welcome-shortcut-keys";
    mainKeys.appendChild(renderCombo(MAIN_SHORTCUT.combo));
    mainRow.appendChild(mainLabel);
    mainRow.appendChild(mainKeys);
    mainSection.appendChild(mainRow);

    const mainHint = document.createElement("p");
    mainHint.className = "welcome-section-hint";
    mainHint.textContent = t(MAIN_SHORTCUT.hintKey);
    mainSection.appendChild(mainHint);

    container.appendChild(mainSection);

    const chordSection = document.createElement("div");
    chordSection.className = "welcome-section welcome-section--chord";

    const chordTitle = document.createElement("div");
    chordTitle.className = "welcome-section-title";
    chordTitle.textContent = t("welcome.chord.title");
    chordSection.appendChild(chordTitle);

    const chordDesc = document.createElement("p");
    chordDesc.className = "welcome-section-desc";
    chordDesc.textContent = t("welcome.chord.desc");
    chordSection.appendChild(chordDesc);

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

// ── 步骤导航 ────────────────────────────────────────────────────────────────

function renderStepIndicator() {
    const container = document.getElementById("step-indicator");
    if (!container) return;
    container.innerHTML = "";
    for (let i = 0; i < 4; i++) {
        const dot = document.createElement("span");
        dot.className = i === currentStep ? "welcome-step-dot welcome-step-dot--active" : "welcome-step-dot";
        container.appendChild(dot);
    }
}

function goToStep(step) {
    currentStep = clampStep(step);
    for (let i = 0; i < 4; i++) {
        document.getElementById(`step-${i}`)?.classList.toggle("hidden", i !== currentStep);
    }
    renderStepIndicator();

    const backBtn = document.getElementById("back-btn");
    const nextBtn = document.getElementById("next-btn");
    const skipBtn = document.getElementById("skip-btn");
    if (backBtn) backBtn.disabled = !canGoBack(currentStep);
    if (nextBtn) {
        nextBtn.textContent = isLastStep(currentStep) ? t("welcome.nav.finish") : t("welcome.nav.next");
    }
    if (skipBtn) skipBtn.classList.toggle("hidden", isLastStep(currentStep));

    // 进入引擎步骤时懒探测 OCR 状态（一次性，失败不阻塞）
    if (currentStep === 2 && ocrState === "idle") {
        checkOcr();
    }
}

// ── 第 2 步：常用开关 ────────────────────────────────────────────────────────

function renderToggles() {
    const container = document.getElementById("toggle-list");
    if (!container) return;
    container.innerHTML = "";

    for (const item of TOGGLES) {
        const row = document.createElement("div");
        row.className = "welcome-toggle-row";

        const text = document.createElement("div");
        text.className = "welcome-toggle-text";
        const label = document.createElement("span");
        label.className = "welcome-toggle-label";
        label.textContent = t(item.labelKey);
        const desc = document.createElement("span");
        desc.className = "welcome-toggle-desc";
        desc.textContent = t(item.descKey);
        text.appendChild(label);
        text.appendChild(desc);

        const wrap = document.createElement("label");
        wrap.className = "welcome-switch";
        const input = document.createElement("input");
        input.type = "checkbox";
        input.checked = toggleValues[item.id] === true;
        input.addEventListener("change", () => applyToggle(item.id, input.checked));
        const slider = document.createElement("span");
        slider.className = "welcome-switch-slider";
        wrap.appendChild(input);
        wrap.appendChild(slider);

        row.appendChild(text);
        row.appendChild(wrap);
        container.appendChild(row);
    }
}

/** 开关写入即生效（与设置页同一 set_config 通道）。 */
async function applyToggle(id, enabled) {
    toggleValues[id] = enabled;
    try {
        if (id === "auto_start") {
            await invoke("set_config", {key: "auto_start", value: enabled});
        } else if (id === "chord_enabled") {
            // chord_toggles 是结构体分片：保留 chord_hint_visible 不被覆盖
            await invoke("set_config", {
                key: "chord_toggles",
                value: {
                    chord_enabled: enabled,
                    chord_hint_visible: toggleValues.chord_hint_visible === true,
                },
            });
        }
    } catch (e) {
        console.error(`welcome: set_config ${id} failed:`, e);
    }
}

// ── 第 3 步：OCR 引导编排（复用引擎页同一条 install 命令与进度事件）────────────

/** installing 态的主文案：按当前阶段显示（下载中/校验中/…），未知阶段回退「准备中」。 */
function ocrStageText() {
    const stage = ocrStage || "preparing";
    return t(installStageTextKey(stage));
}

/** 渲染下载进度区（仅 installing 且收到过进度事件时可见）。 */
function renderOcrProgress() {
    const wrap = document.getElementById("ocr-progress");
    const fill = document.getElementById("ocr-progress-fill");
    const textEl = document.getElementById("ocr-progress-text");
    if (!wrap || !fill || !textEl) return;

    const showProgress = ocrState === "installing" && ocrProgress !== null && ocrStage === "downloading";
    wrap.classList.toggle("hidden", !showProgress);
    if (!showProgress) return;

    const {downloaded, total, samples} = ocrProgress;
    const percent = progressPercent(downloaded, total);

    fill.classList.toggle("welcome-progress-bar-fill--indeterminate", percent === null);
    fill.style.width = percent === null ? "" : `${percent}%`;

    const parts = [];
    if (percent !== null) {
        parts.push(t("welcome.step3.ocr.progress_bytes", {
            downloaded: formatBytes(downloaded),
            total: formatBytes(total),
            percent,
        }));
    } else {
        parts.push(t("welcome.step3.ocr.progress_unknown", {downloaded: formatBytes(downloaded)}));
    }
    const eta = etaTextKeyAndParams(estimateEtaMs(samples, total));
    if (eta) parts.push(t(eta.key, eta.params));
    textEl.textContent = parts.join(" · ");
}

function renderOcrStatus() {
    const statusEl = document.getElementById("ocr-status");
    const btn = document.getElementById("ocr-install-btn");
    if (!statusEl || !btn) return;

    statusEl.classList.remove("welcome-engine-status--ok");
    switch (ocrState) {
        case "checking":
            statusEl.textContent = t("welcome.step3.ocr.checking");
            btn.disabled = true;
            btn.classList.add("hidden");
            break;
        case "not-installed":
            statusEl.textContent = "";
            btn.disabled = false;
            btn.textContent = t("welcome.step3.ocr.action");
            btn.classList.remove("hidden");
            break;
        case "installing":
            statusEl.textContent = ocrStageText();
            btn.disabled = true;
            btn.classList.add("hidden");
            break;
        case "ready":
            statusEl.textContent = t("welcome.step3.ocr.ready");
            statusEl.classList.add("welcome-engine-status--ok");
            btn.classList.add("hidden");
            break;
        case "failed":
            statusEl.textContent = t("welcome.step3.ocr.failed");
            btn.disabled = false;
            btn.textContent = t("welcome.step3.ocr.retry");
            btn.classList.remove("hidden");
            break;
        case "unavailable":
        default:
            statusEl.textContent = t("welcome.step3.ocr.unavailable");
            btn.classList.add("hidden");
            break;
    }
    renderOcrProgress();
}

function setOcrState(state) {
    ocrState = state;
    if (state !== "installing") {
        ocrStage = "";
        ocrProgress = null;
    }
    renderOcrStatus();
}

/** 后端 CommandError 的 code 提取（非对象错误返回空串）。 */
function errorCodeOf(err) {
    return err && typeof err === "object" ? String(err.code ?? "") : "";
}

async function checkOcr() {
    const gen = ++ocrGeneration;
    setOcrState("checking");
    try {
        // 就绪判定用引擎级状态（ORT+模型联合提交，environment=ready 即可用）；
        // 模型目录 list_engine_models 只注册了 FunASR，OCR 查询恒为空。
        const list = await invoke("get_local_engine_status", {engineId: OCR_ENGINE_ID});
        if (gen !== ocrGeneration) return; // 旧代际结果丢弃
        setOcrState(isOcrReady(pickEngineStatus(list, OCR_ENGINE_ID)) ? "ready" : "not-installed");
    } catch (e) {
        console.error("welcome: get_local_engine_status failed:", e);
        if (gen !== ocrGeneration) return;
        setOcrState("unavailable");
    }
}

async function installOcr() {
    const gen = ++ocrGeneration;
    setOcrState("installing");
    try {
        // PP-OCR 一键安装 = 引擎级安装：ORT DLL 与模型在同一安装事务内联合提交
        // （0.22 §3.9），没有独立模型安装步骤；幂等，已就绪时后端自动跳过。
        await invoke("install_local_engine", {engineId: OCR_ENGINE_ID, computePreference: null});
        if (gen !== ocrGeneration) return;
        // 完成后复查状态定终态；未达 ready（异常场景）给失败态可重试
        const list = await invoke("get_local_engine_status", {engineId: OCR_ENGINE_ID});
        if (gen !== ocrGeneration) return;
        setOcrState(isOcrReady(pickEngineStatus(list, OCR_ENGINE_ID)) ? "ready" : "failed");
    } catch (e) {
        console.error("welcome: OCR install failed:", e);
        if (gen !== ocrGeneration) return;
        // already_running = 已有安装在进行（如设置页发起）→ 留在 installing，
        // 由 install-stage 终态事件接管刷新；其余错误给失败态 + 重试
        if (errorCodeOf(e) !== "already_running") setOcrState("failed");
    }
}

/** 监听引擎安装进度事件：接管「外部发起的安装」的进度展示与终态刷新。 */
function watchInstallEvents() {
    // 阶段事件：更新 installing 态的主文案（下载中/校验中/…）
    listen(EVENTS.LOCAL_ENGINE_INSTALL_STAGE, (ev) => {
        const p = ev?.payload;
        if (!p || p.engine_id !== OCR_ENGINE_ID) return;
        const kind = classifyInstallStage(p.stage);
        if (kind === "active") {
            if (ocrState === "installing") {
                ocrStage = p.stage;
                renderOcrStatus();
            }
            return;
        }
        // 终态：稍候重查（给安装命令收尾提交状态留出时间）；已就绪则不折腾
        if (ocrState === "ready") return;
        setTimeout(() => {
            if (ocrState === "installing") checkOcr();
        }, 600);
    });

    // 字节进度事件：更新下载进度条与 ETA（仅 installing 态消费）
    listen(EVENTS.LOCAL_ENGINE_INSTALL_PROGRESS, (ev) => {
        const p = ev?.payload;
        if (!p || p.engine_id !== OCR_ENGINE_ID) return;
        if (ocrState !== "installing") return;
        const downloaded = Number(p.downloaded);
        if (!Number.isFinite(downloaded) || downloaded < 0) return;
        const total = Number.isFinite(Number(p.total)) && p.total > 0 ? Number(p.total) : null;
        const samples = pushProgressSample(ocrProgress?.samples ?? [], Date.now(), downloaded);
        ocrProgress = {downloaded, total, samples};
        renderOcrProgress();
    });
}

// ── 完成/退出 ───────────────────────────────────────────────────────────────

async function finish() {
    try {
        await invoke("complete_onboarding");
    } catch (e) {
        console.error("welcome: complete_onboarding failed:", e);
    }
    getCurrentWindow()?.close();
}

// ── 初始化 ────────────────────────────────────────────────────────────────

async function init() {
    await applyI18nFromConfig();

    // 读取开关初始值（get_config 一次拿全量，向导会话内够用）
    try {
        const cfg = await invoke("get_config");
        toggleValues = {
            auto_start: cfg.auto_start === true,
            chord_enabled: cfg.chord_enabled === true,
            chord_hint_visible: cfg.chord_hint_visible === true,
        };
    } catch (e) {
        console.error("welcome: get_config failed:", e);
    }

    renderShortcuts();
    renderToggles();
    renderOcrStatus();
    watchInstallEvents();
    onLangChange(() => {
        renderShortcuts();
        renderToggles();
        renderOcrStatus();
        goToStep(currentStep); // 刷新底部按钮文案
    });

    // 导航按钮
    document.getElementById("back-btn")?.addEventListener("click", () => goToStep(prevStep(currentStep)));
    document.getElementById("next-btn")?.addEventListener("click", () => {
        if (isLastStep(currentStep)) {
            finish();
        } else {
            goToStep(nextStep(currentStep));
        }
    });
    document.getElementById("skip-btn")?.addEventListener("click", finish);

    // 第 3 步按钮
    document.getElementById("ocr-install-btn")?.addEventListener("click", installOcr);
    document.getElementById("voice-engines-btn")?.addEventListener("click", async () => {
        try {
            await invoke("open_settings_tab", {tab: "engines"});
        } catch (e) {
            console.error("welcome: open_settings_tab failed:", e);
        }
    });

    goToStep(0);
}

init().catch((e) => console.error("welcome init failed:", e));
