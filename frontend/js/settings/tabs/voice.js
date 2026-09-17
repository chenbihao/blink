/**
 * 语音输入 Tab 模块（0.22.5 重构）
 * STT 配置：总开关 / 模式切换 / 云端供应商 / 音频设备选择 + 调试 /
 * 高级选项（VAD、Preview / Draft）
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
import {commandErrorText, invoke, listen} from "../../shared/tauri.js";
import {copyToClipboard} from "../../shared/api.js";
import {EVENTS} from "../../shared/event-names.js";
import {iconHTML} from "../../shared/icon.js";
import {onLangChange, t} from "../../i18n/index.js";
import {ensureLocalRuntimeMounted, getLocalEngineEntry, waitForEngineCard} from "../index.js";
import {navigateSettings} from "../navigation.js";
import {formatAudioTranscriptionIdentity, parseAudioTranscriptionCapability,} from "./voice-file-transcribe.js";
import {buildVadDebugCopyText, renderVadDebugResult, vadDebugProgressState, renderCoordinatorTrace} from "./voice-vad-debug.js";
import {VAD_DEFAULTS, VAD_WINDOW_KEYS, VAD_WINDOW_RANGE, ensureVadWindowFields, normalizeVadWindows,} from "./voice-vad.js";
import {RECOGNITION_DEFAULTS, RECOGNITION_RANGE, ensureRecognitionFields, normalizeRecognitionConfig,} from "./voice-recognition.js";

/**
 * 顺序化保存队列——确保 set_stt_config 请求严格按发起顺序到达后端，
 * 避免快速连续操作时旧请求覆盖新值（乐观并发控制）。
 *
 * 每次保存都会发送完整的 config 快照，如果两个请求并发发出，
 * 后到的请求可能用不含前一次修改的旧快照覆盖——串行化后此问题消除。
 */
let sttSaveQueue = Promise.resolve();

/**
 * 保存 STT 配置。
 * scope 决定后端控制台日志打印哪个区段，避免改本地配置时把云端字段也全部打印出来：
 * - "global": 总开关 / 模式 / 流式 / 音频设备
 * - "cloud":  云端供应商
 * - "local":  本地引擎（VAD、Preview / Draft）
 */
