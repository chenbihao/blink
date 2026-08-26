/**
 * 语音输入 Tab 模块（0.22.5 重构）
 * STT 配置：总开关 / 模式切换 / 云端供应商 / 音频设备选择 + 调试 / 高级选项（热词 / ITN / VAD）
 *
 * FunASR 生命周期管理（环境安装 / 服务启停 / 设备切换 / 日志 / 空间管理）
 * 已迁移至引擎页「本地模型运行时」区域（engines/local-runtime）。
 * 本模块仅保留语音业务配置，并提供跳转入口。
 *
 * 云端 STT 架构（独立模式）：
 * - 配置完全独立于 AIConfig——用户在语音设置页直接配置 kind/base_url/model_id
 * - API Key 用 stt:cloud 前缀存在 Credential Manager 里，不与 AI 供应商共用
 * - 支持预设快捷填充（OpenAI / Groq / MiMo）
 */
import {invoke, listen} from "../../shared/tauri.js";
import {EVENTS} from "../../shared/event-names.js";
import {onLangChange, t} from "../../i18n/index.js";
import {ensureLocalRuntimeMounted, waitForEngineCard} from "../index.js";

/**
 * 保存 STT 配置。
 * scope 决定后端控制台日志打印哪个区段，避免改本地配置时把云端字段也全部打印出来：
 * - "global": 总开关 / 模式 / 流式 / 音频设备
 * - "cloud":  云端供应商
 * - "local":  本地引擎（热词 / ITN / VAD）
 */
function saveSttConfig(cfg, scope) {
    invoke("set_stt_config", {config: cfg, scope}).catch(console.error);
}

/**
 * 初始化语音输入 Tab。
 *
 * FunASR 生命周期管理已迁移至引擎页，本模块不再 invoke
 * get_funasr_env / setup_python_env / start_funasr_server / stop_funasr_server。
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

    // 音频设备
    const deviceSelect = document.getElementById("voice-audio-device");
    if (deviceSelect) {
        // 加载音频设备列表
        try {
            const devices = await invoke("list_audio_devices");
            deviceSelect.innerHTML = "";
            const defaultOpt = document.createElement("option");
            defaultOpt.value = "";
            defaultOpt.textContent = t("voice.audio_device.default");
            deviceSelect.appendChild(defaultOpt);
            for (const dev of devices) {
                const opt = document.createElement("option");
                opt.value = dev.id;
                opt.textContent = dev.name || t("voice.audio_device.device_n", {id: dev.id});
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
        openai: {kind: "openai", base_url: "https://api.openai.com/v1", model_id: "whisper-1"},
        groq: {kind: "groq", base_url: "https://api.groq.com/openai/v1", model_id: "whisper-large-v3"},
        mimo: {kind: "mimo", base_url: "https://api.xiaomimimo.com/v1", model_id: ""},
        custom: {kind: "openai", base_url: "", model_id: ""},
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
            config.cloud_provider = {kind, base_url, model_id};
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
                await invoke("save_stt_secret", {secret});
                if (apiKeyInput) apiKeyInput.value = "";
                await refreshKeyHint();
                keySaveBtn.textContent = t("voice.cloud.api_key.saved");
                setTimeout(() => {
                    keySaveBtn.textContent = t("voice.cloud.api_key.save_btn");
                }, 1500);
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
                        testResult.textContent = t("voice.cloud.test.success", {text: result.text});
                        testResult.className = "voice-cloud-test-result success";
                    } else {
                        testResult.textContent = t("voice.cloud.test.fail", {err: result.error});
                        testResult.className = "voice-cloud-test-result error";
                    }
                }
            } catch (e) {
                if (testResult) {
                    testResult.textContent = t("voice.cloud.test.fail", {err: e});
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

    // FunASR 本地模型选择（业务设置）
    initLocalModelSelect(config);

    // ── 跳转入口：点击切换到引擎页并定位 FunASR 卡片 ──
    const gotoEnginesBtn = document.getElementById("voice-goto-engines-btn");
    if (gotoEnginesBtn) {
        gotoEnginesBtn.addEventListener("click", async () => {
            const enginesTabBtn = document.querySelector('.tab[data-tab="engines"]');
            if (enginesTabBtn) {
                enginesTabBtn.click();
            }
            try {
                // 1. 激活 engines tab（上面 click 已做）
                // 2. await runtime mount
                await ensureLocalRuntimeMounted();
                // 3. await funasr card 已渲染
                const funasrCard = await waitForEngineCard("funasr");
                if (funasrCard) {
                    // 4. scrollIntoView
                    funasrCard.scrollIntoView({behavior: "smooth", block: "center"});
                    // 5. focus（card 有 tabindex="-1"）
                    funasrCard.focus({preventScroll: true});
                } else {
                    // mount 失败或卡片未渲染——聚焦 error region
                    const errorRegion = document.getElementById("le-error-region");
                    if (errorRegion && !errorRegion.hidden) {
                        errorRegion.scrollIntoView({behavior: "smooth", block: "center"});
                        const textEl = document.getElementById("le-error-text");
                        if (textEl) textEl.focus({preventScroll: true});
                    } else {
                        // 兑现 fallback：滚动到 section anchor
                        const anchor = document.getElementById("local-model-runtime");
                        if (anchor) anchor.scrollIntoView({behavior: "smooth"});
                    }
                }
            } catch (e) {
                console.error("[voice] goto engines failed:", e);
            }
        });
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

    // loadLocalModels 已迁移至引擎页 local-runtime controller
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
        config.local_engine.vad = {...VAD_DEFAULTS};
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
            await invoke("start_audio_test", {deviceId});
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

// ── FunASR 本地模型选择（业务设置） ──────────────────────────────────────────

/**
 * 初始化本地模型选择器。
 *
 * 模型列表来自后端 `list_stt_models`，选择后通过 `download_stt_model` 保存。
 * 这是语音业务设置（选哪个模型），不是生命周期管理（安装/启停）。
 */
