//! 右键上下文菜单（0.5.3+）：独立 Popup 窗口模式。
//!
//! 所有右键菜单都通过独立无边框窗口渲染，真正突破主窗口边界限制，
//! 点击外部/ESC/左键自动关闭，菜单项动作回传主窗口执行。

import { queryEl, resultsEl } from "./dom.js";
import { activateItem } from "./actions.js";
import { openContainingFolder, openLnkTarget, resetItemHistory, runBuiltinAction, copyToClipboard, deleteClipboardItem, deleteClipboardImage, hideWindow, invoke, triggerChord, listChordActions, pinClipboardImage, showStickyManager, createStickyNote, showStickyWindow } from "../shared/api.js";
import { retrigger } from "./search.js";
import { t, getLang } from "../i18n/index.js";
import { showActionError } from "./action-error.js";
import { EVENTS } from "../shared/event-names.js";
import { parse as parseColor } from "../shared/color.js";
import * as clipboardMode from "./clipboard-mode.js";

/** 0.20.3：判断 shortcut 文本是否为颜色格式预览（#xxx / rgb(...) / hsl(...)）。 */
function isColorShortcut(s) {
  return !!s && (s.startsWith("#") || s.startsWith("rgb") || s.startsWith("hsl"));
}

/**
 * 0.20.3：为颜色项构建 HEX/RGB/HSL 复制 + 贴便签菜单项。
 * @param {string} colorHex — canonical HEX
 * @returns {object[]} 菜单项数组（不含前导 separator，由调用方追加）
 */
function colorMenuItems(colorHex) {
  const colorResult = parseColor(colorHex);
  if (!colorResult) return [];
  return [
    { label: t("menu.copyHex"), shortcut: colorResult.hex, run: () => { copyToClipboard(colorResult.hex).catch((e) => console.error("copy hex failed:", e)); hideWindow(); } },
    { label: t("menu.copyRgb"), shortcut: colorResult.rgb, run: () => { copyToClipboard(colorResult.rgb).catch((e) => console.error("copy rgb failed:", e)); hideWindow(); } },
    { label: t("menu.copyHsl"), shortcut: colorResult.hsl, run: () => { copyToClipboard(colorResult.hsl).catch((e) => console.error("copy hsl failed:", e)); hideWindow(); } },
    { separator: true },
    {
      label: t("menu.createStickyFromColor"),
      run: async () => {
        try {
          const note = await createStickyNote(colorResult.hex);
          await showStickyWindow(note.id, true);
          hideWindow();
        } catch (e) {
          showActionError("create_sticky_from_color", e);
        }
      },
    },
  ];
}

/** 当前菜单数据（用于 Popup 点击时回调执行）。 */
let currentItems = [];

/** Chord 动作缓存（shown 时预拉取，右键时同步读）。只含 semantic=tap 的动作。 */
let cachedChordActions = [];