function saveSttConfig(cfg, scope) {
    // 串行化：每个保存操作等前一个完成后才执行，保证后端按序持久化
    sttSaveQueue = sttSaveQueue
        .then(() => invoke("set_stt_config", {config: cfg, scope}))
        .catch((e) => {
            console.error("set_stt_config failed:", e);
        });
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
    initFileTranscription();
    initVadDebug(config);
    initCoordinatorTrace(config);

    // ── 跳转入口：点击切换到引擎页并定位 FunASR 卡片 ──
    const gotoEnginesBtn = document.getElementById("voice-goto-engines-btn");
    if (gotoEnginesBtn) {
        gotoEnginesBtn.addEventListener("click", async () => {
            try {
                let funasrCard = null;
                await navigateSettings({
                    tabId: "engines",
                    prepare: async () => {
                        await ensureLocalRuntimeMounted();
                        funasrCard = await waitForEngineCard("funasr");
                    },
                    target: () => {
                        if (funasrCard) {
                            return document.getElementById("local-model-runtime") || funasrCard;
                        }
                        const errorRegion = document.getElementById("le-error-region");
                        if (errorRegion && !errorRegion.hidden) return errorRegion;
                        return document.getElementById("local-model-runtime");
                    },
                    focusTarget: () => {
                        if (funasrCard) return funasrCard;
                        const textEl = document.getElementById("le-error-text");
                        return textEl && !document.getElementById("le-error-region")?.hidden
                            ? textEl : null;
                    },
                });
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
        const recognitionCard = document.getElementById("voice-recognition-card");
        if (recognitionCard) {
            const isPseudo = config.streaming_mode === "pseudo"
                && document.getElementById("voice-streaming")?.checked !== false;
            recognitionCard.classList.toggle("hidden", !(isLocal && isPseudo));
        }
    }

    // loadLocalModels 已迁移至引擎页 local-runtime controller
}

function initFileTranscription() {
    const button = document.getElementById("voice-file-transcribe-btn");
    const debugButton = document.getElementById("voice-vad-debug-btn");
    const status = document.getElementById("voice-file-transcribe-status");
    const output = document.getElementById("voice-file-transcribe-output");
    const meta = document.getElementById("voice-file-transcribe-meta");
    if (!button || !status || !output || !meta || button.dataset.bound === "true") return;
    button.dataset.bound = "true";

    button.addEventListener("click", async () => {
        if (button.disabled) return;
        button.disabled = true;
        if (debugButton) debugButton.disabled = true;
        status.textContent = t("voice.local.file.picking");
        status.className = "voice-file-transcribe-status";
        output.hidden = true;
        output.textContent = "";
        meta.textContent = "";

        try {
            const audioRef = await invoke("pick_audio_file");
            if (!audioRef) {
                status.textContent = t("voice.local.file.cancelled");
                return;
            }
            status.textContent = t("voice.local.file.running");
            const result = await invoke("transcribe_audio_file", {audioRef});
            const data = parseAudioTranscriptionCapability(result);
            output.textContent = data.noSpeech ? t("voice.local.file.no_speech") : data.text;
            output.hidden = false;
            meta.textContent = formatAudioTranscriptionIdentity(data);
            status.textContent = t("voice.local.file.done");
            status.className = "voice-file-transcribe-status success";
        } catch (error) {
            status.textContent = commandErrorText(error, t("voice.local.file.failed"));
            status.className = "voice-file-transcribe-status error";
        } finally {
            button.disabled = false;
            if (debugButton) debugButton.disabled = false;
        }
    });
}

function initVadDebug(config) {
    const button = document.getElementById("voice-vad-debug-btn");
    const transcribeButton = document.getElementById("voice-file-transcribe-btn");
    const status = document.getElementById("voice-vad-debug-status");
    const file = document.getElementById("voice-vad-debug-file");
    const progress = document.getElementById("voice-vad-debug-progress");
    const progressBar = document.getElementById("voice-vad-debug-progress-bar");
    const progressTime = document.getElementById("voice-vad-debug-progress-time");
    const progressDetail = document.getElementById("voice-vad-debug-progress-detail");
    const result = document.getElementById("voice-vad-debug-result");
    const finalTitle = document.getElementById("voice-vad-debug-final-title");
    const transcript = document.getElementById("voice-vad-debug-transcript");
    const chart = document.getElementById("voice-vad-debug-chart");
    const transport = document.getElementById("voice-vad-debug-transport");
    const playButton = document.getElementById("voice-vad-debug-play");
    const playTime = document.getElementById("voice-vad-debug-play-time");
    const copyButton = document.getElementById("voice-vad-debug-copy");
    if (!button || !status || !file || !progress || !progressBar || !progressTime || !progressDetail
        || !result || !finalTitle || !transcript || !chart || !transport || !playButton || !playTime
        || !copyButton || button.dataset.bound === "true") return;
    button.dataset.bound = "true";
    let runSerial = 0;
    // 最近一次成功回放的完整结果与文件名——"复制调试信息"的唯一数据来源。
    // 新一轮开始时清空，避免失败后复制到上一轮的陈旧数据。
    let lastDebugResult = null;
    let lastFileLabel = "";

    /** 复制载荷里的参数快照；传入最新配置时优先用它。 */
    function debugSettingsSnapshot(source) {
        const cfg = source || config || {};
        const vad = cfg.local_engine?.vad || {};
        const recognition = cfg.local_engine?.recognition || {};
        return {
            silence_threshold: vad.silence_threshold,
            min_silence_ms: vad.min_silence_ms,
            min_sentence_ms: vad.min_sentence_ms,
            soft_window_s: vad.soft_window_s,
            hard_window_s: vad.hard_window_s,
            max_uncommitted_s: vad.max_uncommitted_s,
            draft_min_s: recognition.draft_min_s,
            strong_pause_ms: recognition.strong_pause_ms,
            long_pause_ms: recognition.long_pause_ms,
            preview_window_ms: recognition.preview_window_ms,
            preview_refresh_ms: recognition.preview_refresh_ms,
        };
    }

    // ── 回放（调试增强）：WAV 字节经原始 IPC 到前端后 blob 播放，失败静默降级 ──
    const playback = {audio: null, url: "", raf: 0};

    function formatPlaySeconds(value) {
        return `${Number(value).toFixed(1)}s`;
    }

    function resetPlayback() {
        if (playback.raf) cancelAnimationFrame(playback.raf);
        playback.raf = 0;
        if (playback.audio) {
            playback.audio.pause();
            playback.audio.removeAttribute("src");
            playback.audio.load();
        }
        if (playback.url) URL.revokeObjectURL(playback.url);
        playback.audio = null;
        playback.url = "";
        transport.hidden = true;
        chart.classList.remove("seekable");
    }

    function updatePlayback() {
        const audio = playback.audio;
        if (!audio) return;
        const duration = Number.isFinite(audio.duration) && audio.duration > 0 ? audio.duration : 0;
        const playhead = chart.querySelector(".vad-playhead");
        if (playhead) {
            const x = duration ? (Math.min(1, audio.currentTime / duration) * 1000).toFixed(1) : "0";
            playhead.setAttribute("x1", x);
            playhead.setAttribute("x2", x);
        }
        playTime.textContent = `${formatPlaySeconds(audio.currentTime)} / ${formatPlaySeconds(duration)}`;
    }

    function setPlayButton(playing) {
        playButton.innerHTML = iconHTML(playing ? "pause" : "play");
        const label = t(playing ? "voice.local.vad_debug.pause" : "voice.local.vad_debug.play");
        playButton.setAttribute("aria-label", label);
        playButton.title = label;
    }

    function setupPlayback(playbackRef, epoch) {
        invoke("read_audio_for_playback", {audioRef: playbackRef}).then(buffer => {
            if (epoch !== runSerial) return; // 新一轮已开始，旧回流不得覆盖（竞态防护）
            const url = URL.createObjectURL(new Blob([buffer], {type: "audio/wav"}));
            const audio = new Audio(url);
            playback.audio = audio;
            playback.url = url;
            audio.addEventListener("play", () => {
                setPlayButton(true);
                if (!playback.raf) playback.raf = requestAnimationFrame(function frame() {
                    updatePlayback();
                    if (playback.audio && !playback.audio.paused) playback.raf = requestAnimationFrame(frame);
                    else playback.raf = 0;
                });
            });
            const onStopped = () => {
                setPlayButton(false);
                if (playback.raf) {
                    cancelAnimationFrame(playback.raf);
                    playback.raf = 0;
                }
                updatePlayback();
            };
            audio.addEventListener("pause", onStopped);
            audio.addEventListener("ended", onStopped);
            transport.hidden = false;
            chart.classList.add("seekable");
            setPlayButton(false);
            updatePlayback();
        }).catch(error => {
            console.warn("VAD playback unavailable:", error);
        });
    }

    playButton.addEventListener("click", () => {
        const audio = playback.audio;
        if (!audio) return;
        if (audio.paused) audio.play().catch(() => {});
        else audio.pause();
    });

    // 点击图表按时间定位（SVG viewBox 宽 1000 即全程，线性映射）
    chart.addEventListener("click", event => {
        const audio = playback.audio;
        if (!audio || !Number.isFinite(audio.duration) || audio.duration <= 0) return;
        const svg = chart.querySelector("svg");
        if (!svg) return;
        const rect = svg.getBoundingClientRect();
        if (!rect.width) return;
        const ratio = Math.min(1, Math.max(0, (event.clientX - rect.left) / rect.width));
        audio.currentTime = ratio * audio.duration;
        updatePlayback();
    });

    // ── 复制调试信息：环境与参数 + 最终全文 + 切句与识别 + 能量轨迹 + 原始 JSON ──
    // 走系统剪贴板 command（WebView 的 navigator.clipboard 在设置窗口不可靠）。
    copyButton.addEventListener("click", async () => {
        if (!lastDebugResult || copyButton.disabled) return;
        copyButton.disabled = true;
        // 先取快照再 await：等待期间若发起新一轮回放会清空 lastDebugResult，
        // 捕获后的局部引用保证复制的是用户点下按钮时看到的那一份结果。
        const snapshot = lastDebugResult;
        const fileLabel = lastFileLabel;
        const label = copyButton.querySelector("span");
        try {
            let settings = debugSettingsSnapshot();
            try {
                // 复制瞬间重新取一次配置：面板持有的快照可能落后于刚保存的滑块值。
                settings = debugSettingsSnapshot(await invoke("get_stt_config"));
            } catch (error) {
                console.warn("[vad-debug] settings snapshot unavailable:", error);
            }
            const text = buildVadDebugCopyText(snapshot, {t, fileLabel, settings});
            await copyToClipboard(text);
            if (label) label.textContent = t("voice.local.vad_debug.copy_done");
        } catch (error) {
            console.error("[vad-debug] copy failed:", error);
            if (label) label.textContent = t("voice.local.vad_debug.copy_failed");
        } finally {
            window.setTimeout(() => {
                if (label) label.textContent = t("voice.local.vad_debug.copy_debug");
                copyButton.disabled = false;
            }, 1200);
        }
    });

    function showProgress(payload) {
        const state = vadDebugProgressState(payload, t);
        if (!state) return;
        progress.hidden = false;
        if (status.textContent !== state.status) status.textContent = state.status;
        if (state.percent == null) progressBar.removeAttribute("value");
        else progressBar.value = state.percent;
        progressTime.textContent = state.time;
        progressDetail.textContent = state.detail;
    }

    button.addEventListener("click", async () => {
        if (button.disabled) return;
        button.disabled = true;
        if (transcribeButton) transcribeButton.disabled = true;
        result.hidden = true;
        resetPlayback();
        file.hidden = true;
        finalTitle.hidden = true;
        transcript.hidden = true;
        progress.hidden = true;
        lastDebugResult = null;
        lastFileLabel = "";
        status.className = "voice-file-transcribe-status";
        status.textContent = t("voice.local.file.picking");
        let unlisten = null;
        try {
            const picked = await invoke("pick_audio_file_for_vad_debug");
            if (!picked) {
                status.textContent = t("voice.local.file.cancelled");
                return;
            }
            file.textContent = `${t("voice.local.vad_debug.selected_file")} ${picked.displayName}`;
            file.hidden = false;
            const runId = `${Date.now()}-${++runSerial}`;
            // 分析会一次性消费原 ref；回放取字节用克隆 ref，两条授权互不影响
            let playbackRef = null;
            try {
                playbackRef = await invoke("clone_audio_ref_for_vad_debug", {audioRef: picked.audioRef});
            } catch (error) {
                console.warn("VAD playback clone unavailable:", error);
            }
            showProgress({phase: "preparing", fedMs: 0, durationMs: 0});
            try {
                unlisten = await listen(EVENTS.STT_VAD_DEBUG_PROGRESS, event => {
                    if (event.payload?.runId === runId) showProgress(event.payload);
                });
            } catch (error) {
                console.warn("VAD progress listener unavailable:", error);
            }
            const data = await invoke("debug_vad_audio_file", {audioRef: picked.audioRef, runId});
            showProgress({phase: "done", fedMs: data.duration_ms, durationMs: data.duration_ms});
            renderVadDebugResult(data, {
                chart: document.getElementById("voice-vad-debug-chart"),
                events: document.getElementById("voice-vad-debug-events"),
                transcript,
                meta: document.getElementById("voice-vad-debug-meta"),
            }, t);
            result.hidden = false;
            finalTitle.hidden = false;
            transcript.hidden = false;
            lastDebugResult = data;
            lastFileLabel = picked.displayName || "";
            status.textContent = t("voice.local.vad_debug.done");
            status.className = "voice-file-transcribe-status success";
            if (playbackRef) setupPlayback(playbackRef, runSerial);
        } catch (error) {
            progress.hidden = true;
            status.textContent = commandErrorText(error, t("voice.local.vad_debug.failed"));
            status.className = "voice-file-transcribe-status error";
        } finally {
            if (unlisten) unlisten();
            button.disabled = false;
            if (transcribeButton) transcribeButton.disabled = false;
        }
    });
}

// ── 0.23.9: 实时协调器状态面板 ──────────────────────────────────────────
//
// 录音时从后端 `get_coordinator_trace` 命令拉取当前生产会话的
// RecognitionCoordinator 诊断快照，定期渲染到面板。
// 不录音时显示空状态。面板始终可见（不 hidden），让用户知道功能存在。

/**
 * 初始化协调器状态面板。
 *
 * 轮询策略：设置页可见 + 录音中时每 500ms 拉取一次 trace；
 * 不满足条件时显示空状态。
 */
async function initCoordinatorTrace(config) {
    const panel = document.getElementById("voice-coordinator-trace-panel");
    const body = document.getElementById("voice-coordinator-trace-body");
    if (!panel || !body) return;

    let polling = false;
    let pollTimer = null;

    async function pollOnce() {
        try {
            const maxUncommittedS = Number(config?.local_engine?.vad?.max_uncommitted_s) || 12;
            const trace = await invoke("get_coordinator_trace", {maxUncommittedS});
            renderCoordinatorTrace(trace, body, t);
        } catch (e) {
            // 静默失败：录音未启动或引擎不是伪流式
            renderCoordinatorTrace(null, body, t);
        }
    }

    async function pollLoop() {
        if (!polling) return;
        await pollOnce();
        if (!polling) return;
        pollTimer = setTimeout(pollLoop, 500);
    }

    function startPolling() {
        if (polling) return;
        polling = true;
        pollLoop();
    }

    function stopPolling() {
        polling = false;
        if (pollTimer) {
            clearTimeout(pollTimer);
            pollTimer = null;
        }
        // 恢复空状态
        renderCoordinatorTrace(null, body, t);
    }

    // 通过 is_voice_recording 判断录音状态，启动/停止轮询
    async function checkRecordingState() {
        try {
            const recording = await invoke("is_voice_recording");
            if (recording) {
                startPolling();
            } else {
                stopPolling();
            }
        } catch {
            stopPolling();
        }
    }

    // 监听录音状态变化事件
    try {
        await listen(EVENTS.VOICE_RECORDING_START, () => startPolling());
        await listen(EVENTS.VOICE_RECORDING_END, () => stopPolling());
    } catch {
        // 事件监听失败时降级为定期检查
    }

    // 定期检查录音状态（降级兜底，3s 一次）
    setInterval(checkRecordingState, 3000);
    // 初始检查一次
    checkRecordingState();

    // 语言切换时重新渲染空状态文案
    onLangChange(() => {
        if (!polling) renderCoordinatorTrace(null, body, t);
    });
}

// ── 0.10.3 高级选项（VAD）──────────────────
// VAD 默认值与窗口归一化逻辑在 ./voice-vad.js（纯模块，voice-vad-windows.test.mjs 覆盖）

async function initAdvancedOptions(config) {
    // 流式识别（伪流式：VAD 切句 + 累积预览）——仅本地模式生效
    const streamingCheckbox = document.getElementById("voice-streaming");
    if (streamingCheckbox) {
        streamingCheckbox.checked = config.streaming_mode === "pseudo";
        streamingCheckbox.addEventListener("change", () => {
            config.streaming_mode = streamingCheckbox.checked ? "pseudo" : "off";
            saveSttConfig(config, "global");
            updateModeVisibility();
        });
    }

    // VAD 切句参数
    let recognitionControls = null;
    initVadConfig(config, () => recognitionControls?.syncFromVad());
    // Preview / Draft 识别协调参数（仅本地伪流式模式生效）
    recognitionControls = initRecognitionConfig(config);
}

function initVadConfig(config, onVADChanged) {
    // 确保 vad 对象存在（旧配置可能没有）
    if (!config.local_engine.vad) {
        config.local_engine.vad = {...VAD_DEFAULTS};
    }
    const vad = config.local_engine.vad;
    // 旧配置可能缺窗口字段（0.23.7 前只有 3 个参数）；非法旧值安全归一化，
    // 与后端 VadConfig::sanitize 规则一致
    ensureVadWindowFields(vad);

    const controls = [
        // 0.23.13：silence_threshold 滑杆已移除——底噪自适应主导 on/off 阈值，
        // 字段仅作旧配置兼容保留（Rust 侧默认 0.001）。
        {
            input: document.getElementById("voice-vad-min-silence-ms"),
            val: document.getElementById("voice-vad-min-silence-ms-val"),
            key: "min_silence_ms",
            format: (v) => `${v}ms`,
            validate: (v) => !isNaN(v) && v >= 100 && v <= 1000,
            parse: (raw) => parseInt(raw, 10),
            ariaKey: "voice.local.vad.min_silence_ms.label",
        },
        {
            input: document.getElementById("voice-vad-min-sentence-ms"),
            val: document.getElementById("voice-vad-min-sentence-ms-val"),
            key: "min_sentence_ms",
            format: (v) => `${v}ms`,
            validate: (v) => !isNaN(v) && v >= 200 && v <= 2000,
            parse: (raw) => parseInt(raw, 10),
            ariaKey: "voice.local.vad.min_sentence_ms.label",
        },
        {
            input: document.getElementById("voice-vad-soft-window-s"),
            val: document.getElementById("voice-vad-soft-window-s-val"),
            key: "soft_window_s",
            format: (v) => `${v}s`,
            validate: (v) => !isNaN(v) && v >= VAD_WINDOW_RANGE.soft_window_s.min,
            parse: (raw) => parseInt(raw, 10),
            ariaKey: "voice.local.vad.soft_window_s.label",
            isWindow: true,
        },
        {
            input: document.getElementById("voice-vad-hard-window-s"),
            val: document.getElementById("voice-vad-hard-window-s-val"),
            key: "hard_window_s",
            format: (v) => `${v}s`,
            validate: (v) => !isNaN(v) && v >= VAD_WINDOW_RANGE.hard_window_s.min,
            parse: (raw) => parseInt(raw, 10),
            ariaKey: "voice.local.vad.hard_window_s.label",
            isWindow: true,
        },
        {
            input: document.getElementById("voice-vad-max-uncommitted-s"),
            val: document.getElementById("voice-vad-max-uncommitted-s-val"),
            key: "max_uncommitted_s",
            format: (v) => `${v}s`,
            validate: (v) => !isNaN(v) && v >= VAD_WINDOW_RANGE.max_uncommitted_s.min,
            parse: (raw) => parseInt(raw, 10),
            ariaKey: "voice.local.vad.max_uncommitted_s.label",
            isWindow: true,
        },
    ];

    // 更新滑动条填充进度（CSS 变量 --fill-pct 驱动 linear-gradient）
    function updateSliderFill(slider) {
        if (!slider) return;
        const min = parseFloat(slider.min);
        const max = parseFloat(slider.max);
        const val = parseFloat(slider.value);
        const pct = max > min ? ((val - min) / (max - min)) * 100 : 0;
        slider.style.setProperty("--fill-pct", pct + "%");
    }

    function syncDisplay() {
        for (const control of controls) {
            if (!control.input) continue;
            control.input.value = String(vad[control.key]);
            if (control.val) control.val.textContent = control.format(vad[control.key]);
            updateSliderFill(control.input);
        }
    }

    // 回显当前值 + 可访问名称（滑块 label 是纯 span，读屏需要显式 aria-label）
    syncDisplay();
    for (const control of controls) {
        if (control.input && control.ariaKey) {
            control.input.setAttribute("aria-label", t(control.ariaKey));
        }
    }

    for (const control of controls) {
        if (!control.input) continue;
        control.input.addEventListener("input", () => {
            const val = control.parse(control.input.value);
            if (control.val && !isNaN(val)) control.val.textContent = control.format(val);
            updateSliderFill(control.input);
        });
        control.input.addEventListener("change", () => {
            const val = control.parse(control.input.value);
            if (!control.validate(val)) return;
            vad[control.key] = val;
            if (control.isWindow) {
                // 窗口组合联动钳制：非法组合安全归一化（soft < hard <= uncommitted），
                // 并同步其它滑块的显示
                normalizeVadWindows(vad);
                syncDisplay();
            }
            onVADChanged?.();
            saveSttConfig(config, "local");
        });
    }

    // 恢复默认
    const resetBtn = document.getElementById("voice-vad-reset-btn");
    if (resetBtn) {
        resetBtn.addEventListener("click", () => {
            for (const control of controls) {
                vad[control.key] = VAD_DEFAULTS[control.key];
            }
            syncDisplay();
            onVADChanged?.();
            saveSttConfig(config, "local");
        });
    }

    // 语言切换时刷新滑块可访问名称
    onLangChange(() => {
        for (const control of controls) {
            if (control.input && control.ariaKey) {
                control.input.setAttribute("aria-label", t(control.ariaKey));
            }
        }
    });
}

function initRecognitionConfig(config) {
    if (!config.local_engine.recognition
        || typeof config.local_engine.recognition !== "object"
        || Array.isArray(config.local_engine.recognition)) {
        config.local_engine.recognition = {...RECOGNITION_DEFAULTS};
    }
    const recognition = config.local_engine.recognition;
    const getMaxUncommittedS = () => Number(config.local_engine.vad?.max_uncommitted_s);
    const currentMaxUncommittedS = () => {
        const value = getMaxUncommittedS();
        return Number.isFinite(value) ? value : undefined;
    };
    ensureRecognitionFields(recognition, currentMaxUncommittedS());

    const controls = [
        {
            input: document.getElementById("voice-recognition-preview-window-ms"),
            val: document.getElementById("voice-recognition-preview-window-ms-val"),
            key: "preview_window_ms",
            format: (v) => `${v}ms`,
            parse: (raw) => parseInt(raw, 10),
            ariaKey: "voice.local.recognition.preview_window_ms.label",
        },
        {
            input: document.getElementById("voice-recognition-preview-refresh-ms"),
            val: document.getElementById("voice-recognition-preview-refresh-ms-val"),
            key: "preview_refresh_ms",
            format: (v) => `${v}ms`,
            parse: (raw) => parseInt(raw, 10),
            ariaKey: "voice.local.recognition.preview_refresh_ms.label",
        },
        {
            input: document.getElementById("voice-recognition-draft-min-s"),
            val: document.getElementById("voice-recognition-draft-min-s-val"),
            key: "draft_min_s",
            format: (v) => `${v}s`,
            parse: (raw) => parseInt(raw, 10),
            ariaKey: "voice.local.recognition.draft_min_s.label",
        },
        {
            input: document.getElementById("voice-recognition-strong-pause-ms"),
            val: document.getElementById("voice-recognition-strong-pause-ms-val"),
            key: "strong_pause_ms",
            format: (v) => `${v}ms`,
            parse: (raw) => parseInt(raw, 10),
            ariaKey: "voice.local.recognition.strong_pause_ms.label",
        },
        {
            input: document.getElementById("voice-recognition-long-pause-ms"),
            val: document.getElementById("voice-recognition-long-pause-ms-val"),
            key: "long_pause_ms",
            format: (v) => `${v}ms`,
            parse: (raw) => parseInt(raw, 10),
            ariaKey: "voice.local.recognition.long_pause_ms.label",
        },
    ];

    function updateSliderFill(slider) {
        if (!slider) return;
        const min = parseFloat(slider.min);
        const max = parseFloat(slider.max);
        const val = parseFloat(slider.value);
        const pct = max > min ? ((val - min) / (max - min)) * 100 : 0;
        slider.style.setProperty("--fill-pct", pct + "%");
    }

    function syncDisplay() {
        for (const control of controls) {
            if (!control.input) continue;
            control.input.value = String(recognition[control.key]);
            if (control.val) control.val.textContent = control.format(recognition[control.key]);
            updateSliderFill(control.input);
        }
    }

    syncDisplay();
    for (const control of controls) {
        if (control.input && control.ariaKey) {
            control.input.setAttribute("aria-label", t(control.ariaKey));
        }
    }

    for (const control of controls) {
        if (!control.input) continue;
        control.input.addEventListener("input", () => {
            const value = control.parse(control.input.value);
            if (!Number.isNaN(value) && control.val) control.val.textContent = control.format(value);
            updateSliderFill(control.input);
        });
        control.input.addEventListener("change", () => {
            const value = control.parse(control.input.value);
            const range = RECOGNITION_RANGE[control.key];
            if (!Number.isFinite(value) || value < range.min || value > range.max) {
                syncDisplay();
                return;
            }
            recognition[control.key] = value;
            normalizeRecognitionConfig(recognition, currentMaxUncommittedS());
            syncDisplay();
            saveSttConfig(config, "local");
        });
    }

    const resetBtn = document.getElementById("voice-recognition-reset-btn");
    if (resetBtn) {
        resetBtn.addEventListener("click", () => {
            Object.assign(recognition, RECOGNITION_DEFAULTS);
            normalizeRecognitionConfig(recognition, currentMaxUncommittedS());
            syncDisplay();
            saveSttConfig(config, "local");
        });
    }

    onLangChange(() => {
        for (const control of controls) {
            if (control.input && control.ariaKey) {
                control.input.setAttribute("aria-label", t(control.ariaKey));
            }
        }
    });

    return {
        syncFromVad() {
            normalizeRecognitionConfig(recognition, currentMaxUncommittedS());
            syncDisplay();
        },
    };
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
            bar.style.background = "var(--red)";
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
            bar.style.background = "var(--red)";
        } else if (pct > 90) {
            bar.style.background = "var(--warning)";
        } else {
            bar.style.background = "var(--green)";
        }
    });
}

