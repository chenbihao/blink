/**
 * 语音输入 Tab 模块（0.10）
 * STT 配置：总开关 / 模式切换 / 云端供应商（独立配置）/ FunASR 本地服务管理 / 音频设备选择 + 调试
 *
 * 云端 STT 架构（独立模式）：
 * - 配置完全独立于 AIConfig——用户在语音设置页直接配置 kind/base_url/model_id
 * - API Key 用 stt:cloud 前缀存在 Credential Manager 里，不与 AI 供应商共用
 * - 支持预设快捷填充（OpenAI / Groq / MiMo）
 *
 * 本地 STT 架构：
 * - 工具箱：FunASR（modelscope/阿里达摩院，Python 原生）
 * - 服务：funasr-server（OpenAI 兼容 API, localhost:8000）
 * - 模型：FunASR 自动管理（首次启动时从 ModelScope 自动下载）
 */
import { invoke, listen, confirmDialog } from "../../tauri.js";
import { EVENTS } from "../../event-names.js";
import { t, onLangChange, getLang } from "../../i18n/index.js";

/**
 * 保存 STT 配置。
 * scope 决定后端控制台日志打印哪个区段，避免改本地配置时把云端字段也全部打印出来：
 * - "global": 总开关 / 模式 / 流式 / 音频设备
 * - "cloud":  云端供应商
 * - "local":  本地引擎（模型 / 设备 / 热词 / ITN / VAD）
 */
function saveSttConfig(cfg, scope) {
  invoke("set_stt_config", { config: cfg, scope }).catch(console.error);
}

