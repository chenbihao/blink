//! 右键上下文菜单（0.5.3+）：独立 Popup 窗口模式。
//!
//! 所有右键菜单都通过独立无边框窗口渲染，真正突破主窗口边界限制，
//! 点击外部/ESC/左键自动关闭，菜单项动作回传主窗口执行。

import { queryEl, resultsEl } from "./dom.js";
import { activateItem } from "./actions.js";
import { openContainingFolder, openLnkTarget, resetItemHistory, launchApp, copyToClipboard, invoke } from "./api.js";
import { retrigger } from "./search.js";
import { t } from "./i18n.js";

/** 当前菜单数据（用于 Popup 点击时回调执行）。 */
let currentItems = [];

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

    if (!items.length) return;
    currentItems = items;

    // 估算菜单尺寸（精确匹配 CSS 实际大小）
    // item: padding 8px * 2 + 字体 ~16px = 32px
    // separator: 1px height + 6px * 2 margin = 13px
    // container padding: 6px * 2 = 12px
    const estimatedHeight = items.reduce((h, it) => h + (it.separator ? 13 : 32), 0) + 12;
    const estimatedWidth = 180;

    showPopupWindow(e.screenX, e.screenY, estimatedWidth, estimatedHeight, items);
  });

  // ESC 关闭
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") close();
  });
  // 注意：移除了 window.addEventListener("blur", close)
  // 点击 Popup 窗口时，主窗口先失焦会触发 blur，导致菜单提前关闭
  // Popup 窗口自己有关闭逻辑（点击菜单项/外部/ESC），主窗口被看门狗隐藏时也会联动关闭菜单

  // 监听来自 Popup 窗口的菜单项点击事件
  import("./tauri.js").then(({ listen }) => {
    listen("blink://context-menu-action", (event) => {
      const actionId = event.payload;
      const item = currentItems[actionId];
      if (item && item.run && !item.separator) {
        item.run();
      }
    });
  });
}

function closestLi(target) {
  if (!target || !target.closest || !resultsEl) return null;
  const li = target.closest("li");
  return li && resultsEl.contains(li) ? li : null;
}

/**
 * 调用后端创建独立 Popup 窗口显示菜单。
 * x, y 是屏幕坐标。
 */
function showPopupWindow(x, y, width, height, items) {
  // 智能翻转：屏幕右/下空间不够时，菜单显示在鼠标左/上方
  const screenWidth = window.screen.width;
  const screenHeight = window.screen.height;
  const finalX = x + width + 4 > screenWidth ? Math.max(4, x - width) : x;
  const finalY = y + height + 4 > screenHeight ? Math.max(4, y - height) : y;

  invoke("show_context_menu", {
    x: finalX,
    y: finalY,
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

  const hasSelection = queryEl && queryEl.selectionStart !== queryEl.selectionEnd;
  const items = [];
  if (hasSelection) {
    items.push({ label: t("menu.cut"), run: exec("cut") });
    items.push({ label: t("menu.copy"), run: exec("copy") });
  }
  items.push({ label: t("menu.paste"), run: paste });
  items.push({ separator: true });
  items.push({ label: t("menu.selectAll"), run: exec("selectAll") });
  items.push({ separator: true });
  items.push({ label: t("menu.openSettings"), run: () => launchApp("__BLINK_ACTION_OPEN_SETTINGS__") });
  items.push({ label: t("menu.exit"), run: () => launchApp("__BLINK_ACTION_EXIT__"), danger: true });
  return items;
}

// ── 结果项菜单 ──────────────────────────────────────────────────────────────

function itemMenu(li) {
  const source = li.dataset.source || "";
  const lnkPath = li.dataset.lnkPath || "";
  const isRealPath = lnkPath && !lnkPath.startsWith("__BLINK_ACTION_");
  const items = [];

  items.push({ label: t("menu.open"), run: () => activateItem(readData(li)) });

  if (isRealPath && (source === "file" || source === "start_menu")) {
    const isShellPath = lnkPath.toLowerCase().startsWith("shell:");
    const fileName = lnkPath.split(/[\\/]/).pop() || lnkPath;
    const baseName = fileName.replace(/\.[^.]*$/, "");
    const dirPath = lnkPath.replace(/[\\/][^\\/]*$/, "");
    const isLnk = lnkPath.toLowerCase().endsWith(".lnk");

    if (isShellPath) {
      // UWP/MSIX 应用：无文件路径，隐藏文件相关菜单项
      items.push({ separator: true });
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

  const isResultLike =
    source === "calc" || li.classList.contains("calc-result") || source.startsWith("builtin.");
  if (isResultLike) {
    const result =
      li.dataset.actionPayload ||
      li.dataset.calcValue ||
      li.querySelector(".item-name")?.textContent;
    if (result) {
      items.push({ label: t("menu.copyResult"), run: () => copyText(result) });
    }
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

function readData(li) {
  return {
    lnkPath: li.dataset.lnkPath,
    calcValue: li.dataset.calcValue,
    payload: li.dataset.actionPayload,
    action: {
      kind: li.dataset.actionKind,
      hint: li.dataset.actionHint,
    },
  };
}
