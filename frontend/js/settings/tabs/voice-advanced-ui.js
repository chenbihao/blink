/**
 * 语音高级输入的 DOM 装配（0.23.17）。
 *
 * 把「多条相关滑杆」升级为多手柄时序条（一条 0..max 的时间轴 + 若干可
 * 拖手柄，顶住式约束）+ 策略预设行。纯逻辑（模型/约束/预设匹配）在
 * ./voice-multi-slider.js 与 ./voice-presets.js；本模块只做 DOM 与事件。
 *
 * 结构（0.23.17 调整后）：
 * - 策略预设行：紧跟「流式识别」开关，位于两张参数卡之前——预设覆盖两卡
 *   的全部字段，放在任一张卡内部都会让另一张卡的归属变得含糊
 * - 卡1「切分时钟」：候选最短语音滑杆 + 停顿时序条（静默/强停顿/长静音）
 *   + 切分窗口时序条（软/硬/上限）
 * - 卡2「双层识别」：预览节奏时序条（刷新/冻结节奏/窗口）+ 定稿目标
 *   选区条（最短上下文手柄 + 目标±宽容选区带，可开关）+ 渐进上屏保留窗口
 *
 * 独立滑杆（`input[type=range]`）的填充高亮由 `--fill-pct` 驱动，**必须**
 * 在每次同步时写入——CSS 的兜底值 50% 会让高亮停在轴中点而与手柄脱节
 * （0.23.17 修复）。
 */

import {t} from "../../i18n/index.js";
import {assignLabelRows, createTimespanModel, draftTargetBand} from "./voice-multi-slider.js";
import {VOICE_PRESETS, getPreset, matchPreset} from "./voice-presets.js";
import {
    DRAFT_TARGET_ACTIVE_MIN_S,
    RECOGNITION_DEFAULTS,
    RECOGNITION_RANGE,
    normalizeRecognitionConfig,
} from "./voice-recognition.js";
import {VAD_DEFAULTS, normalizeVadWindows} from "./voice-vad.js";

/** 时序条通用样式类名（CSS 在 settings-voice.css）。 */
const CLASS_HANDLE = "voice-timespan-handle";
const CLASS_LABEL = "voice-timespan-label";

/** 标签分行后的单行高度（px），与 CSS `.voice-timespan-label` 行高一致。 */
const LABEL_ROW_HEIGHT = 15;

/** 同一行内两个标签之间的最小水平间距（px）。 */
const LABEL_MIN_GAP_PX = 10;

/**
 * 挂载一条多手柄时序条（含可选选区带）。
 *
 * @param {HTMLElement} root .voice-timespan 容器（HTML 内静态存在）
 * @param {object} spec
 * @param {ReturnType<typeof createTimespanModel>} spec.model 已 setValues 的模型
 * @param {(key: string, value: number) => string} spec.format 手柄值显示
 * @param {(key: string) => string} spec.labelOf 手柄短名（i18n）
 * @param {(key: string) => string} [spec.hintOf] 手柄悬停说明（i18n，title）
 * @param {(key: string, value: number, committed: boolean) => void} spec.onChange
 * @param {null|{getRange: () => ({start: number, end: number} | null),
 *   onDrag: (phase: "start"|"move"|"end", part: "center"|"left"|"right",
 *            axisValue: number) => void}} spec.band 选区带交互
 * @returns {{sync: (values: object) => void}}
 */
