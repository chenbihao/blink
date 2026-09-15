/** WAV 伪流式诊断的只读时间轴。正文只用 textContent 写入。 */
const SVG_NS = "http://www.w3.org/2000/svg";

export function parseVadDebugResult(value) {
    if (!value || !Number.isFinite(value.duration_ms) || value.duration_ms < 0
        || !Number.isFinite(value.min_sentence_ms) || value.min_sentence_ms < 0
        || !Number.isFinite(value.wall_ms) || typeof value.final_text !== "string"
        || !value.trace || !Array.isArray(value.trace.points)
        || !Array.isArray(value.boundaries) || !Array.isArray(value.commits)
        || !Array.isArray(value.text_events)) {
        throw new Error("invalid VAD debug result");
    }
    return value;
}

export function vadChartY(rms) {
    const db = 20 * Math.log10(Math.max(0.00001, Number(rms) || 0));
    return Math.max(8, Math.min(116, 116 - (db + 70) / 50 * 108));
}

function svgElement(name, attributes = {}) {
    const node = document.createElementNS(SVG_NS, name);
    for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, String(value));
    return node;
}

function seconds(ms) {
    return `${(ms / 1000).toFixed(2)}s`;
}

/** 进度只按后端实际喂入的 PCM 计算；末段定稿不伪装成可倒计时阶段。 */
export function vadDebugProgressState(payload, t) {
    if (!payload || !["preparing", "replaying", "finalizing", "done"].includes(payload.phase)) return null;
    const duration = Number.isFinite(payload.durationMs) ? Math.max(0, payload.durationMs) : 0;
    const fed = Number.isFinite(payload.fedMs) ? Math.max(0, Math.min(duration, payload.fedMs)) : 0;
    if (payload.phase === "preparing") {
        return {status: t("voice.local.vad_debug.preparing"), percent: null, time: "",
            detail: t("voice.local.vad_debug.preparing_detail")};
    }
    if (payload.phase === "finalizing") {
        return {status: t("voice.local.vad_debug.finalizing"), percent: 100,
            time: seconds(duration), detail: t("voice.local.vad_debug.finalizing_detail")};
    }
    if (payload.phase === "done") {
        return {status: t("voice.local.vad_debug.done"), percent: 100,
            time: seconds(duration), detail: ""};
    }
    return {status: t("voice.local.vad_debug.running"),
        percent: duration ? Math.round(fed / duration * 100) : 0,
        time: `${seconds(fed)} / ${seconds(duration)}`,
        detail: `${t("voice.local.vad_debug.audio_remaining")} ${seconds(duration - fed)} · ${t("voice.local.vad_debug.finalize_extra")}`};
}

/** 仅按同一个 PCM 截止点配对切句与定稿，避免把相邻句子的延迟误算到一起。 */
export function buildVadDebugCutRows(result) {
    const commitsByAudioMs = new Map();
    for (const commit of result.commits) {
        const queue = commitsByAudioMs.get(commit.audio_ms) || [];
        queue.push(commit);
        commitsByAudioMs.set(commit.audio_ms, queue);
    }
    const rows = result.boundaries.map(boundary => {
        const commit = commitsByAudioMs.get(boundary.audio_ms)?.shift();
        return {
            kind: "boundary",
            audioMs: boundary.audio_ms,
            reason: boundary.reason,
            commitWallMs: commit?.observed_wall_ms,
            latencyMs: commit ? Math.max(0, commit.observed_wall_ms - boundary.audio_ms) : null,
        };
    });
    for (const event of result.trace.rejected_short_sentences || []) {
        rows.push({kind: "rejected", audioMs: event.time_ms, sentenceMs: event.sentence_ms});
    }
    for (const [audioMs, queue] of commitsByAudioMs) {
        for (const commit of queue) {
            rows.push({kind: "commit", audioMs, commitWallMs: commit.observed_wall_ms});
        }
    }
    return rows.sort((a, b) => a.audioMs - b.audioMs);
}

