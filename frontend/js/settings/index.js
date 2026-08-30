/**
 * 设置页入口模块
 * 协调各 Tab 模块，管理全局生命周期
 *
 * 0.9.5 前端架构重整：从 settings.js（4169行）拆分为模块化结构
 */

import {applyTheme} from "../shared/theme.js";
import {applyI18n, setLang, t} from "../i18n/index.js";
import {ensureSpriteLoaded} from "../shared/icon.js";
import {hideSettingsWindow, loadConfig} from "./shared/ipc.js";
import {setCurrentConfig} from "./shared/state.js";
import {initGeneralTab} from "./tabs/general.js";
import {initHotkeyTab} from "./tabs/hotkey.js";
import {initEnginesTab} from "./tabs/engines.js";
import {initPluginsTab} from "./tabs/plugins.js";
import {initCapabilitiesTab} from "./tabs/capabilities.js";
import {initNetworkTab} from "./tabs/network.js";
import {initContextTab} from "./tabs/context.js";
import {initStorageTab} from "./tabs/storage.js";
import {initDebugTab} from "./tabs/debug.js";
import {initAboutTab} from "./tabs/about.js";
import {initAITab} from "./tabs/ai.js";
import {initVoiceTab} from "./tabs/voice.js";
import {initChordTab} from "./tabs/chord.js";
import {initMcPTab} from "./tabs/mcp.js";
import {initMcpServerSection} from "./tabs/mcp-server.js";
import {createLocalEngineController} from "./tabs/engines/local-runtime.js";
import {renderEngineCard} from "./tabs/engines/local-engine-card.js";
import {computeRuntimeSummary} from "./tabs/engines/local-engine-summary.js";
import {processDisplay, processClass} from "./tabs/engines/local-engine-process.js";
import {statusClass} from "./tabs/engines/local-engine-card-utils.js";
import {registerLocalEngineHooks, unregisterLocalEngineHooks} from "./tabs/engines/local-engine-hooks.js";
import {createCleanupModal, aggregateSharedTargets} from "./tabs/engines/local-engine-cleanup-modal.js";
import {onLangChange} from "../i18n/index.js";

// ── Tab 切换 + 生命周期管理 ─────────────────────────────────────────────────

// local-runtime controller 实例（仅在 engines tab 活跃时 mount）
let _leController = null;
let _leActive = false;
let _cleanupModal = null;
let _mountResolvers = []; // mount 完成 promise resolvers

// 运行时底座拉取状态（loading / ok / error）——foundation 摘要加载失败
// 必须反馈到顶部摘要行，不能只藏在折叠区里
let _leFoundationStatus = "loading";

/**
 * mount 本地引擎运行时 controller。
 * 幂等：重复调用不创建重复监听。
 */