export function mountTimespan(root, spec) {
    if (!root) return {sync: () => {}};
    const {model, format, labelOf, onChange} = spec;
    root.innerHTML = "";
    const track = document.createElement("div");
    track.className = "voice-timespan-track";
    const labels = document.createElement("div");
    labels.className = "voice-timespan-labels";
    const scale = document.createElement("div");
    scale.className = "voice-timespan-scale";
    const scaleMin = document.createElement("span");
    const scaleMax = document.createElement("span");
    scaleMin.textContent = format("_min", 0);
    scaleMax.textContent = format("_max", model.axisMax);
    scale.append(scaleMin, scaleMax);

    let bandEl = null;
    let bandEdgeEls = null;
    if (spec.band) {
        bandEl = document.createElement("div");
        bandEl.className = "voice-timespan-band";
        const left = document.createElement("div");
        left.className = "voice-timespan-band-edge voice-timespan-band-edge-left";
        const right = document.createElement("div");
        right.className = "voice-timespan-band-edge voice-timespan-band-edge-right";
        bandEdgeEls = {left, right};
        track.append(bandEl, left, right);
    }

    const handleEls = new Map();
    const labelEls = new Map();
    for (const handle of model.handles) {
        const el = document.createElement("div");
        el.className = CLASS_HANDLE;
        el.dataset.key = handle.key;
        el.tabIndex = 0;
        el.setAttribute("role", "slider");
        el.setAttribute("aria-valuemin", String(handle.min));
        el.setAttribute("aria-valuemax", String(handle.max));
        el.setAttribute("aria-orientation", "horizontal");
        if (spec.hintOf) {
            el.title = spec.hintOf(handle.key);
        }
        const label = document.createElement("div");
        label.className = CLASS_LABEL;
        label.dataset.key = handle.key;
        const name = document.createElement("span");
        name.className = "voice-timespan-label-name";
        name.textContent = labelOf(handle.key);
        const value = document.createElement("span");
        value.className = "voice-timespan-label-value";
        label.append(name, value);
        handleEls.set(handle.key, el);
        labelEls.set(handle.key, {root: label, value});
        labels.append(label);
        track.append(el);
    }
    root.append(track, labels, scale);

    function syncVisual() {
        const values = model.getValues();
        // 标签像素度量：分行需要实际宽度（手柄靠近时标签会互相压字）
        const labelMetrics = [];
        for (const handle of model.handles) {
            const percent = model.percentOf(handle.key);
            const el = handleEls.get(handle.key);
            el.style.left = `${percent}%`;
            const value = values[handle.key];
            el.setAttribute("aria-valuenow", String(value));
            el.setAttribute("aria-valuetext", format(handle.key, value));
            const label = labelEls.get(handle.key);
            label.root.style.left = `${percent}%`;
            // 标签中心对齐手柄；轴两端贴边防溢出
            if (percent < 8) label.root.style.transform = "translateX(0)";
            else if (percent > 92) label.root.style.transform = "translateX(-100%)";
            else label.root.style.transform = "translateX(-50%)";
            label.value.textContent = format(handle.key, value);
            labelMetrics.push({
                key: handle.key,
                percent,
                width: label.root.offsetWidth || 0,
            });
        }
        // 标签分行：放不下就落下一行，行高由容器撑开
        const {rows, rowCount} = assignLabelRows(labelMetrics, {
            trackWidth: track.clientWidth,
            gapPx: LABEL_MIN_GAP_PX,
        });
        for (const [key, row] of Object.entries(rows)) {
            const label = labelEls.get(key);
            label.root.style.top = `${row * LABEL_ROW_HEIGHT}px`;
            label.root.dataset.row = String(row);
        }
        labels.style.height = `${rowCount * LABEL_ROW_HEIGHT + 2}px`;
        if (spec.band) {
            const range = spec.band.getRange();
            const visible = range && range.end > range.start;
            for (const el of [bandEl, bandEdgeEls.left, bandEdgeEls.right]) {
                el.classList.toggle("hidden", !visible);
            }
            if (visible) {
                const startPct = Math.max(0, (range.start / model.axisMax) * 100);
                const endPct = Math.min(100, (range.end / model.axisMax) * 100);
                bandEl.style.left = `${startPct}%`;
                bandEl.style.width = `${endPct - startPct}%`;
                bandEdgeEls.left.style.left = `${startPct}%`;
                bandEdgeEls.right.style.left = `${endPct}%`;
            }
        }
    }

    function ratioFromEvent(event) {
        const rect = track.getBoundingClientRect();
        if (rect.width <= 0) return 0;
        return (event.clientX - rect.left) / rect.width;
    }

    /** 统一拖拽会话：pointerdown 捕获 → move 回调 → up 提交。 */
    function startDrag(onMove, onEnd) {
        const move = (moveEvent) => onMove(moveEvent);
        const up = (upEvent) => {
            window.removeEventListener("pointermove", move);
            window.removeEventListener("pointerup", up);
            onEnd?.(upEvent);
        };
        window.addEventListener("pointermove", move);
        window.addEventListener("pointerup", up);
    }

    for (const handle of model.handles) {
        const el = handleEls.get(handle.key);
        el.addEventListener("pointerdown", (event) => {
            event.preventDefault();
            el.focus();
            const key = handle.key;
            startDrag((moveEvent) => {
                const value = model.setHandle(key, model.valueAtPosition(ratioFromEvent(moveEvent)));
                syncVisual();
                onChange(key, value, false);
            }, () => {
                onChange(key, model.getValues()[key], true);
            });
        });
        el.addEventListener("keydown", (event) => {
            const key = handle.key;
            const step = handle.step > 0 ? handle.step : 1;
            let handled = true;
            if (event.key === "ArrowLeft" || event.key === "ArrowDown") {
                model.nudge(key, -step);
            } else if (event.key === "ArrowRight" || event.key === "ArrowUp") {
                model.nudge(key, step);
            } else if (event.key === "PageDown") {
                model.nudge(key, -step * 10);
            } else if (event.key === "PageUp") {
                model.nudge(key, step * 10);
            } else if (event.key === "Home") {
                model.setHandle(key, handle.min);
            } else if (event.key === "End") {
                model.setHandle(key, handle.max);
            } else {
                handled = false;
            }
            if (handled) {
                event.preventDefault();
                syncVisual();
                onChange(key, model.getValues()[key], true);
            }
        });
    }

    // 点轨道空白处：抓取最近手柄跳到该位置
    track.addEventListener("pointerdown", (event) => {
        if (event.target.closest(`.${CLASS_HANDLE}`)) return;
        if (spec.band
            && event.target.closest(".voice-timespan-band, .voice-timespan-band-edge")) {
            return;
        }
        const ratio = ratioFromEvent(event);
        const key = model.nearestHandle(ratio);
        if (!key) return;
        const value = model.setHandle(key, model.valueAtPosition(ratio));
        handleEls.get(key)?.focus();
        syncVisual();
        onChange(key, value, true);
    });

    if (spec.band) {
        const bindBand = (element, part) => {
            element.addEventListener("pointerdown", (event) => {
                event.preventDefault();
                spec.band.onDrag("start", part, model.valueAtPosition(ratioFromEvent(event)));
                startDrag((moveEvent) => {
                    spec.band.onDrag("move", part, model.valueAtPosition(ratioFromEvent(moveEvent)));
                }, () => {
                    spec.band.onDrag("end", part, NaN);
                });
            });
        };
        bindBand(bandEl, "center");
        bindBand(bandEdgeEls.left, "left");
        bindBand(bandEdgeEls.right, "right");
    }

    // 轴宽变化会改变标签的像素占用（窗口缩放 / 折叠卡展开），需要重排分行
    window.addEventListener("resize", syncVisual);
    syncVisual();
    // 首帧 offsetWidth 可能为 0（容器刚插入 DOM）：下一帧再量一次
    requestAnimationFrame(syncVisual);
    return {
        sync(values) {
            model.setValues(values);
            syncVisual();
        },
    };
}