/** 每个切句节点收拢前一段的预览，并用真实落字时间配对定稿正文。 */
export function buildVadDebugTimeline(result) {
    const cutRows = buildVadDebugCutRows(result);
    const confirmed = result.text_events.filter(event => event.kind === "confirmed");
    const matched = new Set();
    let previousMs = 0;
    let recognitionStartMs = 0;
    const entries = cutRows.map(row => {
        const previews = result.text_events.filter(event =>
            event.kind === "preview" && event.fed_ms > previousMs && event.fed_ms <= row.audioMs);
        let confirmedText;
        if (row.commitWallMs != null) {
            let bestIndex = -1;
            let bestDistance = Infinity;
            confirmed.forEach((event, index) => {
                const distance = Math.abs(event.wall_ms - row.commitWallMs);
                if (!matched.has(index) && distance <= 250 && distance < bestDistance) {
                    bestIndex = index;
                    bestDistance = distance;
                }
            });
            if (bestIndex >= 0) {
                confirmedText = confirmed[bestIndex];
                matched.add(bestIndex);
            }
        }
        const entry = {...row, previews, confirmedText, recognitionStartMs};
        previousMs = row.audioMs;
        if (row.kind === "boundary") recognitionStartMs = row.audioMs;
        return entry;
    });
    const tailPreviews = result.text_events.filter(event =>
        event.kind === "preview" && event.fed_ms > previousMs);
    if (tailPreviews.length || !entries.length) {
        entries.push({kind: "tail", audioMs: result.duration_ms, previews: tailPreviews,
            recognitionStartMs, noCuts: !cutRows.length});
    }
    confirmed.forEach((event, index) => {
        if (!matched.has(index)) {
            entries.push({kind: "confirmed_text", audioMs: event.fed_ms, previews: [],
                recognitionStartMs, confirmedText: event});
        }
    });
    return entries.sort((a, b) => a.audioMs - b.audioMs);
}

function addText(parent, tag, className, value) {
    const node = document.createElement(tag);
    node.className = className;
    node.textContent = value;
    parent.append(node);
    return node;
}

function waveStrip(points, startMs, endMs) {
    const strip = document.createElement("div");
    strip.className = "voice-vad-debug-wave";
    const values = points.filter(point => point.time_ms > startMs && point.time_ms <= endMs);
    const step = Math.max(1, Math.ceil(values.length / 36));
    for (let index = 0; index < values.length; index += step) {
        const point = values[index];
        const bar = document.createElement("span");
        const rms = Number.isFinite(point.rms) ? point.rms : 0;
        bar.className = rms < point.off ? "voice-vad-debug-wave-bar quiet" : "voice-vad-debug-wave-bar";
        const width = 6 + (Math.log10(Math.max(0.00001, rms)) + 3) * 12;
        bar.style.width = `${Math.max(3, Math.min(34, width)).toFixed(0)}px`;
        strip.append(bar);
    }
    return strip;
}

export function vadIntervalSummary(trace, startMs, endMs) {
    let longestQuietMs = 0;
    for (const span of trace.quiet_spans || []) {
        const overlap = Math.max(0, Math.min(endMs, span.end_ms) - Math.max(startMs, span.start_ms));
        longestQuietMs = Math.max(longestQuietMs, overlap);
    }
    return {durationMs: Math.max(0, endMs - startMs), longestQuietMs};
}

function appendPreviewHistory(parent, previews, t) {
    if (!previews.length) return;
    const details = document.createElement("details");
    details.className = "voice-vad-debug-preview-history";
    addText(details, "summary", "", `${t("voice.local.vad_debug.preview_history")} (${previews.length})`);
    for (const preview of previews) {
        const row = document.createElement("div");
        row.className = "voice-vad-debug-preview-row";
        addText(row, "time", "voice-vad-debug-preview-time", seconds(preview.fed_ms));
        const content = document.createElement("span");
        if (preview.text.trim() === "/sil") {
            addText(content, "span", "voice-vad-debug-preview-silence", `${t("voice.local.vad_debug.silence_preview")} · `);
        }
        addText(content, "span", "voice-vad-debug-preview-text", preview.text);
        row.append(content);
        details.append(row);
    }
    parent.append(details);
}

