/**
 * 内容编辑器窗口入口（0.16.3；0.23.1 会话化重构为纯装配层）。
 *
 * 职责只剩装配：主题/i18n/图标、构造 Adapter + EditorSession、绑定窗口控制
 * 与快捷键、注册后端事件并分发。编辑语义在 EditorAdapter + Engine，
 * 会话语义在 EditorSession，后端会话编排在 EditorSessionService。
 *
 * 关联模块：
 * - editor-adapter.js     Source/MD 双视图唯一操作面（检查点/风险门/revision）
 * - editor-session.js     前端会话状态机（commit/end 代际防护）
 * - engines/              SourceEngine（textarea）/ MarkdownIrEngine（Tiptap）
 * - shared/tiptap-editor.js  Tiptap vendor 封装（EOF 换行约定）
 */

import {applyThemeFromConfig} from "../shared/theme.js";
import {applyI18nFromConfig, onLangChange, t} from "../i18n/index.js";
import {ensureSpriteLoaded} from "../shared/icon.js";
import {choiceDialog, getCurrentWindow, listen, normalizeError} from "../shared/tauri.js";
import {renderComboHTML} from "../shared/kbd.js";
import {EVENTS} from "../shared/event-names.js";
import {
    cancelEditorTransform,
    commitEditorSession,
    copyToClipboard,
    endEditorSession,
    getChatStatus,
    getEditorSession,
    getEditorVoiceSnapshot,
    getStickyNote,
    resolveEditorExit,
    startEditorTransform,
    startEditorVoice,
    stopEditorVoice,
} from "../shared/api.js";
import {EditorAdapter} from "./editor-adapter.js";
import {EditorSession} from "./editor-session.js";
import {EditorActions} from "./actions.js";
import {EditorVoiceController} from "./voice.js";
import {EditorTransformController} from "./transform.js";

// ── DOM 引用 ──────────────────────────────────────────

const titleEl = document.getElementById("editor-title");
const textareaEl = document.getElementById("editor-textarea");
const mdContainerEl = document.getElementById("editor-tiptap-container");
const mdToolbarEl = document.getElementById("md-toolbar");
const saveBtn = document.getElementById("btn-save");
const statusEl = document.getElementById("editor-status");
const viewSourceBtn = document.getElementById("btn-view-source");
const viewMdBtn = document.getElementById("btn-view-md");
const moreBtn = document.getElementById("btn-more");
const moreMenuEl = document.getElementById("more-menu");
const menuAnchorEl = moreBtn?.parentElement ?? null;
const targetZoneEl = document.getElementById("target-zone");
const targetIconUseEl = document.querySelector("#target-icon use");
const targetLabelEl = document.getElementById("target-label");
const dirtyDotEl = document.getElementById("dirty-dot");
const micBtn = document.getElementById("btn-mic");
const voiceChipsEl = document.getElementById("voice-chips");
const chipLocateEl = document.getElementById("chip-locate");
const chipTidyEl = document.getElementById("chip-tidy");
const chipTidySelectionEl = document.getElementById("chip-tidy-selection");
const chipDismissEl = document.getElementById("chip-dismiss");
// 0.23.4：整理候选卡
const transformCardEl = document.getElementById("transform-card");
const transformEls = {
    card: transformCardEl,
    title: document.getElementById("transform-title"),
    staleStrip: document.getElementById("transform-stale"),
    body: document.getElementById("transform-body"),
    applyBtn: document.getElementById("transform-apply"),
    copyBtn: document.getElementById("transform-copy"),
    discardBtn: document.getElementById("transform-discard"),
    closeBtn: document.getElementById("transform-close"),
};

// ── 模块装配 ──────────────────────────────────────────

/** 关闭已被会话语义确认（跳过 onCloseRequested 拦截） */
let allowClose = false;

/** 当前来源的便签 id（非便签来源为 null） */
function originStickyId() {
    return session.source?.kind === "sticky" ? session.source.stickyId : null;
}