/**
 * 初始化高级输入（两张卡 + 预设行 + 恢复默认）。
 *
 * @param {object} config get_stt_config 的完整配置（就地修改 + save 保存）
 * @param {(scope: string) => void} save 保存回调（scope = "local"）
 * @returns {{syncFromVad: () => void}} VAD 变化后的外部同步入口
 */
export function initAdvancedVoiceControls(config, save) {
    const vad = config.local_engine.vad;
    const recognition = config.local_engine.recognition;
    const currentMaxUncommittedS = () => {
        const value = Number(vad.max_uncommitted_s);
        return Number.isFinite(value) ? value : undefined;
    };

    // ── 预设行 ──────────────────────────────────────────────────────
    const presetRow = document.getElementById("voice-preset-row");
    let presetRowRenderedFor = null;
    const renderPresetRow = () => {
        if (!presetRow) return;
        const active = matchPreset(vad, recognition);
        // 命中预设未变化时只更新按压态，不重建 chips
        if (presetRowRenderedFor === active) {
            for (const chip of presetRow.querySelectorAll(".voice-preset-chip")) {
                chip.setAttribute("aria-pressed", String(active === chip.dataset.preset));
            }
            return;
        }
        presetRowRenderedFor = active;
        presetRow.innerHTML = "";
        for (const preset of VOICE_PRESETS) {
            const chip = document.createElement("button");
            chip.type = "button";
            chip.className = "voice-preset-chip";
            chip.dataset.preset = preset.id;
            chip.setAttribute("aria-pressed", String(active === preset.id));
            chip.textContent = t(`voice.local.preset.${preset.id}`);
            chip.addEventListener("click", () => {
                const target = getPreset(preset.id);
                if (!target) return;
                Object.assign(vad, target.vad);
                Object.assign(recognition, target.recognition);
                commitAll();
            });
            presetRow.append(chip);
        }
        if (active === "custom") {
            const note = document.createElement("span");
            note.className = "voice-preset-custom";
            note.textContent = t("voice.local.preset.custom");
            presetRow.append(note);
        }
    };

    // ── 卡1：停顿时序条（静默 / 强停顿 / 长静音，顶住排序）──────────
    // min_silence 是 VAD 字段，strong/long 是 recognition 字段；同一条
    // 停顿轴上呈现（语义分组优先于字段归属，保存时各写各的 key）。
    const pauseModel = createTimespanModel({
        max: 2000,
        handles: [
            {key: "min_silence_ms", min: 100, max: 1000, step: 50, order: 1},
            {key: "strong_pause_ms", min: 500, max: 1500, step: 50, order: 2, gapPrev: 50},
            {key: "long_pause_ms", min: 800, max: 2000, step: 50, order: 3, gapPrev: 50},
        ],
    });
    const pauseTimespan = mountTimespan(document.getElementById("voice-ts-pauses"), {
        model: pauseModel,
        labelOf: (key) => t(
            key === "min_silence_ms"
                ? "voice.local.vad.min_silence_ms.short"
                : `voice.local.recognition.${key}.short`,
        ),
        hintOf: (key) => t(
            key === "min_silence_ms"
                ? "voice.local.vad.min_silence_ms.hint"
                : `voice.local.recognition.${key}.hint`,
        ),
        format: (_key, value) => `${value}ms`,
        onChange: (key, value, committed) => {
            if (!committed) return;
            if (key === "min_silence_ms") {
                vad[key] = value;
            } else {
                recognition[key] = value;
            }
            normalizeRecognitionConfig(recognition, currentMaxUncommittedS());
            commitAll();
        },
    });

    // ── 卡1：切分窗口时序条（软 / 硬 / 上限，顶住 + 后端同构约束）───
    const windowModel = createTimespanModel({
        max: 30,
        handles: [
            // soft < hard（严格小于，gapPrev=1）；hard ≤ cap（允许相等，
            // 默认 12/12 是不动点，gapPrev=0）
            {key: "soft_window_s", min: 3, max: 30, step: 1, order: 1},
            {key: "hard_window_s", min: 5, max: 30, step: 1, order: 2, gapPrev: 1},
            {key: "max_uncommitted_s", min: 5, max: 30, step: 1, order: 3, gapPrev: 0},
        ],
    });
    const windowTimespan = mountTimespan(document.getElementById("voice-ts-windows"), {
        model: windowModel,
        labelOf: (key) => t(`voice.local.vad.${key}.short`),
        hintOf: (key) => t(`voice.local.vad.${key}.hint`),
        format: (_key, value) => `${value}s`,
        onChange: (key, value, committed) => {
            if (!committed) return;
            vad[key] = value;
            normalizeVadWindows(vad);
            normalizeRecognitionConfig(recognition, currentMaxUncommittedS());
            commitAll();
        },
    });

    // ── 独立滑杆通用夹具（0.23.17）───────────────────────────────────
    // 高亮填充由 `--fill-pct` 驱动。修复前该变量从未写入，CSS 兜底 50%
    // 让高亮恒定停在轴中点（min_sentence 轴 = 1100ms），与手柄位置脱节。
    const rangeSyncs = [];
    const bindRangeControl = ({id, get, set, format}) => {
        const input = document.getElementById(id);
        const valueEl = document.getElementById(`${id}-val`);
        if (!input) return;
        const paint = (raw) => {
            const min = Number(input.min);
            const max = Number(input.max);
            const span = max - min;
            const percent = span > 0 ? ((raw - min) / span) * 100 : 0;
            input.style.setProperty("--fill-pct", `${Math.min(100, Math.max(0, percent))}%`);
            if (valueEl) valueEl.textContent = format(raw);
        };
        input.addEventListener("input", () => {
            // 拖拽中即时跟随（不写配置，避免每帧一次保存）
            paint(Number(input.value));
        });
        input.addEventListener("change", () => {
            const value = Number(input.value);
            if (!Number.isFinite(value)) {
                rangeSyncs.forEach((fn) => fn());
                return;
            }
            set(value);
            commitAll();
        });
        rangeSyncs.push(() => {
            const value = get();
            input.value = String(value);
            paint(value);
        });
    };

    // ── 卡1：候选最短语音（独立滑杆，语义为语音长度而非停顿长度）────
    bindRangeControl({
        id: "voice-vad-min-sentence-ms",
        get: () => vad.min_sentence_ms,
        set: (value) => {
            vad.min_sentence_ms = Math.min(2000, Math.max(200, Math.round(value)));
        },
        format: (value) => `${value}ms`,
    });

    // ── 卡2：渐进上屏保留窗口（0.23.17 新增，段数）──────────────────
    // G2 = 定稿后留在浮窗的句数（0 = 定稿即注入前台应用）；
    // Editor = 同上（1 = 下一句定稿时前一句写入正文）。
    bindRangeControl({
        id: "voice-g2-retention",
        get: () => recognition.g2_retention_segments,
        set: (value) => {
            recognition.g2_retention_segments = Math.min(
                RECOGNITION_RANGE.g2_retention_segments.max,
                Math.max(RECOGNITION_RANGE.g2_retention_segments.min, Math.round(value)),
            );
        },
        format: (value) =>
            value === 0
                ? t("voice.local.recognition.retention.immediate")
                : t("voice.local.recognition.retention.segments", {count: value}),
    });
    bindRangeControl({
        id: "voice-editor-retention",
        get: () => recognition.editor_retention_segments,
        set: (value) => {
            recognition.editor_retention_segments = Math.min(
                RECOGNITION_RANGE.editor_retention_segments.max,
                Math.max(RECOGNITION_RANGE.editor_retention_segments.min, Math.round(value)),
            );
        },
        format: (value) =>
            value === 0
                ? t("voice.local.recognition.retention.immediate_editor")
                : t("voice.local.recognition.retention.segments", {count: value}),
    });

    // ── 卡2：预览节奏时序条（刷新 / 冻结节奏 / 窗口）────────────────
    // freeze（order 0）与 refresh/window 无顶住关系——它是独立节奏参数，
    // 唯一硬约束 refresh < window 由 order 1/2 承担。
    const previewModel = createTimespanModel({
        max: 4000,
        handles: [
            {key: "preview_refresh_ms", min: 500, max: 1000, step: 50, order: 1},
            {key: "phrase_freeze_interval_ms", min: 0, max: 3000, step: 100, order: 0},
            {key: "preview_window_ms", min: 2000, max: 4000, step: 100, order: 2, gapPrev: 100},
        ],
    });
    const previewTimespan = mountTimespan(document.getElementById("voice-ts-preview"), {
        model: previewModel,
        labelOf: (key) => t(`voice.local.recognition.${key}.short`),
        hintOf: (key) => t(`voice.local.recognition.${key}.hint`),
        format: (key, value) =>
            key === "phrase_freeze_interval_ms" && value === 0
                ? t("voice.local.recognition.off_short")
                : `${value}ms`,
        onChange: (key, value, committed) => {
            if (!committed) return;
            recognition[key] = value;
            normalizeRecognitionConfig(recognition, currentMaxUncommittedS());
            commitAll();
        },
    });

    // ── 卡2：定稿目标选区条（最短上下文手柄 + 目标±宽容选区带）─────
    // draft_min（order 0）与选区带无顶住关系；带中心=目标、带宽=2×宽容。
    // 目标窗口关闭（draft_target_s=0）时隐藏带与中心手柄，由开关表达状态。
    const draftModel = createTimespanModel({
        max: 30,
        handles: [
            {key: "draft_min_s", min: 3, max: 10, step: 1, order: 0},
            {key: "draft_target_s", min: 4, max: 30, step: 1, order: 0},
        ],
    });
    /** 选区带中心拖拽的抓取偏移（pointer 起点与目标的差，保持相对位置）。 */
    const dragState = {bandGrabOffset: 0};
    /** 目标中心在当前未提交上限下的可达上界（0 = 放不下目标下限）。 */
    const targetCeiling = () => draftTargetBand.maxAchievableTarget(
        currentMaxUncommittedS(),
        recognition.draft_target_tolerance_s,
        {minS: DRAFT_TARGET_ACTIVE_MIN_S, maxS: RECOGNITION_RANGE.draft_target_s.max},
    );
    const draftTimespan = mountTimespan(document.getElementById("voice-ts-draft"), {
        model: draftModel,
        labelOf: (key) => t(`voice.local.recognition.${key}.short`),
        hintOf: (key) => t(`voice.local.recognition.${key}.hint`),
        format: (_key, value) => `${value}s`,
        band: {
            getRange: () => draftTargetBand.range(
                recognition.draft_target_s,
                recognition.draft_target_tolerance_s,
            ),
            onDrag: (phase, part, axisValue) => {
                if (phase === "start") {
                    dragState.bandGrabOffset =
                        part === "center" && Number.isFinite(axisValue)
                            ? axisValue - recognition.draft_target_s
                            : 0;
                    return;
                }
                if (phase === "move" && Number.isFinite(axisValue)) {
                    if (part === "center") {
                        // 0.23.17：中心停在「未提交上限 − 宽容」处。再往右的
                        // 目标永远等不到合格停顿（只会被硬窗口强制切），
                        // 后端 sanitize 也会把它收回来或直接关闭——所以这里
                        // 就停在可达边界，并把原因写在选区条下方的提示里，
                        // 而不是让手柄"拖不动却看不出为什么"。
                        const ceiling = targetCeiling();
                        const upper = ceiling > 0 ? ceiling : DRAFT_TARGET_ACTIVE_MIN_S;
                        recognition.draft_target_s = Math.round(
                            Math.min(upper, Math.max(
                                DRAFT_TARGET_ACTIVE_MIN_S,
                                axisValue - dragState.bandGrabOffset,
                            )),
                        );
                    } else {
                        // 边缘拖宽容：优先保持中心不动，宽容不超过
                        // 「未提交上限 − 目标」（拖右边缘不该把中心带走）。
                        const cap = currentMaxUncommittedS();
                        const raw = draftTargetBand.toleranceFromEdge(
                            axisValue,
                            recognition.draft_target_s,
                        );
                        const limit = Number.isFinite(cap)
                            ? Math.max(1, cap - recognition.draft_target_s)
                            : RECOGNITION_RANGE.draft_target_tolerance_s.max;
                        recognition.draft_target_tolerance_s = Math.round(Math.min(
                            RECOGNITION_RANGE.draft_target_tolerance_s.max,
                            Math.min(limit, Math.max(
                                RECOGNITION_RANGE.draft_target_tolerance_s.min, raw,
                            )),
                        ));
                    }
                    syncTimespans();
                    return;
                }
                if (phase === "end") {
                    normalizeRecognitionConfig(recognition, currentMaxUncommittedS());
                    commitAll();
                }
            },
        },
        onChange: (key, value, committed) => {
            if (!committed) return;
            recognition[key] = value;
            normalizeRecognitionConfig(recognition, currentMaxUncommittedS());
            commitAll();
        },
    });

    // 选区条下方的"实际生效区间 + 为什么到不了更远"提示（0.23.17）。
    const targetHint = document.getElementById("voice-draft-target-effective");
    const renderTargetHint = () => {
        if (!targetHint) return;
        const tolerance = recognition.draft_target_tolerance_s;
        if (!(recognition.draft_target_s > 0)) {
            targetHint.textContent = t("voice.local.recognition.draft_target.off_hint");
            targetHint.classList.remove("is-limited");
            return;
        }
        const band = draftTargetBand.range(recognition.draft_target_s, tolerance);
        const cap = currentMaxUncommittedS();
        const ceiling = targetCeiling();
        let text = t("voice.local.recognition.draft_target.effective", {
            start: band.start,
            end: band.end,
        });
        if (ceiling === 0) {
            text += ` · ${t("voice.local.recognition.draft_target.unreachable", {
                cap: Number.isFinite(cap) ? cap : 30,
            })}`;
            targetHint.classList.add("is-limited");
        } else if (recognition.draft_target_s >= ceiling) {
            text += ` · ${t("voice.local.recognition.draft_target.limited", {
                cap: Number.isFinite(cap) ? cap : 30,
                max: ceiling,
            })}`;
            targetHint.classList.add("is-limited");
        } else {
            targetHint.classList.remove("is-limited");
        }
        targetHint.textContent = text;
    };

    // 目标窗口开关（0=关）
    const targetToggle = document.getElementById("voice-draft-target-toggle");
    const renderTargetToggle = () => {
        if (!targetToggle) return;
        const enabled = recognition.draft_target_s > 0;
        targetToggle.setAttribute("aria-pressed", String(enabled));
        targetToggle.classList.toggle("is-on", enabled);
        targetToggle.textContent = t(
            enabled
                ? "voice.local.recognition.draft_target.toggle_on"
                : "voice.local.recognition.draft_target.toggle_off",
        );
        const container = document.getElementById("voice-ts-draft");
        const handleEl = container?.querySelector(`.${CLASS_HANDLE}[data-key="draft_target_s"]`);
        const labelEl = container?.querySelector(`.${CLASS_LABEL}[data-key="draft_target_s"]`);
        handleEl?.classList.toggle("hidden", !enabled);
        labelEl?.classList.toggle("hidden", !enabled);
    };
    targetToggle?.addEventListener("click", () => {
        if (recognition.draft_target_s > 0) {
            // 关闭：0 是合法的"首个合格停顿即定稿"，预设匹配随之变成自定义
            recognition.draft_target_s = 0;
        } else {
            // 开启：先把宽容收到能被上限容纳的范围（否则中心放不下下限
            // 4s，开关点了也开不出来），再落一个可达的中心。
            const cap = currentMaxUncommittedS();
            if (Number.isFinite(cap)) {
                recognition.draft_target_tolerance_s = Math.min(
                    RECOGNITION_RANGE.draft_target_tolerance_s.max,
                    Math.max(
                        RECOGNITION_RANGE.draft_target_tolerance_s.min,
                        cap - DRAFT_TARGET_ACTIVE_MIN_S,
                    ),
                );
            }
            const ceiling = targetCeiling();
            recognition.draft_target_s = ceiling > 0
                ? Math.min(10, ceiling)
                : DRAFT_TARGET_ACTIVE_MIN_S;
        }
        normalizeRecognitionConfig(recognition, currentMaxUncommittedS());
        commitAll();
    });

    // ── 恢复默认（卡1：切分时钟；卡2：双层识别）────────────────────
    // 卡1 覆盖其呈现的全部参数（含跨字段呈现的 strong/long）；卡2 覆盖
    // 预览节奏与定稿目标。两卡按钮同名但各自负责自己的小节，预设行与
    // 手动调整都会即时反映。
    document.getElementById("voice-vad-reset-btn")?.addEventListener("click", () => {
        Object.assign(vad, VAD_DEFAULTS);
        recognition.strong_pause_ms = RECOGNITION_DEFAULTS.strong_pause_ms;
        recognition.long_pause_ms = RECOGNITION_DEFAULTS.long_pause_ms;
        commitAll();
    });
    document.getElementById("voice-recognition-reset-btn")?.addEventListener("click", () => {
        Object.assign(recognition, RECOGNITION_DEFAULTS);
        commitAll();
    });

    // ── 统一提交：归一化 → 同步 UI → 保存 → 刷新预设行 ─────────────
    function commitAll() {
        normalizeVadWindows(vad);
        normalizeRecognitionConfig(recognition, currentMaxUncommittedS());
        syncAll();
        save("local");
    }

    /** 轻量同步：只刷时序条与开关视觉（拖拽 move 高频路径，不重建预设行）。 */
    function syncTimespans() {
        pauseTimespan.sync({
            min_silence_ms: vad.min_silence_ms,
            strong_pause_ms: recognition.strong_pause_ms,
            long_pause_ms: recognition.long_pause_ms,
        });
        windowTimespan.sync({
            soft_window_s: vad.soft_window_s,
            hard_window_s: vad.hard_window_s,
            max_uncommitted_s: vad.max_uncommitted_s,
        });
        previewTimespan.sync({
            preview_refresh_ms: recognition.preview_refresh_ms,
            phrase_freeze_interval_ms: recognition.phrase_freeze_interval_ms,
            preview_window_ms: recognition.preview_window_ms,
        });
        draftModel.setValues({
            draft_min_s: recognition.draft_min_s,
            draft_target_s: recognition.draft_target_s > 0 ? recognition.draft_target_s : 4,
        });
        draftTimespan.sync(draftModel.getValues());
        renderTargetToggle();
        renderTargetHint();
    }

    function syncAll() {
        syncTimespans();
        // 独立滑杆（含新增的渐进上屏保留窗口）：值 + 高亮填充一起回写
        for (const sync of rangeSyncs) sync();
        renderPresetRow();
    }

    syncAll();
    return {syncFromVad: syncAll};
}