/**
 * 初始化语音输入 Tab（轻量：只绑定事件 + 加载配置，不跑探测命令）。
 *
 * 昂贵的探测命令（get_funasr_env / get_stt_space_usage / list_stt_models）
 * 延迟到用户首次切换到语音 Tab 时才执行（见 activateVoiceTab）。
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
      saveSttConfig(config, "global");
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
        saveSttConfig(config, "global");
        updateModeVisibility();
      }
    });
    localRadio.addEventListener("change", () => {
      if (localRadio.checked) {
        config.mode = "local";
        saveSttConfig(config, "global");
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
        defaultOption.textContent = t("voice.audio_device.default_with_name", { name: defaultDev.name });
      } else if (defaultOption) {
        defaultOption.textContent = t("voice.audio_device.default");
      }

      for (const dev of devices) {
        const opt = document.createElement("option");
        opt.value = dev.id;
        opt.textContent = dev.name || t("voice.audio_device.device_n", { id: dev.id });
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
      saveSttConfig(config, "global");
    });
  }

  // 音频调试测试
  initAudioTest(config);

  // 云端供应商（独立模式：直接配置 kind/base_url/model_id/api_key）
  const testBtn = document.getElementById("voice-cloud-test-btn");
  const testResult = document.getElementById("voice-cloud-test-result");
  const presetSelect = document.getElementById("voice-cloud-preset");
  const kindSelect = document.getElementById("voice-cloud-kind");
  const baseUrlInput = document.getElementById("voice-cloud-base-url");
  const modelIdInput = document.getElementById("voice-cloud-model-id");
  const apiKeyInput = document.getElementById("voice-cloud-api-key");
  const keySaveBtn = document.getElementById("voice-cloud-key-save-btn");
  const keyClearBtn = document.getElementById("voice-cloud-key-clear-btn");

  // 供应商预设 → 默认值映射
  const STT_PRESETS = {
    openai: { kind: "openai", base_url: "https://api.openai.com/v1", model_id: "whisper-1" },
    groq: { kind: "groq", base_url: "https://api.groq.com/openai/v1", model_id: "whisper-large-v3" },
    mimo: { kind: "mimo", base_url: "https://api.xiaomimimo.com/v1", model_id: "" },
    custom: { kind: "openai", base_url: "", model_id: "" },
  };

  // 回显当前配置
  if (config.cloud_provider) {
    const cp = config.cloud_provider;
    if (kindSelect) kindSelect.value = cp.kind || "openai";
    if (baseUrlInput) baseUrlInput.value = cp.base_url || "";
    if (modelIdInput) modelIdInput.value = cp.model_id || "";
    // 自动匹配预设
    if (presetSelect) {
      const matchedPreset = Object.entries(STT_PRESETS).find(([_, v]) =>
        v.kind === cp.kind && (!v.base_url || v.base_url === cp.base_url)
      );
      presetSelect.value = matchedPreset ? matchedPreset[0] : "custom";
    }
  } else {
    if (presetSelect) presetSelect.value = "custom";
    if (kindSelect) kindSelect.value = "openai";
  }

  // 加载 API Key 掩码 → 回显到输入框 placeholder（与 AI 供应商一致）
  async function refreshKeyHint() {
    if (!apiKeyInput) return;
    try {
      const hint = await invoke("get_stt_secret_hint");
      if (hint) {
        apiKeyInput.placeholder = hint + " — " + t("voice.cloud.api_key.ph.edit");
        apiKeyInput.classList.add("has-secret-hint");
      } else {
        apiKeyInput.placeholder = t("voice.cloud.api_key.ph");
        apiKeyInput.classList.remove("has-secret-hint");
      }
    } catch (e) {
      console.error("get_stt_secret_hint failed:", e);
    }
  }
  refreshKeyHint();

  // 保存云端配置（kind/base_url/model_id → cloud_provider）
  function saveCloudProvider() {
    const kind = kindSelect?.value || "openai";
    const base_url = baseUrlInput?.value?.trim() || null;
    const model_id = modelIdInput?.value?.trim() || "";
    if (!model_id) {
      delete config.cloud_provider;
    } else {
      config.cloud_provider = { kind, base_url, model_id };
    }
    saveSttConfig(config, "cloud");
  }

  // 预设切换 → 自动填充 kind/base_url/model_id
  if (presetSelect) {
    presetSelect.addEventListener("change", () => {
      const preset = STT_PRESETS[presetSelect.value];
      if (!preset) return;
      if (kindSelect) kindSelect.value = preset.kind;
      if (baseUrlInput) baseUrlInput.value = preset.base_url;
      if (modelIdInput) modelIdInput.value = preset.model_id;
      saveCloudProvider();
    });
  }

  // 各字段失焦时保存
  if (kindSelect) kindSelect.addEventListener("change", saveCloudProvider);
  if (baseUrlInput) baseUrlInput.addEventListener("blur", saveCloudProvider);
  if (modelIdInput) modelIdInput.addEventListener("blur", saveCloudProvider);

  // API Key 保存
  if (keySaveBtn) {
    keySaveBtn.addEventListener("click", async () => {
      const secret = apiKeyInput?.value;
      if (!secret) return;
      keySaveBtn.textContent = t("voice.cloud.api_key.saving");
      keySaveBtn.disabled = true;
      try {
        await invoke("save_stt_secret", { secret });
        if (apiKeyInput) apiKeyInput.value = "";
        await refreshKeyHint();
        keySaveBtn.textContent = t("voice.cloud.api_key.saved");
        setTimeout(() => { keySaveBtn.textContent = t("voice.cloud.api_key.save_btn"); }, 1500);
      } catch (e) {
        console.error("save_stt_secret failed:", e);
        keySaveBtn.textContent = t("voice.cloud.api_key.save_btn");
      } finally {
        keySaveBtn.disabled = false;
      }
    });
  }

  // API Key 清除
  if (keyClearBtn) {
    keyClearBtn.addEventListener("click", async () => {
      keyClearBtn.disabled = true;
      try {
        await invoke("delete_stt_secret");
        await refreshKeyHint();
      } catch (e) {
        console.error("delete_stt_secret failed:", e);
      } finally {
        keyClearBtn.disabled = false;
      }
    });
  }

  // 语言切换时刷新 placeholder 文案
  onLangChange(() => {
    refreshKeyHint();
  });

  // 云端连接测试
  if (testBtn) {
    testBtn.addEventListener("click", async () => {
      testBtn.textContent = t("voice.cloud.test.testing");
      testBtn.disabled = true;
      if (testResult) {
        testResult.textContent = "";
        testResult.className = "voice-cloud-test-result";
      }
      try {
        const result = await invoke("test_cloud_stt");
        if (testResult) {
          if (result.success) {
            testResult.textContent = t("voice.cloud.test.success", { text: result.text });
            testResult.className = "voice-cloud-test-result success";
          } else {
            testResult.textContent = t("voice.cloud.test.fail", { err: result.error });
            testResult.className = "voice-cloud-test-result error";
          }
        }
      } catch (e) {
        if (testResult) {
          testResult.textContent = t("voice.cloud.test.fail", { err: e });
          testResult.className = "voice-cloud-test-result error";
        }
      } finally {
        testBtn.textContent = t("voice.cloud.test.btn");
        testBtn.disabled = false;
      }
    });
  }

  // 0.10.3 高级选项（轻量，不跑探测）——流式识别开关也在此初始化
  initAdvancedOptions(config);

  // 模式可见性
  updateModeVisibility();

  // ── 延迟激活：首次切换到语音 Tab 时才跑探测命令 ──
  let activated = false;

  async function activateVoiceTab() {
    if (activated) return;
    activated = true;

    // FunASR 环境管理 + 统一日志 + 诊断 + 设备切换（含 refreshEnv 探测）
    initFunasrEnv(config);

    // 本地模型选择（下拉框）
    loadLocalModels(config);

    // 空间管理（含 get_stt_space_usage 探测）
    initSpaceManagement();
  }

  // 监听语音 Tab 按钮点击，首次点击时激活
  const voiceTabBtn = document.querySelector('.tab[data-tab="voice"]');
  if (voiceTabBtn) {
    voiceTabBtn.addEventListener("click", activateVoiceTab, { once: true });
  }

  // 如果设置页打开时语音 Tab 已是激活状态（如从深链跳转），立即激活
  if (voiceTabBtn?.classList.contains("active")) {
    activateVoiceTab();
  }

  function updateModeVisibility() {
    const cloudSection = document.getElementById("voice-cloud-section");
    const localSection = document.getElementById("voice-local-section");
    const isLocal = localRadio?.checked;
    if (cloudSection && localSection) {
      cloudSection.classList.toggle('hidden', isLocal);
      localSection.classList.toggle('hidden', !isLocal);
    }
    // 高级选项卡内的流式识别字段：仅本地模式生效
    const streamingField = document.getElementById("voice-streaming-field");
    const streamingCheckbox = document.getElementById("voice-streaming");
    const streamingHint = document.getElementById("voice-streaming-hint");
    if (streamingField) {
      streamingField.classList.toggle("setting-row-dimmed", !isLocal);
    }
    if (streamingCheckbox) {
      streamingCheckbox.disabled = !isLocal;
    }
    if (streamingHint) {
      streamingHint.textContent = isLocal ? "" : t("voice.mode.local_only_hint");
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
      select.innerHTML = `<option value="">${t("voice.local.model.empty")}</option>`;
      return;
    }

    select.innerHTML = models
      .map((m) => {
        const selected = m.is_selected ? "selected" : "";
        const sizeStr = m.size_mb >= 1024
          ? t("voice.local.model.size_gb", { size: (m.size_mb / 1024).toFixed(1) })
          : t("voice.local.model.size_mb", { size: m.size_mb });
        const label = t("voice.local.model.option", { name: m.display_name, params: m.params, size: sizeStr });
        return `<option value="${m.id}" ${selected} title="${m.description || ""}">${label}</option>`;
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
          appendLog(t("voice.local.model.switched_log", { name: m.display_name }));
          // 同步 config（download_stt_model 已持久化 funasr_model + local_model_id）
          cfg.local_engine.funasr_model = m.funasr_model_id;
          cfg.local_model_id = modelId;

          // 如果服务正在运行，提示重启
          const isRunning = startBtn?.classList.contains("running");
          if (isRunning) {
            appendLog(t("voice.local.model.switched_running_log"));
          }
        }
      } catch (e) {
        console.error("download_stt_model failed:", e);
      }
    });
  }
}

// ── 0.10.3 高级选项（热词 / ITN / VAD）──────────────────

// VAD 参数默认值（与 Rust 侧 default_vad_* 一致）
const VAD_DEFAULTS = {
  silence_threshold: 0.005,
  min_silence_ms: 300,
  min_sentence_ms: 800,
};

async function initAdvancedOptions(config) {
  // 流式识别（伪流式：VAD 切句 + 累积预览）——仅本地模式生效
  const streamingCheckbox = document.getElementById("voice-streaming");
  if (streamingCheckbox) {
    streamingCheckbox.checked = config.streaming_mode === "pseudo";
    streamingCheckbox.addEventListener("change", () => {
      config.streaming_mode = streamingCheckbox.checked ? "pseudo" : "off";
      saveSttConfig(config, "global");
    });
  }

  // 热词
  const hotwordsTextarea = document.getElementById("voice-hotwords");
  if (hotwordsTextarea) {
    hotwordsTextarea.value = config.local_engine.hotwords || "";
    // 失焦时保存（避免每次按键都触发保存）
    hotwordsTextarea.addEventListener("blur", () => {
      config.local_engine.hotwords = hotwordsTextarea.value || null;
      saveSttConfig(config, "local");
    });
    // 自动收扁：无内容时高度收扁为单行，有内容时按内容自适应
    function autoResizeHotwords() {
      hotwordsTextarea.style.height = "auto";
      const h = Math.min(Math.max(hotwordsTextarea.scrollHeight, 56), 200);
      hotwordsTextarea.style.height = h + "px";
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
      saveSttConfig(config, "local");
    });
  }

  // VAD 切句参数
  initVadConfig(config);
}

function initVadConfig(config) {
  // 确保 vad 对象存在（旧配置可能没有）
  if (!config.local_engine.vad) {
    config.local_engine.vad = { ...VAD_DEFAULTS };
  }
  const vad = config.local_engine.vad;

  const thresholdInput = document.getElementById("voice-vad-silence-threshold");
  const silenceMsInput = document.getElementById("voice-vad-min-silence-ms");
  const sentenceMsInput = document.getElementById("voice-vad-min-sentence-ms");
  const thresholdVal = document.getElementById("voice-vad-silence-threshold-val");
  const silenceMsVal = document.getElementById("voice-vad-min-silence-ms-val");
  const sentenceMsVal = document.getElementById("voice-vad-min-sentence-ms-val");
  const resetBtn = document.getElementById("voice-vad-reset-btn");

  // 更新滑动条填充进度（CSS 变量 --fill-pct 驱动 linear-gradient）
  function updateSliderFill(slider) {
    if (!slider) return;
    const min = parseFloat(slider.min);
    const max = parseFloat(slider.max);
    const val = parseFloat(slider.value);
    const pct = max > min ? ((val - min) / (max - min)) * 100 : 0;
    slider.style.setProperty("--fill-pct", pct + "%");
  }

  // 回显当前值（缺失时用默认值）
  if (thresholdInput) {
    const v = vad.silence_threshold ?? VAD_DEFAULTS.silence_threshold;
    thresholdInput.value = v;
    if (thresholdVal) thresholdVal.textContent = v.toFixed(3);
    updateSliderFill(thresholdInput);
    thresholdInput.addEventListener("input", () => {
      const val = parseFloat(thresholdInput.value);
      if (thresholdVal) thresholdVal.textContent = val.toFixed(3);
      updateSliderFill(thresholdInput);
    });
    thresholdInput.addEventListener("change", () => {
      const val = parseFloat(thresholdInput.value);
      if (!isNaN(val) && val >= 0.001 && val <= 0.02) {
        vad.silence_threshold = val;
        saveSttConfig(config, "local");
      }
    });
  }

  if (silenceMsInput) {
    const v = vad.min_silence_ms ?? VAD_DEFAULTS.min_silence_ms;
    silenceMsInput.value = v;
    if (silenceMsVal) silenceMsVal.textContent = `${v}ms`;
    updateSliderFill(silenceMsInput);
    silenceMsInput.addEventListener("input", () => {
      const val = parseInt(silenceMsInput.value, 10);
      if (silenceMsVal) silenceMsVal.textContent = `${val}ms`;
      updateSliderFill(silenceMsInput);
    });
    silenceMsInput.addEventListener("change", () => {
      const val = parseInt(silenceMsInput.value, 10);
      if (!isNaN(val) && val >= 100 && val <= 1000) {
        vad.min_silence_ms = val;
        saveSttConfig(config, "local");
      }
    });
  }

  if (sentenceMsInput) {
    const v = vad.min_sentence_ms ?? VAD_DEFAULTS.min_sentence_ms;
    sentenceMsInput.value = v;
    if (sentenceMsVal) sentenceMsVal.textContent = `${v}ms`;
    updateSliderFill(sentenceMsInput);
    sentenceMsInput.addEventListener("input", () => {
      const val = parseInt(sentenceMsInput.value, 10);
      if (sentenceMsVal) sentenceMsVal.textContent = `${val}ms`;
      updateSliderFill(sentenceMsInput);
    });
    sentenceMsInput.addEventListener("change", () => {
      const val = parseInt(sentenceMsInput.value, 10);
      if (!isNaN(val) && val >= 200 && val <= 2000) {
        vad.min_sentence_ms = val;
        saveSttConfig(config, "local");
      }
    });
  }

  // 恢复默认
  if (resetBtn) {
    resetBtn.addEventListener("click", () => {
      vad.silence_threshold = VAD_DEFAULTS.silence_threshold;
      vad.min_silence_ms = VAD_DEFAULTS.min_silence_ms;
      vad.min_sentence_ms = VAD_DEFAULTS.min_sentence_ms;
      if (thresholdInput) thresholdInput.value = VAD_DEFAULTS.silence_threshold;
      if (silenceMsInput) silenceMsInput.value = VAD_DEFAULTS.min_silence_ms;
      if (sentenceMsInput) sentenceMsInput.value = VAD_DEFAULTS.min_sentence_ms;
      if (thresholdVal) thresholdVal.textContent = VAD_DEFAULTS.silence_threshold.toFixed(3);
      if (silenceMsVal) silenceMsVal.textContent = `${VAD_DEFAULTS.min_silence_ms}ms`;
      if (sentenceMsVal) sentenceMsVal.textContent = `${VAD_DEFAULTS.min_sentence_ms}ms`;
      updateSliderFill(thresholdInput);
      updateSliderFill(silenceMsInput);
      updateSliderFill(sentenceMsInput);
      saveSttConfig(config, "local");
    });
  }
}

// ── 模块级统一日志（供 initFunasrEnv 和 initSpaceManagement 共享）──

const _MAX_LOG_LINES = 200;
let _logLines = [];

function appendLog(line) {
  const locale = getLang() === "en" ? "en-US" : "zh-CN";
  const ts = new Date().toLocaleTimeString(locale, { hour12: false });
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
        logCopyBtn.textContent = t("voice.local.log.copied");
        setTimeout(() => { logCopyBtn.textContent = t("voice.local.log.copy_btn"); }, 1500);
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
      saveSttConfig(config, "local");
      appendLog(t("voice.local.device.switched_log", { device: newDevice.toUpperCase() }));

      // 如果服务正在运行，提示重启
      const isRunning = startBtn?.classList.contains("running");
      if (isRunning) {
        appendLog(t("voice.local.device.switched_running_log"));
      }
    });
  }

  // ── 自动启动服务开关 ──
  const autoStartToggle = document.getElementById("funasr-auto-start-toggle");
  if (autoStartToggle) {
    autoStartToggle.checked = config.local_engine.auto_start_server || false;
    autoStartToggle.addEventListener("change", () => {
      config.local_engine.auto_start_server = autoStartToggle.checked;
      saveSttConfig(config, "local");
      if (autoStartToggle.checked) {
        appendLog(t("voice.local.funasr.auto_start.log_enabled"));
        // 如果服务还没启动，立即启动一次
        const isRunning = startBtn?.classList.contains("running");
        if (!isRunning && startBtn) {
          appendLog(t("voice.local.funasr.auto_start.log_starting_now"));
          startBtn.click();
        }
      }
    });
  }

  // ── 查询初始状态 ──
  // 缓存最近一次的 env 快照，语言切换时用它重渲染（避免重新 IPC 探测）
  let _lastEnv = null;

  async function refreshEnv() {
    try {
      const env = await invoke("get_funasr_env");
      _lastEnv = env;
      updateEnvUI(env);
      return env;
    } catch (e) {
      console.error("get_funasr_env failed:", e);
      if (statusText) statusText.textContent = t("voice.local.python_env.status_query_failed");
    }
  }

  await refreshEnv();

  // 语言切换时用缓存的 env 快照重渲染所有状态文本/按钮文案
  // （这些元素文本由 updateEnvUI 动态控制，不能带 data-i18n，否则 applyI18n 会覆盖）
  // 同时纠正诊断按钮的瞬态文案（诊断进行中时 applyI18n 会重置为空闲态默认值）
  onLangChange(() => {
    if (_lastEnv) updateEnvUI(_lastEnv);
    if (diagnoseBtn?.disabled) {
      diagnoseBtn.textContent = t("voice.local.diagnose.running");
    }
  });

  // ── 安装环境按钮 ──
  if (setupBtn) {
    setupBtn.addEventListener("click", async () => {
      setupBtn.classList.remove("success");
      setupBtn.textContent = t("voice.local.python_env.installing");
      if (statusText) statusText.textContent = t("voice.local.python_env.install_status_installing");
      appendLog(t("voice.local.python_env.install_log_start"));

      try {
        await invoke("setup_python_env");
      } catch (e) {
        console.error("setup_python_env failed:", e);
        if (statusText) statusText.textContent = t("voice.local.python_env.install_status_failed", { err: e });
        appendLog(t("voice.local.python_env.install_log_failed", { err: e }));
        setupBtn.textContent = t("voice.local.python_env.install_btn");
      }
    });
  }

  // ── 启动服务按钮 ──
  if (startBtn) {
    startBtn.addEventListener("click", async () => {
      startBtn.classList.remove("running");
      startBtn.textContent = t("voice.local.funasr.btn_starting");
      if (serverStatusText) serverStatusText.textContent = t("voice.local.funasr.start_status_starting");
      appendLog(t("voice.local.funasr.log_starting"));

      try {
        await invoke("start_funasr_server");
      } catch (e) {
        console.error("start_funasr_server failed:", e);
        if (serverStatusText) serverStatusText.textContent = t("voice.local.funasr.start_failed", { err: e });
        appendLog(t("voice.local.funasr.start_failed", { err: e }));
        startBtn.textContent = t("voice.local.funasr.start_btn");
      }
    });
  }

  // ── 停止服务按钮 ──
  if (stopBtn) {
    stopBtn.addEventListener("click", async () => {
      stopBtn.disabled = true;
      appendLog(t("voice.local.funasr.log_stopping"));
      try {
        await invoke("stop_funasr_server");
        if (serverStatusText) serverStatusText.textContent = t("voice.local.funasr.status_stopped");
        appendLog(t("voice.local.funasr.log_stopped"));
        startBtn.classList.remove("running");
        startBtn.textContent = t("voice.local.funasr.start_btn");
        startBtn.disabled = false;
        stopBtn.disabled = false;
      } catch (e) {
        console.error("stop_funasr_server failed:", e);
        appendLog(t("voice.local.funasr.stop_failed", { err: e }));
      }
      refreshEnv();
    });
  }

  // ── 诊断按钮 ──
  if (diagnoseBtn) {
    diagnoseBtn.addEventListener("click", async () => {
      diagnoseBtn.disabled = true;
      diagnoseBtn.textContent = t("voice.local.diagnose.running");
      appendLog(t("voice.local.diagnose.log_start"));

      try {
        const report = await invoke("diagnose_stt");
        // 格式化诊断报告到日志窗口
        const env = report.funasr_env || {};
        appendLog(t("voice.local.diagnose.log_uv", { status: env.uv_available ? "✅" : "❌", version: env.uv_version || "" }));
        appendLog(t("voice.local.diagnose.log_venv", { status: env.venv_exists ? "✅" : "❌", version: env.venv_python_version || "" }));
        appendLog(t("voice.local.diagnose.log_torch", { status: env.torch_installed ? "✅" : "❌", version: env.torch_version || "" }));
        appendLog(t("voice.local.diagnose.log_funasr", { status: env.funasr_installed ? "✅" : "❌", version: env.funasr_version || "" }));
        appendLog(t("voice.local.diagnose.log_server_running", { status: env.server_running ? "✅" : "❌", port: env.server_port }));
        appendLog(t("voice.local.diagnose.log_server_ready", { status: env.server_ready ? "✅" : "❌" }));
        if (env.model_status) {
          appendLog(t("voice.local.diagnose.log_model_status", { status: env.model_status }));
        }
        if (env.websocket_ready !== undefined) {
          appendLog(t("voice.local.diagnose.log_websocket_ready", { status: env.websocket_ready ? "✅" : "❌", err: env.websocket_error ? "(" + env.websocket_error + ")" : "" }));
        }

        const cfg = report.config || {};
        appendLog(t("voice.local.diagnose.log_config", { mode: cfg.mode, model: cfg.local_model_id || "—", device: cfg.device, streaming: cfg.streaming_mode }));

        if (report.api_test) {
          const api = report.api_test;
          if (api.skipped) {
            appendLog(t("voice.local.diagnose.log_api_skip", { reason: api.reason }));
          } else if (api.result?.success) {
            appendLog(t("voice.local.diagnose.log_api_success", { text: api.result.text }));
          } else if (api.result?.error) {
            appendLog(t("voice.local.diagnose.log_api_fail", { err: api.result.error }));
          }
        }
        appendLog(t("voice.local.diagnose.log_end"));
      } catch (e) {
        appendLog(t("voice.local.diagnose.log_failed", { err: e }));
      } finally {
        diagnoseBtn.disabled = false;
        diagnoseBtn.textContent = t("voice.local.diagnose.title");
      }
    });
  }

  // ── 监听 Python 环境安装进度 ──
  listen(EVENTS.PYTHON_ENV_PROGRESS, (event) => {
    const p = event.payload;
    if (!p) return;

    if (p.stage === "complete" && p.status === "ready") {
      if (statusText) statusText.textContent = t("voice.local.python_env.install_status_done");
      if (setupBtn) {
        setupBtn.classList.add("success");
        setupBtn.textContent = t("voice.local.python_env.install_btn_ready");
      }
      if (startBtn) startBtn.disabled = false;
      appendLog(t("voice.local.python_env.install_log_done"));
      refreshEnv();
    } else if (p.stage === "error") {
      if (statusText) statusText.textContent = t("voice.local.python_env.install_status_failed", { err: p.error || "" });
      if (setupBtn) {
        setupBtn.classList.remove("success");
        setupBtn.textContent = t("voice.local.python_env.install_btn");
      }
      appendLog(t("voice.local.python_env.install_log_failed", { err: p.error || "" }));
    } else {
      // 进度更新
      const messages = {
        uv: { starting: t("voice.install.uv.starting"), done: t("voice.install.uv.done") },
        venv: { starting: t("voice.install.venv.starting"), done: t("voice.install.venv.done") },
        torch: { installing: t("voice.install.torch.installing"), done: t("voice.install.torch.done") },
        funasr: { installing: t("voice.install.funasr.installing"), done: t("voice.install.funasr.done") },
      };
      const msg = messages[p.stage]?.[p.status];
      if (msg && statusText) statusText.textContent = msg;
      if (msg) appendLog(`[Blink] ${msg}`);
    }
  });

  // ── 回补历史日志 ──
  // 服务可能在设置页打开前就自启动（auto_start_server），此时前端
  // listen 尚未注册，日志只存在于后端缓冲区。先拉取历史日志再注册监听。
  try {
    const history = await invoke("get_funasr_log_history");
    if (Array.isArray(history) && history.length > 0) {
      _logLines = history.slice(-_MAX_LOG_LINES);
      if (logPre) {
        logPre.textContent = _logLines.join("\n");
        logPre.scrollTop = logPre.scrollHeight;
      }
    }
  } catch (e) {
    console.error("[voice] get_funasr_log_history failed:", e);
  }

  // ── 监听 funasr-server 日志输出 ──
  listen(EVENTS.FUNASR_SERVER_LOG, (event) => {
    const line = event.payload?.line;
    if (line) appendLog(line);
  });

  // ── 监听服务状态事件 ──
  listen(EVENTS.FUNASR_SERVER_STATUS, (event) => {
    const p = event.payload;
    if (!p) return;

    if (p.stage === "ready") {
      if (serverStatusText) serverStatusText.textContent = t("voice.local.funasr.status_ready", { model: p.model });
      if (startBtn) {
        startBtn.classList.add("running");
        startBtn.textContent = t("voice.local.funasr.btn_running");
        startBtn.disabled = false;
      }
      if (stopBtn) stopBtn.disabled = false;
      appendLog(t("voice.local.funasr.log_started", { model: p.model }));
    } else if (p.stage === "already_running") {
      if (serverStatusText) serverStatusText.textContent = t("voice.local.funasr.status_ready", { model: p.model });
      if (startBtn) {
        startBtn.classList.add("running");
        startBtn.textContent = t("voice.local.funasr.btn_running");
        startBtn.disabled = false;
      }
      if (stopBtn) stopBtn.disabled = false;
      appendLog(t("voice.local.funasr.log_already_running", { model: p.model }));
    } else if (p.stage === "error") {
      if (serverStatusText) serverStatusText.textContent = t("voice.local.funasr.status_error", { err: p.error || "" });
      if (startBtn) {
        startBtn.classList.remove("running");
        startBtn.textContent = t("voice.local.funasr.start_btn");
        startBtn.disabled = false;
      }
      if (stopBtn) stopBtn.disabled = true;
      appendLog(t("voice.local.funasr.log_error", { err: p.error || "" }));
    } else if (p.stage === "starting") {
      if (serverStatusText) serverStatusText.textContent = t("voice.local.funasr.status_starting", { model: p.model, port: p.port });
      appendLog(t("voice.local.funasr.log_starting_detail", { model: p.model, port: p.port, device: config.local_engine.device }));
    } else if (p.stage === "loading_model") {
      if (serverStatusText) serverStatusText.textContent = t("voice.local.funasr.status_loading_model", { model: p.model });
      if (startBtn) {
        startBtn.textContent = t("voice.local.funasr.btn_loading_model");
        startBtn.disabled = true;
      }
      appendLog(t("voice.local.funasr.log_loading_model"));
    } else if (p.stage === "setup_env") {
      if (serverStatusText) serverStatusText.textContent = t("voice.local.funasr.status_setup_env");
      appendLog(t("voice.local.funasr.log_setup_env"));
    }
  });

  function updateEnvUI(env) {
    if (!statusText) return;

    const parts = [];

    if (!env.uv_available) {
      parts.push(t("voice.env.uv.not_installed"));
    } else {
      parts.push(t("voice.env.uv.installed", { version: env.uv_version || "" }));
    }

    if (!env.venv_exists) {
      parts.push(t("voice.env.venv.not_created"));
    } else {
      parts.push(t("voice.env.venv.created", { version: env.venv_python_version || "" }));
    }

    if (!env.torch_installed) {
      parts.push(t("voice.env.torch.not_installed"));
    } else {
      parts.push(t("voice.env.torch.installed", { version: env.torch_version || "" }));
    }

    if (!env.funasr_installed) {
      parts.push(t("voice.env.funasr.not_installed"));
    } else {
      parts.push(t("voice.env.funasr.installed", { version: env.funasr_version || "" }));
    }

    // 综合状态 + 按钮控制（不置灰，用绿色样式标记成功）
    if (env.env_ready) {
      if (setupBtn) {
        setupBtn.classList.add("success");
        setupBtn.textContent = t("voice.local.python_env.install_btn_ready");
        setupBtn.disabled = false;
      }
      if (startBtn && !env.server_running) {
        startBtn.classList.remove("running");
        startBtn.textContent = t("voice.local.funasr.start_btn");
        startBtn.disabled = false;
      }
    } else {
      if (setupBtn) {
        setupBtn.classList.remove("success");
        setupBtn.textContent = t("voice.local.python_env.install_btn");
        setupBtn.disabled = false;
      }
      if (startBtn) {
        startBtn.classList.remove("running");
        startBtn.textContent = t("voice.local.funasr.start_btn");
        startBtn.disabled = true;
      }
    }

    // 服务运行状态
    if (env.server_running) {
      if (startBtn) {
        startBtn.classList.add("running");
        startBtn.textContent = t("voice.local.funasr.btn_running");
        startBtn.disabled = false;
      }
      if (stopBtn) stopBtn.disabled = false;
      if (serverStatusText) serverStatusText.textContent = t("voice.local.funasr.server_running", { model: env.server_model });
    } else {
      if (startBtn && env.env_ready) {
        startBtn.classList.remove("running");
        startBtn.textContent = t("voice.local.funasr.start_btn");
        startBtn.disabled = false;
      }
      if (stopBtn) stopBtn.disabled = true;
    }

    // 设备选择器在服务运行时禁用
    if (deviceSelect) {
      deviceSelect.disabled = env.server_running;
    }

    statusText.innerHTML = parts.join("<br>");

    // Python 环境卡片：环境就绪时自动收起（仅显示摘要），未就绪时自动展开
    const envCard = document.getElementById("python-env-card");
    const envSummary = document.getElementById("python-env-summary");
    if (envCard && envSummary) {
      if (env.env_ready) {
        envSummary.textContent = t("voice.local.python_env.summary_ready");
        envSummary.classList.add("ready");
        envCard.open = false;
      } else {
        envSummary.textContent = t("voice.local.python_env.summary_not_ready");
        envSummary.classList.remove("ready");
        envCard.open = true;
      }
    }
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
      btn.textContent = t("voice.audio_test.start");
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
    btn.textContent = t("voice.audio_test.stop");
    btn.classList.add("active");

    const deviceSelect = document.getElementById("voice-audio-device");
    const deviceId = deviceSelect?.value || null;

    try {
      await invoke("start_audio_test", { deviceId });
    } catch (e) {
      console.error("[voice] start_audio_test failed:", e);
      audioTestActive = false;
      btn.textContent = t("voice.audio_test.start");
      btn.classList.remove("active");
      bar.style.background = "var(--color-danger, #e53e3e)";
      bar.style.width = "100%";
      bar.textContent = e;
    }
  });

  // 语言切换时刷新按钮文案（测试进行中显示「停止测试」，否则显示「开始测试」）
  onLangChange(() => {
    if (!btn) return;
    btn.textContent = audioTestActive ? t("voice.audio_test.stop") : t("voice.audio_test.start");
  });

  listen(EVENTS.AUDIO_TEST_LEVEL, (event) => {
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

// ── helper ────────────────────────────────────────────────────────────


async function initSpaceManagement() {
  const container = document.getElementById("stt-space-usage");
  if (!container) return;

  // 缓存最近一次加载的数据，语言切换时用它重渲染（避免重新探测）
  let lastData = null;

  async function loadUsage() {
    // 首次加载时显示 loading；刷新时保留旧内容避免闪屏
    const isFirstLoad = !container.querySelector(".stt-space-row");
    if (isFirstLoad) {
      container.innerHTML = `<div class="stt-space-loading">${t("voice.local.space.loading")}</div>`;
    }
    try {
      const data = await invoke("get_stt_space_usage");
      lastData = data;
      renderUsage(data);
    } catch (e) {
      console.error("get_stt_space_usage failed:", e);
      container.innerHTML = `<div class="stt-space-loading">${t("voice.local.space.load_failed")}</div>`;
    }
  }

  // 语言切换时用缓存数据重渲染（仅在已加载过的情况下）
  onLangChange(() => {
    if (lastData) renderUsage(lastData);
  });

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
        <span class="stt-space-total-label">${t("voice.local.space.total_label")}</span>
        <span class="stt-space-total-size">${formatSize(total)}</span>
      </div>
      <div class="stt-space-actions">
        <button type="button" class="stt-space-cleanup-btn" id="stt-space-cleanup-btn">${t("voice.local.space.cleanup_btn")}</button>
        <button type="button" class="stt-space-open-folder-btn" id="stt-space-open-folder-btn">${t("voice.local.space.open_folder_btn")}</button>
        <button type="button" class="stt-space-refresh-btn" id="stt-space-refresh-btn">${t("voice.local.space.refresh_btn")}</button>
      </div>
      <p class="stt-space-hint">
        ${t("voice.local.space.hint")}
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
          const confirmed = await confirmDialog(t("voice.local.space.cleanup_confirm_body"));
          if (!confirmed) return;
          appendLog(t("voice.local.space.cleanup_log_stopping"));
          try {
            await invoke("stop_funasr_server");
            appendLog(t("voice.local.space.cleanup_log_stopped"));

            // 同步重置启动/停止按钮状态（服务已在后端停止）
            const startBtn = document.getElementById("funasr-start-btn");
            const stopBtn = document.getElementById("funasr-stop-btn");
            const serverStatusText = document.getElementById("funasr-server-status-text");
            if (startBtn) {
              startBtn.classList.remove("running");
              startBtn.textContent = t("voice.local.funasr.start_btn");
              startBtn.disabled = false;
            }
            if (stopBtn) stopBtn.disabled = true;
            if (serverStatusText) serverStatusText.textContent = t("voice.local.space.cleanup_status_stopped");
          } catch (e) {
            console.error("stop_funasr_server failed:", e);
            appendLog(t("voice.local.space.cleanup_log_stop_failed", { err: e }));
          }
        }

        cleanupBtn.disabled = true;
        cleanupBtn.textContent = t("voice.local.space.cleanup_btn_cleaning");
        try {
          await invoke("cleanup_stt_space");
          cleanupBtn.textContent = t("voice.local.space.cleanup_btn_done");
          appendLog(t("voice.local.space.cleanup_log_done"));
          setTimeout(() => {
            loadUsage();
          }, 1000);
        } catch (e) {
          console.error("cleanup_stt_space failed:", e);
          cleanupBtn.textContent = t("voice.local.space.cleanup_btn_failed");
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
          appendLog(t("voice.local.space.log_open_failed", { err: e }));
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
