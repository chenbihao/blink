/**
 * 便签窗口入口（0.16.8 / 0.18.3 IR 编辑）。
 *
 * 从 URL 参数读取 sticky_id，从后端拉取便签数据，初始化编辑/自动保存/颜色/生命周期。
 * 编辑逻辑在前端，后端只做数据持久化和窗口管理。
 *
 * 0.18.3：编辑器从纯文本 textarea 升级为 Tiptap IR（即时渲染）编辑器。
 * 输入 markdown 语法实时渲染（# → H1、**粗体** 等），存储仍为 markdown 文本。
 * 若 Tiptap 加载失败，降级为 textarea（降级方案 B 兜底）。
 *
 * 设计见 phases/0.16-clipboard-polish.md §5.9、phases/0.18-enhancement-chord.md §3.2。
 */

import {applyThemeFromConfig} from "../shared/theme.js";
import {ensureSpriteLoaded} from "../shared/icon.js";
import {getCurrentWindow, invoke, listen, normalizeError} from "../shared/tauri.js";
import {EVENTS} from "../shared/event-names.js";
import {t} from "../i18n/index.js";
import {bindMdToolbar, createMdToolbar, updateToolbarStates} from "../shared/md-toolbar.js";
import {
    closeStickyNote,
    getStickyNote,
    openContentEditor,
    setStickyAlwaysOnTop,
    setStickyVisible,
    showStickyManager,
    trashStickyNote,
    updateStickyAppearance,
    updateStickyContent,
    updateStickyGeometry,
} from "../shared/api.js";
import {createSwatchRow} from "./palette.js";

// ── 状态 ──────────────────────────────────────────────

/** 当前便签 ID */
let stickyId = null;

/** 当前便签数据 */
let stickyNote = null;

/** 当前便签的 updated_at（Unix 秒），用于乐观并发校验（spec-backend §7.5）。
 *  每次成功保存后由后端返回新值更新；关闭时传给 close_sticky_note。
 *
 *  P0-1：revision 只允许单调递增——多个 mutation 的 IPC 响应可能乱序返回，
 *  用 max(newRev, stickyUpdatedAt) 防止迟到响应倒退 revision。 */
let stickyUpdatedAt = null;

/** 便签会话 generation（每次加载/复用时递增）。
 *  异步保存回流时校验，spare 窗口复用后旧便签的保存结果不得写入新便签。 */
let stickyGeneration = 0;

/** P0-1：便签 mutation 串行化队列。
 *  内容、几何、外观、可见性、置顶、关闭全部通过此队列执行，
 *  确保 IPC 响应严格按发起顺序处理，revision 不会被乱序响应倒退。
 *  close 也排在此前 mutation 之后，保证关闭前的写操作全部完成。 */
let mutationQueue = Promise.resolve();

/** P0-1：将一个 async 函数排入 mutation 队列串行执行。
 *  捕获 generation，回调中校验防止 spare 复用后旧结果写入新便签。
 *  @param {() => Promise<any>} fn — mutation 函数，返回值忽略
 *  @returns {Promise<any>} fn 的返回值（或 void） */
function enqueueMutation(fn) {
    const gen = stickyGeneration;
    const result = mutationQueue.then(() => fn(gen));
    // 无论成功失败，都把链推进；失败不阻断后续 mutation
    mutationQueue = result.catch(() => {
    });
    return result;
}

/** P0-1：单调递增更新 stickyUpdatedAt，防止乱序响应倒退 revision。 */
function applyRevision(newUpdatedAt) {
    if (newUpdatedAt != null && (stickyUpdatedAt == null || newUpdatedAt > stickyUpdatedAt)) {
        stickyUpdatedAt = newUpdatedAt;
    }
}

/** 内容保存防抖计时器 */
let saveTimer = null;

/** 几何保存防抖计时器 */
let geometryTimer = null;

/** 防抖延迟（ms） */
const SAVE_DEBOUNCE_MS = 500;
const GEOMETRY_DEBOUNCE_MS = 300;

/** Tiptap IR 编辑器实例（null = 未初始化或降级 textarea 模式） */
let tiptapEditor = null;

/** 0.18.3 fix: 程序化设入内容时为 true，抑制 update 事件触发的防抖保存。
 *
 * 根因：Tiptap setContent(json, false) 的 emitUpdate=false 在 Markdown 扩展
 * parse→setContent 路径下仍可能触发 update 事件，导致 loadStickyData →
 * setContent → update → scheduleSave → 500ms 后保存 → emit
 * STICKY_CONTENT_CHANGED → 又触发 loadStickyData 的反馈循环。
 *
 * 通过 isLoading 标志在 setContent 期间屏蔽 scheduleSave，斩断循环。
 */
let isLoading = false;

// ── 0.18.3 fix: 预热窗口未初始化完即被借用的竞态防护 ──
//
// init() 前两个 await（ensureSpriteLoaded / applyThemeFromConfig）是异步 IPC，
// 后端可能在它们完成前就 eval __stickyReload。此时注册一个早期 stub，
// 暂存 pending id，init 完成后再处理。

/** init 是否完成（preheat 或 normal 路径都设为 true） */
let initReady = false;

/** init 完成前收到的 pending reload id */
let pendingStickyId = null;

/** 真正的 __stickyReload 实现（registerStickyReload 注册） */
let realStickyReload = null;

// ── DOM 引用 ──────────────────────────────────────────