/** 绑定全局右键事件（main.js 装配时调用一次）。 */
export function init() {
  // 左键点击任何地方 → 关闭菜单
  document.addEventListener("mousedown", (e) => {
    if (e.button === 0) close();
  });

  document.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    close(); // 先关闭旧的
    const target = e.target;

    let items = [];
    const li = closestLi(target);
    if (li) {
      items = itemMenu(li);
    } else {
      items = unifiedMenu();
    }

    // 0.20.2: 剪贴板模式下有多选时，在菜单顶部插入「复制选中的 N 条」
    if (clipboardMode.isActive() && clipboardMode.hasSelection()) {
      const count = clipboardMode.getSelectionCount();
      items = [
        { label: t("menu.copySelected", { count }), run: () => clipboardMode.batchCopy() },
        { separator: true },
        ...items,
      ];
    }

    if (!items.length) return;
    currentItems = items;

    // 估算菜单尺寸（CSS 像素，后端会按目标显示器 DPI 缩放为物理像素）
    // item: padding 8px * 2 + font 13px * line-height 1.5 ≈ 35.5px
    // separator: 1px height + margin 4px * 2 = 9px
    // container padding: var(--space-sm) 8px * 2 = 16px
    //
    // 缓冲量（--shadow-card 的三层需要呼吸位，否则 overflow:hidden 会把边剪掉）：
    //   - 宽：右侧 box-shadow spread 1px 外描边 + CSS→物理像素 round 半像素累计 → +8
    //   - 高：底部 spread 1px + `0 1px 2px` / `0 24px 48px` 阴影想画在窗口内 → +8
    // 不给缓冲会体感"右边少 2~3px、下面少 4~6px"。
    const H_BUFFER = 8;
    const V_BUFFER = 8;
    const rows = items.reduce((h, it) => h + (it.separator ? 9 : 36), 0);
    const estimatedHeight = rows + 16 + V_BUFFER;
    // 0.20.3：颜色项 shortcut 同行显示，但格式预览较长，需要更宽的菜单
    const hasColorShortcut = items.some((it) => isColorShortcut(it.shortcut));
    const estimatedWidth = (hasColorShortcut ? 260 : 180) + H_BUFFER;

    // 光标物理坐标由后端 GetCursorPos 直接读取——MouseEvent.screenX/Y 在 WebView2
    // 里是 CSS 像素，高 DPI 屏当物理像素用会偏 1/3；索性不传，避免 dpr 猜谜。
    showPopupWindow(estimatedWidth, estimatedHeight, items);
  });

  // ESC 关闭
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") close();
  });
  // 注意：移除了 window.addEventListener("blur", close)
  // 点击 Popup 窗口时，主窗口先失焦会触发 blur，导致菜单提前关闭
  // Popup 窗口自己有关闭逻辑（点击菜单项/外部/ESC），主窗口被看门狗隐藏时也会联动关闭菜单

  // 监听来自 Popup 窗口的菜单项点击事件
  import("../shared/tauri.js").then(({ listen }) => {
    listen(EVENTS.CONTEXT_MENU_ACTION, (event) => {
      const actionId = event.payload;
      const item = currentItems[actionId];
      if (item && item.run && !item.separator) {
        item.run();
      }
    });

    // 主窗 shown 时预拉取 chord 动作，右键时同步读缓存（无延迟）
    listen(EVENTS.SHOWN, refreshChordActions);
  });
}

/** 拉取 chord 动作并缓存（只保留 tap 语义——截图/剪贴板等，排除 hold 语义的语音输入）。 */
async function refreshChordActions() {
  try {
    const cfg = await invoke("get_config");
    if (!cfg || cfg.chord_enabled !== true) {
      cachedChordActions = [];
      return;
    }
    const all = await listChordActions();
    cachedChordActions = all.filter((a) => a.semantic === "tap");
  } catch {
    cachedChordActions = [];
  }
}

function closestLi(target) {
  if (!target || !target.closest || !resultsEl) return null;
  const li = target.closest("li");
  return li && resultsEl.contains(li) ? li : null;
}

/**
 * 调用后端创建独立 Popup 窗口显示菜单。
 * `width/height` 是 CSS 像素尺寸。光标物理坐标 + DPI 缩放 + 多屏边界翻转
 * 均由后端 `clamp_context_menu`（`GetCursorPos`）处理。
 */
function showPopupWindow(width, height, items) {
  invoke("show_context_menu", {
    width: width,
    height: height,
    items: JSON.stringify(items),
  }).catch((e) => {
    console.error("Failed to show context menu popup:", e);
  });
}

/** 关闭菜单（通知后端销毁 Popup 窗口）。 */
function close() {
  invoke("hide_context_menu").catch(() => {});
}

// ── 各区域菜单构建 ──────────────────────────────────────────────────────────

