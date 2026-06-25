const TAU = window.__TAURI__;
const invoke = TAU?.core?.invoke ?? TAU?.invoke;

// WebView2 下按 Alt 会激活宿主窗口的系统菜单、进入菜单模态，webview 消息泵随之
// 暂停——后端返回的 invoke 响应会堆在队列里无法分发，表现就是「录制时按钮卡住、
// 非得点一下鼠标才恢复」。设置页本身不需要 Alt 菜单，这里在捕获阶段吞掉 Alt 的
// 默认行为以规避。若仍无效，需改用 Win32 拦截 SC_KEYMENU（更底层）。
document.addEventListener(
  "keydown",
  (e) => {
    if (e.key === "Alt") e.preventDefault();
  },
  true
);

// ESC 键隐藏设置窗口
document.addEventListener("keydown", async (e) => {
  if (e.key === "Escape") {
    try {
      // 设置窗口调用专门的隐藏命令（不影响主窗口）
      await invoke("hide_settings_window");
    } catch (err) {
      console.error("hide_settings_window failed:", err);
    }
  }
});

// ── Tab 切换 ─────────────────────────────────────────────────────────────────

document.querySelectorAll(".tab").forEach((btn) => {
  btn.addEventListener("click", () => {
    document.querySelectorAll(".tab").forEach((t) => t.classList.remove("active"));
    document.querySelectorAll(".panel").forEach((p) => p.classList.remove("active"));
    btn.classList.add("active");
    document.getElementById(btn.dataset.tab).classList.add("active");
  });
});

// ── 配置加载与保存 ───────────────────────────────────────────────────────────

let currentConfig = null;

async function loadConfig() {
  try {
    currentConfig = await invoke("get_config");
    applyConfigToUI(currentConfig);
  } catch (e) {
    console.error("loadConfig failed:", e);
  }
}

function applyConfigToUI(config) {
  // 快捷键显示
  const hotkeyBtn = document.getElementById("hotkey-record");
  if (hotkeyBtn && config.hotkey) {
    hotkeyBtn.textContent = config.hotkey.display || "RightAlt";
  }

  // tap 阈值
  const tapSlider = document.getElementById("tap-threshold");
  const tapValue = document.getElementById("tap-threshold-value");
  if (tapSlider && config.tap_threshold) {
    tapSlider.value = config.tap_threshold;
    tapValue.textContent = `${config.tap_threshold}ms`;
  }

  // grace period
  const graceSlider = document.getElementById("grace-ms");
  const graceValue = document.getElementById("grace-ms-value");
  if (graceSlider && config.grace_period) {
    graceSlider.value = config.grace_period;
    graceValue.textContent = `${config.grace_period}ms`;
  }

  // 开机自启
  const autoStart = document.getElementById("auto-start");
  if (autoStart && config.auto_start !== undefined) {
    autoStart.checked = config.auto_start;
  }

  // 语言
  const language = document.getElementById("language");
  if (language && config.language) {
    language.value = config.language;
  }

  // 日志级别
  const logLevel = document.getElementById("log-level");
  if (logLevel && config.log_level) {
    logLevel.value = config.log_level;
  }
}

// ── 快捷键录制（使用后端 rdev）─────────────────────────────────────────────────

const hotkeyRecordBtn = document.getElementById("hotkey-record");
const hotkeyResetBtn = document.getElementById("hotkey-reset");

if (hotkeyRecordBtn) {
  hotkeyRecordBtn.addEventListener("click", async (e) => {
    e.stopPropagation();
    await startRecording();
  });
}

if (hotkeyResetBtn) {
  hotkeyResetBtn.addEventListener("click", async (e) => {
    e.stopPropagation();
    const defaultHotkey = { modifiers: [], key: "ralt", display: "RightAlt" };
    await invoke("update_hotkey", {
      modifiers: defaultHotkey.modifiers,
      key: defaultHotkey.key,
      display: defaultHotkey.display,
    });
    hotkeyRecordBtn.textContent = "RightAlt";
    if (currentConfig) currentConfig.hotkey = defaultHotkey;
  });
}