const adapter = new EditorAdapter(
    {sourceEl: textareaEl, mdContainerEl, mdToolbarEl},
    {
        onNotice: (reasonKey) => {
            // 弱内容检测只提示，不阻断编辑（Source 恒可编辑）
            const key = t(reasonKey);
            if (key && key !== reasonKey) setStatus(key);
        },
        onContentChanged: () => {
            updateDirtyDot();
            // 0.23.4：请求后任何正文变化 → 候选 stale（§3.7）
            transform.notifyContentChanged();
            updateActionChips();
        },
        onSelectionChanged: () => updateActionChips(),
    },
);

const session = new EditorSession(
    {
        api: {
            getEditorSession,
            commitEditorSession,
            endEditorSession,
        },
        adapter,
    },
    {
        onTitle: (title) => {
            titleEl.textContent = title || t("editor.title.default");
        },
        onStatus: (message) => setStatus(message),
        onError: (message) => setStatus(message),
        onTargetChanged: () => updateTargetDisplay(),
        onSnapshotApplied: () => {
            updateViewSwitchState();
            updateDirtyDot();
        },
        onSessionCleared: () => {
            voice.handleSessionEnded();
            void transform.cancel({silent: true});
            updateViewSwitchState();
            updateDirtyDot();
            updateTargetDisplay();
            updateActionChips();
        },
    },
);

const actions = new EditorActions({
    session,
    adapter,
    callbacks: {
        onStatus: (message) => setStatus(message),
        onError: (error) => reportSaveError(error),
        onTargetSaved: () => updateTargetDisplay(),
    },
});

/** 听写错误码 → 文案（前端按 code 分类，不解析后端中文 message） */
function describeVoiceError(code /* , message */) {
    switch (code) {
        case "voice_busy":
            return t("editor.voice.busy");
        case "stt_disabled":
            return t("editor.voice.disabled");
        case "stale_session":
            return t("editor.voice.stale");
        case "voice_failed":
            return t("editor.voice.failed");
        default:
            return t("editor.voice.error", {message: code});
    }
}

// ── 连续听写（0.23.3 §3.6）────────────────────────────

const voice = new EditorVoiceController(
    {
        api: {startEditorVoice, stopEditorVoice, getEditorVoiceSnapshot},
        adapter,
        getSession: () => session,
        listen,
    },
    {
        onPhaseChanged: () => updateVoiceUi(),
        onSegmentAppended: () => updateDirtyDot(),
        onGapLost: () => setStatus(t("editor.voice.gapLost")),
        onEnded: ({count}) => {
            updateVoiceUi();
            updateDirtyDot();
            setStatus(t("editor.voice.ended", {count}));
        },
        onError: (message) => {
            updateVoiceUi();
            setStatus(message);
        },
        describeError: describeVoiceError,
    },
);

// ── AI 整理（0.23.4 §3.7：只产候选，确认后单事务替换）────

const transform = new EditorTransformController(
    {
        api: {startEditorTransform, cancelEditorTransform},
        adapter,
        getSession: () => session,
        listen,
        copyToClipboard,
        el: transformEls,
    },
    {
        onPhaseChanged: () => updateActionChips(),
        onStatus: (message) => setStatus(message),
        onError: (message) => setStatus(message),
    },
);

// ── 初始化 ────────────────────────────────────────────

async function init() {
    // 主题 + i18n + 图标
    await ensureSpriteLoaded();
    await applyThemeFromConfig();
    await applyI18nFromConfig();
    onLangChange(refreshLocalizedUi);

    bindToolbar();
    bindVoiceControls();
    bindTransformControls();
    bindWindowControls();
    bindKeyboard();
    bindBackendEvents();
    await voice.bind();
    await transform.bind();
    refreshAiAvailability();

    // init 主动拉取：覆盖“事件先于监听器注册”的时序（§1.4 存在不等于就绪）
    try {
        await session.activate();
    } catch (e) {
        const err = normalizeError(e);
        console.error(`[content-editor] init 拉取会话失败 [${err.code}]: ${err.message}`);
    }

    const win = getCurrentWindow();
    if (win) {
        try {
            await win.show();
            await win.setFocus();
            // 0.23.3：窗口重新聚焦时补齐听写段（§3.6 恢复/重新聚焦拉 snapshot）
            win.onFocusChanged?.(({payload: focused}) => {
                if (focused) voice.resyncIfActive().catch(() => {});
            });
        } catch (e) {
            console.error("[content-editor] show window 失败:", e);
        }
    }

    tracing("editor window: init 完成");
}

