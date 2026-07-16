/**
 * 语音输入 Tab 模块（0.10）
 * STT 配置：总开关 / 模式切换 / 云端供应商 / FunASR 本地服务管理 / 音频设备选择 + 调试
 *
 * 本地 STT 架构：
 * - 工具箱：FunASR（modelscope/阿里达摩院，Python 原生）
 * - 服务：funasr-server（OpenAI 兼容 API, localhost:8000）
 * - 模型：FunASR 自动管理（首次启动时从 ModelScope 自动下载）
 */
import { invoke, listen } from "../../tauri.js";

/**
 * 初始化语音输入 Tab
 */
export async function initVoiceTab() {
  const panel = document.getElementById("voice");
  if (!panel) return;

  let config = null;
  try {
    config = await invoke("get_stt_config");
  } catch (e) {
    console.error("get_stt_config failed:", e);
    return;
  }

  // 总开关
  const enabledCheckbox = document.getElementById("voice-enabled");
  if (enabledCheckbox) {
    enabledCheckbox.checked = config.enabled;
    enabledCheckbox.addEventListener("change", () => {
      config.enabled = enabledCheckbox.checked;
      invoke("set_stt_config", { config }).catch(console.error);
    });
  }

  // 模式切换
  const cloudRadio = document.getElementById("voice-mode-cloud");
  const localRadio = document.getElementById("voice-mode-local");
  if (cloudRadio && localRadio) {
    if (config.mode === "local") {
      localRadio.checked = true;
    } else {
      cloudRadio.checked = true;
    }
    cloudRadio.addEventListener("change", () => {
      if (cloudRadio.checked) {
        config.mode = "cloud";
        invoke("set_stt_config", { config }).catch(console.error);
        updateModeVisibility();
      }
    });
    localRadio.addEventListener("change", () => {
      if (localRadio.checked) {
        config.mode = "local";
        invoke("set_stt_config", { config }).catch(console.error);
        updateModeVisibility();
      }
    });
  }

  // 音频设备选择
  const deviceSelect = document.getElementById("voice-audio-device");
  if (deviceSelect) {
    try {
      const devices = await invoke("list_audio_devices");

      const defaultDev = devices.find((d) => d.is_default);
      const defaultOption = deviceSelect.querySelector('option[value=""]');
      if (defaultOption && defaultDev) {
        defaultOption.textContent = `系统默认（${defaultDev.name}）`;
      }

      for (const dev of devices) {
        const opt = document.createElement("option");
        opt.value = dev.id;
        opt.textContent = dev.name || `设备 ${dev.id}`;
        deviceSelect.appendChild(opt);
      }
      if (config.audio_device_id != null) {
        deviceSelect.value = config.audio_device_id;
      }
    } catch (e) {
      console.error("list_audio_devices failed:", e);
    }
    deviceSelect.addEventListener("change", () => {
      const val = deviceSelect.value;
      config.audio_device_id = val || null;
      invoke("set_stt_config", { config }).catch(console.error);
    });
  }

  // 音频调试测试
  initAudioTest(config);

  // 云端供应商
  const kindSelect = document.getElementById("voice-cloud-kind");
  const modelInput = document.getElementById("voice-cloud-model");
  const baseUrlInput = document.getElementById("voice-cloud-base-url");
  if (kindSelect && modelInput) {
    const cp = config.cloud_provider;
    if (cp) {
      kindSelect.value = cp.kind;
      modelInput.value = cp.model_id;
      if (baseUrlInput && cp.base_url) baseUrlInput.value = cp.base_url;
    }
    kindSelect.addEventListener("change", saveCloudProvider);
    modelInput.addEventListener("blur", saveCloudProvider);
    if (baseUrlInput) baseUrlInput.addEventListener("blur", saveCloudProvider);
  }

  function saveCloudProvider() {
    const kind = kindSelect?.value || "openai";
    const model_id = modelInput?.value || "";
    const base_url = baseUrlInput?.value || null;
    if (!model_id) return;
    config.cloud_provider = { kind, model_id, base_url };
    invoke("set_stt_config", { config }).catch(console.error);
  }

  // 伪流式识别开关（VAD 切句 + 累积预览）
  const streamingCheckbox = document.getElementById("voice-streaming");
  if (streamingCheckbox) {
    streamingCheckbox.checked = config.streaming_mode === "pseudo";
    streamingCheckbox.addEventListener("change", () => {
      config.streaming_mode = streamingCheckbox.checked ? "pseudo" : "off";
      invoke("set_stt_config", { config }).catch(console.error);
    });
  }

  // FunASR 环境管理 + 统一日志 + 诊断 + 设备切换
  initFunasrEnv(config);

  // 本地模型选择（下拉框）
  loadLocalModels(config);

  // 0.10.3 高级选项
  initAdvancedOptions(config);

  // 空间管理
  initSpaceManagement();

  // 模式可见性
  updateModeVisibility();

  function updateModeVisibility() {
    const cloudSection = document.getElementById("voice-cloud-section");
    const localSection = document.getElementById("voice-local-section");
    if (cloudSection && localSection) {
      const isLocal = localRadio?.checked;
      cloudSection.style.display = isLocal ? "none" : "";
      localSection.style.display = isLocal ? "" : "none";
    }
  }

  async function loadLocalModels(cfg) {
    const select = document.getElementById("funasr-model-select");
    if (!select) return;

    let models = [];
    try {
      models = await invoke("list_stt_models");
    } catch (e) {
      console.error("list_stt_models failed:", e);
      return;
    }

    if (!Array.isArray(models) || models.length === 0) {
      select.innerHTML = '<option value="">暂无可用模型</option>';
      return;
    }

    select.innerHTML = models
      .map((m) => {
        const selected = m.is_selected ? "selected" : "";
        const sizeStr = m.size_mb >= 1024
          ? (m.size_mb / 1024).toFixed(1) + " GB"
          : m.size_mb + " MB";
        return `<option value="${m.id}" ${selected} title="${m.description || ""}">${m.display_name} · ${m.params} · 约 ${sizeStr}</option>`;
      })
      .join("");

    // 如果当前没有选中的，默认选第一个
    if (!models.some((m) => m.is_selected) && models.length > 0) {
      select.value = models[0].id;
    }

    select.addEventListener("change", async () => {
      const modelId = select.value;
      if (!modelId) return;

      try {
        await invoke("download_stt_model", { modelId });
        const m = models.find((m) => m.id === modelId);
        if (m) {
          appendLog(`[Blink] 已选择模型: ${m.display_name}`);
          // 同步 config（download_stt_model 已持久化 funasr_model + local_model_id）
          cfg.local_engine.funasr_model = m.funasr_model_id;
          cfg.local_model_id = modelId;

          // 如果服务正在运行，提示重启
          const isRunning = startBtn?.classList.contains("running");
          if (isRunning) {
            appendLog("[Blink] 模型已切换，请先停止再重新启动服务以加载新模型");
          }
        }
      } catch (e) {
        console.error("download_stt_model failed:", e);
      }
    });
  }
}