async function mountLocalRuntime() {
    if (_leController) return; // 已 mount

    const container = document.getElementById("le-cards-container");
    if (!container) return;

    const loadingRegion = document.getElementById("le-loading-region");
    const errorRegion = document.getElementById("le-error-region");
    const emptyRegion = document.getElementById("le-empty-region");

    if (loadingRegion) loadingRegion.hidden = false;
    if (errorRegion) errorRegion.hidden = true;
    if (emptyRegion) emptyRegion.hidden = true;

    // 注册受限 adapter hooks（FunASR / PaddleOCR 专属配置区）
    registerLocalEngineHooks();

    _leController = createLocalEngineController({
        onStateChange: (state) => {
            renderLocalEngineCards(container, state);
            // 同时刷新公共缓存区与顶部运行时摘要
            renderSharedStorage(state);
            renderRuntimeSummary(state);
        },
        onError: (err) => {
            console.error("[local-engine] error:", err);
            if (loadingRegion) loadingRegion.hidden = true;
            if (errorRegion) {
                errorRegion.hidden = false;
                const textEl = document.getElementById("le-error-text");
                if (textEl) textEl.textContent = err.message || String(err);
            }
        },
    });

    // 初始化 cleanup modal
    const cleanupModalEl = document.getElementById("le-cleanup-modal");
    const cleanupBody = document.getElementById("le-cleanup-body");
    const cleanupConfirmBtn = document.getElementById("le-cleanup-confirm");
    if (cleanupModalEl && cleanupBody && cleanupConfirmBtn) {
        _cleanupModal = createCleanupModal({
            modalEl: cleanupModalEl,
            bodyEl: cleanupBody,
            confirmBtn: cleanupConfirmBtn,
            onConfirm: async (targetIds, mode) => {
                if (!_leController) return;
                if (mode === "shared") {
                    // 公共缓存——不限定单引擎，用第一个 affected engine 的 cleanup
                    // 后端重新解析 target_id
                    const state = _leController.getState();
                    const shared = aggregateSharedTargets(state);
                    const target = shared.find((t) => t.target_id === targetIds[0]);
                    const engineId = target?.affected_engine_ids?.[0] || target?.engine_id || "funasr";
                    await _leController.cleanup(engineId, targetIds);
                } else {
                    // 单引擎清理——targetIds 已来自该引擎 storage
                    // engineId 由 open 调用时传入（见 onCleanupOpen）
                    const engineId = cleanupModalEl.dataset.leCleanupEngineId || "";
                    await _leController.cleanup(engineId, targetIds);
                }
                // 成功后刷新 status/storage
                await _leController.refreshStatus();
                await _leController.refreshAllStorage();
            },
        });
    }

    try {
        await _leController.mount();
        if (loadingRegion) loadingRegion.hidden = true;

        // 检查是否为空
        const state = _leController.getState();
        if (state.size === 0) {
            if (emptyRegion) emptyRegion.hidden = false;
        }

    // 0.22.6: 初始化运行时底座（只读）与「运行时与缓存」折叠区
    initFoundation();
    initRuntimeDetailsToggle();

    // 初始渲染顶部摘要（loading 态）
    renderRuntimeSummary(_leController.getState());

        // resolve mount resolvers
        for (const resolve of _mountResolvers) resolve();
        _mountResolvers = [];
    } catch (e) {
        console.error("[local-engine] mount failed:", e);
        if (loadingRegion) loadingRegion.hidden = true;
        if (errorRegion) {
            errorRegion.hidden = false;
            const textEl = document.getElementById("le-error-text");
            if (textEl) textEl.textContent = e.message || String(e);
        }
    }
}

/**
 * dispose 本地引擎运行时 controller。
 */
function disposeLocalRuntime() {
    // 注销受限 adapter hooks
    unregisterLocalEngineHooks();

    // 强制关闭 cleanup modal
    if (_cleanupModal) {
        _cleanupModal.dispose();
        _cleanupModal = null;
    }
    // 拒绝所有 pending mount resolvers
    for (const resolve of _mountResolvers) resolve();
    _mountResolvers = [];

    if (!_leController) return;
    _leController.dispose();
    _leController = null;

    const container = document.getElementById("le-cards-container");
    if (container) container.textContent = "";

    const loadingRegion = document.getElementById("le-loading-region");
    const errorRegion = document.getElementById("le-error-region");
    const emptyRegion = document.getElementById("le-empty-region");
    if (loadingRegion) loadingRegion.hidden = true;
    if (errorRegion) errorRegion.hidden = true;
    if (emptyRegion) emptyRegion.hidden = true;

    // 0.22.6: 清空底座与运行时摘要
    const foundationBody = document.getElementById("le-foundation-body");
    if (foundationBody) foundationBody.textContent = "";
    _leFoundationStatus = "loading";
    renderRuntimeSummary(null);
}

// ── 0.22.6: 运行时底座（只读）+ 运行时摘要（中密度重设计）────────────────

/**
 * 初始化「运行时与缓存」折叠区开关。
 * 运行时底座与公共缓存默认折叠，不再占据引擎卡片之前的纵向空间。
 */
function initRuntimeDetailsToggle() {
    const toggle = document.getElementById("le-runtime-details-toggle");
    const details = document.getElementById("le-runtime-details");
    if (!toggle || !details) return;
    if (toggle.dataset.leWired) return;
    toggle.dataset.leWired = "1";
    toggle.addEventListener("click", () => {
        const willExpand = details.hidden;
        details.hidden = !willExpand;
        toggle.setAttribute("aria-expanded", String(willExpand));
    });
}