// ── 后端事件 ──────────────────────────────────────────

function bindBackendEvents() {
    // 会话绑定/结束（事件 + init 拉取双路径，§3.9）
    listen(EVENTS.EDITOR_SESSION_CHANGED, (event) => {
        session.handleSessionEvent(event.payload);
    });

    // 用户主动退出的一次汇总确认（§3.5，0.23.2 请求-应答协议）
    listen(EVENTS.EDITOR_EXIT_REQUEST, (event) => {
        handleExitRequest(event.payload);
    });

    // 0.23.3：会话被外部结束时停听写（confirmed 已在正文中，不回撤）
    listen(EVENTS.EDITOR_SESSION_CHANGED, (event) => {
        if (event.payload?.kind === "ended") voice.handleSessionEnded();
    });

    // 便签联动（来源为 sticky 时生效）
    listen(EVENTS.STICKY_CONTENT_CHANGED, (event) => {
        const payload = event.payload;
        if (!payload || payload.stickyId !== originStickyId()) return;
        if (payload.source === "content-editor") return; // 自己刚保存的
        reloadFromSticky();
    });

    listen(EVENTS.STICKY_TRASHED, (event) => {
        if (event.payload?.stickyId === originStickyId()) lifecycleClose("便签被回收");
    });

    listen(EVENTS.STICKY_DELETED, (event) => {
        if (event.payload?.stickyId === originStickyId()) lifecycleClose("便签已删除");
    });

    listen(EVENTS.STICKY_VISIBILITY_CHANGED, (event) => {
        const payload = event.payload;
        if (payload?.stickyId === originStickyId() && payload.visible === false) {
            lifecycleClose("便签已隐藏");
        }
    });

    // 0.23.4：AI 配置热更新 → 刷新整理入口可见性（未配置时隐藏，§3.8）
    listen(EVENTS.CONFIG_CHANGED, (event) => {
        void applyThemeFromConfig();
        // 通用 save_config 广播 unit payload；细分设置广播携带 key。
        if (!event.payload?.key || event.payload.key === "language") {
            void applyI18nFromConfig();
        }
        if (typeof event.payload?.key === "string" && event.payload.key.startsWith("ai")) {
            refreshAiAvailability();
        }
    });
}

/** AI 可用性探测（provider_configured 决定整理入口是否可见；失败按不可用） */
async function refreshAiAvailability() {
    try {
        const status = await getChatStatus();
        transform.aiAvailable = !!status?.provider_configured;
    } catch (e) {
        console.warn("[content-editor] AI 状态探测失败:", e);
        transform.aiAvailable = false;
    }
    updateActionChips();
}

/** 便签来源且本地 clean 时同步最新内容（身份墙与 dirty 重查在 EditorSession 内，§5.7）；
 *  force = 冲突后用户显式放弃修改的重载（跳过 dirty 保护，仍受身份墙约束） */
async function reloadFromSticky({force = false} = {}) {
    const stickyId = originStickyId();
    if (!stickyId || !session.isActive) return;
    const result = await session.reloadFromSticky({stickyId, force, getNote: getStickyNote});
    if (result.applied) updateDirtyDot();
    if (result.error) {
        console.error(`[content-editor] 便签同步失败 [${result.error.code}]: ${result.error.message}`);
    }
}

// ── 工具栏 ────────────────────────────────────────────