function appendTimelineEntry(container, entry, startMs, result, t) {
    const item = document.createElement("article");
    const isForced = entry.reason === "hard_window" || entry.reason === "uncommitted_cap";
    const variant = entry.kind === "rejected" ? "rejected" : isForced ? "forced" : entry.kind;
    item.className = `voice-vad-debug-entry voice-vad-debug-entry-${variant}`;
    const rail = document.createElement("div");
    rail.className = "voice-vad-debug-rail";
    if (entry.kind !== "start") rail.append(waveStrip(result.trace.points, startMs, entry.audioMs));
    const node = document.createElement("span");
    node.className = "voice-vad-debug-rail-node";
    rail.append(node);

    const body = document.createElement("div");
    body.className = "voice-vad-debug-entry-body";
    const heading = document.createElement("div");
    heading.className = "voice-vad-debug-entry-heading";
    const range = entry.kind === "start" ? seconds(0) :
        `${seconds(startMs)}–${seconds(entry.audioMs)}`;
    addText(heading, "time", "voice-vad-debug-entry-time", range);
    const reason = entry.kind === "boundary" ? t(`voice.local.vad_debug.${entry.reason}`) :
        entry.kind === "rejected" ? t("voice.local.vad_debug.min_sentence") :
            entry.kind === "commit" ? t("voice.local.vad_debug.confirmed") :
                entry.kind === "confirmed_text" ? t("voice.local.vad_debug.unmatched_confirmed") :
                entry.kind === "start" ? t("voice.local.vad_debug.start") :
                    t(entry.noCuts ? "voice.local.vad_debug.no_cut" : "voice.local.vad_debug.tail");
    addText(heading, "span", "voice-vad-debug-entry-reason", reason);
    body.append(heading);
    if (entry.kind !== "start") {
        const interval = vadIntervalSummary(result.trace, startMs, entry.audioMs);
        const details = [
            `${t("voice.local.vad_debug.interval")} ${seconds(interval.durationMs)}`,
        ];
        if (entry.kind === "rejected") {
            details.push(`${t("voice.local.vad_debug.speech_length")} ${entry.sentenceMs}ms < ${t("voice.local.vad_debug.sentence_minimum")} ${result.min_sentence_ms}ms`);
        } else {
            const vadEvent = result.trace.events?.find(event => event.time_ms === entry.audioMs && event.reason === entry.reason);
            if (vadEvent?.silence_ms > 0 && entry.reason === "natural_silence") {
                details.push(`${t("voice.local.vad_debug.cut_silence")} ${vadEvent.silence_ms}ms`);
            }
        }
        if (interval.longestQuietMs >= 50) {
            details.push(`${t("voice.local.vad_debug.longest_quiet")} ${seconds(interval.longestQuietMs)}`);
        }
        addText(body, "div", "voice-vad-debug-entry-interval", details.join(" · "));
    }
    if (entry.commitWallMs != null) {
        const latency = entry.latencyMs == null ? "" : ` · ${t("voice.local.vad_debug.cut_to_commit")} ${entry.latencyMs}ms`;
        addText(body, "div", "voice-vad-debug-entry-meta",
            `${t("voice.local.vad_debug.committed_at")} ${seconds(entry.commitWallMs)}${latency}`);
    } else if (entry.kind === "confirmed_text") {
        addText(body, "div", "voice-vad-debug-entry-meta",
            `${t("voice.local.vad_debug.committed_at")} ${seconds(entry.confirmedText.wall_ms)}`);
    }
    const primary = entry.confirmedText || entry.previews.at(-1);
    if (entry.kind === "start") {
        item.append(rail, body);
        container.append(item);
        return;
    }
    if (primary) {
        const confirmed = primary.kind === "confirmed";
        const silence = !confirmed && primary.text.trim() === "/sil";
        const transcript = document.createElement("div");
        transcript.className = `voice-vad-debug-entry-transcript ${confirmed ? "confirmed" : silence ? "silence" : "preview"}`;
        const label = t(confirmed ? "voice.local.vad_debug.confirmed" : silence ?
            "voice.local.vad_debug.silence_preview" : "voice.local.vad_debug.latest_preview");
        const coverage = `${t("voice.local.vad_debug.recognition_span")} ${seconds(entry.recognitionStartMs ?? startMs)}–${seconds(entry.audioMs)}`;
        addText(transcript, "div", "voice-vad-debug-entry-text-label",
            confirmed ? `${label} · ${coverage}` :
                `${label} · ${coverage} · ${t("voice.local.vad_debug.elapsed")} ${seconds(primary.wall_ms)}`);
        addText(transcript, "div", "voice-vad-debug-entry-text", primary.text);
        body.append(transcript);
    } else {
        addText(body, "div", "voice-vad-debug-entry-empty", t("voice.local.vad_debug.no_recognition"));
    }
    if (entry.confirmedText || entry.previews.length > 1) appendPreviewHistory(body, entry.previews, t);
    item.append(rail, body);
    container.append(item);
}