// ── 0.10.3 高级选项（热词 / ITN / 流式模型 / 注入方式）──────────────────

async function initAdvancedOptions(config) {
  // G2 注入方式
  const injectMethodSelect = document.getElementById("voice-inject-method-select");
  if (injectMethodSelect) {
    const method = config.inject_method || "sendinput";
    injectMethodSelect.value = method;
    injectMethodSelect.addEventListener("change", () => {
      config.inject_method = injectMethodSelect.value;
      invoke("set_stt_config", { config }).catch(console.error);
    });
  }

  // 热词
  const hotwordsTextarea = document.getElementById("voice-hotwords");
  if (hotwordsTextarea) {
    hotwordsTextarea.value = config.local_engine.hotwords || "";
    // 失焦时保存（避免每次按键都触发保存）
    hotwordsTextarea.addEventListener("blur", () => {
      config.local_engine.hotwords = hotwordsTextarea.value || null;
      invoke("set_stt_config", { config }).catch(console.error);
    });
    // 自动收扁：无内容时高度收扁为单行，有内容时按内容自适应
    function autoResizeHotwords() {
      hotwordsTextarea.style.height = "auto";
      hotwordsTextarea.style.height = Math.max(hotwordsTextarea.scrollHeight, 32) + "px";
    }
    hotwordsTextarea.addEventListener("input", autoResizeHotwords);
    autoResizeHotwords();
  }

  // ITN 开关
  const itnToggle = document.getElementById("voice-use-itn-toggle");
  if (itnToggle) {
    itnToggle.checked = config.local_engine.use_itn !== false;
    itnToggle.addEventListener("change", () => {
      config.local_engine.use_itn = itnToggle.checked;
      invoke("set_stt_config", { config }).catch(console.error);
    });
  }
}