function bindToolbar() {
    // 0.23.2：保存按钮带 Ctrl+S 键位提示（spec-frontend §4.1 kbd.js 统一渲染）
    saveBtn.innerHTML = `${t("editor.save")} ${renderComboHTML("Ctrl+S")}`;
    saveBtn.addEventListener("click", handleSave);

    // 更多动作菜单（保存到/另存副本/复制/创建便签/发送到 AI 对话）
    actions.renderMenu(moreMenuEl);
    actions.bindMenuTrigger(moreBtn);
    actions.bindMenuHover(menuAnchorEl);
    updateTargetDisplay();
    updateDirtyDot();

    viewSourceBtn?.addEventListener("click", () => switchView("source"));
    viewMdBtn?.addEventListener("click", () => switchView("markdown"));
    document.getElementById("editor-view-switch")?.addEventListener("keydown", (event) => {
        if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
        event.preventDefault();
        const target = event.key === "Home" || event.key === "ArrowLeft" ? "source" : "markdown";
        const targetBtn = target === "source" ? viewSourceBtn : viewMdBtn;
        if (targetBtn?.disabled) return;
        switchView(target);
        targetBtn?.focus();
    });
}

// ── 连续听写 UI（0.23.3 §3.6/§3.8）────────────────────

/** 麦克风按钮 + 本次听写 chips + 整理入口的状态投影 */
function bindVoiceControls() {
    if (chipLocateEl) chipLocateEl.textContent = t("editor.voice.locate");

    micBtn?.addEventListener("click", () => {
        if (!session.isActive) {
            setStatus(t("editor.voice.stale"));
            return;
        }
        voice.toggle();
    });

    // 定位本次听写：直接消费听写结束时冻结的真实追加范围 handle
    //（0.23.6 二次 Review）——右边界是冻结时的文末，听写结束后继续输入
    // 不会被一并选中；前部相同文本也不会误命中。
    chipLocateEl?.addEventListener("click", () => {
        if (!voice.runHandle) {
            setStatus(t("editor.voice.locateFailed"));
            return;
        }
        if (adapter.locateDictationRun(voice.runHandle)) {
            setStatus(t("editor.voice.located"));
        } else {
            setStatus(t("editor.voice.locateFailed"));
        }
    });

    // 整理本次听写（0.23.4 §3.7 显式入口；0.23.6 使用冻结的听写范围 handle）
    chipTidyEl?.addEventListener("click", () => {
        if (!voice.runHandle) return;
        void transform.start("dictation", {handle: voice.runHandle});
    });

    // 整理选中内容（选区非空时出现，§3.8 按上下文出现的整理动作）
    chipTidySelectionEl?.addEventListener("click", () => {
        void transform.start("selection");
    });

    chipDismissEl?.addEventListener("click", hideVoiceChips);
}

// ── 整理候选卡（0.23.4 §3.7）──────────────────────────

/** 候选卡静态按钮文案（i18n 一次性装配） */
function bindTransformControls() {
    if (transformEls.applyBtn) transformEls.applyBtn.textContent = t("editor.transform.apply");
    if (transformEls.copyBtn) transformEls.copyBtn.textContent = t("editor.transform.copy");
    if (transformEls.discardBtn) transformEls.discardBtn.textContent = t("editor.transform.discard");
    if (transformEls.staleStrip) transformEls.staleStrip.textContent = t("editor.transform.stale");
    if (transformEls.closeBtn) transformEls.closeBtn.title = t("editor.transform.close");
    const hintEl = document.getElementById("transform-hint");
    if (hintEl) hintEl.textContent = t("editor.transform.hint");
}

/** 听写按钮态：idle=开始，recording/paused=结束（脉冲高亮），其余禁用 */
function updateVoiceUi() {
    if (!micBtn) return;
    const phase = voice.phase;
    micBtn.classList.toggle("is-recording", phase === "recording");
    micBtn.classList.toggle("is-paused", phase === "paused");
    micBtn.disabled = phase === "starting" || phase === "stopping";
    micBtn.title = phase === "recording" || phase === "paused"
        ? t("editor.voice.stop")
        : t("editor.voice.start");
    micBtn.setAttribute("aria-label", micBtn.title);
    micBtn.setAttribute("aria-pressed", String(phase === "recording" || phase === "paused"));
    updateActionChips();
}