export function renderVadDebugResult(raw, elements, t) {
    const result = parseVadDebugResult(raw);
    const {chart, events, transcript, meta} = elements;
    const duration = Math.max(1, result.duration_ms);
    meta.textContent = `${result.engine_id} · ${result.model_id} · ${seconds(result.duration_ms)} · ${t("voice.local.vad_debug.elapsed")} ${seconds(result.wall_ms)}`;
    transcript.textContent = result.final_text;
    chart.replaceChildren();
    const svg = svgElement("svg", {viewBox: "0 0 1000 140", role: "img", "aria-label": t("voice.local.vad_debug.legend")});
    for (const span of result.trace.quiet_spans || []) {
        const x = Math.max(0, Math.min(1000, span.start_ms / duration * 1000));
        const width = Math.max(0, Math.min(1000 - x, (span.end_ms - span.start_ms) / duration * 1000));
        svg.append(svgElement("rect", {x, y: 0, width, height: 122, class: "vad-quiet"}));
    }
    for (const [field, cssClass] of [["rms", "vad-energy"], ["on", "vad-on"], ["off", "vad-off"]]) {
        const values = result.trace.points.filter(point => Number.isFinite(point.time_ms) && Number.isFinite(point[field]));
        if (values.length) {
            svg.append(svgElement("polyline", {
                class: cssClass,
                points: values.map(point => `${Math.min(1000, point.time_ms / duration * 1000).toFixed(1)},${vadChartY(point[field]).toFixed(1)}`).join(" "),
            }));
        }
    }
    for (const event of result.trace.rejected_short_sentences || []) {
        const x = Math.min(1000, Math.max(0, event.time_ms / duration * 1000));
        svg.append(svgElement("line", {x1: x, x2: x, y1: 0, y2: 122, class: "vad-rejected"}));
    }
    for (const boundary of result.boundaries) {
        const x = Math.min(1000, Math.max(0, boundary.audio_ms / duration * 1000));
        const marker = svgElement("line", {x1: x, x2: x, y1: 0, y2: 122, class: "vad-cut"});
        const title = svgElement("title");
        title.textContent = `${seconds(boundary.audio_ms)} ${t(`voice.local.vad_debug.${boundary.reason}`)}`;
        marker.append(title);
        svg.append(marker);
    }
    for (let quarter = 0; quarter <= 4; quarter++) {
        const x = quarter * 250;
        const label = svgElement("text", {
            x,
            y: 137,
            class: "vad-time-label",
            "text-anchor": quarter === 0 ? "start" : quarter === 4 ? "end" : "middle",
        });
        label.textContent = seconds(result.duration_ms * quarter / 4);
        svg.append(label);
    }
    chart.append(svg);

    events.replaceChildren();
    const timeline = buildVadDebugTimeline(result);
    appendTimelineEntry(events, {kind: "start", audioMs: 0, previews: []}, 0, result, t);
    let previousMs = 0;
    for (let index = 0; index < timeline.length; index++) {
        appendTimelineEntry(events, timeline[index], previousMs, result, t);
        previousMs = timeline[index].audioMs;
    }
}