/**
 * 渲染页面顶部运行时摘要行（运行正常度 · 引擎数 · 运行数 · 总占用）。
 * foundation 拉取失败时显示明确反馈，不静默。
 * @param {Map|null} state - engine_id → EngineStateEntry
 */
function renderRuntimeSummary(state) {
    const el = document.getElementById("le-runtime-summary");
    if (!el) return;
    const summary = computeRuntimeSummary(state || new Map(), {
        foundationError: _leFoundationStatus === "error",
    }, t);
    const cls = `le-runtime-summary le-tone-${summary.tone}`;
    if (el.dataset.sig === summary.text) return;
    el.dataset.sig = summary.text;
    el.className = cls;
    el.textContent = summary.text;
}

/**
 * 初始化运行时底座区域。
 * 绑定刷新和打开根目录按钮，拉取底座状态。
 */
function initFoundation() {
    const refreshBtn = document.getElementById("le-foundation-refresh");
    const openBtn = document.getElementById("le-foundation-open");

    // 绑定刷新按钮（只绑一次）
    if (refreshBtn && !refreshBtn.dataset.leWired) {
        refreshBtn.dataset.leWired = "1";
        refreshBtn.addEventListener("click", () => {
            refreshFoundation();
        });
    }

    // 绑定打开根目录按钮（只绑一次）
    if (openBtn && !openBtn.dataset.leWired) {
        openBtn.dataset.leWired = "1";
        openBtn.addEventListener("click", () => {
            if (_leController && !_leController.isDisposed()) {
                _leController.openRuntimeFolder().catch((e) => {
                    console.error("[local-engine] openRuntimeFolder failed:", e);
                });
            }
        });
    }

    // 首次拉取底座状态
    refreshFoundation();
}

/**
 * 刷新运行时底座状态。
 * 拉取结果（含失败）同步到顶部运行时摘要行。
 */
async function refreshFoundation() {
    if (!_leController || _leController.isDisposed()) return;

    const body = document.getElementById("le-foundation-body");
    if (!body) return;

    // loading 占位
    body.textContent = "";
    const loading = document.createElement("div");
    loading.className = "le-foundation-no-data";
    loading.textContent = t("local_engine.foundation.no_data");
    body.appendChild(loading);

    try {
        const foundation = await _leController.getFoundationStatus();
        if (_leController.isDisposed()) return;
        _leFoundationStatus = "ok";
        renderFoundationBody(body, foundation);
    } catch (e) {
        if (_leController.isDisposed()) return;
        _leFoundationStatus = "error";
        body.textContent = "";
        const errEl = document.createElement("div");
        errEl.className = "le-foundation-no-data";
        errEl.textContent = e?.message || String(e);
        body.appendChild(errEl);
    }
    renderRuntimeSummary(_leController.getState());
}

/**
 * 渲染底座内容。
 * DTO shape（0.22.6）：{ engines: [{engine_id, display_name, environment,
 * process(对象), service}], python_provider }——python_provider 是内部提供方
 * 概念，不展示；每个引擎渲染 环境/进程/服务 概览行。
 * @param {HTMLElement} body
 * @param {Object} foundation - RuntimeFoundationDto
 */
function renderFoundationBody(body, foundation) {
    body.textContent = "";
    const engines = foundation?.engines;
    if (!Array.isArray(engines) || engines.length === 0) {
        const empty = document.createElement("div");
        empty.className = "le-foundation-no-data";
        empty.textContent = t("local_engine.foundation.no_data");
        body.appendChild(empty);
        return;
    }

    for (const engine of engines) {
        const row = document.createElement("div");
        row.className = "le-foundation-row";

        const name = document.createElement("span");
        name.className = "le-info-label le-foundation-engine-name";
        name.textContent = engine.display_name || engine.engine_id || "—";

        const badges = document.createElement("span");
        badges.className = "le-info-value le-foundation-engine-status";
        const items = [
            {value: engine.environment, cls: statusClass(engine.environment)},
            {value: processDisplay(engine.process), cls: processClass(engine.process)},
            {value: engine.service, cls: statusClass(engine.service)},
        ];
        for (const item of items) {
            const badge = document.createElement("span");
            badge.className = `le-status-value status-badge ${item.cls}`;
            badge.textContent = item.value;
            badges.appendChild(badge);
        }

        row.appendChild(name);
        row.appendChild(badges);
        body.appendChild(row);
    }
}