const rootEl = document.getElementById("sticky-root");
const editorEl = document.getElementById("sticky-editor");
const fallbackEl = document.getElementById("sticky-fallback");
const textareaEl = document.getElementById("sticky-textarea");
const colorBtn = document.getElementById("btn-color");
const colorPalette = document.getElementById("color-palette");
const pinBtn = document.getElementById("btn-pin");
const moreBtn = document.getElementById("btn-more");
const moreMenu = document.getElementById("more-menu");
const moreOpenEditor = document.getElementById("more-open-editor");
const moreHide = document.getElementById("more-hide");
const moreOpenManager = document.getElementById("more-open-manager");
const closeBtn = document.getElementById("btn-close");
const toolbarBtn = document.getElementById("btn-toolbar");
const mdToolbar = document.getElementById("md-toolbar");

// ── 初始化 ────────────────────────────────────────────

async function init() {
    // 0.18.3 fix: 立即注册早期 stub，在任何 await 之前。
    // 如果后端在 init 的异步步骤（ensureSpriteLoaded / applyThemeFromConfig）完成前
    // 就 eval __stickyReload，stub 会暂存 id，init 完成后处理。
    window.__stickyReload = function (id) {
        if (!initReady) {
            pendingStickyId = id;
            console.log("[sticky] __stickyReload 在 init 完成前被调用，已暂存:", id);
            return;
        }
        // init 完成后，交给真正的 registerStickyReload 注册的实现处理
        if (typeof realStickyReload === "function") {
            realStickyReload(id);
        }
    };

    // 主题 + 图标
    await ensureSpriteLoaded();
    await applyThemeFromConfig();

    // 0.18.3：预热模式——跳过完整初始化，等待 __stickyReload 唤醒
    const params = new URLSearchParams(window.location.search);
    const isPreheat = params.get("preheat") === "1";

    if (isPreheat) {
        // 预创建 Tiptap 编辑器（加载脚本 + 初始化），但不拉取便签数据
        initTiptapEditor();
        console.log("[sticky] 预热模式：Tiptap 已初始化，等待 __stickyReload 唤醒");
        window.__stickyPreheated = true;
        // 预热分支也注册 __stickyReload，否则后端 eval 调用时找不到函数
        registerStickyReload();

        // 0.18.3 fix: 标记 init 完成，处理可能在 await 期间收到的 pending reload
        initReady = true;
        if (pendingStickyId) {
            console.log("[sticky] 预热 init 完成，处理 pending reload:", pendingStickyId);
            const id = pendingStickyId;
            pendingStickyId = null;
            realStickyReload(id);
        }

        // 0.18.3 N+1: 通知后端 spare 已就绪，可被借用
        invoke("sticky_spare_ready").catch((e) => {
            const err = normalizeError(e);
            console.error(`[sticky] sticky_spare_ready 调用失败 [${err.code}]: ${err.message}`);
        });
        return;
    }

    // 从 URL 参数读取 sticky_id
    stickyId = params.get("id");

    if (!stickyId) {
        console.error("[sticky] 未提供 sticky_id");
        return;
    }

    // 初始化 Tiptap IR 编辑器（0.18.3）
    // 必须在 loadStickyData 之前，因为 loadStickyData 会 setContent
    initTiptapEditor();

    // 从后端拉取便签数据
    await loadStickyData();

    // 绑定事件
    bindEditing();
    bindColorPalette();
    bindToolbar();
    bindMoreMenu();
    bindWindowControls();
    bindGeometryTracking();
    bindKeyboard();
    bindContextMenu();

    // 内容变更：只在用户未在编辑时刷新，避免打断输入
    // 外部入口（内容编辑器 / Capability）的变更始终 reload；本窗口防抖写入才避开输入中刷新。
    listen(EVENTS.STICKY_CONTENT_CHANGED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === stickyId) {
            if (payload.source !== "sticky") {
                loadStickyData();
                return;
            }
            // sticky 来源：用户正在本窗口编辑时不 reload，避免反馈循环和光标跳动
            const activeEl = document.activeElement;
            const isEditing = tiptapEditor
                ? activeEl === editorEl || editorEl?.contains(activeEl)
                : activeEl === textareaEl;
            if (!isEditing) {
                loadStickyData();
            }
        }
    });

    // 外观变更：只刷新颜色，不 reload content
    listen(EVENTS.STICKY_APPEARANCE_CHANGED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === stickyId && payload.color) {
            applyColor(payload.color);
        }
    });

    // 0.17.7：便签被移入回收站时隐藏窗口
    listen(EVENTS.STICKY_TRASHED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === stickyId) {
            const win = getCurrentWindow();
            if (win) win.hide();
        }
    });

    // 0.18.0：管理面板设置 visible=false 时，桌面便签窗口同步隐藏
    // 后端 set_sticky_visible 已 emit STICKY_VISIBILITY_CHANGED，此处补监听
    listen(EVENTS.STICKY_VISIBILITY_CHANGED, (event) => {
        const payload = event.payload;
        if (payload && payload.stickyId === stickyId && payload.visible === false) {
            const win = getCurrentWindow();
            if (win) win.hide();
        }
    });

    // 注册窗口复用回调（正常路径也注册，与预热分支共享同一函数）
    registerStickyReload();

    // 0.18.3 fix: 标记 init 完成，处理可能在 await 期间收到的 pending reload
    initReady = true;
    if (pendingStickyId) {
        console.log("[sticky] 正常 init 完成，处理 pending reload:", pendingStickyId);
        const id = pendingStickyId;
        pendingStickyId = null;
        realStickyReload(id);
    }

    console.log("[sticky] init 完成");
}