// ── 0.23.9 PreviewDraft 协调器状态映射 ──
//
// 这些函数将后端 CoordinatorTrace DTO 映射为调试界面可显示的状态对象。
// VAD 停顿在 PreviewDraft 模式下只是候选边界，经过 Coordinator 接受后
// 才成为稳定 Draft。

/** 拒绝原因的安全兜底文案 key。 */
const REJECT_FALLBACK_KEY = "voice.local.vad_debug.reject_unknown";

/** 拒绝原因 code → i18n key 映射。 */
const REJECT_REASON_KEYS = {
    below_draft_min: "voice.local.vad_debug.reject_below_draft_min",
    strong_pause_owned_too_short: "voice.local.vad_debug.reject_strong_pause_owned_too_short",
    strong_pause_voiced_too_short: "voice.local.vad_debug.reject_strong_pause_voiced_too_short",
    request_already_reserved: "voice.local.vad_debug.reject_request_already_reserved",
    terminal_takeover: "voice.local.vad_debug.reject_terminal_takeover",
};

/**
 * 将 CoordinatorTrace.profile 映射为安全标签。
 * @param {string|null|undefined} profile
 * @returns {string}
 */
export function mapProfileLabel(profile) {
    if (profile === "PreviewDraft") return "PreviewDraft";
    if (profile === "Legacy") return "Legacy";
    return "Unknown";
}

/**
 * 将拒绝原因 code 映射为 i18n key；未知 code 使用安全兜底。
 * @param {string|null|undefined} reason
 * @returns {string}
 */
export function mapRejectReasonKey(reason) {
    if (typeof reason !== "string" || !reason) return REJECT_FALLBACK_KEY;
    return REJECT_REASON_KEYS[reason] || REJECT_FALLBACK_KEY;
}

/**
 * 将候选 trace 映射为调试状态对象。
 * 候选不能被错误显示为 committed Draft。
 * @param {object|null|undefined} candidate
 * @returns {object|null}
 */
export function mapCandidateStatus(candidate) {
    if (!candidate || typeof candidate !== "object") return null;
    const boundarySample = Number.isFinite(candidate.boundary_sample) ? candidate.boundary_sample : 0;
    const voicedSamples = Number.isFinite(candidate.voiced_samples) ? candidate.voiced_samples : 0;
    const quietSamples = Number.isFinite(candidate.quiet_samples) ? candidate.quiet_samples : 0;
    const reason = typeof candidate.reason === "string" ? candidate.reason : "unknown";
    // 空对象（没有有意义的字段）返回 null
    if (boundarySample === 0 && voicedSamples === 0 && quietSamples === 0 && reason === "unknown") {
        return null;
    }
    return {
        boundarySample,
        quietStartSample: Number.isFinite(candidate.quiet_start_sample) ? candidate.quiet_start_sample : 0,
        reason,
        voicedSamples,
        quietSamples,
        accepted: Boolean(candidate.accepted),
        rejectReasonKey: candidate.accepted ? null : mapRejectReasonKey(candidate.reject_reason),
    };
}

/**
 * 将请求 trace（Draft 或 Preview）映射为安全状态。
 * @param {object|null|undefined} req
 * @param {string} kind
 * @returns {object|null}
 */