/**
 * 渲染引擎卡片列表。
 * @param {HTMLElement} container
 * @param {Map} state - engine_id → EngineStateEntry
 */
function renderLocalEngineCards(container, state) {
    if (!container) return;

    const loadingRegion = document.getElementById("le-loading-region");
    const emptyRegion = document.getElementById("le-empty-region");
    if (loadingRegion) loadingRegion.hidden = true;

    if (state.size === 0) {
        if (emptyRegion) emptyRegion.hidden = false;
        return;
    }
    if (emptyRegion) emptyRegion.hidden = true;

    for (const [engineId, entry] of state) {
        if (!_leController) break;
        renderEngineCard(container, entry, _leController, {
            t: undefined, // 使用默认 t()
            onCleanupOpen: (engineId, entry, controller) => {
                openCleanupModal(engineId, entry, controller, "engine");
            },
        });
    }
}

/**
 * 渲染公共缓存区。
 * @param {Map} state
 */
function renderSharedStorage(state) {
    const sharedSection = document.getElementById("le-shared-storage");
    if (!sharedSection) return;

    const shared = aggregateSharedTargets(state);
    if (shared.length === 0) {
        sharedSection.hidden = true;
        return;
    }
    sharedSection.hidden = false;

    const content = document.getElementById("le-shared-storage-content");
    if (content) {
        content.textContent = "";
        for (const target of shared) {
            const row = document.createElement("div");
            row.className = "le-shared-target";
            const label = document.createElement("span");
            label.textContent = target.label_fallback || target.target_id;
            const size = document.createElement("span");
            size.textContent = formatSharedBytes(target.size_bytes);
            row.appendChild(label);
            row.appendChild(size);
            content.appendChild(row);
        }
    }

    const cleanupBtn = document.getElementById("le-shared-cleanup-btn");
    if (cleanupBtn) {
        cleanupBtn.hidden = false;
        // 只注册一次
        if (!cleanupBtn.dataset.leWired) {
            cleanupBtn.dataset.leWired = "1";
            cleanupBtn.addEventListener("click", () => {
                if (!_leController) return;
                const state = _leController.getState();
                const shared = aggregateSharedTargets(state);
                openCleanupModal(null, {storage: {targets: shared}}, _leController, "shared");
            });
        }
    }
}

/**
 * 格式化字节。
 */