// ── FunASR 本地模型展示（0.22.7：只读展示 + 跳转引擎页管理；0.22.9 Handoff 09）──

/**
 * revision 比较（字符串化 u64，BigInt 数值比较，禁字典序）。
 * @param {string} a
 * @param {string} b
 * @returns {boolean} a > b
 */
function revisionGreaterThan(a, b) {
    try {
        return BigInt(a) > BigInt(b);
    } catch {
        return false;
    }
}

/**
 * 初始化本地模型只读展示（0.22.7 契约收口；0.22.9 升级状态跟随）。
 *
 * 模型选择的**唯一写入口**在引擎页 FunASR 卡片的模型行"使用"按钮。
 * 语音页只读展示当前选中模型名称；跳转引擎页的入口收敛至上方
 * FunASR 本地引擎卡片的"前往本地模型运行时"按钮。
 *
 * 展示逻辑：
 * - 有选中模型 → 显示"FunASR · 模型名"
 * - 选中模型与运行中模型不一致 → 追加"等待重启"标记
 * - 切换事务在途（引擎页 selection 状态机）→ 显示"切换中"
 * - 无已安装模型 → 显示空状态 + 前往引擎页安装 CTA
 *
 * 竞态防护：LOCAL_ENGINE_STATUS 事件按 epoch/revision 防护——同 epoch
 * 只接受更大 revision，旧快照不得覆盖新显示；事件到达后防抖重拉模型列表
 * （切换事务 stop/commit/start 期间会推送多条 status）。
 *
 * 模型能力联动（流式开关可见性）仍在此处消费 DTO 的 `stt_capabilities`。
 */
