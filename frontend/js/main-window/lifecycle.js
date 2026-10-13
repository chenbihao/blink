//! 窗口生命周期：响应后端 blink://shown / blink://hidden，复位输入与列表。

import {invoke, listen} from "../shared/tauri.js";
import {resetSuggestionSession} from "../shared/api.js";
import {EVENTS} from "../shared/event-names.js";
import {aiQueryEl, queryEl, appEl} from "./dom.js";
import {syncWindowSize, setWindowVisibility, acceptsWindowRevision, lockWindowPlacement} from "./window-size.js";
import * as results from "./results.js";
import * as search from "./search.js";
import * as chord from "./chord.js";
import * as ghost from "./ghost.js";
import * as aiMode from "./ai-mode.js";
import * as cmdMode from "./command-mode.js";
import * as clipboardMode from "./clipboard-mode.js";
import * as inputState from "./input-state.js";
import * as autosuggestConfig from "./autosuggest-config.js";
import * as dictation from "./dictation.js";
import {applyGlassOpacityFromConfigData, applyThemeFromConfigData} from "../shared/theme.js";
import {applyI18nFromConfigData} from "../i18n/index.js";
import {createConfigRefresher} from "../shared/config-sync.js";

// 启动、所有模式的 SHOWN 和配置广播共用单飞读取，失效前发出的响应不得回填。
const configRefresher = createConfigRefresher({
    read: () => invoke("get_config"),
    apply: (cfg) => {
        if (!cfg) return;
        applyThemeFromConfigData(cfg);
        applyGlassOpacityFromConfigData(cfg);
        applyI18nFromConfigData(cfg);
        clipboardMode.refreshWheelMode(cfg.clipboard);
        results.refreshMaxResultsFromConfigData(cfg);
        autosuggestConfig.refreshFromConfigData(cfg);
        chord.refreshFromConfigData(cfg).then(() => inputState.reevaluate());
    },
    onError: (error) => console.error("主窗口配置刷新失败", error),
});