// ── 模块级统一日志（供 initFunasrEnv 和 initSpaceManagement 共享）──

const _MAX_LOG_LINES = 200;
let _logLines = [];

function appendLog(line) {
  const ts = new Date().toLocaleTimeString("zh-CN", { hour12: false });
  _logLines.push(`[${ts}] ${line}`);
  if (_logLines.length > _MAX_LOG_LINES) {
    _logLines = _logLines.slice(-_MAX_LOG_LINES);
  }
  const logPre = document.getElementById("voice-server-log");
  if (logPre) {
    logPre.textContent = _logLines.join("\n");
    logPre.scrollTop = logPre.scrollHeight;
  }
}

function clearLog() {
  _logLines = [];
  const logPre = document.getElementById("voice-server-log");
  if (logPre) logPre.textContent = "";
}

// ── Python 环境 + FunASR 服务管理 + 统一日志 + 诊断 + 设备切换 ──

async function initFunasrEnv(config) {
  const setupBtn = document.getElementById("funasr-setup-btn");
  const startBtn = document.getElementById("funasr-start-btn");
  const stopBtn = document.getElementById("funasr-stop-btn");
  const statusText = document.getElementById("funasr-status-text");
  const serverStatusText = document.getElementById("funasr-server-status-text");
  const logPre = document.getElementById("voice-server-log");
  const logClearBtn = document.getElementById("funasr-log-clear-btn");
  const logCopyBtn = document.getElementById("funasr-log-copy-btn");
  const diagnoseBtn = document.getElementById("stt-diagnose-btn");
  const deviceSelect = document.getElementById("funasr-device-select");

  if (!setupBtn && !startBtn && !statusText) return;

  // 日志缓冲已提升为模块级（appendLog / clearLog），此处仅绑定按钮事件

  if (logClearBtn) {
    logClearBtn.addEventListener("click", clearLog);
  }

  if (logCopyBtn) {
    logCopyBtn.addEventListener("click", async () => {
      const text = _logLines.join("\n");
      if (!text) return;
      try {
        await navigator.clipboard.writeText(text);
        logCopyBtn.textContent = "已复制 ✓";
        setTimeout(() => { logCopyBtn.textContent = "复制"; }, 1500);
      } catch (e) {
        console.error("clipboard write failed:", e);
        // fallback: 选中日志区域文本
        if (logPre) {
          const range = document.createRange();
          range.selectNodeContents(logPre);
          const sel = window.getSelection();
          sel.removeAllRanges();
          sel.addRange(range);
        }
      }
    });
  }

  // ── 设备选择 ──
  if (deviceSelect) {
    // 回显当前配置
    deviceSelect.value = config.local_engine.device || "cpu";
    deviceSelect.addEventListener("change", () => {
      const newDevice = deviceSelect.value;
      config.local_engine.device = newDevice;
      invoke("set_stt_config", { config }).catch(console.error);
      appendLog(`[Blink] 推理设备切换为 ${newDevice.toUpperCase()}，需重启服务生效`);

      // 如果服务正在运行，提示重启
      const isRunning = startBtn?.classList.contains("running");
      if (isRunning) {
        appendLog("[Blink] 服务正在运行，切换设备后请先停止再重新启动");
      }
    });
  }

  // ── 自动启动服务开关 ──
  const autoStartToggle = document.getElementById("funasr-auto-start-toggle");
  if (autoStartToggle) {
    autoStartToggle.checked = config.local_engine.auto_start_server || false;
    autoStartToggle.addEventListener("change", () => {
      config.local_engine.auto_start_server = autoStartToggle.checked;
      invoke("set_stt_config", { config }).catch(console.error);
      if (autoStartToggle.checked) {
        appendLog("[Blink] 已开启自动启动服务，Blink 启动时会自动在后台启动 funasr-server");
        // 如果服务还没启动，立即启动一次
        const isRunning = startBtn?.classList.contains("running");
        if (!isRunning && startBtn) {
          appendLog("[Blink] 立即启动服务...");
          startBtn.click();
        }
      }
    });
  }

  // ── 查询初始状态 ──
  async function refreshEnv() {
    try {
      const env = await invoke("get_funasr_env");
      updateEnvUI(env);
      return env;
    } catch (e) {
      console.error("get_funasr_env failed:", e);
      if (statusText) statusText.textContent = "状态查询失败";
    }
  }

  await refreshEnv();

  // ── 安装环境按钮 ──
  if (setupBtn) {
    setupBtn.addEventListener("click", async () => {
      setupBtn.classList.remove("success");
      setupBtn.textContent = "安装中...";
      if (statusText) statusText.textContent = "正在安装 Python 环境...";
      appendLog("[Blink] 开始安装 Python 环境（uv + venv + torch + funasr）");

      try {
        await invoke("setup_python_env");
      } catch (e) {
        console.error("setup_python_env failed:", e);
        if (statusText) statusText.textContent = `安装失败: ${e}`;
        appendLog(`[Blink] ❌ 安装失败: ${e}`);
        setupBtn.textContent = "安装环境";
      }
    });
  }

  // ── 启动服务按钮 ──
  if (startBtn) {
    startBtn.addEventListener("click", async () => {
      startBtn.classList.remove("running");
      startBtn.textContent = "启动中...";
      if (serverStatusText) serverStatusText.textContent = "正在启动 funasr-server（首次需要下载模型，可能需要几分钟）...";
      appendLog("[Blink] 正在启动 funasr-server...");

      try {
        await invoke("start_funasr_server");
      } catch (e) {
        console.error("start_funasr_server failed:", e);
        if (serverStatusText) serverStatusText.textContent = `启动失败: ${e}`;
        appendLog(`[Blink] ❌ 启动失败: ${e}`);
        startBtn.textContent = "启动服务";
      }
    });
  }

  // ── 停止服务按钮 ──
  if (stopBtn) {
    stopBtn.addEventListener("click", async () => {
      stopBtn.disabled = true;
      appendLog("[Blink] 正在停止 funasr-server...");
      try {
        await invoke("stop_funasr_server");
        if (serverStatusText) serverStatusText.textContent = "服务已停止";
        appendLog("[Blink] ✅ funasr-server 已停止");
        startBtn.classList.remove("running");
        startBtn.textContent = "启动服务";
        startBtn.disabled = false;
        stopBtn.disabled = false;
      } catch (e) {
        console.error("stop_funasr_server failed:", e);
        appendLog(`[Blink] ❌ 停止失败: ${e}`);
      }
      refreshEnv();
    });
  }

  // ── 诊断按钮 ──
  if (diagnoseBtn) {
    diagnoseBtn.addEventListener("click", async () => {
      diagnoseBtn.disabled = true;
      diagnoseBtn.textContent = "诊断中...";
      appendLog("[Blink] === STT 诊断开始 ===");

      try {
        const report = await invoke("diagnose_stt");
        // 格式化诊断报告到日志窗口
        const env = report.funasr_env || {};
        appendLog(`[诊断] uv: ${env.uv_available ? "✅" : "❌"} ${env.uv_version || ""}`);
        appendLog(`[诊断] venv: ${env.venv_exists ? "✅" : "❌"} ${env.venv_python_version || ""}`);
        appendLog(`[诊断] torch: ${env.torch_installed ? "✅" : "❌"} ${env.torch_version || ""}`);
appendLog(`[诊断] funasr: ${env.funasr_installed ? "✅" : "❌"} ${env.funasr_version || ""}`);
        appendLog(`[诊断] server_running: ${env.server_running ? "✅" : "❌"} port=${env.server_port}`);
        appendLog(`[诊断] server_ready: ${env.server_ready ? "✅" : "❌"}`);
        if (env.websocket_ready !== undefined) {
          appendLog(`[诊断] websocket_ready: ${env.websocket_ready ? "✅" : "❌"} ${env.websocket_error ? "(" + env.websocket_error + ")" : ""}`);
        }

        const cfg = report.config || {};
        appendLog(`[诊断] mode=${cfg.mode}, model=${cfg.local_model_id || "未选择"}, device=${cfg.device}, streaming_mode=${cfg.streaming_mode}`);

        if (report.api_test) {
          const api = report.api_test;
          if (api.skipped) {
            appendLog(`[诊断] API 测试: 跳过（${api.reason}）`);
          } else if (api.result?.success) {
            appendLog(`[诊断] API 测试: ✅ 成功，识别结果: "${api.result.text}"`);
          } else if (api.result?.error) {
            appendLog(`[诊断] API 测试: ❌ 失败: ${api.result.error}`);
          }
        }
        appendLog("[Blink] === STT 诊断完成 ===");
      } catch (e) {
        appendLog(`[诊断] ❌ 诊断失败: ${e}`);
      } finally {
        diagnoseBtn.disabled = false;
        diagnoseBtn.textContent = "诊断 STT";
      }
    });
  }

  // ── 监听 Python 环境安装进度 ──
  listen("blink://python-env-progress", (event) => {
    const p = event.payload;
    if (!p) return;

    if (p.stage === "complete" && p.status === "ready") {
      if (statusText) statusText.textContent = "✅ Python 环境就绪";
      if (setupBtn) {
        setupBtn.classList.add("success");
        setupBtn.textContent = "环境就绪 ✓";
      }
      if (startBtn) startBtn.disabled = false;
      appendLog("[Blink] ✅ Python 环境安装完成");
      refreshEnv();
    } else if (p.stage === "error") {
      if (statusText) statusText.textContent = `❌ 安装失败: ${p.error || ""}`;
      if (setupBtn) {
        setupBtn.classList.remove("success");
        setupBtn.textContent = "安装环境";
      }
      appendLog(`[Blink] ❌ 安装失败: ${p.error || ""}`);
    } else {
      // 进度更新
      const messages = {
        uv: { starting: "下载 uv 中...", done: "✅ uv 就绪" },
        venv: { starting: "创建 Python venv 中...", done: "✅ venv 就绪" },
        torch: { installing: "安装 PyTorch 中（~200MB，请耐心等待）...", done: "✅ PyTorch 安装完成" },
        funasr: { installing: "安装 funasr 中（可能需要几分钟）...", done: "✅ funasr 安装完成" },
      };
      const msg = messages[p.stage]?.[p.status];
      if (msg && statusText) statusText.textContent = msg;
      if (msg) appendLog(`[Blink] ${msg}`);
    }
  });

  // ── 监听 funasr-server 日志输出 ──
  listen("blink://funasr-server-log", (event) => {
    const line = event.payload?.line;
    if (line) appendLog(line);
  });

  // ── 监听服务状态事件 ──
  listen("blink://funasr-server-status", (event) => {
    const p = event.payload;
    if (!p) return;

    if (p.stage === "ready") {
      if (serverStatusText) serverStatusText.textContent = `✅ FunASR 服务就绪（${p.model}）`;
      if (startBtn) {
        startBtn.classList.add("running");
        startBtn.textContent = "已运行 ✓";
        startBtn.disabled = false;
      }
      if (stopBtn) stopBtn.disabled = false;
      appendLog(`[Blink] ✅ funasr-server 就绪（${p.model}）`);
    } else if (p.stage === "already_running") {
      if (serverStatusText) serverStatusText.textContent = `✅ FunASR 服务就绪（${p.model}）`;
      if (startBtn) {
        startBtn.classList.add("running");
        startBtn.textContent = "已运行 ✓";
        startBtn.disabled = false;
      }
      if (stopBtn) stopBtn.disabled = false;
      appendLog(`[Blink] ✅ funasr-server 已在运行（${p.model}）`);
    } else if (p.stage === "error") {
      if (serverStatusText) serverStatusText.textContent = `❌ 服务错误: ${p.error || ""}`;
      if (startBtn) {
        startBtn.classList.remove("running");
        startBtn.textContent = "启动服务";
        startBtn.disabled = false;
      }
      if (stopBtn) stopBtn.disabled = true;
      appendLog(`[Blink] ❌ 服务错误: ${p.error || ""}`);
    } else if (p.stage === "starting") {
      if (serverStatusText) serverStatusText.textContent = `启动中... (模型: ${p.model}, 端口: ${p.port})`;
      appendLog(`[Blink] funasr-server 启动中 (模型: ${p.model}, 端口: ${p.port}, 设备: ${config.local_engine.device})`);
    } else if (p.stage === "loading_model") {
      if (serverStatusText) serverStatusText.textContent = `⏳ 正在下载/加载模型 ${p.model}...（首次约 234MB，请耐心等待）`;
      if (startBtn) {
        startBtn.textContent = "加载模型中...";
        startBtn.disabled = true;
      }
      appendLog(`[Blink] ⏳ 服务已启动，模型加载中（首次需下载 ~234MB，可能需要数分钟）...`);
    } else if (p.stage === "setup_env") {
      if (serverStatusText) serverStatusText.textContent = `正在安装 Python 环境...`;
      appendLog("[Blink] 服务启动需要先安装 Python 环境...");
    }
  });

  function updateEnvUI(env) {
    if (!statusText) return;

    const parts = [];

    if (!env.uv_available) {
      parts.push("❌ uv 未安装");
    } else {
      parts.push(`✅ uv ${env.uv_version || ""}`);
    }

    if (!env.venv_exists) {
      parts.push("❌ Python venv 未创建");
    } else {
      parts.push(`✅ Python ${env.venv_python_version || ""}`);
    }

    if (!env.torch_installed) {
      parts.push("❌ PyTorch 未安装");
    } else {
      parts.push(`✅ torch ${env.torch_version || ""}`);
    }

    if (!env.funasr_installed) {
      parts.push("❌ funasr 未安装");
    } else {
      parts.push(`✅ funasr ${env.funasr_version || ""}`);
    }

    // 综合状态 + 按钮控制（不置灰，用绿色样式标记成功）
    if (env.env_ready) {
      if (setupBtn) {
        setupBtn.classList.add("success");
        setupBtn.textContent = "环境就绪 ✓";
        setupBtn.disabled = false;
      }
      if (startBtn && !env.server_running) {
        startBtn.classList.remove("running");
        startBtn.textContent = "启动服务";
        startBtn.disabled = false;
      }
    } else {
      if (setupBtn) {
        setupBtn.classList.remove("success");
        setupBtn.textContent = "安装环境";
        setupBtn.disabled = false;
      }
      if (startBtn) {
        startBtn.classList.remove("running");
        startBtn.textContent = "启动服务";
        startBtn.disabled = true;
      }
    }

    // 服务运行状态
    if (env.server_running) {
      if (startBtn) {
        startBtn.classList.add("running");
        startBtn.textContent = "已运行 ✓";
        startBtn.disabled = false;
      }
      if (stopBtn) stopBtn.disabled = false;
      if (serverStatusText) serverStatusText.textContent = `✅ 服务运行中（${env.server_model}）`;
    } else {
      if (startBtn && env.env_ready) {
        startBtn.classList.remove("running");
        startBtn.textContent = "启动服务";
        startBtn.disabled = false;
      }
      if (stopBtn) stopBtn.disabled = true;
    }

    // 设备选择器在服务运行时禁用
    if (deviceSelect) {
      deviceSelect.disabled = env.server_running;
    }

    statusText.innerHTML = parts.join("<br>");
  }
}

