/**
 * 内容编辑器窗口入口（0.16.3）。
 *
 * 装配主题 / i18n / 图标 sprite，从后端拉取 payload，初始化编辑/保存逻辑。
 * 编辑逻辑全在前端，后端只做窗口创建 + 剪贴板读写桥接。
 *
 * 0.18.3：移除编辑/预览模式切换（死代码），统一使用 Tiptap IR 编辑。
 *         MD 工具栏移入 editor-toolbar-left，默认展示。
 */

import {applyThemeFromConfig} from "../shared/theme.js";
import {applyI18nFromConfig, t} from "../i18n/index.js";
import {ensureSpriteLoaded} from "../shared/icon.js";
import {choiceDialog, getCurrentWindow, listen} from "../shared/tauri.js";
import {EVENTS} from "../shared/event-names.js";
import {getContentEditorPayload, getStickyNote, saveContentEditor} from "../shared/api.js";
import {bindMdToolbar, createMdToolbar, updateToolbarStates} from "../shared/md-toolbar.js";

// ── 状态 ──────────────────────────────────────────────

/** 原始 payload（用于对比是否有未保存改动） */
let originalBody = "";

/** 当前 originRef（保存时传给后端继承 hit_count） */
let originRef = null;

/** 当前 savePolicy（0.16.9：clipboard_new | sticky_update） */
let savePolicy = "clipboard_new";

/** 当前内容格式：plain | markdown */
let currentFormat = "plain";

/** 当前来源：clipboard | sticky | query */
let currentOrigin = "";

/** Tiptap IR 编辑器实例（null = 纯文本模式） */
let tiptapEditor = null;

/** 防止重复保存 */
let saving = false;

/** 已确认关闭——跳过 onCloseRequested 拦截 */
let allowClose = false;

/** 0.18.3: 已废弃——保存改为立即隐藏。保留变量供生命周期处理器安全清理 */
let closeTimeout = null;

/** 0.18.3: 编辑会话代际计数器。
 *
 * 每次 loadPayload 或生命周期关闭时递增。handleSave 捕获当前 gen 作为 savedGen，
 * 后台保存完成后对比——不匹配说明窗口已被复用或被生命周期关闭，
 * 回调不再触碰模块级状态（避免污染新会话或错误地重新显示窗口）。
 */
let sessionGen = 0;

// ── DOM 引用 ──────────────────────────────────────────

const titleEl = document.getElementById("editor-title");
const textareaEl = document.getElementById("editor-textarea");
const mdContainerEl = document.getElementById("editor-tiptap-container");
const mdToolbarEl = document.getElementById("md-toolbar");
const saveBtn = document.getElementById("btn-save");
const cancelBtn = document.getElementById("btn-cancel");
const statusEl = document.getElementById("editor-status");

// ── 初始化 ────────────────────────────────────────────

async function init() {
    // 主题 + i18n + 图标
    await ensureSpriteLoaded();
    await applyThemeFromConfig();
    await applyI18nFromConfig();

    // 从后端拉取 payload
    await loadPayload();

    // 绑定事件
    bindToolbar();
    bindWindowControls();
    bindKeyboard();

    // 0.18.3 fix: 监听便签内容变更——当编辑器来源是 sticky 且用户无未保存改动时自动刷新
    // 0.18.3 fix: 跳过 source="content-editor" 的变更（自己刚保存的，无需 reload）
    listen(EVENTS.STICKY_CONTENT_CHANGED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === originRef && currentOrigin === "sticky") {
            if (payload.source === "content-editor") return;
            if (!hasUnsavedChanges()) {
                reloadFromSticky();
            }
        }
    });

    // 0.18.3: 生命周期绑定——编辑器从便签打开时，便签关闭/隐藏/删除/回收 时自动关闭编辑器
    // 递增 sessionGen 确保正在进行的后台保存不会在完成后重新显示窗口
    listen(EVENTS.STICKY_TRASHED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === originRef && currentOrigin === "sticky") {
            lifecycleClose("便签被回收");
        }
    });

    listen(EVENTS.STICKY_VISIBILITY_CHANGED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === originRef && currentOrigin === "sticky" && payload.visible === false) {
            lifecycleClose("便签已隐藏");
        }
    });

    listen(EVENTS.STICKY_DELETED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === originRef && currentOrigin === "sticky") {
            lifecycleClose("便签已删除");
        }
    });

    // 注册窗口复用回调（后端 eval 调用）
    window.__contentEditorReload = loadPayload;

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

/**
 * 从后端拉取 payload 并填充编辑器。
 * 窗口首次打开和复用时都会调用。
 */