function unifiedMenu() {
  const exec = (cmd) => async () => {
    queryEl?.focus();
    setTimeout(() => {
      try {
        document.execCommand(cmd);
      } catch (e) {
        console.error("execCommand failed:", e);
      }
    }, 10);
  };
  const paste = async () => {
    queryEl?.focus();
    setTimeout(async () => {
      try {
        const text = await navigator.clipboard.readText();
        if (queryEl) {
          const start = queryEl.selectionStart || 0;
          const end = queryEl.selectionEnd || 0;
          queryEl.value = queryEl.value.slice(0, start) + text + queryEl.value.slice(end);
          queryEl.selectionStart = queryEl.selectionEnd = start + text.length;
          queryEl.dispatchEvent(new Event("input", { bubbles: true }));
        }
      } catch (e) {
        try {
          document.execCommand("paste");
        } catch (e2) {
          console.error("paste failed:", e2);
        }
      }
    }, 10);
  };

  // 0.18.0: 除 input 选区外，也检查 div 选区（AI 输出区选中文本）
  const inputSelection = queryEl && queryEl.selectionStart !== queryEl.selectionEnd;
  const divSelection = (() => {
    try {
      const sel = window.getSelection();
      return sel && sel.toString().length > 0 ? sel.toString() : "";
    } catch {
      return "";
    }
  })();
  const hasText = queryEl && queryEl.value && queryEl.value.length > 0;
  const items = [];
  if (inputSelection) {
    items.push({ label: t("menu.cut"), run: exec("cut") });
    items.push({ label: t("menu.copy"), run: exec("copy") });
  } else if (divSelection) {
    // 0.18.0: AI 输出区 div 选区——复制走 copyToClipboard 而非 execCommand
    items.push({ label: t("menu.copy"), run: () => copyToClipboard(divSelection) });
  }
  items.push({ label: t("menu.paste"), run: paste });
  // 全选门控（0.16.0）：无文本时不显示全选——select all 空输入无意义
  if (hasText) {
    items.push({ separator: true });
    items.push({ label: t("menu.selectAll"), run: exec("selectAll") });
  }

  // Chord 快捷入口（tap 语义——截图/剪贴板等，排除 hold 语义的语音输入）
  // 0.18.4：快捷键用括号包裹并淡色显示（shortcut 字段单独渲染）
  if (cachedChordActions.length) {
    items.push({ separator: true });
    const isZh = getLang() === "zh";
    for (const a of cachedChordActions) {
      const keyLabel = a.key === " " ? "Space" : a.key.toUpperCase();
      const shortcut = isZh
        ? `（Alt+${keyLabel}）`
        : ` (Alt+${keyLabel})`;
      items.push({
        label: a.label,
        shortcut,
        run: () => triggerChord(a.key),
      });
    }
  }

  // 0.20.0：显式“从查询创建便签”动作——Alt+S 永远创建空白便签后，
  // 用户仍可通过右键菜单从当前 query 文本创建带内容的便签
  if (hasText) {
    items.push({ separator: true });
    items.push({
      label: t("menu.createStickyFromQuery"),
      run: async () => {
        try {
          const text = queryEl.value;
          const note = await createStickyNote(text);
          await showStickyWindow(note.id, true);
          hideWindow();
        } catch (e) {
          showActionError("create_sticky_from_query", e);
        }
      },
    });
  }

  // 0.18.4：新增便签管理入口（独立分组，在打开设置上方）
  items.push({ separator: true });
  items.push({ label: t("menu.stickyManager"), run: () => showStickyManager() });
  items.push({ separator: true });
  items.push({ label: t("menu.openSettings"), run: () => runBuiltinAction("open_settings") });
  items.push({ label: t("menu.exit"), run: () => runBuiltinAction("exit_blink"), danger: true });
  return items;
}

// ── 结果项菜单 ──────────────────────────────────────────────────────────────

/** action.kind → 默认菜单文案的 i18n key 映射。有 hint 时优先用 t(hint)。 */
const KIND_LABELS = {
  copy: () => t("menu.copy"),
  open: () => t("menu.open"),
};

/**
 * 0.16.1: 右键菜单按 actions 数组组装。
 * (a) 先展开 actions 全部动作（label 由 kind 派生，有 hint 用 hint）
 * (b) 若 lnkPath 存在且为真实路径（首个 action 非 run），追加文件管理附加项
 * (c) 0.16.0 止血分支删除，由 actions 派生取代
 */
