/**
 * 0.22.13 首次启动引导窗口——分步向导。
 *
 * 4 步（步步可跳过/后退）：核心快捷键 → 常用开关 → 引擎增强 → 完成。
 * 第 1 步每个 Chord 快捷键右侧带开关，直接开启该动作的全局快捷键
 * （与设置页 chord tab 同一 chord_bindings.global 契约，开启 = 跟随触发键）。
 * 任何退出路径（完成/跳过/关窗）都标记已完成当前版引导（onboarding_version）：
 * - 本页走 complete_onboarding 命令（版本常量唯一真源在后端）；
 * - 窗口 X 关闭由后端 CloseRequested 回调兜底。
 * 引导内容随版本更新：后端 ONBOARDING_VERSION 自增即会让老用户补看一次。
 *
 * 纯逻辑（步骤状态机/OCR 编排纯函数）在 ./welcome/wizard.js，本模块只做 DOM 与 invoke。
 */

import {commandErrorText, getCurrentWindow, invoke, listen} from "./shared/tauri.js";
import {applyI18nFromConfig, onLangChange, t} from "./i18n/index.js";
import {normalizeCombo, renderCombo} from "./shared/kbd.js";
import {EVENTS} from "./shared/event-names.js";
import {buildChordTogglesPayload, saveConfig} from "./shared/config-keys.js";
import {
    estimateEtaMs,
    etaTextKeyAndParams,
    formatBytes,
    progressPercent,
    pushProgressSample,
} from "./shared/download-progress.js";
import {
    activeOperationId,
    applyChordGlobalToBindings,
    canGoBack,
    clampStep,
    classifyInstallStage,
    installStageTextKey,
    isLastStep,
    isOcrReady,
    nextStep,
    OCR_ENGINE_ID,
    pickEngineStatus,
    prevStep,
    shouldAcceptInstallEvent,
} from "./welcome/wizard.js";

// ── 快捷键数据（第 1 步）──────────────────────────────────────────────────────

// 主快捷键：全局热键，任何地方按下即可触发
const MAIN_SHORTCUT = {
    combo: "Alt+Space",
    labelKey: "welcome.shortcut.voice_input",
    hintKey: "welcome.main.hint",
};