async function initLocalModelSelect(config) {
    const nameEl = document.getElementById("voice-local-model-name");
    if (!nameEl) return;

    const currentModelId = config.local_stt_selection?.model_id
        || config.local_engine?.funasr_model || "";

    // ── 渲染（以一次模型列表拉取 + 引擎页 selection 快照为输入）────────
    async function refreshDisplay() {
        let models;
        try {
            models = await invoke("list_engine_models", {engineId: "funasr"});
        } catch (e) {
            console.error("list_engine_models failed:", e);
            nameEl.textContent = t("voice.local.model.load_failed");
            return;
        }

        // 切换事务在途：显示"切换中"（selection 真源在引擎页状态机，
        // 语音页只消费不复制规则）
        const entry = getLocalEngineEntry("funasr");
        const selection = entry?.selection;
        if (selection?.phase === "switching") {
            const target = (models || []).find((m) => m.model_id === selection.targetModelId);
            nameEl.textContent = t("voice.local.model.switching", {
                model: target?.display_name || selection.targetModelId,
            });
            hideRestartHint();
            return;
        }

        // 只保留已安装且校验通过的模型
        const USABLE_VERIFICATION = ["verified", "unverified", "unknown"];
        const installed = (models || []).filter(
            (m) => m.install_state === "installed"
                && USABLE_VERIFICATION.includes((m.verification_state || "").toLowerCase())
        );

        if (installed.length === 0) {
            // 无已安装模型 → 空状态 + CTA
            nameEl.textContent = t("voice.local.model.empty");
            showEmptyModelCta();
            return;
        }

        // 找到选中的模型
        const selectedModel = installed.find((m) => m.is_selected)
            || installed.find((m) => m.model_id === currentModelId);

        if (selectedModel) {
            const engineName = "FunASR";
            const modelName = selectedModel.display_name || selectedModel.model_id;
            nameEl.textContent = `${engineName} · ${modelName}`;

            // 检查选中模型与运行中模型是否一致
            const activeModel = installed.find((m) => m.is_active);
            if (activeModel && activeModel.model_id !== selectedModel.model_id) {
                // 选中但运行实例不一致 → 显示"等待重启"提示
                showRestartHint();
            } else {
                hideRestartHint();
            }

            // 根据选中模型的能力更新高级选项可见性
            applyModelCapabilities(selectedModel.stt_capabilities);
        } else {
            // 有已安装模型但无选中 → 提示前往引擎页选择
            nameEl.textContent = t("voice.local.model.not_selected");
        }
    }

    await refreshDisplay();

    // ── 状态跟随：LOCAL_ENGINE_STATUS 驱动只读显示刷新 ─────────────────
    // epoch/revision 防护（spec-frontend §5.1）：同 epoch 只接受更大
    // revision；epoch 变化（服务重启）直接接受。旧快照不得覆盖新显示。
    let lastEpoch = null;
    let lastRevision = null;
    let refreshTimer = null;

    function onFunasrStatus(payload) {
        if (!payload || payload.engine_id !== "funasr") return;
        const epoch = payload.service_epoch;
        const revision = payload.revision;
        if (lastEpoch !== null && epoch === lastEpoch
            && lastRevision !== null && !revisionGreaterThan(revision, lastRevision)) {
            return; // 旧/相同 revision → 丢弃
        }
        lastEpoch = epoch;
        lastRevision = revision;

        // 防抖：切换事务 stop/commit/start 期间推送多条 status，
        // 只需按最终状态重拉一次模型列表
        if (refreshTimer != null) clearTimeout(refreshTimer);
        refreshTimer = setTimeout(() => {
            refreshTimer = null;
            refreshDisplay().catch((e) => console.warn("[voice] refresh model display failed:", e));
        }, 300);
    }

    try {
        await listen(EVENTS.LOCAL_ENGINE_STATUS, (event) => onFunasrStatus(event.payload));
    } catch (e) {
        console.warn("[voice] LOCAL_ENGINE_STATUS listen failed:", e);
    }
}