// ── 音频调试测试 ──────────────────────────────────────────────────────

let audioTestActive = false;

function initAudioTest(config) {
  const btn = document.getElementById("voice-audio-test-btn");
  const bar = document.getElementById("audio-test-bar");
  if (!btn || !bar) return;

  btn.addEventListener("click", async () => {
    if (audioTestActive) {
      audioTestActive = false;
      btn.textContent = "开始测试";
      btn.classList.remove("active");
      bar.style.width = "0%";
      try {
        await invoke("stop_audio_test");
      } catch (e) {
        console.error("stop_audio_test failed:", e);
      }
      return;
    }

    audioTestActive = true;
    btn.textContent = "停止测试";
    btn.classList.add("active");

    const deviceSelect = document.getElementById("voice-audio-device");
    const deviceId = deviceSelect?.value || null;

    try {
      await invoke("start_audio_test", { deviceId });
    } catch (e) {
      console.error("[voice] start_audio_test failed:", e);
      audioTestActive = false;
      btn.textContent = "开始测试";
      btn.classList.remove("active");
      bar.style.background = "var(--color-danger, #e53e3e)";
      bar.style.width = "100%";
      bar.textContent = e;
    }
  });

  listen("blink://audio-test-level", (event) => {
    if (!audioTestActive) return;
    const level = event.payload?.level ?? 0;
    const pct = Math.max(0, Math.min(100, level * 100));
    bar.style.width = `${pct}%`;
    if (pct < 5) {
      bar.style.background = "var(--color-danger, #e53e3e)";
    } else if (pct > 90) {
      bar.style.background = "var(--color-warning, #dd6b20)";
    } else {
      bar.style.background = "var(--color-success, #38a169)";
    }
  });
}

