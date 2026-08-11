//! 输入状态前端投影。
//!
//! 后端输入状态的前端投影。改为：
//! 1. 注册 `INPUT_STATE_CHANGED` listener（先注册）
//! 2. 调用 `register_main_input_view` 获取初始快照 + view_epoch
//! 3. 以 UI revision 去重/拒绝旧状态
//! 4. 投影 `body.alt-active` / `body.chord-visible`
//! 5. query 是否为空 / AI mode 变化时上报后端

import { listen } from "../shared/tauri.js";
import { EVENTS } from "../shared/event-names.js";
import { registerMainInputView, updateMainInputContext } from "../shared/api.js";
import { createInputStateCore } from "./input-state-core.js";
import * as chord from "./chord.js";
import * as aiMode from "./ai-mode.js";
import * as clipboardMode from "./clipboard-mode.js";
import * as cmdMode from "./command-mode.js";
import { queryEl } from "./dom.js";

const core = createInputStateCore();

// ── UI 投影 ──────────────────────────────────────────────────────────────────

/**
 * 把后端 InputUiState 投影到 DOM。
 *
 * - `body.alt-active` ← `state.altDown`（物理真相）
 * - `body.chord-visible` ← `state.altDown && chord.isEnabled()`（前端本地投影）
 */
function projectUi() {
  const state = core.state;
  if (!state) return;

  const altDown = state.altDown;
  // 0.19.15: 独占模式（AI / 剪贴板 / 命令）下不显示 chord 待命提示——
  // 用户已在特定模式中交互，Alt+字母待命列表无意义。
  const inExclusiveMode =
    aiMode.isActive() || clipboardMode.isActive() || cmdMode.isActive();
  const chordEligible = chord.isEnabled() && !inExclusiveMode;
  const showChord = altDown && chordEligible;

  const prevChordVisible = document.body.classList.contains("chord-visible");
  document.body.classList.toggle("alt-active", altDown);
  document.body.classList.toggle("chord-visible", showChord);

  if (showChord !== prevChordVisible) {
    chord.notifyVisibilityChange();
  }
}

// ── Context 上报 ──────────────────────────────────────────────────────────────

/**
 * 检测 query 是否为空 + AI mode 是否活跃，变化时上报后端。
 * 只在离散状态变化时发送，不逐字符上报。
 */
function reportContext() {
  if (core.viewEpoch === 0) return; // 未注册
  const queryEmpty = !queryEl.value.trim();
  const aiModeActive = aiMode.isActive();
  const ctx = core.updateContext(queryEmpty, aiModeActive);
  if (ctx) {
    updateMainInputContext(ctx.viewEpoch, ctx.revision, ctx.queryEmpty, ctx.aiMode).catch(
      (e) => console.warn("[input-state] update_main_input_context 失败", e),
    );
  }
}

// ── 初始化 ────────────────────────────────────────────────────────────────────

/** 初始化输入状态桥接（main.js 启动时调一次）。 */
export function init() {
  // 1. 先注册 INPUT_STATE_CHANGED listener（§3.9 要求 listener 先于 register）
  listen(EVENTS.INPUT_STATE_CHANGED, (event) => {
    if (core.applyState(event.payload)) {
      projectUi();
    }
  });

  // 2. 注册 view，获取初始快照 + view_epoch
  const queryEmpty = !queryEl.value.trim();
  const aiModeActive = aiMode.isActive();
  registerMainInputView(queryEmpty, aiModeActive)
    .then((result) => {
      core.setViewEpoch(result.viewEpoch);
      core.applyState(result.state);
      projectUi();
    })
    .catch((e) => {
      console.warn("[input-state] register_main_input_view 失败", e);
    });

  // 3. 监听搜索框 input 事件，检测 query 空/非空切换
  queryEl.addEventListener("input", reportContext);

  // 4. 用 MutationObserver 检测 AI 模式切换（不修改 ai-mode.js）
  const aiModeEl = document.getElementById("ai-mode");
  if (aiModeEl) {
    const observer = new MutationObserver(() => reportContext());
    observer.observe(aiModeEl, { attributes: true, attributeFilter: ["hidden"] });
  }
}

// ── 公开接口 ──────────────────────────────────────────────────────────────────

/**
 * 当前 Alt 是否按下（供 keyboard.js 的 `inputState.isAltDown() || e.altKey` 用）。
 *
 * 后端快照抵抗 WebView synthetic keyup，事件自带 `altKey` 覆盖状态事件
 * 尚未到达的即时边沿。
 */
export function isAltDown() {
  const state = core.state;
  return state ? state.altDown : false;
}

/**
 * SHOWN 后调用：query 被清空后上报 context（programmatic value= 不触发 input 事件）。
 */
export function onShown() {
  reportContext();
}

/**
 * chord 配置刷新+动作列表刷新后调用：重新投影 chord-visible。
 *
 * chord.refresh() 完成后 chord.isEnabled() 可能从 false 变 true，
 * 需要重算 chord-visible。
 */
export function reevaluate() {
  projectUi();
}