/**
 * 注册 __stickyReload 回调——预热和正常路径共享。
 * 预热分支也调用此函数，确保后端 eval 能找到 __stickyReload。
 */
function registerStickyReload() {
    // 真正的实现存入 realStickyReload，供早期 stub 转发
    realStickyReload = (id) => {
        // 窗口复用前先销毁旧编辑器实例，避免 DOM 争抢
        if (window.__stickyDestroyEditor) {
            window.__stickyDestroyEditor();
        }
        // 重新初始化 Tiptap（editorEl 已被 destroy 清理）
        initTiptapEditor();

        // 0.18.3：首次从预热唤醒需绑定全部 DOM 事件；
        // recycled spare（非首次）只需重建 Tiptap 实例级监听
        const isFirstBorrow = window.__stickyPreheated;
        if (isFirstBorrow) {
            window.__stickyPreheated = false;
            bindColorPalette();
            bindToolbar();
            bindMoreMenu();
            bindWindowControls();
            bindGeometryTracking();
            bindKeyboard();
            bindContextMenu();

            // 内容变更监听
            // 外部入口（内容编辑器 / Capability）的变更始终 reload
            listen(EVENTS.STICKY_CONTENT_CHANGED, (event) => {
                const payload = event.payload;
                if (payload && payload.stickyId === stickyId) {
                    if (payload.source !== "sticky") {
                        loadStickyData();
                        return;
                    }
                    const activeEl = document.activeElement;
                    const isEditing = tiptapEditor
                        ? activeEl === editorEl || editorEl?.contains(activeEl)
                        : activeEl === textareaEl;
                    if (!isEditing) {
                        loadStickyData();
                    }
                }
            });

            listen(EVENTS.STICKY_APPEARANCE_CHANGED, (event) => {
                const payload = event.payload;
                if (payload && payload.stickyId === stickyId && payload.color) {
                    applyColor(payload.color);
                }
            });

            listen(EVENTS.STICKY_TRASHED, (event) => {
                const payload = event.payload;
                if (payload && payload.stickyId === stickyId) {
                    const win = getCurrentWindow();
                    if (win) win.hide();
                }
            });

            listen(EVENTS.STICKY_VISIBILITY_CHANGED, (event) => {
                const payload = event.payload;
                if (payload && payload.stickyId === stickyId && payload.visible === false) {
                    const win = getCurrentWindow();
                    if (win) win.hide();
                }
            });

            // bindEditing 单独调用（在 setContent 之前绑定 update 事件）
            bindEditing();
        } else {
            // 0.18.3：recycled spare — 编辑器已重建，需重新绑定 Tiptap 实例级事件
            bindEditing();
            // 重建 MD 工具栏（bindMdToolbar 闭包捕获旧 editor 引用，必须重建）
            if (mdToolbar) {
                mdToolbar.innerHTML = "";
                const toolbar = createMdToolbar("md-toolbar-inner");
                mdToolbar.appendChild(toolbar);
                bindMdToolbar(toolbar, tiptapEditor, {editorEl, autoHide: true, toggleBtn: toolbarBtn});
            }
            if (tiptapEditor) {
                tiptapEditor.on("selectionUpdate", () => updateToolbarStates(mdToolbar, tiptapEditor));
                tiptapEditor.on("transaction", () => updateToolbarStates(mdToolbar, tiptapEditor));
            }
        }

        stickyId = id;
        loadStickyData();
    };
}

/**
 * 从后端拉取便签数据并填充 UI。
 */
async function loadStickyData() {
    try {
        stickyNote = await getStickyNote(stickyId);
        if (!stickyNote) {
            console.error("[sticky] 便签不存在:", stickyId);
            return;
        }

        // 0.20.7：初始化 revision 和 generation（spec-backend §7.5）
        stickyUpdatedAt = stickyNote.updatedAt ?? null;
        stickyGeneration++;

        // 填充编辑器
        setContent(stickyNote.content || "");

        // 应用颜色
        applyColor(stickyNote.color || "theme");

        // 应用置顶状态
        updatePinButton(stickyNote.alwaysOnTop);

        // 0.20.0：同步窗口标题（从内容派生）
        syncWindowTitle(stickyNote.content || "");

        // 聚焦编辑器
        focusEditor();
    } catch (e) {
        const err = normalizeError(e);
        console.error(`[sticky] 加载便签数据失败 [${err.code}]: ${err.message}`);
    }
}

/**
 * 0.20.0：从便签内容派生标题并设置到窗口。
 * 空内容时设置为"便签"（或 "Sticky"）。
 */
function syncWindowTitle(content) {
    const win = getCurrentWindow();
    if (!win) return;
    const isZh = document.documentElement?.lang?.startsWith("zh");
    const title = deriveTitle(content, isZh ? "zh" : "en");
    win.setTitle(title).catch((e) => {
        console.warn("[sticky] 设置窗口标题失败:", e);
    });
}

/**
 * 0.20.0：从便签内容派生标题（前端侧简化版，与后端 derive_sticky_title 同语义）。
 * 前端无法调 Rust 纯函数，此处实现一个等价的 JS 版本。
 */