/** 注册生命周期事件监听。 */
export function init() {
    // 先订阅再首读，覆盖预热后直接唤起剪贴板、以及隐藏期间的设置变更。
    listen(EVENTS.CONFIG_CHANGED, () => configRefresher.refresh())
        .then(() => configRefresher.refresh())
        .catch((error) => console.error("主窗口配置监听失败", error));
    // 捕获输入、选择和滚动的起点，随后到达的 caret 不再移动窗口。
    document.addEventListener("keydown", (event) => {
        if (!["Alt", "Control", "Shift", "Meta"].includes(event.key)) lockWindowPlacement();
    }, true);
    queryEl.addEventListener("input", lockWindowPlacement);
    appEl.addEventListener("pointerdown", lockWindowPlacement, true);
    appEl.addEventListener("wheel", lockWindowPlacement, {capture: true, passive: true});

    let observedRevision = -1;
    const onShown = (event) => {
        if (!setWindowVisibility(true, event.payload?.revision)) return;
        observedRevision = event.payload.revision;
        void configRefresher.refresh();
        // SHOWN 自带模式，预告事件丢失时也不会先跑普通搜索。
        if (event.payload?.clipboard) clipboardMode.enter();
        else clipboardMode.reset();
        // 0.20-fix: 防御性清理 readOnly 残留（HIDDEN 已调 forceClearReadOnly，但双保险）
        // 0.21.x: 但 chord 待命态（Alt 按下）必须保留 readOnly——SHOWN 时机窗内
        // native 独占会话可能尚未建立（window.visible 翻转竞态），此刻清掉 readOnly
        // 会让 chord 键落进输入法（onChordTrigger 对 229/组字放行）。HIDDEN 已负责清
        // 残留，这里仅在非待命态防御性清理。
        if (!inputState.inChordStandby()) {
            inputState.forceClearReadOnly();
        }
        // 0.17.6: AiMode 下 SHOWN 只 focus AI 输入框，不重置搜索状态
        if (aiMode.isActive()) {
            aiQueryEl.focus();
            inputState.onShown();
            return;
        }
        // 剪贴板模式下保持过滤词与列表，不清 query、不跑默认上下文搜索（模式 bypass
        // 搜索管线，重复查询还会烧 suggestion impression 计数）
        if (clipboardMode.isActive()) {
            queryEl.focus();
            inputState.onShown();
            // ready 标记区分首搜中的临时布局与结果完成后的真实高度。
            syncWindowSize();
            return;
        }
        queryEl.value = "";
        // 先解冻 ghost（上次录音可能残留 frozen），再 reset 让 ghost.clear 正常清 DOM
        ghost.unfreeze();
        // 清除语音残留（voice-active / voice-error / 指示器），确保唤起时 chord 提示不被隐藏
        dictation.resetOnShown();
        search.reset(); // 作废在途搜索请求
        results.clear();
        cmdMode.reset(); // 0.18.6: 复位命令模式
        queryEl.focus();
        // 0.8.3 §4.13 P0-1：唤起瞬间发一次空 query,拉后端产的 Context Suggestion（Ghost）。
        // 0.8.2 此调用是拉 Context 召回条目（AppEntry）;0.8.3 契约变更后 Context 不产
        // candidate,该调用现在的作用是拿 `response.suggestion` 走 Ghost 通道——函数名保留,
        // 内部实现在 search.js 已重写。
        search.fetchContextSuggestions();
        // query 被清空后上报 context（programmatic value= 不触发 input 事件）
        inputState.onShown();
    };

    const onHidden = (event) => {
        if (!setWindowVisibility(false, event.payload?.revision)) return;
        observedRevision = event.payload.revision;
        // Alt/Chord 状态由后端 INPUT_STATE_CHANGED 事件驱动，不在 HIDDEN 补偿清理。
        // 0.20-fix: 强制恢复 readOnly = false，防止 chord 待命态 readOnly 残留
        // （WebView 可能收不到 keyup，wasChordStandby 残留 true 导致输入框永久 readOnly）
        inputState.forceClearReadOnly();
        // 0.17.6: AiMode 下 HIDDEN 也清理 AI 状态
        if (aiMode.isActive()) {
            aiMode.exitAiMode();
        }
        clipboardMode.reset(); // 0.19.15: 复位剪贴板模式
        queryEl.value = "";
        search.reset();
        results.clear();
        cmdMode.reset(); // 0.18.6: 复位命令模式
        // 0.24.3 §3.8：建议会话结束——后端降频计数与遥测环形表清零（每次唤起是新会话）
        resetSuggestionSession().catch(() => {});
    };
    // 先监听再拉只读快照，覆盖首次 show 早于页面监听器就绪的窗口。
    Promise.all([listen(EVENTS.SHOWN, onShown), listen(EVENTS.HIDDEN, onHidden)])
        .then(() => invoke("get_main_window_layout_state"))
        .then((state) => {
            if (!state || state.revision <= observedRevision) return;
            (state.visible ? onShown : onHidden)({payload: state});
        }).catch((error) => console.warn("主窗口初始布局状态获取失败", error));

// 0.8.5 §6.4：Chord Alt+C 剪贴板改走 fill-query——后端 ClipboardHistoryAction
// execute 里 window::invoke + emit "剪贴板 " → 前端填搜索框 + 触发 ClipboardEngine 召回。
// 0.19.14：改用 search.fillQuery 跳过 40ms 防抖（程序化输入无需合并）。
// 0.19.15：改为 CHORD_ENTER_MODE，前端进入剪贴板独占模式，bypass SearchService pipeline。
    listen(EVENTS.CHORD_ENTER_MODE, (event) => {
        const {mode, preserveQuery = false, revision} = event.payload ?? {};
        if (revision !== undefined && !acceptsWindowRevision(revision)) return;
        if (mode === "clipboard") {
            void configRefresher.refresh();
            clipboardMode.enter({preserveQuery});
        }
    });

    // 0.9.2.1：剪贴板变化 / 选区就绪 → AwarenessSnapshot 已局部刷新 → 用当前
    // query 重跑一次让 Context Ghost / AI 四筛子读到新值。retrigger 内部区分空/非空
    // query 分别走 fetchContextSuggestions / onInput。
    // **0.x 闪烁修复**：retrigger 在空 query 时直接调 fetchContextSuggestions，
    // 不先 clear results/ghost——避免「旧结果消失 → 新结果到达」的视觉闪烁。
    // 后端只在主窗口可见时才 emit，前端无需再判可见。
    listen(EVENTS.AWARENESS_UPDATED, () => {
        search.retrigger();
    });
}