async function loadPayload() {
    // 0.18.3 fix: 立即清除可能残留的 closeTimeout，防止在 await 期间
    // 旧计时器触发 closeWindow() 导致窗口被隐藏（"一次失败一次成功"竞态）
    if (closeTimeout) {
        clearTimeout(closeTimeout);
        closeTimeout = null;
    }

    // 0.18.3: 递增代际计数器，使正在进行的后台保存回调不再操作当前状态
    sessionGen++;

    try {
        const payload = await getContentEditorPayload();
        if (!payload) {
            tracing("loadPayload: 无 payload，可能是窗口已关闭后再次打开");
            return;
        }

        // 0.16.13: 归一化 \r\n → \n
        originalBody = (payload.body || "").replace(/\r\n/g, "\n");
        originRef = payload.originRef || null;
        savePolicy = payload.savePolicy || "clipboard_new";
        currentFormat = payload.format || "plain";
        currentOrigin = payload.origin || "";

        // format=markdown 时使用 Tiptap IR 编辑器
        if (currentFormat === "markdown" && mdContainerEl) {
            textareaEl.hidden = true;
            mdContainerEl.hidden = false;
            if (mdToolbarEl) mdToolbarEl.style.display = "";

            // 创建或复用 Tiptap 编辑器
            if (tiptapEditor) {
                // 复用：设入新内容
                const json = tiptapEditor.markdown.parse(originalBody);
                tiptapEditor.commands.setContent(json, false);
            } else if (window.BlinkTiptap) {
                try {
                    const {Editor, StarterKit, Markdown, TaskList, TaskItem} = window.BlinkTiptap;
                    tiptapEditor = new Editor({
                        element: mdContainerEl,
                        extensions: [StarterKit, Markdown, TaskList, TaskItem],
                        content: originalBody,
                        contentType: "markdown",
                        editorProps: {
                            attributes: {
                                class: "content-editor-tiptap",
                                spellcheck: "false",
                            },
                        },
                    });
                    // 创建并绑定 MD 格式工具栏（与便签共用逻辑）
                    if (mdToolbarEl) {
                        mdToolbarEl.innerHTML = "";
                        const toolbar = createMdToolbar("md-toolbar-inner");
                        mdToolbarEl.appendChild(toolbar);
                        bindMdToolbar(toolbar, tiptapEditor, {editorEl: mdContainerEl});
                        tiptapEditor.on("selectionUpdate", () => updateToolbarStates(toolbar, tiptapEditor));
                        tiptapEditor.on("transaction", () => updateToolbarStates(toolbar, tiptapEditor));
                    }
                    console.log("[content-editor] Tiptap IR 编辑器初始化成功");
                } catch (e) {
                    console.error("[content-editor] Tiptap 初始化失败，降级为纯文本:", e);
                    tiptapEditor = null;
                    textareaEl.hidden = false;
                    mdContainerEl.hidden = true;
                    if (mdToolbarEl) mdToolbarEl.style.display = "none";
                    textareaEl.value = originalBody;
                }
            } else {
                console.warn("[content-editor] BlinkTiptap 未加载，降级为纯文本");
                textareaEl.hidden = false;
                mdContainerEl.hidden = true;
                if (mdToolbarEl) mdToolbarEl.style.display = "none";
                textareaEl.value = originalBody;
            }
        } else {
            // 纯文本模式：显示 textarea，隐藏 Tiptap 编辑器
            textareaEl.hidden = false;
            mdContainerEl.hidden = true;
            if (mdToolbarEl) mdToolbarEl.style.display = "none";

            // 销毁 Tiptap 编辑器（如果之前创建过）
            if (tiptapEditor) {
                tiptapEditor.destroy();
                tiptapEditor = null;
            }

            // 填充编辑器
            textareaEl.value = originalBody;
        }

        // 设置标题
        titleEl.textContent = payload.title || t("editor.title.default");

        // 重置状态——saving/allowClose 已在函数开头清除 closeTimeout
        saving = false;
        allowClose = false;
        statusEl.textContent = "";
        saveBtn.disabled = false;

        // 聚焦编辑器
        if (currentFormat === "markdown" && tiptapEditor) {
            tiptapEditor.commands.focus();
        } else {
            textareaEl.focus();
        }
    } catch (e) {
        console.error("[content-editor] loadPayload 失败:", e);
    }
}

/**
 * 0.18.3 fix: 从后端重新拉取便签内容并更新编辑器。
 * 当编辑器来源是 sticky 且收到 STICKY_CONTENT_CHANGED 事件时调用。
 * 仅在用户无未保存改动时执行（hasUnsavedChanges() === false）。
 */
async function reloadFromSticky() {
    if (!originRef) return;
    try {
        const note = await getStickyNote(originRef);
        if (!note) {
            tracing("reloadFromSticky: 便签不存在");
            return;
        }
        const newBody = (note.content || "").replace(/\r\n/g, "\n");
        originalBody = newBody;

        if (tiptapEditor) {
            try {
                const json = tiptapEditor.markdown.parse(newBody);
                tiptapEditor.commands.setContent(json, false);
            } catch (e) {
                console.error("[content-editor] reloadFromSticky setContent 失败:", e);
            }
        } else {
            textareaEl.value = newBody;
        }
        tracing("reloadFromSticky: 已从便签同步最新内容");
    } catch (e) {
        console.error("[content-editor] reloadFromSticky 失败:", e);
    }
}