function deriveTitle(content, locale) {
    // 取第一条非空行
    const lines = (content || "").split("\n");
    let firstLine = "";
    for (const line of lines) {
        const trimmed = line.trim();
        if (trimmed) {
            firstLine = trimmed;
            break;
        }
    }
    if (!firstLine) {
        return locale === "zh" ? "便签" : "Sticky";
    }
    // 剥离 Markdown 前缀和行内标记
    let cleaned = stripMarkdown(firstLine);
    // 截断到 48 字符
    cleaned = Array.from(cleaned).slice(0, 48).join("");
    return cleaned || (locale === "zh" ? "便签" : "Sticky");
}

/** 剥离 Markdown 标记 */
function stripMarkdown(s) {
    let r = s;
    // 标题前缀
    r = r.replace(/^#{1,6}\s*/, "");
    // 引用
    r = r.replace(/^>\s*/, "");
    // 任务列表
    r = r.replace(/^[-*+]\s*\[[ xX]\]\s*/, "");
    // 无序列表
    r = r.replace(/^[-*+]\s+/, "");
    // 有序列表
    r = r.replace(/^\d{1,3}[.)]\s+/, "");
    // 行内标记
    r = r.replace(/\*\*(.+?)\*\*/g, "$1");
    r = r.replace(/__(.+?)__/g, "$1");
    r = r.replace(/\*(.+?)\*/g, "$1");
    r = r.replace(/_(.+?)_/g, "$1");
    r = r.replace(/~~(.+?)~~/g, "$1");
    r = r.replace(/`(.+?)`/g, "$1");
    // 链接
    r = r.replace(/\[([^\]]*)\]\([^)]*\)/g, "$1");
    return r;
}

// ── Tiptap IR 编辑器初始化（0.18.3）──────────────────────────

/**
 * 初始化 Tiptap IR 编辑器。
 * 若 window.BlinkTiptap 不存在或初始化失败，降级为 textarea。
 */
function initTiptapEditor() {
    if (!window.BlinkTiptap) {
        console.warn("[sticky] BlinkTiptap 未加载，降级为 textarea");
        enableFallback();
        return;
    }

    try {
        const {Editor, StarterKit, Markdown, TaskList, TaskItem} = window.BlinkTiptap;
        tiptapEditor = new Editor({
            element: editorEl,
            extensions: [
                StarterKit,
                Markdown,
                TaskList,
                TaskItem,
            ],
            content: "",
            contentType: "markdown",
            editorProps: {
                attributes: {
                    class: "sticky-tiptap",
                    spellcheck: "false",
                },
                handlePaste: (view, event) => {
                    // 0.18.3：粘贴纯文本时走 Markdown 解析，使 - [ ] / **bold** 等语法实时生效
                    const text = event.clipboardData?.getData("text/plain");
                    if (!text || !tiptapEditor) return false;
                    try {
                        const json = tiptapEditor.markdown.parse(text);
                        tiptapEditor.commands.insertContent(json);
                        event.preventDefault();
                        return true;
                    } catch (e) {
                        console.warn("[sticky] paste markdown parse failed, fallback to default:", e);
                        return false;
                    }
                },
            },
        });
        console.log("[sticky] Tiptap IR 编辑器初始化成功");
    } catch (e) {
        console.error("[sticky] Tiptap 初始化失败，降级为 textarea:", e);
        tiptapEditor = null;
        enableFallback();
    }
}

/** 启用 textarea 降级模式 */
function enableFallback() {
    tiptapEditor = null;
    editorEl.hidden = true;
    if (fallbackEl) fallbackEl.hidden = false;
}

// ── 内容读写 ───────────────────────────────────────

/** 获取当前便签内容（markdown 文本） */
function getContent() {
    if (tiptapEditor) {
        return tiptapEditor.getMarkdown();
    }
    return textareaEl.value;
}

/** 设置便签内容（markdown 文本） */
function setContent(text) {
    if (tiptapEditor) {
        // 0.18.3 fix: isLoading 标志确保 setContent 期间不会触发防抖保存
        isLoading = true;
        try {
            // 通过 markdown.parse 将 markdown 文本转为 JSON，再 setContent
            const json = tiptapEditor.markdown.parse(text);
            tiptapEditor.commands.setContent(json, false);
        } catch (e) {
            console.error("[sticky] setContent 解析失败:", e);
            // 降级：直接设为纯文本 HTML
            tiptapEditor.commands.setContent(`<p>${escapeHtml(text)}</p>`, false);
        } finally {
            isLoading = false;
        }
    } else {
        textareaEl.value = text;
    }
}

/** 转义 HTML 特殊字符（降级路径用） */
function escapeHtml(s) {
    return s
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;");
}

/** 聚焦当前编辑器（Tiptap 或 textarea） */
function focusEditor() {
    if (tiptapEditor) {
        tiptapEditor.commands.focus();
    } else {
        textareaEl.focus();
    }
}

// ── 编辑与自动保存 ────────────────────────────────────

function bindEditing() {
    if (tiptapEditor) {
        // Tiptap 的 update 事件替代 textarea 的 input 事件
        tiptapEditor.on("update", () => {
            scheduleSave();
        });
    } else {
        textareaEl.addEventListener("input", () => {
            scheduleSave();
        });
    }
}

/** 安排防抖保存 */
function scheduleSave() {
    // 0.18.3 fix: 程序化设入内容时跳过，避免反馈循环
    if (isLoading) return;
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(() => {
        saveTimer = null; // 0.18.3 fix: 触发后清空，避免 flushSave 误判
        saveContent();
    }, SAVE_DEBOUNCE_MS);
}