/** 刷新由 JS 动态生成的文案；静态 data-i18n 文案由 i18n 模块负责。 */
function refreshLocalizedUi() {
    if (saveBtn) saveBtn.innerHTML = `${t("editor.save")} ${renderComboHTML("Ctrl+S")}`;
    actions.renderMenu(moreMenuEl);
    bindTransformControls();
    if (chipLocateEl) chipLocateEl.textContent = t("editor.voice.locate");
    updateViewSwitchState();
    updateTargetDisplay();
    updateDirtyDot();
    updateVoiceUi();
}

/** 动作条可见性（§3.8 按上下文出现的整理动作）：
 * 录音结束且有本次追加 → 定位/整理听写；AI 可用且有选区 → 整理选中。 */
function updateActionChips() {
    if (!voiceChipsEl) return;
    const dictationReady = voice.phase === "idle" && voice.segments.length > 0;
    const selectionText = session.isActive ? adapter.getSelectionText() : "";
    const selectionReady = transform.aiAvailable && !!selectionText.trim()
        && !transform.isBusy && !transform.candidate;
    const show = dictationReady || selectionReady;
    voiceChipsEl.classList.toggle("hidden", !show);
    if (!show) return;

    if (chipLocateEl) chipLocateEl.classList.toggle("hidden", !dictationReady);
    if (chipTidyEl) {
        chipTidyEl.classList.toggle("hidden", !dictationReady || !transform.aiAvailable);
        if (dictationReady) chipTidyEl.textContent = t("editor.voice.tidy");
    }
    if (chipTidySelectionEl) {
        chipTidySelectionEl.classList.toggle("hidden", !selectionReady);
        if (selectionReady) {
            chipTidySelectionEl.textContent = t("editor.transform.tidySelection", {
                count: selectionText.length,
            });
        }
    }
    if (chipDismissEl) chipDismissEl.classList.toggle("hidden", !dictationReady);
}

function hideVoiceChips() {
    voice.clearRunResult();
    updateActionChips();
}

/** 视图切换（Source/MD 是同一文本的双视图，§3.3）。
 *  切换取消运行中的整理请求并丢弃候选（§3.3/§3.7），引擎内 handle 随旧引擎作废；
 *  拒绝原因提示由 adapter.onNotice 给出（structure/large 分档）。
 *  听写进行中时在新引擎上重新记录本轮追加锚点（已入正文的旧段不再纳入
 *  定位/整理范围）；非听写中时本次范围随旧引擎作废，chips 同步隐藏。 */
function switchView(target) {
    if (!adapter.switchView(target)) return;
    void transform.cancel({silent: true});
    if (voice.isBusy) {
        adapter.beginDictationRun();
    } else {
        voice.invalidateRun();
    }
    updateViewSwitchState();
    updateActionChips();
    adapter.focus();
}

function updateViewSwitchState() {
    if (!viewSourceBtn || !viewMdBtn) return;
    const view = adapter.view;
    viewSourceBtn.classList.toggle("is-active", view === "source");
    viewMdBtn.classList.toggle("is-active", view === "markdown");
    viewSourceBtn.setAttribute("aria-selected", String(view === "source"));
    viewMdBtn.setAttribute("aria-selected", String(view === "markdown"));
    viewSourceBtn.tabIndex = view === "source" ? 0 : -1;
    viewMdBtn.tabIndex = view === "markdown" ? 0 : -1;
    textareaEl?.setAttribute("aria-hidden", String(view !== "source"));
    mdContainerEl?.setAttribute("aria-hidden", String(view !== "markdown"));
    const mdAllowed = adapter.markdownPolicy !== "disabled"
        && (!adapter.gate || adapter.gate.allowed);
    viewMdBtn.disabled = !mdAllowed;
    if (!mdAllowed) {
        viewMdBtn.title = t("editor.gate.rejected");
    } else if (adapter.gate?.sizeWarn) {
        viewMdBtn.title = t("editor.gate.slow");
    } else {
        viewMdBtn.title = "";
    }
}