function itemMenu(li) {
  const source = li.dataset.source || "";
  const lnkPath = li.dataset.lnkPath || "";
  let actions = [];
  try {
    actions = JSON.parse(li.dataset.actions || "[]");
  } catch (e) {
    console.error("actions parse failed:", e);
  }
  const firstKind = actions[0]?.kind || "";
  // 内置动作（kind=run）不是真实文件路径，隐藏"打开文件夹/复制路径"等文件相关菜单
  const isRealPath = lnkPath && firstKind !== "run";
  const items = [];

  // (a) 展开 actions 全部动作（hint 存 i18n key，用 t() 渲染）
  // 0.20.3：颜色项不再跳过 (a) 段——当做文本处理，默认 Copy 动作正常显示。
  for (const action of actions) {
    const label = action.hint ? t(action.hint) : (KIND_LABELS[action.kind]?.() ?? null);
    if (!label) continue; // 无标签的动作（如无 hint 的 run）跳过
    items.push({
      label,
      run: () => activateItem({ ...readData(li), actions: [action] }),
    });
  }

  // P2-#18: edit/pin 已由后端 actions 声明（edit_text_item / pin_text_item），
  // (a) 段从 actions 数组派生即可，不再在此硬编码追加。

  // 0.20.3：颜色结果项或剪贴板颜色项——追加多格式复制 + 贴便签
  let colorHex = null;
  if (source === "color") {
    colorHex = actions[0]?.payload || "";
  } else if (source === "clipboard") {
    // 剪贴板文本项降级检测：如果文本恰好是颜色字面量
    const clipText = li.querySelector(".item-name")?.textContent || "";
    const clipColor = parseColor(clipText);
    if (clipColor) colorHex = clipColor.hex;
  }
  if (colorHex) {
    const colorItems = colorMenuItems(colorHex);
    if (colorItems.length) {
      items.push({ separator: true });
      items.push(...colorItems);
    }
  }

  // 0.16.5：剪贴板图片项追加"钉图"动作
  // 图片项的 lnkPath 存的是 image_id，调 pin_clipboard_image 后端命令
  const isImage = li.dataset.isImage === "true";
  if (isImage && lnkPath) {
    items.push({ separator: true });
    items.push({
      label: t("menu.pin"),
      run: () => {
        pinClipboardImage(lnkPath).catch((e) => console.error("pinClipboardImage failed:", e));
        hideWindow();
      },
    });
  }

  // 0.16.13：剪贴板文本项追加“删除”动作
  // 0.17.0：图片项也追加“删除”，按 isImage 分发到不同后端命令
  // hitId 在首个 action（Copy）上，为 clipboard_history 表主键（仅文本项）
  const clipboardId = actions[0]?.hitId;
  if (source === "clipboard") {
    if (isImage && lnkPath) {
      // 图片项：lnkPath 持有 image_id（engine.rs image 分支投影）
      items.push({ separator: true });
      items.push({
        label: t("menu.delete"),
        run: () => {
          deleteClipboardImage(lnkPath)
            .then(() => retrigger())
            .catch((e) => console.error("deleteClipboardImage failed:", e));
        },
        danger: true,
      });
    } else if (clipboardId) {
      // 文本项：clipboardId 为 clipboard_history 表主键
      items.push({ separator: true });
      items.push({
        label: t("menu.delete"),
        run: () => {
          deleteClipboardItem(clipboardId)
            .then(() => retrigger())
            .catch((e) => console.error("deleteClipboardItem failed:", e));
        },
        danger: true,
      });
    }
  }

  // (b) 文件管理附加项（不混入 capability actions，仍由前端按 lnkPath 追加）
  if (isRealPath && (source === "file" || source === "start_menu")) {
    const isShellPath = lnkPath.toLowerCase().startsWith("shell:");
    const fileName = lnkPath.split(/[\\/]/).pop() || lnkPath;
    const baseName = fileName.replace(/\.[^.]*$/, "");
    const dirPath = lnkPath.replace(/[\\/][^\\/]*$/, "");
    const isLnk = lnkPath.toLowerCase().endsWith(".lnk");

    items.push({ separator: true });
    if (isShellPath) {
      // UWP/MSIX 应用：无文件路径，隐藏文件相关菜单项
      items.push({ label: t("menu.copyId"), run: () => copyText(lnkPath) });
    } else {
      // 传统桌面应用：完整菜单
      items.push({ label: t("menu.openFolder"), run: () => { openContainingFolder(lnkPath).catch((e) => console.error(e)); } });
      if (isLnk) {
        items.push({ label: t("menu.openLnkTarget"), run: () => { openLnkTarget(lnkPath).catch((e) => console.error(e)); } });
      }
      items.push({ separator: true });
      items.push({ label: t("menu.copyPath"), run: () => copyText(dirPath) });
      items.push({ label: t("menu.copyFullPath"), run: () => copyText(lnkPath) });
      items.push({ label: t("menu.copyName"), run: () => copyText(baseName) });
      items.push({ label: t("menu.copyFullName"), run: () => copyText(fileName) });
    }
    items.push({ separator: true });
    items.push({ label: t("menu.resetHistory"), run: () => { resetItemHistory(lnkPath).then(() => retrigger()).catch((e) => console.error(e)); }, danger: true });
  }

  return items;
}

async function copyText(text) {
  try {
    await copyToClipboard(text);
  } catch (e) {
    console.error("clipboard write failed:", e);
  }
}

/** 0.16.1: 从 <li> 读出激活所需数据，返回 actions 数组。 */
function readData(li) {
  let actions = [];
  try {
    actions = JSON.parse(li.dataset.actions || "[]");
  } catch (e) {
    console.error("actions parse failed:", e);
  }
  return {
    lnkPath: li.dataset.lnkPath,
    calcValue: li.dataset.calcValue,
    isError: li.dataset.isError === "true",
    actions,
  };
}