/** 立即保存内容 */
async function saveContent() {
    if (!stickyId) return;
    const content = getContent();
    const expectedRev = stickyUpdatedAt;
    return enqueueMutation(async (gen) => {
        try {
            const newUpdatedAt = await updateStickyContent(stickyId, content, expectedRev);
            if (gen !== stickyGeneration) {
                console.debug("[sticky] saveContent 回流时 generation 已变，丢弃结果");
                return;
            }
            // P0-1：单调递增，防止乱序响应倒退 revision
            applyRevision(newUpdatedAt);
            // 0.20.0：内容保存后同步窗口标题（从派生标题跟随内容变化）
            syncWindowTitle(content);
        } catch (e) {
            // generation 变化后不再处理错误（窗口已复用）
            if (gen !== stickyGeneration) return;
            const err = normalizeError(e);
            console.error(`[sticky] 保存内容失败 [${err.code}]: ${err.message}`);
            // 冲突时不动 stickyUpdatedAt——用户下次保存会再次冲突
            // 提示错误但不覆盖编辑器内容
            if (err.code === "conflict") {
                showError("便签已被其他入口修改，请刷新后重试");
            }
        }
    });
}

/** 强制 flush（失焦/隐藏前调用） */
async function flushSave() {
    if (saveTimer) {
        clearTimeout(saveTimer);
        saveTimer = null;
        await saveContent();
    }
}

// ── 颜色面板 ──────────────────────────────────────────

function bindColorPalette() {
    // 切换面板显示
    colorBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        moreMenu.hidden = true;
        colorPalette.hidden = !colorPalette.hidden;
    });

    // 选择颜色
    colorPalette.querySelectorAll(".color-swatch").forEach((swatch) => {
        swatch.addEventListener("click", async () => {
            const color = swatch.dataset.color;
            colorPalette.hidden = true;
            applyColor(color);
            enqueueMutation(async (gen) => {
                try {
                    const newUpdatedAt = await updateStickyAppearance(stickyId, color);
                    if (gen !== stickyGeneration) return;
                    applyRevision(newUpdatedAt);
                } catch (e) {
                    if (gen !== stickyGeneration) return;
                    const err = normalizeError(e);
                    console.error(`[sticky] 更新颜色失败 [${err.code}]: ${err.message}`);
                }
            });
        });
    });

    // 点击外部关闭面板
    document.addEventListener("click", () => {
        colorPalette.hidden = true;
        moreMenu.hidden = true;
    });
}

/** 应用颜色 class + 更新色板选中态 */
function applyColor(color) {
    rootEl.className = "sticky-root";
    rootEl.classList.add(`color-${color}`);
    // 更新色板选中指示
    colorPalette.querySelectorAll(".color-swatch").forEach((swatch) => {
        swatch.classList.toggle("selected", swatch.dataset.color === color);
    });
}

// ── MD 工具栏（0.18.3）──────────────────────────────────

function bindToolbar() {
    // 切换工具栏显示
    toolbarBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        colorPalette.hidden = true;
        moreMenu.hidden = true;
        mdToolbar.hidden = !mdToolbar.hidden;
        // 工具栏激活态
        toolbarBtn.classList.toggle("active", !mdToolbar.hidden);
    });

    // 0.18.3：工具栏按钮事件 + autoHide 逻辑由共享模块 bindMdToolbar 处理
    bindMdToolbar(mdToolbar, tiptapEditor, {
        editorEl,
        autoHide: true,
        toggleBtn: toolbarBtn,
    });

    // 编辑器更新时刷新工具栏按钮激活态
    if (tiptapEditor) {
        tiptapEditor.on("selectionUpdate", () => updateToolbarStates(mdToolbar, tiptapEditor));
        tiptapEditor.on("transaction", () => updateToolbarStates(mdToolbar, tiptapEditor));
    }

    // Ctrl+滚轮缩放字体 + Ctrl+中键还原
    bindFontZoom();
}

// ── Ctrl+滚轮缩放字体（0.18.3）─────────────────────────

/** 当前字体缩放倍率（1.0 = 默认） */
let fontScale = 1.0;
const FONT_SCALE_MIN = 0.7;
const FONT_SCALE_MAX = 2.0;
const FONT_SCALE_STEP = 0.1;

function bindFontZoom() {
    editorEl.addEventListener("wheel", (e) => {
        if (!e.ctrlKey) return;
        e.preventDefault();
        if (e.deltaY < 0) {
            fontScale = Math.min(FONT_SCALE_MAX, fontScale + FONT_SCALE_STEP);
        } else {
            fontScale = Math.max(FONT_SCALE_MIN, fontScale - FONT_SCALE_STEP);
        }
        applyFontScale();
    });

    // Ctrl+中键还原
    editorEl.addEventListener("auxclick", (e) => {
        if (e.button === 1 && e.ctrlKey) {
            e.preventDefault();
            fontScale = 1.0;
            applyFontScale();
        }
    });
}

function applyFontScale() {
    const pm = editorEl.querySelector(".ProseMirror");
    if (pm) {
        pm.style.fontSize = `calc(var(--text-sm) * ${fontScale})`;
    }
}

// ── 更多菜单 ──────────────────────────────────────────

