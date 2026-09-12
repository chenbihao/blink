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
import {choiceDialog, confirmDialog, getCurrentWindow, listen, normalizeError} from "../shared/tauri.js";
import {renderComboHTML} from "../shared/kbd.js";
import {EVENTS} from "../shared/event-names.js";
import {
    cancelEditorTransform,
    clearEditorDraft,
    archiveEditorDraft,
    commitEditorSession,
    copyToClipboard,
    endEditorSession,
    getChatStatus,
    getEditorSession,
    getEditorVoiceSnapshot,
    getStickyNote,
    listEditorDrafts,
    loadEditorDraft,
    orphanEditorDraft,
    resolveEditorExit,
    saveEditorDraft,
    startEditorTransform,
    startEditorVoice,
    stopEditorVoice,
} from "../shared/api.js";
import {EditorAdapter} from "./editor-adapter.js";
import {EditorSession} from "./editor-session.js";
import {EditorActions, fileConflictAction, stickyConflictAction} from "./actions.js";
import {EditorVoiceController} from "./voice.js";
import {EditorTransformController} from "./transform.js";
import {
    RecoveryDraft,
    draftKeyForSource,
    recoveryFenceMatches,
    recoverySourceMatches,
} from "./recovery-draft.js";
import {showRecoveryConflictDialog} from "./recovery-dialog.js";
import {RecoveryCandidates, RecoveryConflictResolver} from "./recovery-candidates.js";
import {EditorExit, EditorLifecycle} from "./editor-lifecycle.js";
import {fnv1a64Hex} from "./canonical-buffer.js";

// ── DOM 引用 ──────────────────────────────────────────

const titleEl = document.getElementById("editor-title");
const sourceEl = document.getElementById("editor-source");
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
const readOnlyBadgeEl = document.getElementById("md-readonly-badge");
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

function activeDraftKey() {
    return session.draftKey || draftKeyForSource(session.source);
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
            // 实时恢复草稿：防抖落盘（大文档 MD 不做高频整篇物化，只等边界 flush）
            draft.schedule({defer: adapter.isHeavyMd()});
        },
        onSelectionChanged: () => updateActionChips(),
        onMdModeChanged: () => updateViewSwitchState(),
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
            updateSourceDisplay();
            // 会话绑定：冻结草稿身份并尝试恢复崩溃残留草稿
            void bindRecoveryDraft();
        },
        onSessionCleared: () => {
            voice.handleSessionEnded();
            void transform.cancel({silent: true});
            draft.unbind();
            updateViewSwitchState();
            updateDirtyDot();
            updateTargetDisplay();
            updateSourceDisplay();
            updateActionChips();
        },
    },
);

// ── 实时恢复草稿（与 Ctrl+S 分离的持久化通道）──────────────

/**
 * 草稿控制器：只走草稿专用 IPC（绝不触发文件/剪贴板/便签等保存目标副作用）。
 * getText 会触发 MD 延迟物化，因此大文档 MD 走 defer（只在关键边界 flush）。
 */
const draft = new RecoveryDraft({
    api: {saveEditorDraft, loadEditorDraft, clearEditorDraft, orphanEditorDraft, archiveEditorDraft},
    getIdentity: () => (session.isActive
        ? {sessionRef: session.sessionRef, generation: session.generation}
        : null),
    getText: () => adapter.getText(),
    getRevision: () => adapter.revision,
    getCheckpoint: () => adapter.checkpointText,
    getSourceRevision: () => session.sourceRevision,
    isDirty: () => adapter.isDirty(),
    hashOf: fnv1a64Hex,
    onError: (error) => {
        // 旧 revision / 旧会话被拒是预期结果（更新的一次写入已覆盖），只记日志
        if (error.code === "stale_revision" || error.code === "stale_session") {
            console.warn(`[content-editor] 草稿写入被拒（已有更新正文）[${error.code}]`);
            return;
        }
        console.error(`[content-editor] 草稿保存失败 [${error.code}]: ${error.message}`);
        setStatus(t("editor.draft.failed", {message: error.message ?? error.code}));
    },
});

/**
 * 会话绑定后：冻结草稿身份，并尝试恢复上次异常退出残留的草稿。
 * 恢复只改变工作正文（checkpoint 不动）→ 会话保持 dirty，等待用户显式保存或放弃；
 * 异步读取期间会话被替换时丢弃回流（身份墙）。
 */