/**
 * 根据模型能力声明更新高级选项的可见性与可用性（Handoff 02：DTO 驱动 UI）。
 *
 * 前端不再硬编码 model_id → 能力映射，而是直接消费后端 DTO 的 `stt_capabilities`：
 * - `pseudo_streaming` 不支持 → 禁用流式开关，强制关闭
 *
 * `CapabilityFlag` wire shape: `{ supported: true }` 或 `{ supported: false, reason: "..." }`。
 *
 * 0.22.7 契约收口：hotwords/itn 能力声明已删除（GGUF worker 不消费），
 * 前端不再有热词/ITN 控件。
 *
 * @param {object} caps - `SttModelCapabilities` from DTO (may be undefined)
 */
function applyModelCapabilities(caps) {
    if (!caps) return;

    // ── 伪流式 ──
    const streamingCheckbox = document.getElementById("voice-streaming");
    const streamingField = document.getElementById("voice-streaming-field");
    if (streamingCheckbox && streamingField) {
        const supported = caps.pseudo_streaming?.supported === "yes";
        streamingCheckbox.disabled = !supported;
        if (!supported) {
            streamingCheckbox.checked = false;
        }
        // 可选：在不支持时显示原因提示
        const streamingHint = document.getElementById("voice-streaming-hint");
        if (streamingHint) {
            if (!supported && caps.pseudo_streaming?.reason) {
                streamingHint.textContent = t("voice.local.streaming.unsupported_hint");
            } else {
                streamingHint.textContent = "";
            }
        }
    }
}