function bindMoreMenu() {
    moreBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        colorPalette.hidden = true;
        moreMenu.hidden = !moreMenu.hidden;
    });

    // 在编辑器中打开
    moreOpenEditor.addEventListener("click", async () => {
        moreMenu.hidden = true;
        await flushSave();
        try {
            await openContentEditor({
                body: getContent(),
                format: "markdown",
                title: "编辑便签内容",
                origin: "sticky",
                originRef: stickyId,
                savePolicy: "sticky_update",
            });
        } catch (e) {
            const err = normalizeError(e);
            console.error(`[sticky] 打开编辑器失败 [${err.code}]: ${err.message}`);
        }
    });

    // 隐藏（仅隐藏窗口，不进回收站）
    moreHide.addEventListener("click", async () => {
        moreMenu.hidden = true;
        await flushSave();
        enqueueMutation(async (gen) => {
            try {
                const newUpdatedAt = await setStickyVisible(stickyId, false);
                if (gen !== stickyGeneration) return;
                applyRevision(newUpdatedAt);
            } catch (e) {
                if (gen !== stickyGeneration) return;
                const err = normalizeError(e);
                console.error(`[sticky] 设置可见性失败 [${err.code}]: ${err.message}`);
            }
        });
        const win = getCurrentWindow();
        if (win) win.hide();
    });

    // 便签管理（0.17.7：从便签页唤起管理窗口）
    if (moreOpenManager) {
        moreOpenManager.addEventListener("click", async () => {
            moreMenu.hidden = true;
            try {
                await showStickyManager();
            } catch (e) {
                const err = normalizeError(e);
                console.error(`[sticky] 打开便签管理失败 [${err.code}]: ${err.message}`);
            }
        });
    }
}

// ── 窗口控制 ──────────────────────────────────────────

function bindWindowControls() {
    // 0.20.0：关闭 = 原子关闭工作流（空→删除，非空→保存+回收站）
    // 前端先 flush 未保存防抖，然后调用 closeStickyNote API，
    // 成功后才隐藏窗口。失败时保持窗口与内容并显示错误。
    closeBtn.addEventListener("click", () => {
        closeSticky();
    });

    // 置顶切换
    pinBtn.addEventListener("click", async () => {
        const newState = !pinBtn.classList.contains("active");
        updatePinButton(newState);
        enqueueMutation(async (gen) => {
            try {
                const newUpdatedAt = await setStickyAlwaysOnTop(stickyId, newState);
                if (gen !== stickyGeneration) return;
                applyRevision(newUpdatedAt);
                const win = getCurrentWindow();
                if (win) win.setAlwaysOnTop(newState);
            } catch (e) {
                if (gen !== stickyGeneration) return;
                const err = normalizeError(e);
                console.error(`[sticky] 切换置顶失败 [${err.code}]: ${err.message}`);
                updatePinButton(!newState);
            }
        });
    });
}

/** 更新置顶按钮状态 */
function updatePinButton(active) {
    if (active) {
        pinBtn.classList.add("active");
    } else {
        pinBtn.classList.remove("active");
    }
}

// ── 键盘快捷键（0.16.11）──────────────────────────────

function bindKeyboard() {
    document.addEventListener("keydown", (e) => {
        // ESC：关闭便签窗口（走原子关闭工作流）
        if (e.key === "Escape") {
            e.preventDefault();
            closeSticky();

        }
    });
}

/**
 * 关闭便签窗口（0.20.0 原子关闭工作流）。
 *
 * 直接调用 closeStickyNote API，由后端在一次调用中完成：
 * 保存最终内容 + delete/trash 决策。
 * 空内容 → 物理删除；非空 → 保存最终内容并移入回收站。
 * 成功后隐藏窗口。失败时保持窗口与内容并显示错误。
 *
 * 0.20.7：传当前 revision（stickyUpdatedAt）做乐观并发校验，
 * 防止旧窗口无条件物理删除已被其他入口改写为非空的便签（spec-backend §7.5）。
 *
 * P0-1：close 排在 mutation 队列尾部，确保此前发起的 saveContent/saveGeometry
 * 等全部完成后才执行关闭，避免“关闭读取旧 revision 覆盖刚保存的新 revision”的竞态。
 */
async function closeSticky() {
    if (!stickyId) {
        // 无 stickyId（预热/异常态）→ 直接隐藏
        const win = getCurrentWindow();
        if (win) win.hide();
        return;
    }

    const content = getContent();
    const expectedRev = stickyUpdatedAt;
    return enqueueMutation(async (gen) => {
        try {
            const outcome = await closeStickyNote(stickyId, content, expectedRev);
            if (gen !== stickyGeneration) {
                console.debug("[sticky] closeSticky 回流时 generation 已变，丢弃结果");
                return;
            }
            // 成功：根据结果打日志（不记录正文）
            if (outcome?.kind === "deleted_empty") {
                console.log("[sticky] 便签已关闭（空→删除）");
            } else if (outcome?.kind === "trashed") {
                console.log("[sticky] 便签已关闭（非空→回收站）");
            }
            // 隐藏窗口（后端 close_sticky_and_notify 已调 hide_sticky_window，
            // 但前端也 hide 确保 UI 即时响应）
            const win = getCurrentWindow();
            if (win) win.hide();
        } catch (e) {
            if (gen !== stickyGeneration) return;
            // 失败：保持窗口与内容，显示错误
            const err = normalizeError(e);
            console.error(`[sticky] 原子关闭失败 [${err.code}]: ${err.message}`);
            // 冲突时展示结构化提示，不自动用服务端正文覆盖本地编辑器
            if (err.code === "conflict") {
                showError("便签已被其他入口修改，请刷新后重试");
            } else {
                showError(err.message);
            }
        }
    });
}

/**
 * 显示错误反馈（便签窗口内）。
 * 复用右键菜单区域的临时提示，避免引入额外的 DOM 结构。
 */
