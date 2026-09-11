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
import {applyI18nFromConfig, t} from "../i18n/index.js";
import {ensureSpriteLoaded} from "../shared/icon.js";
import {choiceDialog, getCurrentWindow, listen, normalizeError} from "../shared/tauri.js";
import {EVENTS} from "../shared/event-names.js";
import {
    commitEditorSession,
    endEditorSession,
    getEditorSession,
    getStickyNote,
} from "../shared/api.js";
import {EditorAdapter} from "./editor-adapter.js";
import {EditorSession} from "./editor-session.js";

// ── DOM 引用 ──────────────────────────────────────────

const titleEl = document.getElementById("editor-title");
const textareaEl = document.getElementById("editor-textarea");
const mdContainerEl = document.getElementById("editor-tiptap-container");
const mdToolbarEl = document.getElementById("md-toolbar");
const saveBtn = document.getElementById("btn-save");
const cancelBtn = document.getElementById("btn-cancel");
const statusEl = document.getElementById("editor-status");
const viewSourceBtn = document.getElementById("btn-view-source");
const viewMdBtn = document.getElementById("btn-view-md");

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
        onSnapshotApplied: () => {
            updateViewSwitchState();
        },
        onSessionCleared: () => {
            updateViewSwitchState();
        },
    },
);

// ── 初始化 ────────────────────────────────────────────

async function init() {
    // 主题 + i18n + 图标
    await ensureSpriteLoaded();
    await applyThemeFromConfig();
    await applyI18nFromConfig();

    bindToolbar();
    bindWindowControls();
    bindKeyboard();
    bindBackendEvents();

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
}

/** 便签来源且本地 clean 时同步最新内容（保留 0.18.3 行为语义） */
async function reloadFromSticky() {
    const stickyId = originStickyId();
    if (!stickyId || !session.isActive) return;
    if (adapter.isDirty()) return; // 有未保存改动时不打断用户
    try {
        const note = await getStickyNote(stickyId);
        if (!note || stickyId !== originStickyId()) return; // 会话已切换，丢弃旧结果
        session.syncExternalContent(note.content || "");
    } catch (e) {
        const err = normalizeError(e);
        console.error(`[content-editor] 便签同步失败 [${err.code}]: ${err.message}`);
    }
}

// ── 工具栏 ────────────────────────────────────────────

function bindToolbar() {
    saveBtn.addEventListener("click", handleSave);
    cancelBtn.addEventListener("click", handleCancel);

    viewSourceBtn?.addEventListener("click", () => switchView("source"));
    viewMdBtn?.addEventListener("click", () => switchView("markdown"));
}

/** 视图切换（Source/MD 是同一文本的双视图，§3.3）。
 *  拒绝原因提示由 adapter.onNotice 给出（structure/large 分档）。 */
function switchView(target) {
    if (!adapter.switchView(target)) return;
    updateViewSwitchState();
    adapter.focus();
}

function updateViewSwitchState() {
    if (!viewSourceBtn || !viewMdBtn) return;
    const view = adapter.view;
    viewSourceBtn.classList.toggle("is-active", view === "source");
    viewMdBtn.classList.toggle("is-active", view === "markdown");
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
        setStatus(rewrittenHint || t("editor.saved"));
        return;
    }
    if (result.error) {
        if (result.error.code === "source_conflict") {
            setStatus(t("editor.conflict"));
        } else {
            setStatus(t("editor.saveFailed", {message: result.error.message}));
        }
    }
}

// ── 关闭 / 取消 ──────────────────────────────────────

/**
 * 0.22.11 三态关闭（0.23.1 接入会话 end）：
 * 保存并关闭（commit + end(saved)）/ 放弃更改（end(abandoned)）/ 继续编辑。
 */
async function handleCancel() {
    if (session.isActive && adapter.isDirty()) {
        const choice = await choiceDialog(t("editor.unsavedWarning"), {
            kind: "warning",
            okLabel: t("editor.discard"),
            cancelLabel: t("editor.continueEdit"),
            thirdAction: {label: t("editor.saveAndClose")},
        });
        if (choice === "cancel") return; // 继续编辑
        if (choice === "third") {
            allowClose = true;
            const result = await session.commit();
            // commit 失败（冲突等）：留在编辑器，正文不丢
            if (!result.ok) {
                allowClose = false;
                setStatus(
                    result.error?.code === "source_conflict"
                        ? t("editor.conflict")
                        : t("editor.saveFailed", {message: result.error?.message ?? ""}),
                );
                return;
            }
            await session.end("saved");
            closeWindow();
            return;
        }
        // "ok" → 放弃更改
        allowClose = true;
        await session.end("abandoned");
        closeWindow();
        return;
    }
    allowClose = true;
    if (session.isActive) await session.end("abandoned");
    closeWindow();
}

/**
 * 生命周期强制关闭（来源便签被回收/删除/隐藏）——不弹确认，正文丢弃
 * （沿用 0.18.3 语义；会话 end 释放便签租约）。
 */
function lifecycleClose(reason) {
    if (!session.isActive) {
        closeWindow();
        return;
    }
    tracing(`${reason}，自动关闭编辑器`);
    allowClose = true;
    session.end("abandoned").finally(() => closeWindow());
}

/** 关闭窗口（hide 复用模式；会话已 end，窗口回预热态） */
function closeWindow() {
    const win = getCurrentWindow();
    if (win) {
        win.hide();
    }
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
            if (allowClose || !session.isActive || !adapter.isDirty()) {
                if (session.isActive) await session.end("abandoned");
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
            handleCancel();
        }
    });
}

// ── 工具 ──────────────────────────────────────────────

/** 简易日志（绕过 frontendLog，直接 console） */
function tracing(msg) {
    console.log(`[content-editor] ${msg}`);
}

// ── 启动 ──────────────────────────────────────────────

init().catch((e) => console.error("[content-editor] init 失败:", e));
