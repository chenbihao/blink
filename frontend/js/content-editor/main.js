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
import {renderComboHTML} from "../shared/kbd.js";
import {EVENTS} from "../shared/event-names.js";
import {
    commitEditorSession,
    copyToClipboard,
    endEditorSession,
    getEditorSession,
    getStickyNote,
    resolveEditorExit,
} from "../shared/api.js";
import {EditorAdapter} from "./editor-adapter.js";
import {EditorSession} from "./editor-session.js";
import {EditorActions} from "./actions.js";

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
        onContentChanged: () => updateDirtyDot(),
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
            updateViewSwitchState();
            updateDirtyDot();
            updateTargetDisplay();
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

    // 用户主动退出的一次汇总确认（§3.5，0.23.2 请求-应答协议）
    listen(EVENTS.EDITOR_EXIT_REQUEST, (event) => {
        handleExitRequest(event.payload);
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

/** 便签来源且本地 clean 时同步最新内容（保留 0.18.3 行为语义）；
 *  force = 冲突后用户显式放弃修改的重载（跳过 dirty 保护） */
async function reloadFromSticky({force = false} = {}) {
    const stickyId = originStickyId();
    if (!stickyId || !session.isActive) return;
    if (!force && adapter.isDirty()) return; // 有未保存改动时不打断用户
    try {
        const note = await getStickyNote(stickyId);
        if (!note || stickyId !== originStickyId()) return; // 会话已切换，丢弃旧结果
        session.syncExternalContent(note.content || "", note.updatedAt ?? null);
        updateDirtyDot();
    } catch (e) {
        const err = normalizeError(e);
        console.error(`[content-editor] 便签同步失败 [${err.code}]: ${err.message}`);
    }
}

// ── 工具栏 ────────────────────────────────────────────

function bindToolbar() {
    // 0.23.2：保存按钮带 Ctrl+S 键位提示（spec-frontend §4.1 kbd.js 统一渲染）
    saveBtn.innerHTML = `${t("editor.save")} ${renderComboHTML("Ctrl+S")}`;
    saveBtn.addEventListener("click", handleSave);

    // 更多动作菜单（保存到/另存副本/复制/创建便签/发送到 AI 对话）
    actions.renderMenu(moreMenuEl);
    moreBtn?.addEventListener("click", () => actions.toggleMenu());
    actions.bindMenuHover(menuAnchorEl);
    updateTargetDisplay();
    updateDirtyDot();

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

/** 保存错误统一上报：冲突转专用处理，其余显示状态行 */
function reportSaveError(error) {
    if (!error) return;
    if (error.code === "source_conflict") {
        handleSaveConflict();
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
                reportSaveError(result.error);
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
            // 更多菜单打开时先关菜单，不触发关闭流程
            if (moreMenuEl && !moreMenuEl.classList.contains("hidden")) {
                actions.closeMenu();
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