// ── 空间管理 ──────────────────────────────────────────────────────────

async function initSpaceManagement() {
  const container = document.getElementById("stt-space-usage");
  if (!container) return;

  async function loadUsage() {
    container.innerHTML = '<div class="stt-space-loading">加载中...</div>';
    try {
      const data = await invoke("get_stt_space_usage");
      renderUsage(data);
    } catch (e) {
      console.error("get_stt_space_usage failed:", e);
      container.innerHTML = '<div class="stt-space-loading">获取空间信息失败</div>';
    }
  }

  function renderUsage(data) {
    const items = data.items || [];
    const total = data.total_mb || 0;

    let html = items.map((item) => `
      <div class="stt-space-row">
        <div>
          <div class="stt-space-label">${item.label}</div>
          <div class="stt-space-path">${item.path}</div>
        </div>
        <div class="stt-space-size">${formatSize(item.size_mb)}</div>
      </div>
    `).join("");

    html += `
      <div class="stt-space-total">
        <span class="stt-space-total-label">总占用</span>
        <span class="stt-space-total-size">${formatSize(total)}</span>
      </div>
      <div class="stt-space-actions">
        <button type="button" class="stt-space-cleanup-btn" id="stt-space-cleanup-btn">清理 Python 环境</button>
        <button type="button" class="stt-space-open-folder-btn" id="stt-space-open-folder-btn">打开文件夹</button>
        <button type="button" class="stt-space-refresh-btn" id="stt-space-refresh-btn">刷新</button>
      </div>
      <p class="stt-space-hint">
        清理会删除 Python venv、uv 二进制和模型缓存。清理后需重新安装环境才能使用本地 STT。
      </p>
    `;

    container.innerHTML = html;

    const cleanupBtn = document.getElementById("stt-space-cleanup-btn");
    if (cleanupBtn) {
      cleanupBtn.addEventListener("click", async () => {
        // 检查服务是否正在运行——如果在运行，提醒用户先停止
        let serverRunning = false;
        try {
          const env = await invoke("get_funasr_env");
          serverRunning = env.server_running;
        } catch (e) {
          // 查询失败，继续清理流程
        }

        if (serverRunning) {
          const confirmed = confirm(
            "FunASR 服务正在运行。\n\n" +
            "清理环境会先停止服务，然后删除 Python venv、uv 和模型缓存。\n" +
            "清理后需重新安装环境才能使用本地 STT。\n\n" +
            "确定要继续清理吗？"
          );
          if (!confirmed) return;
          appendLog("[Blink] 清理环境：正在停止 funasr-server...");
          try {
            await invoke("stop_funasr_server");
            appendLog("[Blink] ✅ 服务已停止，开始清理环境");

            // 同步重置启动/停止按钮状态（服务已在后端停止）
            const startBtn = document.getElementById("funasr-start-btn");
            const stopBtn = document.getElementById("funasr-stop-btn");
            const serverStatusText = document.getElementById("funasr-server-status-text");
            if (startBtn) {
              startBtn.classList.remove("running");
              startBtn.textContent = "启动服务";
              startBtn.disabled = false;
            }
            if (stopBtn) stopBtn.disabled = true;
            if (serverStatusText) serverStatusText.textContent = "服务已停止（清理环境）";
          } catch (e) {
            console.error("stop_funasr_server failed:", e);
            appendLog(`[Blink] ⚠️ 停止服务失败: ${e}，继续清理...`);
          }
        }

        cleanupBtn.disabled = true;
        cleanupBtn.textContent = "清理中...";
        try {
          await invoke("cleanup_stt_space");
          cleanupBtn.textContent = "清理完成 ✓";
          appendLog("[Blink] ✅ 环境清理完成");
          setTimeout(() => {
            loadUsage();
          }, 1000);
        } catch (e) {
          console.error("cleanup_stt_space failed:", e);
          cleanupBtn.textContent = "清理失败";
          cleanupBtn.disabled = false;
        }
      });
    }

    const refreshBtn = document.getElementById("stt-space-refresh-btn");
    if (refreshBtn) {
      refreshBtn.addEventListener("click", loadUsage);
    }

    const openFolderBtn = document.getElementById("stt-space-open-folder-btn");
    if (openFolderBtn) {
      openFolderBtn.addEventListener("click", async () => {
        try {
          await invoke("open_stt_folder");
        } catch (e) {
          console.error("open_stt_folder failed:", e);
          appendLog(`[Blink] ❌ 打开文件夹失败: ${e}`);
        }
      });
    }
  }

  function formatSize(mb) {
    if (mb < 1) return `${(mb * 1024).toFixed(0)} KB`;
    if (mb < 1024) return `${mb.toFixed(1)} MB`;
    return `${(mb / 1024).toFixed(2)} GB`;
  }

  await loadUsage();
}