async function bindRecoveryDraft() {
    if (!session.isActive) return;
    const key = activeDraftKey();
    const identity = {
        sessionRef: session.sessionRef,
        generation: session.generation,
        key,
        sourceInstanceId: session.sourceInstanceId,
    };
    draft.bind(identity);
    if (!key) return;

    // Freeze every observable boundary before the disk read.  The editor stays
    // focusable; only the eventual recovery decision is gated by this fence.
    const frozen = currentRecoveryFence();
    const restored = await draft.restore({
        key,
        authoritativeBody: frozen.body,
        authoritativeDigest: frozen.bodyHash,
        authoritativeRevision: frozen.sourceRevision,
    });
    if (!restored) return;
    const current = currentRecoveryFence();
    if (!recoverySourceMatches(frozen, current)) {
        // Session/source/window changes make the response unrelated; do not
        // even present it in the new session.
        return;
    }
    if (!recoveryFenceMatches(frozen, current)) {
        // User input or an external source refresh raced the disk read.  Keep
        // the current body and offer the late draft as an explicit candidate;
        // only the user's choice may replace the current text.  The dialog's
        // wall is the post-read state (the divergence is the reason we ask).
        await conflictResolver.resolve(current, restored);
        return;
    }
    if (restored.status === "same") {
        // The authoritative source already contains the draft; clear only the
        // exact loaded identity, never by key alone.
        await draft.discardCandidate(restored.identity);
        return;
    }
    if (restored.status === "conflict") {
        await conflictResolver.resolve(frozen, restored);
        return;
    }
    if (!adapter.restoreDraft(restored.text)) return;
    updateDirtyDot();
    updateViewSwitchState();
    setStatus(t("editor.draft.restored"));
}

/** 当前完整恢复围栏（session/generation/来源实例/revision/正文）。 */
function currentRecoveryFence() {
    const body = session.isActive ? adapter.getText() : "";
    return {
        sessionActive: session.isActive,
        sessionRef: session.sessionRef,
        generation: session.generation,
        key: activeDraftKey(),
        sourceInstanceId: session.sourceInstanceId,
        sourceRevision: session.sourceRevision,
        adapterRevision: adapter.revision,
        body,
        bodyHash: fnv1a64Hex(body),
    };
}

/** 恢复草稿正文进入编辑器（resolver 回调：替换工作正文 + 刷新投影）。 */
function restoreDraftIntoEditor(text) {
    const ok = adapter.restoreDraft(text);
    if (ok) {
        updateDirtyDot();
        updateViewSwitchState();
    }
    return ok;
}

/**
 * 同键恢复冲突的四动作解析（0.23.6）：
 * - keep    精确清理冻结旧候选 + 立即 flush 当前正文；
 * - restore 恢复草稿到编辑器；
 * - copy    显式复制候选正文（失败保留候选）→ 清理旧候选 + flush 当前；
 * - dismiss 关闭/Esc/遮罩/异常 = 暂不处理：无剪贴板写入、无清理、正文不动。
 */
const conflictResolver = new RecoveryConflictResolver({
    api: {copyToClipboard},
    draft,
    getFenceState: currentRecoveryFence,
    restoreIntoEditor: restoreDraftIntoEditor,
    showDialog: ({untrusted}) => showRecoveryConflictDialog({
        message: t(untrusted ? "editor.draft.untrusted" : "editor.draft.conflict"),
        kind: "warning",
        labels: {
            keepCurrent: t("editor.draft.keepCurrent"),
            restore: t("editor.draft.restore"),
            copy: t("editor.draft.copy"),
            dismiss: t("editor.draft.dismiss"),
        },
    }),
    onStatus: (message) => setStatus(message),
    t,
});

/** "更多 → 恢复未保存草稿…"：临时来源草稿崩溃后的人工找回入口。 */
const recoveryCandidates = new RecoveryCandidates({
    api: {listEditorDrafts, clearEditorDraft, copyToClipboard},
    getFenceState: currentRecoveryFence,
    isDirty: () => adapter.isDirty(),
    flushVerified: () => draft.flushVerified(),
    restoreIntoEditor: restoreDraftIntoEditor,
    confirmDialog,
    onStatus: (message) => setStatus(message),
    t,
});

const actions = new EditorActions({
    session,
    adapter,
    callbacks: {
        onStatus: (message) => setStatus(message),
        onError: (error) => reportSaveError(error),
        onTargetSaved: () => updateTargetDisplay(),
        // 任一提交成功（保存到/另存副本/覆盖）：提交水位对应的草稿可清理
        onCommitted: () => {
            void draft.discardCurrent();
            updateDirtyDot();
        },
        // "更多 → 恢复未保存草稿…"：打开候选列表对话框
        onRecoverDraft: () => {
            void recoveryCandidates.open();
        },
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
        // 只读 MD 预览：听写（富文本编辑的一种）同样被禁止
        case "readonly":
            return t("editor.md.readOnly");
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
                if (focused) {
                    voice.resyncIfActive().catch(() => {});
                } else {
                    // 关键边界强制 flush：失焦后可能长期不回来，先落草稿
                    void draft.flush();
                }
            });
        } catch (e) {
            console.error("[content-editor] show window 失败:", e);
        }
    }

    bindDraftBoundaries();
    tracing("editor window: init 完成");
}