function setStatus(message) {
    statusEl.textContent = message ?? "";
}

/** 主保存区真实去向（快照/提交结果驱动，§3.5） */
function updateTargetDisplay() {
    actions.renderTarget(targetLabelEl, targetIconUseEl);
    if (targetZoneEl) {
        const target = session.target;
        targetZoneEl.title = target?.kind === "confirmed_file" ? (target.path ?? "") : "";
    }
}

/** 脏状态点（保存成功/外部同步/清空后收敛为隐藏） */
function updateDirtyDot() {
    if (!dirtyDotEl) return;
    const dirty = session.isActive && adapter.isDirty();
    dirtyDotEl.classList.toggle("hidden", !dirty);
    if (dirty) dirtyDotEl.title = t("editor.dirty");
}

/** 保存错误统一上报：冲突转专用处理，stale_revision 分类展示（§5.7 ⑤），
 * 其余显示状态行 */
function reportSaveError(error) {
    if (!error) return;
    if (error.code === "source_conflict") {
        handleSaveConflict();
        return;
    }
    if (error.code === "stale_revision") {
        setStatus(t("editor.staleRevision"));
        return;
    }
    setStatus(t("editor.saveFailed", {message: error.message ?? ""}));
}

// ── 保存 ──────────────────────────────────────────────

/**
 * 保存（§3.5：保存/Ctrl+S 只保存不关闭；关闭走 handleCancel 三态）。
 * MD 首次编辑保存时提示"已按编辑器规范重写"（§3.10）。
 */
async function handleSave() {
    if (!session.isActive) return;
    saveBtn.disabled = true;

    const rewrittenHint = adapter.isNormalizedMd() ? t("editor.rewritten") : null;
    const result = await session.commit();
    saveBtn.disabled = false;

    if (result.ok) {
        updateDirtyDot();
        updateTargetDisplay();
        setStatus(rewrittenHint || t("editor.saved"));
        return;
    }
    reportSaveError(result.error);
}

/**
 * 保存冲突处理（0.23.2，§5.3）：按目标分派三选。
 * - 便签：复制当前内容 / 放弃修改并重新载入 / 取消（DB 为真源，不提供覆盖）；
 * - 已确认文件：仍要覆盖 / 另存为副本 / 取消。
 * 冲突与取消均不改变正文基线、dirty 或主目标。
 */
async function handleSaveConflict() {
    if (session.target?.kind === "confirmed_file") {
        return handleFileConflict();
    }
    return handleStickyConflict();
}

/** 便签冲突三选（§5.3 冻结交互） */
async function handleStickyConflict() {
    const choice = await choiceDialog(t("editor.conflict"), {
        kind: "warning",
        okLabel: t("editor.conflict.copy"),
        cancelLabel: t("editor.conflict.cancel"),
        thirdAction: {label: t("editor.conflict.reload")},
    });
    if (choice === true) {
        // 复制当前内容：保住用户编辑，由用户自行决定去向
        try {
            await copyToClipboard(adapter.getText());
            setStatus(t("editor.copied"));
        } catch (e) {
            console.error("[content-editor] 冲突复制失败:", e);
            setStatus(t("editor.actionFailed", {message: String(e)}));
        }
        return;
    }
    if (choice === "third") {
        // 放弃修改并重新载入：DB 为真源，revision 基线一并前移
        await reloadFromSticky({force: true});
        setStatus(t("editor.conflict.reloaded"));
    }
    // false（取消/Esc）→ 继续编辑，保持现状
}