// ── 工具栏 ────────────────────────────────────────────

function bindToolbar() {
    saveBtn.addEventListener("click", handleSave);
    cancelBtn.addEventListener("click", handleCancel);
}

// ── 保存 ──────────────────────────────────────────────

/**
 * 0.18.3: 生命周期关闭——递增代际、清理计时器、隐藏窗口。
 * 供 STICKY_TRASHED / VISIBILITY_CHANGED / DELETED 事件处理器共用。
 * 递增 sessionGen 确保正在进行的后台保存回调不会重新显示窗口。
 */
function lifecycleClose(reason) {
    tracing(`${reason}，自动关闭编辑器`);
    allowClose = true;
    sessionGen++;
    if (closeTimeout) {
        clearTimeout(closeTimeout);
        closeTimeout = null;
    }
    closeWindow();
}

async function handleSave() {
    if (saving) return;
    saving = true;
    saveBtn.disabled = true;

    // 获取内容
    let body;
    if (tiptapEditor) {
        try {
            body = tiptapEditor.getMarkdown();
        } catch (mdErr) {
            console.error("[content-editor] getMarkdown 失败:", mdErr);
            body = textareaEl.value;
        }
    } else {
        body = textareaEl.value;
    }

    // 捕获保存参数——后台保存期间窗口可能被复用，模块级变量会被 loadPayload 重置
    const savedBody = body;
    const savedOriginRef = originRef;
    const savedPolicy = savePolicy;
    const savedGen = sessionGen;

    tracing(`handleSave: policy=${savedPolicy}, originRef=${savedOriginRef}, bodyLen=${savedBody.length}`);

    // 0.18.3: 立即隐藏窗口——不阻塞用户，保存转为后台进行
    allowClose = true;
    closeWindow();

    // 后台保存（不阻塞 UI）——窗口已隐藏，用户无需等待
    try {
        await saveContentEditor(savedBody, savedOriginRef, savedPolicy);
        tracing("后台保存成功");
        // 仅当窗口未被复用（代际未变）时更新状态
        if (savedGen === sessionGen) {
            originalBody = savedBody;
            saving = false;
        }
    } catch (e) {
        console.error("[content-editor] 后台保存失败:", e);
        const msg = typeof e === "string" ? e : String(e?.message || e);
        // 仅当窗口未被复用时重新显示并提示错误
        if (savedGen === sessionGen) {
            saving = false;
            saveBtn.disabled = false;
            allowClose = false;
            statusEl.textContent = t("editor.saveFailed", {message: msg});
            const win = getCurrentWindow();
            if (win) win.show();
        }
    }
}

// ── 关闭 / 取消 ───────────────────────────────────────

/**
 * 0.22.11: 关闭确认三态化——保存并关闭（主按钮/Enter）/ 放弃更改 / 继续编辑（Esc）。
 * 标题栏 X、Esc、Alt+F4 三个关闭入口都收敛到这里。
 */
async function handleCancel() {
    if (hasUnsavedChanges() && !allowClose) {
        const choice = await choiceDialog(t("editor.unsavedWarning"), {
            kind: "warning",
            okLabel: t("editor.discard"),
            cancelLabel: t("editor.continueEdit"),
            thirdAction: {label: t("editor.saveAndClose")},
        });
        if (choice === "cancel") return; // 继续编辑
        if (choice === "third") {
            // 保存并关闭——handleSave 自带「隐藏 → 后台保存 → 失败回显」闭环，失败时内容不丢
            await handleSave();
            return;
        }
        // "ok" → 放弃更改，走下方关闭路径
    }
    allowClose = true;
    closeWindow();
}

/** 检查是否有未保存的改动 */
function hasUnsavedChanges() {
    let current;
    if (tiptapEditor) {
        try {
            current = tiptapEditor.getMarkdown();
        } catch {
            current = textareaEl.value;
        }
    } else {
        current = textareaEl.value;
    }
    return current !== originalBody;
}

/** 关闭窗口（改为 hide 复用模式，不再销毁窗口） */
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
            if (allowClose || !hasUnsavedChanges()) {
                closeWindow(); // win.hide()
            } else {
                handleCancel(); // 显示未保存确认对话框
            }
        });
    }
}

// ── 键盘快捷键 ────────────────────────────────────────

function bindKeyboard() {
    document.addEventListener("keydown", (e) => {
        // 0.22.11: 自绘弹窗（关闭确认三态框等）打开时交给弹窗内部键盘逻辑——
        // 否则 Ctrl+S 会绕过模态对话框直接隐藏窗口，留下悬挂的遮罩层。
        // 与 settings/index.js 的 ESC 让路模式一致。
        if (document.querySelector(".modal-overlay")) return;

        // Ctrl+S：保存
        if ((e.ctrlKey || e.metaKey) && e.key === "s") {
            e.preventDefault();
            handleSave();
            return;
        }

        // Esc：关闭（有未保存改动时提示）
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