let stickyErrorTimer = 0;

function showError(message) {
    // 简单方案：在 closeBtn 旁边显示临时提示，3 秒后消失
    let errEl = document.getElementById("sticky-error-hint");
    if (!errEl) {
        errEl = document.createElement("div");
        errEl.id = "sticky-error-hint";
        errEl.className = "sticky-error-hint";
        rootEl.appendChild(errEl);
    }
    errEl.textContent = message;
    errEl.hidden = false;
    // 取消上一次的计时器，防止旧计时器提前隐藏新错误
    clearTimeout(stickyErrorTimer);
    stickyErrorTimer = setTimeout(() => {
        if (errEl) errEl.hidden = true;
        stickyErrorTimer = 0;
    }, 3000);
}

// ── 窗口几何追踪与持久化 ──────────────────────────────

function bindGeometryTracking() {
    const win = getCurrentWindow();
    if (!win) return;

    // 监听窗口移动/缩放（Tauri 的 onResized / onMoved）
    if (win.onResized) {
        win.onResized(() => scheduleGeometrySave());
    }
    if (win.onMoved) {
        win.onMoved(() => scheduleGeometrySave());
    }
}

/** 安排几何保存防抖 */
function scheduleGeometrySave() {
    if (geometryTimer) clearTimeout(geometryTimer);
    geometryTimer = setTimeout(() => {
        saveGeometry();
    }, GEOMETRY_DEBOUNCE_MS);
}

/** 保存窗口几何 */
async function saveGeometry() {
    if (!stickyId) return;
    const win = getCurrentWindow();
    if (!win) return;
    return enqueueMutation(async (gen) => {
        try {
            const pos = await win.outerPosition();
            const size = await win.outerSize();
            const newUpdatedAt = await updateStickyGeometry(
                stickyId,
                pos.x,
                pos.y,
                size.width,
                size.height,
            );
            if (gen !== stickyGeneration) return;
            applyRevision(newUpdatedAt);
        } catch (e) {
            if (gen !== stickyGeneration) return;
            const err = normalizeError(e);
            console.error(`[sticky] 保存几何失败 [${err.code}]: ${err.message}`);
        }
    });
}

// ── 右键菜单（0.17.7）──────────────────────────────

/** 当前右键菜单元素（null = 未显示） */
let contextMenuEl = null;

function bindContextMenu() {
    // 屏蔽原生右键菜单
    document.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        showContextMenu(e.clientX, e.clientY);
    });

    // 点击外部 / ESC 关闭右键菜单
    document.addEventListener("click", (e) => {
        if (contextMenuEl && !contextMenuEl.contains(e.target)) {
            hideContextMenu();
        }
    });
}

function showContextMenu(x, y) {
    hideContextMenu();

    const menu = document.createElement("div");
    menu.className = "sticky-context-menu";

    // 在编辑器中打开
    const itemEditor = document.createElement("button");
    itemEditor.className = "ctx-item";
    itemEditor.textContent = "在编辑器中打开";
    itemEditor.addEventListener("click", async () => {
        hideContextMenu();
        await flushSave();
        try {
            await openContentEditor({
                body: getContent(),
                format: "markdown",
                title: "编辑便签内容",
                origin: "sticky",
                originRef: stickyId,
                savePolicy: "sticky_update",
            });
        } catch (e) {
            const err = normalizeError(e);
            console.error(`[sticky] 打开编辑器失败 [${err.code}]: ${err.message}`);
        }
    });
    menu.appendChild(itemEditor);

    // 分割线
    menu.appendChild(makeSeparator());

    // 0.20.0：改颜色——共享色板模块，只消费颜色 id 与 CSS class
    const colorRow = createSwatchRow({
        selectedColor: stickyNote?.color,
        onSelect: async (color) => {
            hideContextMenu();
            applyColor(color);
            try {
                await updateStickyAppearance(stickyId, color);
            } catch (e) {
                const err = normalizeError(e);
                console.error(`[sticky] 更新颜色失败 [${err.code}]: ${err.message}`);
            }
        },
    });
    menu.appendChild(colorRow);

    // 分割线
    menu.appendChild(makeSeparator());

    // 0.20.8：便签管理——与顶部更多菜单同一入口
    const itemManager = document.createElement("button");
    itemManager.className = "ctx-item";
    itemManager.textContent = t("menu.stickyManager");
    itemManager.addEventListener("click", async () => {
        hideContextMenu();
        try {
            await showStickyManager();
        } catch (e) {
            const err = normalizeError(e);
            console.error(`[sticky] 打开便签管理失败 [${err.code}]: ${err.message}`);
        }
    });
    menu.appendChild(itemManager);

    // 隐藏
    const itemHide = document.createElement("button");
    itemHide.className = "ctx-item";
    itemHide.textContent = "隐藏";
    itemHide.addEventListener("click", async () => {
        hideContextMenu();
        await flushSave();
        enqueueMutation(async (gen) => {
            try {
                const newUpdatedAt = await setStickyVisible(stickyId, false);
                if (gen !== stickyGeneration) return;
                applyRevision(newUpdatedAt);
            } catch (e) {
                if (gen !== stickyGeneration) return;
                const err = normalizeError(e);
                console.error(`[sticky] 设置可见性失败 [${err.code}]: ${err.message}`);
            }
        });
        const win = getCurrentWindow();
        if (win) win.hide();
    });
    menu.appendChild(itemHide);

    // 删除（=移入回收站）
    const itemDelete = document.createElement("button");
    itemDelete.className = "ctx-item ctx-danger";
    itemDelete.textContent = "删除";
    itemDelete.addEventListener("click", async () => {
        hideContextMenu();
        await flushSave();
        try {
            await trashStickyNote(stickyId);
            const win = getCurrentWindow();
            if (win) win.hide();
        } catch (e) {
            const err = normalizeError(e);
            console.error(`[sticky] 删除便签失败 [${err.code}]: ${err.message}`);
        }
    });
    menu.appendChild(itemDelete);

    // 边缘检测定位
    rootEl.appendChild(menu);
    const menuRect = menu.getBoundingClientRect();
    const winRect = rootEl.getBoundingClientRect();
    let mx = x;
    let my = y;
    if (mx + menuRect.width > winRect.width) {
        mx = winRect.width - menuRect.width - 2;
    }
    if (my + menuRect.height > winRect.height) {
        my = winRect.height - menuRect.height - 2;
    }
    mx = Math.max(0, mx);
    my = Math.max(0, my);
    menu.style.left = `${mx}px`;
    menu.style.top = `${my}px`;

    contextMenuEl = menu;
}