export function mapRequestStatus(req, kind) {
    if (!req || typeof req !== "object") return null;
    const requestId = Number.isFinite(req.request_id) ? req.request_id : 0;
    const rangeStart = Number.isFinite(req.audio_range_start) ? req.audio_range_start : 0;
    const rangeEnd = Number.isFinite(req.audio_range_end) ? req.audio_range_end : 0;
    const revision = Number.isFinite(req.revision) ? req.revision : 0;
    const state = typeof req.state === "string" ? req.state : "unknown";
    return {kind, requestId, rangeStart, rangeEnd, revision, state};
}

/**
 * 将 CoordinatorTrace 映射为完整的调试面板状态对象。
 * 空数据、缺字段和异常数值不产生 NaN 或界面崩溃。
 * @param {object|null|undefined} trace
 * @returns {object|null}
 */
export function mapCoordinatorTrace(trace) {
    if (!trace || typeof trace !== "object") return null;
    const safeNum = (value) => (Number.isFinite(value) ? value : 0);
    const safeBool = (value) => Boolean(value);
    const profile = mapProfileLabel(trace.profile);
    // 空对象（没有有意义的字段）返回 null
    const hasProfile = profile !== "Unknown";
    const hasNumeric = ["preview_window_ms", "preview_refresh_ms", "draft_min_s",
        "strong_pause_ms", "sample_rate", "captured_audio_end",
        "draft_committed_audio_end", "draft_reserved_audio_end",
        "backlog_samples", "backlog_limit_samples", "committed_spans",
        "drain_preview_count"].some(key => Number.isFinite(trace[key]) && trace[key] !== 0);
    if (!hasProfile && !hasNumeric && !trace.candidate && !trace.running_draft
        && !trace.pending_draft && !trace.running_preview && !trace.pending_preview
        && !trace.overloaded && !trace.closing) {
        return null;
    }
    return {
        profile,
        previewWindowMs: safeNum(trace.preview_window_ms),
        previewRefreshMs: safeNum(trace.preview_refresh_ms),
        draftMinS: safeNum(trace.draft_min_s),
        strongPauseMs: safeNum(trace.strong_pause_ms),
        sampleRate: safeNum(trace.sample_rate),
        capturedAudioEnd: safeNum(trace.captured_audio_end),
        draftCommittedAudioEnd: safeNum(trace.draft_committed_audio_end),
        draftReservedAudioEnd: safeNum(trace.draft_reserved_audio_end),
        backlogSamples: safeNum(trace.backlog_samples),
        backlogLimitSamples: safeNum(trace.backlog_limit_samples),
        overloaded: safeBool(trace.overloaded),
        closing: safeBool(trace.closing),
        candidate: mapCandidateStatus(trace.candidate),
        runningDraft: mapRequestStatus(trace.running_draft, "draft"),
        pendingDraft: mapRequestStatus(trace.pending_draft, "draft"),
        runningPreview: mapRequestStatus(trace.running_preview, "preview"),
        pendingPreview: mapRequestStatus(trace.pending_preview, "preview"),
        committedSpans: safeNum(trace.committed_spans),
        drainPreviewCount: safeNum(trace.drain_preview_count),
    };
}

// ── 0.23.9: 协调器状态面板渲染 ──

/**
 * 将采样点数转为秒显示。
 * @param {number} samples
 * @param {number} sampleRate
 * @returns {string}
 */
function samplesToSeconds(samples, sampleRate) {
    if (!sampleRate || sampleRate <= 0) return `${samples}`;
    return `${(samples / sampleRate).toFixed(2)}s`;
}

/**
 * 渲染单个请求（Draft 或 Preview）的状态。
 * @param {object|null} req - 映射后的请求状态
 * @param {string} titleLabel - i18n key for section title
 * @param {function} t - i18n translate
 * @returns {HTMLElement}
 */