function formatSharedBytes(bytes) {
    if (!bytes || bytes === 0) return "0 B";
    const mb = bytes / (1024 * 1024);
    if (mb < 1024) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

/**
 * 打开 cleanup modal。
 * @param {string|null} engineId - 单引擎清理时传入；公共缓存为 null
 * @param {Object} entry - EngineStateEntry
 * @param {Object} controller
 * @param {"engine"|"shared"} mode
 */
async function openCleanupModal(engineId, entry, controller, mode) {
    if (!_cleanupModal) return;
    const cleanupModalEl = document.getElementById("le-cleanup-modal");
    if (engineId && cleanupModalEl) {
        cleanupModalEl.dataset.leCleanupEngineId = engineId;
    }

    // storage 是异步拉取的（get_local_engine_storage 扫描磁盘）——刚进入
    // engines tab 时 entry.storage 可能还没到。缺失时现场拉一次再打开，
    // 避免清理面板空白。
    let targets = entry?.storage?.targets;
    if (!targets && engineId && typeof controller?.refreshStorage === "function") {
        try {
            await controller.refreshStorage(engineId);
            const fresh = controller.getState?.()?.get(engineId);
            targets = fresh?.storage?.targets;
        } catch (e) {
            console.warn("[settings] refresh storage for cleanup failed:", e);
        }
    }

    _cleanupModal.open({
        triggerEl: document.activeElement,
        targets: targets || [],
        mode,
    });
}

// Tab 切换：增删 active class + 生命周期回调
document.querySelectorAll(".tab").forEach((btn) => {
    btn.addEventListener("click", () => {
        document.querySelectorAll(".tab").forEach((t) => t.classList.remove("active"));
        document.querySelectorAll(".panel").forEach((p) => p.classList.remove("active"));
        btn.classList.add("active");
        const targetPanel = document.getElementById(btn.dataset.tab);
        if (targetPanel) targetPanel.classList.add("active");

        // ── 生命周期回调 ──
        const tab = btn.dataset.tab;

        // 离开 engines tab → dispose runtime
        if (tab !== "engines" && _leActive) {
            disposeLocalRuntime();
            _leActive = false;
        }

        // 进入 engines tab → mount runtime
        if (tab === "engines" && !_leActive) {
            _leActive = true;
            mountLocalRuntime().catch((e) => {
                console.error("[settings] mountLocalRuntime failed:", e);
            });
        }
    });
});

// ── runtime mount promise（供语音页跳转等待）───────────────────────────────────

/**
 * 等待 local runtime mount 完成。
 * 如果已 mount，立即 resolve。
 * 如果未 mount，激活 engines tab 并等待 mount。
 * @returns {Promise<void>}
 */
export function ensureLocalRuntimeMounted() {
    if (_leController && _leController.isMounted()) {
        return Promise.resolve();
    }
    return new Promise((resolve) => {
        _mountResolvers.push(resolve);
        // 如果 engines tab 未激活，激活它
        const enginesTabBtn = document.querySelector('.tab[data-tab="engines"]');
        if (enginesTabBtn && !enginesTabBtn.classList.contains("active")) {
            _leActive = true;
            mountLocalRuntime().then(() => {
                // mountLocalRuntime 内部会 resolve _mountResolvers
            }).catch((e) => {
                console.error("[settings] ensureLocalRuntimeMounted mount failed:", e);
                resolve(); // 即使失败也 resolve，调用方检查 error region
            });
        }
    });
}

/**
 * 等待指定 engine card 已渲染到 DOM。
 * 使用 MutationObserver 而非固定 rAF 次数。
 * @param {string} engineId
 * @param {number} [timeoutMs=5000]
 * @returns {Promise<HTMLElement|null>}
 */
export function waitForEngineCard(engineId, timeoutMs = 5000) {
    return new Promise((resolve) => {
        const container = document.getElementById("le-cards-container");
        if (!container) {
            resolve(null);
            return;
        }
        // 先检查是否已存在
        const existing = container.querySelector(`[data-engine-id="${engineId}"]`);
        if (existing) {
            resolve(existing);
            return;
        }
        // MutationObserver 等待卡片出现
        const observer = new MutationObserver(() => {
            const card = container.querySelector(`[data-engine-id="${engineId}"]`);
            if (card) {
                observer.disconnect();
                resolve(card);
            }
        });
        observer.observe(container, {childList: true, subtree: true});
        // 超时兜底
        setTimeout(() => {
            observer.disconnect();
            const card = container.querySelector(`[data-engine-id="${engineId}"]`);
            resolve(card || null);
        }, timeoutMs);
    });
}

// ── 配置加载与初始化 ─────────────────────────────────────────────────────────

/**
 * 应用配置到 UI
 * @param {Object} cfg - 配置对象
 */
function applyConfigToUI(cfg) {
    // 各 Tab 模块内部处理各自的 UI 更新
    // 这里只处理全局状态
    setCurrentConfig(cfg);
}

/**
 * 初始化设置页
 */
async function init() {
    try {
        // 图标 sprite 先注入（await 保证首屏无 FOUC —— tab 初始化时 innerHTML 拼图标就能立即用）
        await ensureSpriteLoaded();

        // 加载配置
        const cfg = await loadConfig();
        applyConfigToUI(cfg);

        // 应用主题
        applyTheme(cfg.theme || "auto");

        // 应用语言
        if (cfg.language) {
            setLang(cfg.language);
        }
        applyI18n();

        // 初始化各 Tab
        initGeneralTab(cfg);
        initHotkeyTab(cfg);
        initEnginesTab(cfg);
        initPluginsTab(cfg);
        initCapabilitiesTab();
        initNetworkTab(cfg);
        initContextTab(cfg);
        initStorageTab(cfg);
        initDebugTab(cfg);
        initAboutTab(cfg);
        initAITab();
        initVoiceTab();
        initChordTab();
        initMcPTab();
        initMcpServerSection();

        // 如果设置页打开时引擎 Tab 已是激活状态，立即 mount runtime
        const enginesTabBtn = document.querySelector('.tab[data-tab="engines"]');
        if (enginesTabBtn?.classList.contains("active")) {
            _leActive = true;
            mountLocalRuntime().catch((e) => {
                console.error("[settings] initial mountLocalRuntime failed:", e);
            });
        }

        // 语言切换时刷新已 mount 的卡片文案
        onLangChange(() => {
            if (_leController && _leController.isMounted()) {
                // 刷新底座文案 + 顶部运行时摘要
                refreshFoundation();
                renderRuntimeSummary(_leController.getState());

                const container = document.getElementById("le-cards-container");
                if (container) {
                    const state = _leController.getState();
                    // 清空重建以刷新 i18n 文案
                    container.textContent = "";
                    for (const [_, entry] of state) {
                        renderEngineCard(container, entry, _leController, {
                            t: undefined,
                            onCleanupOpen: (engineId, entry, controller) => {
                                openCleanupModal(engineId, entry, controller, "engine");
                            },
                        });
                    }
                }
            }
        });

        console.log("Settings initialized");
    } catch (e) {
        console.error("Failed to initialize settings:", e);
    }
}

// ── 事件监听 ─────────────────────────────────────────────────────────────────

// 窗口关闭按钮
document.getElementById("close-btn")?.addEventListener("click", hideSettingsWindow);

// ESC 隐藏窗口（与主窗口一致）
// 优先级降级：
//   1. 有可见 modal（cleanup / log / AI provider / model edit / context picker 等）→ 交给 modal 内部处理器关闭
//   2. 正在录制热键 → 录制流程会 preventDefault 吞键，此处不处理
//   3. 否则调用 hide_settings_window
document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    // 有 modal 打开：让 modal 内部的 Escape 处理器负责关闭
    // 检查 cleanup modal / log modal / 其他 modal-overlay
    const cleanupModalOpen = document.getElementById("le-cleanup-modal")?.hasAttribute("hidden") === false;
    const modalOpen = cleanupModalOpen
        || Array.from(document.querySelectorAll(".modal-overlay")).some(
            (el) => !el.classList.contains("hidden"),
        );
    if (modalOpen) return;
    // 正在录制热键：交给录制流程
    if (document.querySelector(".hotkey-btn.recording")) return;
    e.preventDefault();
    hideSettingsWindow();
});

// 窗口 shown 事件刷新配置（只刷新当前激活 tab，避免所有探测同时运行）
window.addEventListener("focus", async () => {
    try {
        const cfg = await loadConfig();
        applyConfigToUI(cfg);
    } catch (e) {
        console.error("Failed to refresh config on focus:", e);
    }

    // 只刷新当前激活 tab 的 runtime（如果当前在 engines tab）
    if (_leActive && _leController && _leController.isMounted()) {
        _leController.refreshStatus().catch((e) => {
            console.warn("[local-engine] refreshStatus on focus failed:", e);
        });
    }
});

// 窗口隐藏/销毁时 dispose runtime
window.addEventListener("blur", () => {
    // blur 不立即 dispose，因为可能是 tab 切换导致的短暂失焦
    // 真正的 dispose 在页面 unload 或显式关闭时触发
});

// 页面卸载时清理
window.addEventListener("beforeunload", () => {
    if (_cleanupModal) _cleanupModal.dispose();
    disposeLocalRuntime();
});

// ── 启动 ─────────────────────────────────────────────────────────────────────

init();