function hideContextMenu() {
    if (contextMenuEl) {
        contextMenuEl.remove();
        contextMenuEl = null;
    }
}

function makeSeparator() {
    const sep = document.createElement("div");
    sep.className = "ctx-separator";
    return sep;
}


// ── 退出前 flush（0.16.11）────────────────────────────

/**
 * P0-2：后端 CloseRequested 兜底路径（Alt+F4/系统关闭）调用的前端入口。
 * 带 requestId 的 request/ack 协议：
 * - 后端 eval 传入 requestId，前端 closeSticky() 完成后 invoke sticky_close_ack 回传结果
 * - ack 区分 success/conflict/error，后端收到任何有效 ack 都取消超时降级
 * - conflict 时前端保持窗口可见，后端不降级覆盖
 */
window.__stickyRequestClose = function (requestId) {
    closeSticky()
        .then(() => {
            // closeSticky 成功（窗口已 hide 或已关闭）
            invoke('sticky_close_ack', {requestId, outcome: 'success'}).catch(() => {
            });
        })
        .catch((e) => {
            const err = normalizeError(e);
            const outcome = err.code === 'conflict' ? 'conflict' : 'error';
            invoke('sticky_close_ack', {requestId, outcome, message: err.message}).catch(() => {
            });
        });
};

/**
 * 应用退出时后端 eval 调用此函数，立即保存未写入的内容和几何。
 * 防抖计时器内的内容最多 500ms 未保存，退出时强制 flush。
 *
 * P0-1：flush 也走 mutation 队列，确保内容保存和几何保存串行化。
 */
window.__stickyFlush = async function () {
    // 清除防抖计时器，直接保存
    if (saveTimer) {
        clearTimeout(saveTimer);
        saveTimer = null;
    }
    if (geometryTimer) {
        clearTimeout(geometryTimer);
        geometryTimer = null;
    }
    // flush 也走队列，确保内容保存和几何保存串行化
    await Promise.allSettled([saveContent(), saveGeometry()]);
    // 等待队列中所有 mutation 完成
    await mutationQueue.catch(() => {
    });
};

// ── Tiptap 编辑器销毁（窗口复用前清理）────────────────────

/**
 * 窗口复用（__stickyReload）前先销毁旧编辑器实例，
 * 避免多个 ProseMirror 实例争抢同一 DOM 节点。
 */
window.__stickyDestroyEditor = function () {
    if (tiptapEditor) {
        try {
            tiptapEditor.destroy();
        } catch (e) {
            console.error("[sticky] destroy editor failed:", e);
        }
        tiptapEditor = null;
    }
};

/**
 * 0.18.3 N+1：spare 回收时后端 eval 调用此函数，重置到预热态。
 * 销毁编辑器、清除计时器、重置 stickyId，然后通知后端已就绪。
 * initReady 保持 true——realStickyReload 已注册，下次 __stickyReload 可直接执行。
 *
 * 0.20.0：回收时重置窗口标题，防止下一张便签继承旧标题。
 */
window.__stickyReset = function () {
    if (window.__stickyDestroyEditor) {
        window.__stickyDestroyEditor();
    }
    stickyId = null;
    stickyNote = null;
    // 0.20.7：递增 generation 使旧便签的迟到保存结果失效
    stickyGeneration++;
    stickyUpdatedAt = null;
    // P0-1：重置 mutation 队列，旧 mutation 的迟到响应不再干扰新便签
    mutationQueue = Promise.resolve();
    if (saveTimer) {
        clearTimeout(saveTimer);
        saveTimer = null;
    }
    if (geometryTimer) {
        clearTimeout(geometryTimer);
        geometryTimer = null;
    }
    // 0.20.0：重置窗口标题到本地化默认值
    const isZh = document.documentElement?.lang?.startsWith("zh");
    const win = getCurrentWindow();
    if (win) {
        win.setTitle(isZh ? "便签" : "Sticky").catch(() => {
        });
    }
    console.log("[sticky] spare 已回收，通知后端就绪");
    // 通知后端：回收完成，可被借用
    invoke("sticky_spare_ready").catch((e) => {
        const err = normalizeError(e);
        console.error(`[sticky] sticky_spare_ready (recycle) 调用失败 [${err.code}]: ${err.message}`);
    });
};

// ── 启动 ──────────────────────────────────────────────

init().catch((e) => console.error("[sticky] init 失败:", e));