// Chord 快捷键：仅在主窗口可见时按住 Alt + 字母键触发。
// id = 后端 chord 动作 id（chord_bindings 的 key），右侧开关写入其 global 字段
// 将组合键升级为全局快捷键；combo 为默认键位，自定义过触发键的按实际配置渲染。
const CHORD_SHORTCUTS = [
    {id: "chat", combo: "Alt+Q", labelKey: "welcome.shortcut.chat"},
    {id: "screenshot", combo: "Alt+A", labelKey: "welcome.shortcut.screenshot"},
    {id: "clipboard_history", combo: "Alt+C", labelKey: "welcome.shortcut.clipboard_history"},
    {id: "edit", combo: "Alt+E", labelKey: "welcome.shortcut.edit"},
    {id: "sticky", combo: "Alt+S", labelKey: "welcome.shortcut.sticky"},
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
/** Chord toggles 保存的 revision token：防止快速连续切换时旧请求覆盖新状态。 */
let chordToggleRevision = 0;
/** Chord toggles 最后一次后端已确认的值（用于失败回滚）。 */
let chordToggleConfirmed = {chord_enabled: false, chord_hint_visible: true};
/** chord_toggles shard 写入串行化链（promise chain）。
 *
 *  chord_toggles 是结构体分片（chordEnabled + chordHintVisible），
 *  并发 set_config 会导致 last-writer-wins 覆盖另一个字段的值。
 *  通过 promise chain 串行化所有 chord_toggles 写入，确保每次写入
 *  都基于最新的 toggleValues 构造 payload，不会丢失字段。 */
let chordTogglesWriteChain = Promise.resolve();
/** 第 1 步 chord 全局快捷键当前值（id → boolean；get_config 读入，改动即时 set_config 生效）。 */
let chordGlobalValues = {};
/** chord_bindings 快照（id → binding）：渲染实际触发键位 + 保存成功后同步。 */
let chordBindingsConfig = {};
/** chord_bindings 写入串行化链（promise chain）。
 *
 *  chord_bindings 是结构体分片（每个动作一个条目），并发 read-modify-write
 *  会 last-writer-wins 覆盖其他动作的 global/key 字段（与设置页 chord tab
 *  同一问题域），所有写入必须经此链串行执行。 */
let chordBindingsWriteChain = Promise.resolve();
/** 每个 chord 动作独立的保存 revision：旧请求迟到响应不得覆盖新状态。 */
const chordGlobalRevisions = new Map();
/** 最后一次后端已确认的全局快捷键状态（id → boolean），失败回滚真源
 *  （不重新 get_config，避免读到并发写入的中间态）。 */
const confirmedChordGlobal = new Map();
/** OCR 引导 UI 状态：idle | checking | not-installed | installing | ready | failed | unavailable。 */
let ocrState = "idle";
/** 最近一次 install-stage 的 stage wire 值（installing 态展示对应文案；渲染时翻译）。 */
let ocrStage = "";
/** 下载进度（installing 态非 null）：{ downloaded, total, samples }。 */
let ocrProgress = null;
/** OCR 竞态防护代际：进入新检查/安装时自增，旧异步回调按代际失效。 */
let ocrGeneration = 0;
/** 当前安装操作的 operation_id（从后端状态真源获取，不允许首事件绑定）。
 *  install_local_engine 的终态返回值仅作完成时兜底。
 *  用于隔离不同安装操作的事件——旧操作的迟到事件不能覆盖新操作 UI。 */
let ocrOperationId = null;

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
    for (const {id, combo, labelKey} of CHORD_SHORTCUTS) {
        const binding = chordBindingsConfig[id];
        // 自定义过触发键的按实际配置渲染；空 key = 未覆盖默认（后端按 default_key 解析）
        const comboStr = binding?.key
            ? normalizeCombo(binding.modifiers ?? ["alt"], binding.key)
            : combo;
        const isGlobal = chordGlobalValues[id] === true;

        const row = document.createElement("div");
        row.className = isGlobal ? "welcome-shortcut-row is-global" : "welcome-shortcut-row";
        const label = document.createElement("span");
        label.className = "welcome-shortcut-label";
        label.textContent = t(labelKey);

        const side = document.createElement("span");
        side.className = "welcome-shortcut-side";
        const keys = document.createElement("span");
        keys.className = "welcome-shortcut-keys";
        keys.appendChild(renderCombo(comboStr));
        side.appendChild(keys);

        // 模式状态标签：标注 switch 两态含义（窗口内 Chord / 全局快捷键）
        const mode = document.createElement("span");
        mode.className = isGlobal ? "welcome-shortcut-mode is-on" : "welcome-shortcut-mode";
        mode.textContent = t(isGlobal ? "welcome.shortcut.mode.global" : "welcome.shortcut.mode.window");
        side.appendChild(mode);

        // 全局快捷键开关：打开 = 组合键注册为系统级全局键（任意界面可触发）
        const wrap = document.createElement("label");
        wrap.className = "welcome-switch";
        const input = document.createElement("input");
        input.type = "checkbox";
        input.checked = isGlobal;
        input.addEventListener("change", () => {
            setGlobalRowVisual(row, input.checked);
            applyChordGlobal(id, input.checked);
        });
        const slider = document.createElement("span");
        slider.className = "welcome-switch-slider";
        wrap.appendChild(input);
        wrap.appendChild(slider);
        side.appendChild(wrap);

        row.appendChild(label);
        row.appendChild(side);
        chordList.appendChild(row);
    }
    chordSection.appendChild(chordList);

    const globalHint = document.createElement("p");
    globalHint.className = "welcome-section-hint";
    globalHint.textContent = t("welcome.chord.global_hint");
    chordSection.appendChild(globalHint);

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

/** 开关写入即生效（与设置页同一 set_config 通道）。
 *
 *  chord_toggles 写入通过 chordTogglesWriteChain 串行化，
 *  确保并发切换不会导致后端覆盖（last-writer-wins 数据丢失）。 */
async function applyToggle(id, enabled) {
    const prevValue = toggleValues[id];
    toggleValues[id] = enabled;
    try {
        if (id === "auto_start") {
            await invoke("set_config", {key: "auto_start", value: enabled});
        } else if (id === "chord_enabled") {
            // chord_toggles 是结构体分片：通过串行化链写入，
            // 每次都从最新 toggleValues 构造 payload，避免并发覆盖 chord_hint_visible
            const rev = ++chordToggleRevision;
            const payload = buildChordTogglesPayload(
                toggleValues.chord_enabled === true,
                toggleValues.chord_hint_visible === true,
            );
            await new Promise((resolve, reject) => {
                chordTogglesWriteChain = chordTogglesWriteChain.then(async () => {
                    try {
                        await invoke("set_config", {key: "chord_toggles", value: payload});
                        resolve();
                    } catch (err) {
                        reject(err);
                    }
                });
            });
            // 串行写入一旦成功，payload 就是此刻真实的后端已确认状态。
            // 即使 UI 已有更新 revision，也必须推进 confirmed 快照，供后一笔失败回滚。
            chordToggleConfirmed = {
                chord_enabled: payload.chordEnabled,
                chord_hint_visible: payload.chordHintVisible,
            };
            // 旧请求的迟到响应不再更新 UI。
            if (rev !== chordToggleRevision) return;
        }
    } catch (e) {
        console.error(`welcome: set_config ${id} failed:`, e);
        // 回滚 checkbox 和内存中的 toggleValues
        toggleValues[id] = prevValue;
        if (id === "chord_enabled") {
            chordToggleRevision++; // 使任何在途请求失效
            toggleValues.chord_enabled = chordToggleConfirmed.chord_enabled;
            toggleValues.chord_hint_visible = chordToggleConfirmed.chord_hint_visible;
        }
        renderToggles();
        // 向用户显示可理解的错误
        showWelcomeError(commandErrorText(e, t("welcome.error.save_failed")));
    }
}

// ── 瞬时错误提示（第 1/2 步开关保存失败共用）────────────────────────────────

let welcomeErrorTimer = 0;

/** 底部居中显示一条错误提示，4 秒后自动隐藏；新消息覆盖旧消息与旧 timer。 */
function showWelcomeError(message) {
    const msgEl = document.getElementById("welcome-error");
    if (!msgEl) return;
    msgEl.textContent = message;
    msgEl.classList.remove("hidden");
    if (welcomeErrorTimer) clearTimeout(welcomeErrorTimer);
    welcomeErrorTimer = setTimeout(() => {
        msgEl.classList.add("hidden");
        welcomeErrorTimer = 0;
    }, 4000);
}

// ── 第 1 步：Chord 全局快捷键开关（复用设置页 chord tab 同一契约）────────────

/** 开关切换时原位更新行视觉（状态标签 + 键帽染色），不重建列表避免闪烁。
 *  乐观更新：保存失败由 applyChordGlobal 回滚 chordGlobalValues 后整体重渲染。 */
function setGlobalRowVisual(row, isOn) {
    row.classList.toggle("is-global", isOn);
    const mode = row.querySelector(".welcome-shortcut-mode");
    if (mode) {
        mode.classList.toggle("is-on", isOn);
        mode.textContent = t(isOn ? "welcome.shortcut.mode.global" : "welcome.shortcut.mode.window");
    }
}

/** 开关写入即生效（chord_bindings.global 字段级更新）。
 *
 *  开启 = `{mode:"follow_chord"}`（跟随触发键，系统级注册，主窗隐藏时也可
 *  触发）；关闭 = 删除 global 字段。
 *
 *  **串行化 + revision**：chord_bindings 是结构体分片，写入经 promise chain
 *  串行执行（并发 read-modify-write 会 last-writer-wins 丢其他动作的字段）；
 *  每个动作独立 revision，被新请求取代的旧响应不更新 UI。
 *  失败时从 confirmedChordGlobal 真源回滚，不重新 get_config。 */
async function applyChordGlobal(id, enabled) {
    const rev = (chordGlobalRevisions.get(id) ?? 0) + 1;
    chordGlobalRevisions.set(id, rev);
    chordGlobalValues[id] = enabled;

    let failure = null;
    const result = await new Promise((resolve) => {
        chordBindingsWriteChain = chordBindingsWriteChain.then(async () => {
            if (rev !== chordGlobalRevisions.get(id)) return resolve("superseded");
            try {
                const fullCfg = await invoke("get_config");
                if (rev !== chordGlobalRevisions.get(id)) return resolve("superseded");
                const next = applyChordGlobalToBindings(fullCfg?.chord_bindings, id, enabled);
                await saveConfig("chord_bindings", next);
                if (rev !== chordGlobalRevisions.get(id)) return resolve("superseded");
                // 保存成功：推进已确认状态与本地快照（后续渲染按实际键位显示）
                confirmedChordGlobal.set(id, enabled);
                chordBindingsConfig = next;
                resolve("ok");
            } catch (err) {
                failure = err;
                console.error(`welcome: set chord global (${id}) failed:`, err);
                resolve("failed");
            }
        }).catch((err) => {
            // chain 内部不应抛出（已 try-catch），防御性兜底
            failure = err;
            console.error("welcome: chord bindings write chain error:", err);
            resolve("failed");
        });
    });
    // ok / superseded 都不动 UI：被取代时新请求负责最新状态
    if (result !== "failed") return;
    // 失败返回时已有更新的请求接管同一动作：回滚会覆盖新请求的 UI 状态，跳过
    if (rev !== chordGlobalRevisions.get(id)) return;

    chordGlobalValues[id] = confirmedChordGlobal.get(id) === true;
    renderShortcuts();
    showWelcomeError(commandErrorText(failure, t("welcome.error.save_failed")));
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

    fill.classList.toggle("download-progress__fill--indeterminate", percent === null);
    fill.style.width = percent === null ? "" : `${percent}%`;

    const parts = [];
    if (percent !== null) {
        parts.push(t("local_engine.progress.bytes", {
            downloaded: formatBytes(downloaded),
            total: formatBytes(total),
            percent,
        }));
    } else {
        parts.push(t("local_engine.progress.unknown_total", {downloaded: formatBytes(downloaded)}));
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
        ocrOperationId = null;
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
        const status = pickEngineStatus(list, OCR_ENGINE_ID);
        const activeOpId = activeOperationId(status);
        if (activeOpId) {
            ocrOperationId = activeOpId;
            setOcrState("installing");
        } else {
            setOcrState(isOcrReady(status) ? "ready" : "not-installed");
        }
    } catch (e) {
        console.error("welcome: get_local_engine_status failed:", e);
        if (gen !== ocrGeneration) return;
        setOcrState("unavailable");
    }
}

const OCR_OPERATION_BIND_ATTEMPTS = 40;
const OCR_OPERATION_BIND_INTERVAL_MS = 50;

/**
 * 安装命令本身直到终态才返回，因此安装进行中必须主动从状态真源取得
 * operation_id。在绑定完成前事件监听器保持 fail-closed。
 */
async function bindOcrOperationFromStatus(gen) {
    for (let attempt = 0; attempt < OCR_OPERATION_BIND_ATTEMPTS; attempt++) {
        if (gen !== ocrGeneration || ocrState !== "installing" || ocrOperationId) {
            return ocrOperationId;
        }
        try {
            const list = await invoke("get_local_engine_status", {engineId: OCR_ENGINE_ID});
            if (gen !== ocrGeneration || ocrState !== "installing") return null;
            const activeOpId = activeOperationId(pickEngineStatus(list, OCR_ENGINE_ID));
            if (activeOpId) {
                ocrOperationId = activeOpId;
                return ocrOperationId;
            }
        } catch (error) {
            // 安装命令的错误路径负责最终提示；这里继续短暂轮询以跨过 claim 竞态。
            console.debug("welcome: operation_id 尚不可用", error);
        }
        await new Promise((resolve) => setTimeout(resolve, OCR_OPERATION_BIND_INTERVAL_MS));
    }
    return null;
}

async function installOcr() {
    const gen = ++ocrGeneration;
    // 重置 operation_id：新安装操作的进度事件从此刻起绑定新 operation_id
    ocrOperationId = null;
    setOcrState("installing");
    try {
        // PP-OCR 一键安装 = 引擎级安装：ORT DLL 与模型在同一安装事务内联合提交
        // （0.22 §3.9），没有独立模型安装步骤；幂等，已就绪时后端自动跳过。
        //
        // install_local_engine 是阻塞命令：会等到安装完成（或失败/取消）才返回。
        // 返回值 EngineOperationFinishedDto 包含 operation_id。
        // 安装期间，后端通过 install-stage / install-progress 事件推送进度。
        const installPromise = invoke("install_local_engine", {
            engineId: OCR_ENGINE_ID,
            computePreference: null,
        });
        // 不等待阻塞安装命令结束；从状态真源尽早绑定当前 operation。
        const bindPromise = bindOcrOperationFromStatus(gen);
        const result = await installPromise;
        if (gen !== ocrGeneration) return;
        // 从返回值绑定 operation_id（即使安装已完成，终态事件仍需校验）
        if (result?.operation_id) {
            ocrOperationId = result.operation_id;
        }
        await bindPromise;
        // 完成后复查状态定终态；未达 ready（异常场景）给失败态可重试
        const list = await invoke("get_local_engine_status", {engineId: OCR_ENGINE_ID});
        if (gen !== ocrGeneration) return;
        setOcrState(isOcrReady(pickEngineStatus(list, OCR_ENGINE_ID)) ? "ready" : "failed");
    } catch (e) {
        console.error("welcome: OCR install failed:", e);
        if (gen !== ocrGeneration) return;
        // already_running = 已有安装在进行（如设置页发起）
        // 从后端状态获取当前 operation_id，用于事件隔离
        if (errorCodeOf(e) === "already_running") {
            try {
                const list = await invoke("get_local_engine_status", {engineId: OCR_ENGINE_ID});
                if (gen !== ocrGeneration) return;
                const status = pickEngineStatus(list, OCR_ENGINE_ID);
                ocrOperationId = activeOperationId(status);
            } catch (queryErr) {
                console.warn("welcome: query operation_id for already_running failed:", queryErr);
            }
            // 留在 installing，由 install-stage 终态事件接管刷新
            return;
        }
        // 其余错误给失败态 + 重试
        setOcrState("failed");
    }
}

/** 监听引擎安装进度事件：接管「外部发起的安装」的进度展示与终态刷新。
 *
 *  **operation_id 隔离铁则**：
 *  - operation_id 从 get_local_engine_status 主动获取；命令终态返回值只作兜底，
 *    不允许从首事件绑定（首事件可能来自旧操作的迟到推送）。
 *  - 已绑定 operation_id 时，事件 operation_id 必须匹配才接受。
 *  - 未绑定 operation_id 时拒绝事件，避免旧操作迟到推送污染当前 UI。 */
function watchInstallEvents() {
    // 阶段事件：更新 installing 态的主文案（下载中/校验中/…）
    listen(EVENTS.LOCAL_ENGINE_INSTALL_STAGE, (ev) => {
        const p = ev?.payload;
        if (!p || p.engine_id !== OCR_ENGINE_ID) return;
        if (ocrState !== "installing") return;
        if (!shouldAcceptInstallEvent(ocrOperationId, p.operation_id).accept) return;
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
        // 终态事件到达时清理 operation 状态
        ocrOperationId = null;
        setTimeout(() => {
            if (ocrState === "installing") checkOcr();
        }, 600);
    });

    // 字节进度事件：更新下载进度条与 ETA（仅 installing 态消费）
    listen(EVENTS.LOCAL_ENGINE_INSTALL_PROGRESS, (ev) => {
        const p = ev?.payload;
        if (!p || p.engine_id !== OCR_ENGINE_ID) return;
        if (ocrState !== "installing") return;
        if (!shouldAcceptInstallEvent(ocrOperationId, p.operation_id).accept) return;
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
            chord_hint_visible: cfg.chord_hint_visible === false ? false : true,
        };
        chordToggleConfirmed = {
            chord_enabled: toggleValues.chord_enabled,
            chord_hint_visible: toggleValues.chord_hint_visible,
        };
        // 第 1 步全局快捷键开关初值（chord_bindings.global 字段存在即开启）
        chordBindingsConfig = cfg.chord_bindings ?? {};
        for (const {id} of CHORD_SHORTCUTS) {
            const enabled = chordBindingsConfig[id]?.global != null;
            chordGlobalValues[id] = enabled;
            confirmedChordGlobal.set(id, enabled);
        }
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