/** 已确认文件冲突三选（§5.3：外部修改不被静默覆盖） */
async function handleFileConflict() {
    const choice = await choiceDialog(t("editor.fileConflict"), {
        kind: "warning",
        okLabel: t("editor.conflict.overwrite"),
        cancelLabel: t("editor.conflict.cancel"),
        thirdAction: {label: t("editor.conflict.saveCopy")},
    });
    if (choice === true) {
        // 用户显式覆盖：跳过 identity 校验原位写入
        const result = await session.commit({kind: "overwrite_confirmed_file"});
        if (result.ok) {
            updateDirtyDot();
            updateTargetDisplay();
            setStatus(t("editor.saved"));
        } else {
            reportSaveError(result.error);
        }
        return;
    }
    if (choice === "third") {
        await actions.saveCopy();
    }
}

// ── 关闭 / 取消 ──────────────────────────────────────

/**
 * 0.22.11 三态关闭（0.23.1 接入会话 end）：
 * 保存并关闭（commit + end(saved)）/ 放弃更改（end(abandoned)）/ 继续编辑。
 */
async function handleCancel() {
    // 0.23.3：关闭前停听写（confirmed 已保留；未定稿 preview 按 §3.6 丢弃）
    if (voice.isBusy) await voice.stop();
    if (session.isActive && adapter.isDirty()) {
        const choice = await choiceDialog(t("editor.unsavedWarning"), {
            kind: "warning",
            okLabel: t("editor.discard"),
            cancelLabel: t("editor.continueEdit"),
            thirdAction: {label: t("editor.saveAndClose")},
        });
        if (choice === "cancel") return; // 继续编辑：整理请求/候选保持不动
        // 0.23.4：真正关闭才取消整理请求/丢弃候选（§3.7 关闭取消旧请求）
        await transform.cancel({silent: true});
        if (choice === "third") {
            allowClose = true;
            const result = await session.commit();
            // commit 失败（冲突等）：留在编辑器，正文不丢
            if (!result.ok) {
                allowClose = false;
                reportSaveError(result.error);
                return;
            }
            const ended = await session.end("saved");
            if (!ended.ok) {
                allowClose = false;
                reportEndError(ended.error);
                return;
            }
            closeWindow();
            return;
        }
        // "ok" → 放弃更改
        allowClose = true;
        const ended = await session.end("abandoned");
        if (!ended.ok) {
            allowClose = false;
            reportEndError(ended.error);
            return;
        }
        closeWindow();
        return;
    }
    allowClose = true;
    await transform.cancel({silent: true});
    if (session.isActive) {
        const ended = await session.end("abandoned");
        if (!ended.ok) {
            allowClose = false;
            reportEndError(ended.error);
            return;
        }
    }
    closeWindow();
}

function reportEndError(error) {
    setStatus(t("editor.closeFailed", {message: error?.message ?? ""}));
}

/**
 * 生命周期强制关闭（来源便签被回收/删除/隐藏）——不弹确认，正文丢弃
 * （沿用 0.18.3 语义；会话 end 释放便签租约）。
 */
async function lifecycleClose(reason) {
    if (!session.isActive) {
        closeWindow();
        return;
    }
    tracing(`${reason}，自动关闭编辑器`);
    if (voice.isBusy) await voice.stop();
    await transform.cancel({silent: true});
    allowClose = true;
    const ended = await session.end("abandoned");
    if (ended.ok) {
        closeWindow();
    } else {
        allowClose = false;
        reportEndError(ended.error);
    }
}

/** 关闭窗口（hide 复用模式；会话已 end，窗口回预热态） */
function closeWindow() {
    const win = getCurrentWindow();
    if (win) {
        win.hide();
    }
    // 预热窗口会复用；放行标记只属于本次关闭，不能泄漏到下一会话。
    allowClose = false;
}

// ── 窗口控制 ──────────────────────────────────────────