function renderRequestSection(req, titleLabel, t) {
    const section = document.createElement("div");
    section.className = "voice-coordinator-trace-section";
    const title = document.createElement("div");
    title.className = "voice-coordinator-trace-section-title";
    title.textContent = t(titleLabel);
    section.append(title);
    if (!req) {
        const empty = document.createElement("span");
        empty.className = "voice-coordinator-trace-empty";
        empty.textContent = t("voice.local.vad_debug.coordinator_no_request");
        section.append(empty);
        return section;
    }
    const grid = document.createElement("div");
    grid.className = "voice-coordinator-trace-request";
    const fields = [
        ["coordinator_request_id", `#${req.requestId}`],
        ["coordinator_audio_range", `${samplesToSeconds(req.rangeStart, 0)} – ${samplesToSeconds(req.rangeEnd, 0)}`],
        ["coordinator_revision", `r${req.revision}`],
        ["coordinator_state", req.state],
    ];
    for (const [key, value] of fields) {
        const label = document.createElement("span");
        label.className = "voice-coordinator-trace-request-label";
        label.textContent = t(`voice.local.vad_debug.${key}`);
        const val = document.createElement("span");
        val.className = "voice-coordinator-trace-request-value";
        val.textContent = value;
        grid.append(label, val);
    }
    section.append(grid);
    return section;
}

/**
 * 渲染候选边界状态。
 * @param {object|null} candidate - 映射后的候选状态
 * @param {function} t - i18n translate
 * @returns {HTMLElement}
 */
function renderCandidateSection(candidate, t) {
    const section = document.createElement("div");
    section.className = "voice-coordinator-trace-section";
    const title = document.createElement("div");
    title.className = "voice-coordinator-trace-section-title";
    title.textContent = t("voice.local.vad_debug.coordinator_candidate");
    section.append(title);
    if (!candidate) {
        const empty = document.createElement("span");
        empty.className = "voice-coordinator-trace-empty";
        empty.textContent = t("voice.local.vad_debug.coordinator_no_candidate");
        section.append(empty);
        return section;
    }
    const grid = document.createElement("div");
    grid.className = "voice-coordinator-trace-candidate";
    const fields = [
        ["coordinator_boundary_sample", candidate.boundarySample],
        ["coordinator_quiet_start", candidate.quietStartSample],
        ["coordinator_voiced_samples", candidate.voicedSamples],
        ["coordinator_quiet_samples", candidate.quietSamples],
        ["coordinator_reason", candidate.reason],
    ];
    for (const [key, value] of fields) {
        const label = document.createElement("span");
        label.className = "voice-coordinator-trace-candidate-label";
        label.textContent = t(`voice.local.vad_debug.${key}`);
        const val = document.createElement("span");
        val.className = "voice-coordinator-trace-candidate-value";
        val.textContent = String(value);
        grid.append(label, val);
    }
    // 接受/拒绝状态
    const statusLabel = document.createElement("span");
    statusLabel.className = "voice-coordinator-trace-candidate-label";
    statusLabel.textContent = candidate.accepted
        ? t("voice.local.vad_debug.coordinator_candidate_accepted")
        : t("voice.local.vad_debug.coordinator_candidate_rejected");
    const statusVal = document.createElement("span");
    statusVal.className = `voice-coordinator-trace-candidate-value ${candidate.accepted ? "accepted" : "rejected"}`;
    statusVal.textContent = candidate.accepted ? "✓" : "✗";
    grid.append(statusLabel, statusVal);
    // 拒绝原因
    if (!candidate.accepted && candidate.rejectReasonKey) {
        const reasonLabel = document.createElement("span");
        reasonLabel.className = "voice-coordinator-trace-candidate-label";
        reasonLabel.textContent = t("voice.local.vad_debug.coordinator_reason");
        const reasonVal = document.createElement("span");
        reasonVal.className = "voice-coordinator-trace-candidate-value rejected";
        reasonVal.textContent = t(candidate.rejectReasonKey);
        grid.append(reasonLabel, reasonVal);
    }
    section.append(grid);
    return section;
}

/**
 * 将 CoordinatorTrace 映射后的状态对象渲染到指定容器。
 *
 * @param {object|null} trace - 后端返回的原始 CoordinatorTrace
 * @param {HTMLElement} container - 渲染目标容器
 * @param {function} t - i18n translate
 */