async function startRecording() {
  hotkeyRecordBtn.disabled = true;
  hotkeyResetBtn.disabled = true;
  hotkeyRecordBtn.classList.add("recording");
  hotkeyRecordBtn.textContent = "请按下快捷键...（10秒超时）";

  // 录制期间吞掉所有键盘事件的默认行为：Alt / Alt+Space / F10 等会激活宿主窗口
  // 系统菜单、冻结 WebView2 消息泵（导致按钮卡住、需点鼠标才恢复）。录制由后端
  // ll_proc 处理，前端不需要任何键盘输入，全量拦截是安全的。
  const suppress = (e) => e.preventDefault();
  document.addEventListener("keydown", suppress, true);

  try {
    // 调用后端录制
    const result = await invoke("record_hotkey");
    console.log("[startRecording] record_hotkey resolved:", JSON.stringify(result));

    // 保存录制结果
    await invoke("update_hotkey", {
      modifiers: result.modifiers,
      key: result.key,
      display: result.display,
    });

    // 更新显示
    hotkeyRecordBtn.textContent = result.display;

    // 更新本地配置缓存
    if (currentConfig) {
      currentConfig.hotkey = {
        modifiers: result.modifiers,
        key: result.key,
        display: result.display,
      };
    }
  } catch (e) {
    console.error("[startRecording] failed:", e);
    // 恢复原显示
    if (currentConfig?.hotkey?.display) {
      hotkeyRecordBtn.textContent = currentConfig.hotkey.display;
    } else {
      hotkeyRecordBtn.textContent = "RightAlt";
    }
  } finally {
    document.removeEventListener("keydown", suppress, true);
    hotkeyRecordBtn.classList.remove("recording");
    hotkeyRecordBtn.disabled = false;
    hotkeyResetBtn.disabled = false;
  }
}

// ── 滑块配置 ─────────────────────────────────────────────────────────────────

const tapSlider = document.getElementById("tap-threshold");
const tapValue = document.getElementById("tap-threshold-value");

if (tapSlider) {
  tapSlider.addEventListener("input", (e) => {
    tapValue.textContent = `${e.target.value}ms`;
  });

  tapSlider.addEventListener("change", async (e) => {
    const value = parseInt(e.target.value);
    try {
      await invoke("update_tap_threshold", { threshold: value });
      if (currentConfig) currentConfig.tap_threshold = value;
    } catch (err) {
      console.error("update_tap_threshold failed:", err);
    }
  });
}

const graceSlider = document.getElementById("grace-ms");
const graceValue = document.getElementById("grace-ms-value");

if (graceSlider) {
  graceSlider.addEventListener("input", (e) => {
    graceValue.textContent = `${e.target.value}ms`;
  });

  graceSlider.addEventListener("change", async (e) => {
    const value = parseInt(e.target.value);
    try {
      await invoke("update_grace_period", { period: value });
      if (currentConfig) currentConfig.grace_period = value;
    } catch (err) {
      console.error("update_grace_period failed:", err);
    }
  });
}

// ── 通用设置 ─────────────────────────────────────────────────────────────────

const autoStartCheckbox = document.getElementById("auto-start");

if (autoStartCheckbox) {
  autoStartCheckbox.addEventListener("change", async (e) => {
    try {
      await invoke("update_auto_start", { autoStart: e.target.checked });
      if (currentConfig) currentConfig.auto_start = e.target.checked;
    } catch (err) {
      console.error("update_auto_start failed:", err);
    }
  });
}

const languageSelect = document.getElementById("language");

if (languageSelect) {
  languageSelect.addEventListener("change", async (e) => {
    try {
      await invoke("update_language", { language: e.target.value });
      if (currentConfig) currentConfig.language = e.target.value;
    } catch (err) {
      console.error("update_language failed:", err);
    }
  });
}

// ── 日志 ─────────────────────────────────────────────────────────────────────

const logLevelSelect = document.getElementById("log-level");
if (logLevelSelect) {
  logLevelSelect.addEventListener("change", async (e) => {
    try {
      await invoke("update_log_level", { level: e.target.value });
      if (currentConfig) currentConfig.log_level = e.target.value;
    } catch (err) {
      console.error("update_log_level failed:", err);
    }
  });
}

document.getElementById("open-log-file")?.addEventListener("click", async () => {
  try {
    await invoke("open_log_file");
  } catch (e) {
    console.error("open_log_file failed:", e);
  }
});

document.getElementById("open-log-dir")?.addEventListener("click", async () => {
  try {
    await invoke("open_log_dir");
  } catch (e) {
    console.error("open_log_dir failed:", e);
  }
});

async function loadLogInfo() {
  try {
    const info = await invoke("get_log_info");
    const el = document.getElementById("log-file-path");
    if (el) el.textContent = info.current_file || "-";
  } catch (e) {
    console.error("loadLogInfo failed:", e);
  }
}

// ── 存储面板 ─────────────────────────────────────────────────────────────────

async function loadStorageInfo() {
  try {
    const info = await invoke("get_storage_info");
    document.getElementById("history-count").textContent = `${info.history_count} 条记录`;
    document.getElementById("db-path").textContent = info.db_path;
  } catch (e) {
    console.error("loadStorageInfo failed:", e);
  }
}

document.getElementById("clear-history")?.addEventListener("click", async () => {
  if (confirm("确定清空所有历史记录？")) {
    await invoke("clear_history");
    loadStorageInfo();
  }
});

// ── 初始化 ───────────────────────────────────────────────────────────────────

loadConfig();
loadStorageInfo();
loadLogInfo();
