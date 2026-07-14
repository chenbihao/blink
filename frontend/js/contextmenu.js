//! 右键上下文菜单（0.5.3+）：独立 Popup 窗口模式。
//!
//! 所有右键菜单都通过独立无边框窗口渲染，真正突破主窗口边界限制，
//! 点击外部/ESC/左键自动关闭，菜单项动作回传主窗口执行。

import { queryEl, resultsEl } from "./dom.js";
import { activateItem } from "./actions.js";
import { openContainingFolder, openLnkTarget, resetItemHistory, runBuiltinAction, copyToClipboard, invoke } from "./api.js";
import { retrigger } from "./search.js";
import { t } from "./i18n/index.js";

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
    const estimatedWidth = 180 + H_BUFFER;

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
  items.push({ label: t("menu.openSettings"), run: () => runBuiltinAction("open_settings") });
  items.push({ label: t("menu.exit"), run: () => runBuiltinAction("exit_blink"), danger: true });
  return items;
}

// ── 结果项菜单 ──────────────────────────────────────────────────────────────

function itemMenu(li) {
  const source = li.dataset.source || "";
  const lnkPath = li.dataset.lnkPath || "";
  // 内置动作（kind=run）不是真实文件路径，隐藏"打开文件夹/复制路径"等文件相关菜单
  const isRealPath = lnkPath && li.dataset.actionKind !== "run";
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
  const runArgRaw = li.dataset.actionRunArg;
  let runArg = null;
  if (runArgRaw != null) {
    try {
      runArg = JSON.parse(runArgRaw);
    } catch (e) {
      console.error("actionRunArg parse failed:", e, runArgRaw);
    }
  }
  return {
    lnkPath: li.dataset.lnkPath,
    calcValue: li.dataset.calcValue,
    payload: li.dataset.actionPayload,
    action: {
      kind: li.dataset.actionKind,
      hint: li.dataset.actionHint,
      runId: li.dataset.actionRunId,
      runArg,
    },
  };
}