/**
 * 显示"待重启"提示：用户切换了模型，但当前运行的服务还使用旧模型。
 * 需要前往引擎页重启服务才能生效。
 */
function showRestartHint() {
    let hint = document.getElementById("voice-model-restart-hint");
    if (!hint) {
        hint = document.createElement("div");
        hint.id = "voice-model-restart-hint";
        hint.className = "le-voice-restart-hint";
        const selectRow = document.querySelector(".voice-model-select-row");
        if (selectRow) {
            selectRow.insertAdjacentElement("afterend", hint);
        }
    }
    hint.textContent = t("voice.local.model.restart_hint");
    hint.hidden = false;
}

/** 隐藏"待重启"提示。 */
function hideRestartHint() {
    const hint = document.getElementById("voice-model-restart-hint");
    if (hint) hint.hidden = true;
}

/**
 * 显示"无已安装模型"空状态 CTA。
 * 在模型选择器下方插入一个引导用户前往引擎页安装模型的提示。
 */
function showEmptyModelCta() {
    const selectRow = document.querySelector(".voice-model-select-row");
    if (!selectRow) return;
    // 避免重复插入
    if (document.getElementById("voice-model-empty-cta")) return;

    const cta = document.createElement("div");
    cta.id = "voice-model-empty-cta";
    cta.className = "le-voice-empty-cta";

    const text = document.createElement("span");
    text.textContent = t("voice.local.model.goto_engines_hint");
    cta.appendChild(text);

    const btn = document.createElement("button");
    btn.className = "btn btn-small";
    btn.type = "button";
    btn.textContent = t("voice.local.model.goto_engines_install");
    btn.addEventListener("click", () => {
        // 复用已有的跳转入口逻辑
        const gotoBtn = document.getElementById("voice-goto-engines-btn");
        if (gotoBtn) gotoBtn.click();
    });
    cta.appendChild(btn);

    selectRow.insertAdjacentElement("afterend", cta);
}

// ── FunASR 生命周期管理已迁移至引擎页 local-runtime controller ──
// 以下函数保留为空壳，防止外部引用报错（后端旧兼容 commands 暂不删除）
async function initFunasrEnv(_config) {
    // migrated to engines/local-runtime
}

async function initSpaceManagement() {
    // migrated to engines/local-runtime
}