async function initLocalModelSelect(config) {
    const select = document.getElementById("voice-local-model-select");
    if (!select) return;

    // 加载模型列表
    let models;
    try {
        models = await invoke("list_stt_models");
    } catch (e) {
        console.error("list_stt_models failed:", e);
        return;
    }

    if (!models || models.length === 0) {
        const empty = document.createElement("option");
        empty.textContent = t("voice.local.model.empty");
        empty.disabled = true;
        select.appendChild(empty);
        return;
    }

    // 填充选项
    for (const m of models) {
        const opt = document.createElement("option");
        opt.value = m.id;
        const sizeStr = m.size_mb >= 1024
            ? t("voice.local.model.size_gb", {size: (m.size_mb / 1024).toFixed(1)})
            : t("voice.local.model.size_mb", {size: m.size_mb});
        opt.textContent = t("voice.local.model.option", {name: m.display_name, params: m.params, size: sizeStr});
        if (m.is_selected) {
            opt.selected = true;
        }
        select.appendChild(opt);
    }

    // 选择变更 → 保存
    select.addEventListener("change", async () => {
        const modelId = select.value;
        try {
            await invoke("download_stt_model", {modelId});
            // 更新 config 中的 funasr_model
            const model = models.find((m) => m.id === modelId);
            if (model) {
                config.local_engine.funasr_model = model.funasr_model_id;
                config.local_model_id = modelId;
            }
        } catch (e) {
            console.error("download_stt_model failed:", e);
        }
    });
}

// ── FunASR 生命周期管理已迁移至引擎页 local-runtime controller ──
// 以下函数保留为空壳，防止外部引用报错（后端旧兼容 commands 暂不删除）
async function initFunasrEnv(_config) {
    // migrated to engines/local-runtime
}

async function initSpaceManagement() {
    // migrated to engines/local-runtime
}