function bindWindowControls() {
    const minBtn = document.getElementById("titlebar-minimize");
    const maxBtn = document.getElementById("titlebar-maximize");
    const closeBtn = document.getElementById("titlebar-close");

    if (minBtn) {
        minBtn.addEventListener("click", () => {
            getCurrentWindow()?.minimize();
        });
    }

    if (maxBtn) {
        maxBtn.addEventListener("click", async () => {
            const win = getCurrentWindow();
            if (!win) return;
            const isMax = await win.isMaximized();
            if (isMax) {
                await win.unmaximize();
            } else {
                await win.maximize();
            }
        });
    }

    if (closeBtn) {
        closeBtn.addEventListener("click", handleCancel);
    }

    // 系统级关闭请求（Alt+F4 等）
    const win = getCurrentWindow();
    if (win?.onCloseRequested) {
        win.onCloseRequested(async (event) => {
            event.preventDefault(); // 始终阻止销毁
            if (voice.isBusy) await voice.stop();
            if (allowClose || !session.isActive || !adapter.isDirty()) {
                if (session.isActive) {
                    const ended = await session.end("abandoned");
                    if (!ended.ok) {
                        allowClose = false;
                        reportEndError(ended.error);
                        return;
                    }
                }
                closeWindow();
            } else {
                handleCancel();
            }
        });
    }
}

// ── 键盘快捷键 ────────────────────────────────────────

function bindKeyboard() {
    document.addEventListener("keydown", (e) => {
        // 自绘弹窗打开时交给弹窗内部键盘逻辑（0.22.11 与 settings 一致）
        if (document.querySelector(".modal-overlay")) return;

        if ((e.ctrlKey || e.metaKey) && e.key === "s") {
            e.preventDefault();
            handleSave();
            return;
        }

        if (e.key === "Escape") {
            e.preventDefault();
            // 更多菜单打开时先关菜单，不触发关闭流程
            if (moreMenuEl && !moreMenuEl.classList.contains("hidden")) {
                actions.closeMenu({restoreFocus: true});
                return;
            }
            // 0.23.3 ESC 分层：听写中先停听写（confirmed 保留），不关窗口
            if (voice.isBusy) {
                voice.stop();
                return;
            }
            // 0.23.4 ESC 分层：候选卡打开时先关卡（正文不动），不关窗口
            if (transform.candidate || transform.isBusy) {
                void transform.cancel();
                return;
            }
            handleCancel();
        }
    });
}

// ── 用户退出确认 ──────────────────────────────────────

/** 退出确认对话框去重（§3.5：一次退出只显示一次汇总确认） */
let exitDialogOpen = false;

/**
 * 退出确认请求处理：无会话或 clean 直接放行（无数据丢失）；
 * dirty 时显示一次汇总确认，用户选择后应答后端。
 * 超时兜底在后端（10s 无应答放弃本次退出）。
 */
async function handleExitRequest(payload) {
    const requestId = payload?.requestId;
    if (!requestId) return;
    if (!session.isActive || !adapter.isDirty()) {
        await resolveEditorExit({requestId, confirmed: true}).catch(() => {});
        return;
    }
    if (exitDialogOpen) return; // 已有一次确认在展示，忽略重复请求
    exitDialogOpen = true;
    let confirmed = false;
    try {
        const choice = await choiceDialog(t("editor.exitConfirm"), {
            kind: "warning",
            okLabel: t("editor.exitConfirmQuit"),
            cancelLabel: t("editor.exitConfirmCancel"),
        });
        confirmed = choice === true;
    } finally {
        exitDialogOpen = false;
    }
    await resolveEditorExit({requestId, confirmed}).catch((e) => {
        const err = normalizeError(e);
        console.error(`[content-editor] 退出确认应答失败 [${err.code}]: ${err.message}`);
    });
}

// ── 工具 ──────────────────────────────────────────────

/** 简易日志（绕过 frontendLog，直接 console） */
function tracing(msg) {
    console.log(`[content-editor] ${msg}`);
}

// ── 启动 ──────────────────────────────────────────────

init().catch((e) => console.error("[content-editor] init 失败:", e));