/** 恢复草稿的关键边界（隐藏 / 失焦 / 页面卸载）强制 flush */
function bindDraftBoundaries() {
    document.addEventListener("visibilitychange", () => {
        if (document.hidden) void draft.flush();
    });
    window.addEventListener("blur", () => {
        void draft.flush();
    });
    // 窗口被隐藏/卸载前最后一次落草稿（进程异常退出时这里是最后一道防线）
    window.addEventListener("beforeunload", () => {
        void draft.flush();
    });
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
    // 运行/候选卡的动态文案（等待态标签、取消/放弃按钮）也随语言刷新
    transform.refreshCard();
    if (chipLocateEl) chipLocateEl.textContent = t("editor.voice.locate");
    updateViewSwitchState();
    updateTargetDisplay();
    updateDirtyDot();
    updateSourceDisplay();
    updateVoiceUi();
}

/** 动作条可见性（§3.8 按上下文出现的整理动作）：
 * 录音结束且有本次追加 → 定位/整理听写；AI 可用且有选区 → 整理选中。
 * 0.23.7：整理请求运行中/候选卡打开时隐藏 chips——候选卡与 chips 共用
 * footer 上方同一锚点，避免两层浮层重叠；候选卡自身带取消/关闭入口。 */
function updateActionChips() {
    if (!voiceChipsEl) return;
    const transformIdle = !transform.isBusy && !transform.candidate;
    const dictationReady = voice.phase === "idle" && voice.segments.length > 0 && transformIdle;
    const selectionText = session.isActive ? adapter.getSelectionText() : "";
    const selectionReady = transform.aiAvailable && !!selectionText.trim() && transformIdle;
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
    // 关键边界强制 flush：切换视图前把当前正文落到恢复草稿
    void draft.flush();
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
    // 无法保证无损编辑的结构仍允许进入 MD（只读预览）；策略禁用与超尺寸时不可达
    const mdReachable = adapter.mdReachable();
    viewMdBtn.disabled = !mdReachable;
    const mdReadOnly = adapter.isMdReadOnly();
    if (!mdReachable) {
        viewMdBtn.title = adapter.markdownPolicy === "disabled"
            ? t("editor.gate.rejected")
            : t("editor.gate.large");
    } else if (mdReadOnly) {
        viewMdBtn.title = t("editor.md.readOnly");
    } else if (adapter.gate?.sizeWarn) {
        viewMdBtn.title = t("editor.gate.slow");
    } else {
        viewMdBtn.title = "";
    }
    if (readOnlyBadgeEl) {
        readOnlyBadgeEl.classList.toggle("hidden", !mdReadOnly);
        readOnlyBadgeEl.title = t("editor.md.readOnly");
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

/** 来源徽标（§3.8：顶部必须清楚回答"正在编辑什么、内容从哪来"）。
 *  来源种类固定五种，未知/无会话时整块隐藏——不占位、不留空白噪声。 */
const SOURCE_LABEL_KEYS = {
    empty: "editor.source.empty",
    clipboard_item: "editor.source.clipboard",
    sticky: "editor.source.sticky",
    selection: "editor.source.selection",
    capability_result: "editor.source.capability",
};

function updateSourceDisplay() {
    if (!sourceEl) return;
    const kind = session.source?.kind;
    const key = kind ? SOURCE_LABEL_KEYS[kind] : null;
    if (!key) {
        sourceEl.textContent = "";
        sourceEl.classList.add("hidden");
        return;
    }
    sourceEl.textContent = t(key);
    sourceEl.classList.remove("hidden");
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
        // 成功提交：正文已成为新基线，恢复草稿不再需要（生命周期收口）
        void draft.discardCurrent();
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

/** 便签冲突三选（§5.3 冻结交互；choiceDialog 字符串契约经映射函数分派） */
async function handleStickyConflict() {
    const choice = await choiceDialog(t("editor.conflict"), {
        kind: "warning",
        okLabel: t("editor.conflict.copy"),
        cancelLabel: t("editor.conflict.cancel"),
        thirdAction: {label: t("editor.conflict.reload")},
    });
    const action = stickyConflictAction(choice);
    if (action === "copy") {
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
    if (action === "reload") {
        // 放弃修改并重新载入：DB 为真源，revision 基线一并前移
        await reloadFromSticky({force: true});
        setStatus(t("editor.conflict.reloaded"));
    }
    // "stay"（取消/Esc/遮罩/异常 fallback）→ 继续编辑，保持现状
}

/** 已确认文件冲突三选（§5.3：外部修改不被静默覆盖） */
async function handleFileConflict() {
    const choice = await choiceDialog(t("editor.fileConflict"), {
        kind: "warning",
        okLabel: t("editor.conflict.overwrite"),
        cancelLabel: t("editor.conflict.cancel"),
        thirdAction: {label: t("editor.conflict.saveCopy")},
    });
    const action = fileConflictAction(choice);
    if (action === "overwrite") {
        // 用户显式覆盖：跳过 identity 校验原位写入
        const result = await session.commit({kind: "overwrite_confirmed_file"});
        if (result.ok) {
            updateDirtyDot();
            updateTargetDisplay();
            setStatus(t("editor.saved"));
            void draft.discardCurrent();
        } else {
            reportSaveError(result.error);
        }
        return;
    }
    if (action === "saveCopy") {
        await actions.saveCopy();
    }
    // "stay"（取消/Esc/遮罩/异常 fallback）→ 继续编辑，保持现状
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
            const ended = await lifecycle.endSession("saved", {clearDraft: true});
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
        const ended = await lifecycle.endSession("abandoned", {clearDraft: true});
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
        const ended = await lifecycle.endSession();
        if (!ended.ok) {
            allowClose = false;
            reportEndError(ended.error);
            return;
        }
    }
    closeWindow();
}

/**
 * 会话结束 + 草稿收尾编排（0.23.6 抽为可测控制器）：
 * - `retainDraft`（来源失效强制关闭）：dirty 正文先 flushVerified 确认落盘、
 *   markOrphaned 显式转存成功，才发起 end；失败即中止，编辑器保持可编辑、
 *   自动保存保持绑定；
 * - 只有 end 成功（或后端明确 stale）才解除草稿绑定；
 * - `clearDraft` / clean 收尾走 discardCurrent / discardIfBodyEquals。
 */
const lifecycle = new EditorLifecycle({
    session,
    adapter,
    draft,
    setEditingLocked: (locked) => adapter.setInteractionLocked(locked),
});

function reportEndError(error) {
    if (error?.code === "draft_flush_failed") {
        setStatus(t("editor.draft.flushFailed"));
        return;
    }
    if (error?.code === "draft_orphan_failed") {
        setStatus(t("editor.draft.orphanFailed"));
        return;
    }
    setStatus(t("editor.closeFailed", {message: error?.message ?? ""}));
}

/**
 * 生命周期强制关闭（来源便签被回收/删除/隐藏）——不弹确认，但未保存
 * 正文必须先确认可靠落盘并转存为 orphan 恢复候选；任何一步失败都留在
 * 编辑器（不静默关闭、不丢正文），等用户手动保存后重试。
 */
async function lifecycleClose(reason) {
    if (!session.isActive) {
        closeWindow();
        return;
    }
    tracing(`${reason}，自动关闭编辑器`);
    if (voice.isBusy) await voice.stop();
    await transform.cancel({silent: true});
    const ended = await lifecycle.endSession("abandoned", {retainDraft: true});
    if (ended.ok) {
        allowClose = true;
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
                    const ended = await lifecycle.endSession("abandoned");
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

/**
 * 退出确认控制器（0.23.6 抽为可测）：无会话/clean 快速放行；dirty 时一次
 * 汇总确认；用户确认后必须 `await flushVerified()` 真正等到最新正文落盘
 * （失败/过期/等待期间继续编辑/会话变化 → confirmed:false 阻止退出并提示）。
 * 超时兜底在后端（10s 无应答放弃本次退出，迟到应答被忽略）。
 */
const exitController = new EditorExit({
    api: {resolveEditorExit},
    getSession: () => session,
    isDirty: () => adapter.isDirty(),
    flushVerified: () => draft.flushVerified(),
    setEditingLocked: (locked) => adapter.setInteractionLocked(locked),
    showDialog: () => choiceDialog(t("editor.exitConfirm"), {
        kind: "warning",
        okLabel: t("editor.exitConfirmQuit"),
        cancelLabel: t("editor.exitConfirmCancel"),
    }),
    onStatus: (message) => setStatus(message),
    t,
});

function handleExitRequest(payload) {
    void exitController.handleRequest(payload);
}

// ── 工具 ──────────────────────────────────────────────

/** 简易日志（绕过 frontendLog，直接 console） */
function tracing(msg) {
    console.log(`[content-editor] ${msg}`);
}

// ── 启动 ──────────────────────────────────────────────

init().catch((e) => console.error("[content-editor] init 失败:", e));
