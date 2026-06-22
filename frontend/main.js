const query = document.getElementById("query");
const results = document.getElementById("results");
const debug = document.getElementById("debug");

const TAU = window.__TAURI__;
const invoke = TAU?.core?.invoke ?? TAU?.invoke;

// ── 窗口弹性大小 ─────────────────────────────────────────────────────────────

const INPUT_HEIGHT = 60;   // 输入框高度
const ITEM_HEIGHT = 40;    // 每个结果项高度
const MAX_ITEMS = 8;       // 最多显示的结果数

async function adjustWindowSize(itemCount) {
  const listHeight = Math.min(itemCount, MAX_ITEMS) * ITEM_HEIGHT;
  const height = INPUT_HEIGHT + (itemCount > 0 ? listHeight : 0);
  await invoke("resize_window", { width: 700, height });
}

// ── 搜索 ─────────────────────────────────────────────────────────────────────

let selected = -1;
let searchTimer = null;

query.addEventListener("input", () => {
  clearTimeout(searchTimer);
  const q = query.value.trim();
  if (!q) {
    results.innerHTML = "";
    selected = -1;
    adjustWindowSize(0);
    return;
  }
  searchTimer = setTimeout(async () => {
    try {
      const apps = await invoke("search_apps", { query: q });
      renderResults(apps);
    } catch (e) {
      console.error("search_apps failed:", e);
    }
  }, 150);
});

function renderResults(apps) {
  results.innerHTML = "";
  apps.forEach((app, i) => {
    const li = document.createElement("li");
    li.textContent = app.name;
    li.dataset.lnkPath = app.lnk_path;
    li.dataset.index = i;
    if (app.is_calc) {
      li.classList.add("calc-result");
    }
    li.addEventListener("click", () => launchApp(app.lnk_path));
    results.appendChild(li);
  });
  results.classList.toggle("has-items", apps.length > 0);
  selected = apps.length > 0 ? 0 : -1;
  updateSelection();
  adjustWindowSize(apps.length);
}

// ── 键盘导航 ──────────────────────────────────────────────────────────────────

document.addEventListener("keydown", (e) => {
  const items = results.children;
  if (!items.length) return;

  if (e.key === "ArrowDown") {
    e.preventDefault();
    selected = Math.min(selected + 1, items.length - 1);
    updateSelection();
    items[selected]?.scrollIntoView({ block: "nearest" });
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    selected = Math.max(selected - 1, 0);
    updateSelection();
    items[selected]?.scrollIntoView({ block: "nearest" });
  } else if (e.key === "Enter" && selected >= 0) {
    e.preventDefault();
    const li = items[selected];
    if (li) launchApp(li.dataset.lnkPath);
  }
});

function updateSelection() {
  Array.from(results.children).forEach((li, i) => {
    li.classList.toggle("active", i === selected);
  });
}

async function launchApp(lnkPath) {
  if (!lnkPath) {
    const text = results.children[selected]?.textContent?.replace(/^=\s*/, "");
    if (text) await navigator.clipboard.writeText(text);
    invoke("hide_window");
    return;
  }
  try {
    await invoke("launch_app", { lnkPath });
  } catch (e) {
    console.error("launch_app failed:", e);
  }
}

// ── 唤起/隐藏 ─────────────────────────────────────────────────────────────────

TAU?.event?.listen("blink://shown", () => {
  query.value = "";
  results.innerHTML = "";
  selected = -1;
  query.focus();
  adjustWindowSize(0);
});

TAU?.event?.listen("blink://hidden", () => {
  query.value = "";
  results.innerHTML = "";
  results.classList.remove("has-items");
  selected = -1;
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    invoke?.("hide_window");
  }
});

// ── 调试面板（暂保留占位） ───────────────────────────────────────────────────

(async () => {
  if (TAU?.event?.listen) {
    await TAU.event.listen("blink://debug", (e) => {
      const d = e.payload || {};
      debug.textContent = `唤起 ${fmt(d.invoke_ms)}  show ${fmt(d.show_ms)}  focus ${fmt(d.focus_ms)}  成功率 ${d.success_rate ?? "-"}`;
    });
  }
})();

function fmt(ms) {
  return ms == null ? "-" : `${ms}ms`;
}