export function renderCoordinatorTrace(trace, container, t) {
    if (!container) return;
    const mapped = mapCoordinatorTrace(trace);
    container.replaceChildren();

    if (!mapped) {
        const empty = document.createElement("span");
        empty.className = "voice-coordinator-trace-empty";
        empty.textContent = t("voice.local.vad_debug.coordinator_no_session");
        container.append(empty);
        return;
    }

    // ── 概览网格 ──
    const grid = document.createElement("div");
    grid.className = "voice-coordinator-trace-grid";
    const sr = mapped.sampleRate || 16000;
    const overviewFields = [
        ["coordinator_profile", mapped.profile],
        ["coordinator_watermark", samplesToSeconds(mapped.draftCommittedAudioEnd, sr)],
        ["coordinator_reserved", samplesToSeconds(mapped.draftReservedAudioEnd, sr)],
        ["coordinator_captured", samplesToSeconds(mapped.capturedAudioEnd, sr)],
        ["coordinator_backlog", `${mapped.backlogSamples} / ${mapped.backlogLimitSamples}`],
        ["coordinator_committed_spans", mapped.committedSpans],
        ["coordinator_drain_preview_count", mapped.drainPreviewCount],
        ["coordinator_preview_window", `${mapped.previewWindowMs}ms`],
        ["coordinator_preview_refresh", `${mapped.previewRefreshMs}ms`],
        ["coordinator_draft_min", `${mapped.draftMinS}s`],
        ["coordinator_strong_pause", `${mapped.strongPauseMs}ms`],
        ["coordinator_sample_rate", `${mapped.sampleRate}Hz`],
    ];
    for (const [key, value] of overviewFields) {
        const cell = document.createElement("div");
        cell.className = "voice-coordinator-trace-cell";
        const label = document.createElement("span");
        label.className = "voice-coordinator-trace-cell-label";
        label.textContent = t(`voice.local.vad_debug.${key}`);
        const val = document.createElement("span");
        val.className = "voice-coordinator-trace-cell-value";
        val.textContent = String(value);
        cell.append(label, val);
        grid.append(cell);
    }
    // 过载与收尾标志
    for (const [key, flag] of [["coordinator_overloaded", mapped.overloaded], ["coordinator_closing", mapped.closing]]) {
        const cell = document.createElement("div");
        cell.className = "voice-coordinator-trace-cell";
        const label = document.createElement("span");
        label.className = "voice-coordinator-trace-cell-label";
        label.textContent = t(`voice.local.vad_debug.${key}`);
        const val = document.createElement("span");
        val.className = `voice-coordinator-trace-cell-value${flag ? " flag-true" : ""}`;
        val.textContent = flag ? "✗" : "—";
        cell.append(label, val);
        grid.append(cell);
    }
    container.append(grid);

    // ── 候选边界 ──
    container.append(renderCandidateSection(mapped.candidate, t));

    // ── 请求槽 ──
    const requestsRow = document.createElement("div");
    requestsRow.style.display = "grid";
    requestsRow.style.gridTemplateColumns = "1fr 1fr";
    requestsRow.style.gap = "var(--space-sm)";
    requestsRow.append(
        renderRequestSection(mapped.runningDraft, "voice.local.vad_debug.coordinator_running_draft", t),
        renderRequestSection(mapped.pendingDraft, "voice.local.vad_debug.coordinator_pending_draft", t),
    );
    const previewRow = document.createElement("div");
    previewRow.style.display = "grid";
    previewRow.style.gridTemplateColumns = "1fr 1fr";
    previewRow.style.gap = "var(--space-sm)";
    previewRow.append(
        renderRequestSection(mapped.runningPreview, "voice.local.vad_debug.coordinator_running_preview", t),
        renderRequestSection(mapped.pendingPreview, "voice.local.vad_debug.coordinator_pending_preview", t),
    );
    container.append(requestsRow, previewRow);
}
