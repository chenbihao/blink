import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {buildVadDebugCutRows, buildVadDebugTimeline, parseVadDebugResult, renderVadDebugResult, vadChartY, vadDebugProgressState, vadIntervalSummary, mapProfileLabel, mapRejectReasonKey, mapCandidateStatus, mapRequestStatus, mapCoordinatorTrace, renderCoordinatorTrace} from "./voice-vad-debug.js";

const result = {
    duration_ms: 1500,
    min_sentence_ms: 800,
    wall_ms: 1800,
    final_text: "测试",
    trace: {points: [{time_ms: 50, rms: 0.01, on: 0.02, off: 0.005}], quiet_spans: [], rejected_short_sentences: []},
    boundaries: [{audio_ms: 1200, reason: "natural_silence"}],
    commits: [{audio_ms: 1200, observed_wall_ms: 1700}],
    text_events: [],
};
assert.equal(parseVadDebugResult(result), result);
assert.throws(() => parseVadDebugResult({...result, trace: null}));
assert.throws(() => parseVadDebugResult({...result, duration_ms: Number.NaN}));
assert.ok(vadChartY(0.01) < vadChartY(0.001), "louder audio should appear higher");
assert.ok(vadChartY(0) <= 116, "silence stays within chart");
assert.deepEqual(vadIntervalSummary({quiet_spans: [{start_ms: 100, end_ms: 500}, {start_ms: 800, end_ms: 950}]}, 300, 900),
    {durationMs: 600, longestQuietMs: 200});
const replayProgress = vadDebugProgressState({phase: "replaying", fedMs: 750, durationMs: 1500}, key => key);
assert.equal(replayProgress.percent, 50);
assert.match(replayProgress.detail, /0\.75s/);
assert.equal(vadDebugProgressState({phase: "preparing"}, key => key).percent, null);
assert.equal(vadDebugProgressState({phase: "finalizing", fedMs: 1500, durationMs: 1500}, key => key).percent, 100);

const rows = buildVadDebugCutRows({
    ...result,
    trace: {...result.trace, rejected_short_sentences: [{time_ms: 1400, sentence_ms: 370}]},
    boundaries: [{audio_ms: 1200, reason: "natural_silence"}, {audio_ms: 800, reason: "soft_window"}],
});
assert.deepEqual(rows.map(row => [row.audioMs, row.kind]), [
    [800, "boundary"], [1200, "boundary"], [1400, "rejected"],
]);
assert.equal(rows[0].commitWallMs, undefined, "a later commit must not attach to the previous cut");
assert.equal(rows[1].latencyMs, 500, "cut-to-commit delay remains visible");
assert.equal(rows[2].sentenceMs, 370);

const timelineResult = {
    ...result,
    duration_ms: 5000,
    boundaries: [{audio_ms: 2000, reason: "natural_silence"}, {audio_ms: 4000, reason: "uncommitted_cap"}],
    commits: [{audio_ms: 2000, observed_wall_ms: 2300}, {audio_ms: 4000, observed_wall_ms: 4600}],
    trace: {...result.trace, rejected_short_sentences: [{time_ms: 4800, sentence_ms: 370}]},
    text_events: [
        {kind: "preview", fed_ms: 1000, wall_ms: 1000, text: "第一段预览"},
        {kind: "confirmed", fed_ms: 2310, wall_ms: 2310, text: "第一段定稿"},
        {kind: "preview", fed_ms: 2500, wall_ms: 2500, text: "/sil"},
        {kind: "preview", fed_ms: 3500, wall_ms: 3500, text: "第二段预览"},
        {kind: "confirmed", fed_ms: 4610, wall_ms: 4610, text: "第二段定稿"},
        {kind: "preview", fed_ms: 4700, wall_ms: 4700, text: "短句预览"},
    ],
};
const timeline = buildVadDebugTimeline(timelineResult);
assert.ok(timeline.every(entry => entry.kind === "moment"));
assert.equal(timeline.find(entry => entry.audioMs === 2000).recognitions[0].text, "第一段定稿");
assert.deepEqual(timeline.filter(entry => [2500, 3500].includes(entry.audioMs))
    .flatMap(entry => entry.recognitions.map(event => event.text)), ["/sil", "第二段预览"]);
assert.equal(timeline.find(entry => entry.audioMs === 4000).recognitions[0].text, "第二段定稿");
assert.equal(timeline.find(entry => entry.audioMs === 4700).recognitions[0].text, "短句预览");
assert.equal(timeline.find(entry => entry.audioMs === 4800).decisions[0].outcome, "waiting");
const spanning = buildVadDebugTimeline({
    ...timelineResult,
    trace: {...timelineResult.trace, rejected_short_sentences: [{time_ms: 3000, sentence_ms: 370}]},
});
assert.equal(spanning.find(entry => entry.audioMs === 3500).recognitions[0].audioStartMs, 2000,
    "a rejected short sentence must not reset the inferred Legacy recognition span");
const unmatched = buildVadDebugTimeline({
    ...result,
    boundaries: [],
    commits: [],
    text_events: [{kind: "confirmed", fed_ms: 1400, wall_ms: 1400, text: "独立定稿"}],
});
assert.ok(unmatched.some(entry => entry.recognitions.some(event => event.text === "独立定稿")),
    "an unpaired confirmed result must remain visible");

const typedTimeline = buildVadDebugTimeline({
    ...result,
    duration_ms: 6000,
    boundaries: [{audio_ms: 5000, reason: "natural_silence"}],
    commits: [{audio_ms: 5000, observed_wall_ms: 5200}],
    decisions: [
        {audioMs: 3000, ownedStartMs: 0, ownedEndMs: 3000, reason: "natural_silence",
            outcome: "waiting", waitReason: "below_draft_min", voicedMs: 2200, quietMs: 700},
        {audioMs: 5000, ownedStartMs: 0, ownedEndMs: 5000, reason: "natural_silence",
            outcome: "accepted", voicedMs: 4000, quietMs: 800},
    ],
    text_events: [
        {kind: "preview", fed_ms: 4500, wall_ms: 4600, request_id: 9, revision: 2,
            audio_range: {startSample: 24000, endSample: 72000}, text: "较长上下文预览"},
        {kind: "draft", fed_ms: 5200, wall_ms: 5250, span_id: 3, revision: 1,
            audio_range: {startSample: 0, endSample: 80000}, text: "稳定草稿"},
    ],
});
const typedPreview = typedTimeline.find(entry => entry.audioMs === 4500).recognitions[0];
assert.equal(typedPreview.audioStartMs, 1500);
assert.equal(typedPreview.audioEndMs, 4500);
assert.equal(typedPreview.exactRange, true);
const typedDraftMoment = typedTimeline.find(entry => entry.audioMs === 5000);
assert.equal(typedDraftMoment.recognitions[0].text, "稳定草稿");
assert.equal(typedDraftMoment.decisions[0].outcome, "accepted");

function fakeNode(name) {
    return {
        name, children: [], style: {}, textContent: "", className: "",
        append(...children) { this.children.push(...children); },
        replaceChildren(...children) { this.children = [...children]; },
        setAttribute() {},
        get firstChild() { return this.children[0]; },
    };
}
globalThis.document = {createElement: fakeNode, createElementNS: (_ns, name) => fakeNode(name)};
const elements = Object.fromEntries(["chart", "events", "transcript", "meta"].map(key => [key, fakeNode(key)]));
renderVadDebugResult(timelineResult, elements, key => key);
assert.equal(elements.events.children.length, timeline.length + 1, "all text and decision moments share one vertical timeline");
assert.equal(elements.transcript.textContent, "测试");
const renderedClasses = JSON.stringify(elements.events);
assert.match(renderedClasses, /voice-vad-debug-transcript-lane/);
assert.match(renderedClasses, /voice-vad-debug-decision-lane/);
assert.match(renderedClasses, /voice-vad-debug-recognition-meta/, "request/rev details live in the hover overlay");
assert.match(renderedClasses, /voice-vad-debug-decision-meta/, "decision facts live in the hover overlay");
assert.doesNotMatch(renderedClasses, /voice-vad-debug-entry-meta/, "always-visible meta lines are gone");
assert.match(renderedClasses, /decision-accepted/, "an anchor whose final result is a cut takes the accepted color");
assert.match(renderedClasses, /decision-waiting/, "kept-buffering anchors stay distinguishable");
assert.doesNotMatch(renderedClasses, /voice-vad-debug-wave/, "mini waveform removed in favor of the continuous rail");
assert.doesNotMatch(renderedClasses, /voice-vad-debug-connector/, "decision lead lines are CSS-drawn on the card");
delete globalThis.document;

const source = await readFile(new URL("./voice.js", import.meta.url), "utf8");
assert.match(source, /invoke\("pick_audio_file_for_vad_debug"\)/);
assert.match(source, /invoke\("debug_vad_audio_file", \{audioRef: picked\.audioRef, runId\}\)/);
assert.match(source, /listen\(EVENTS\.STT_VAD_DEBUG_PROGRESS/);
assert.match(source, /event\.payload\?\.runId === runId/);
assert.match(source, /invoke\("transcribe_audio_file", \{audioRef\}\)/);

// ── 0.23.9 PreviewDraft 协调器状态映射测试 ──

// profile 正确显示
assert.equal(mapProfileLabel("PreviewDraft"), "PreviewDraft");
assert.equal(mapProfileLabel("Legacy"), "Legacy");
assert.equal(mapProfileLabel("Unknown"), "Unknown");
assert.equal(mapProfileLabel(null), "Unknown");
assert.equal(mapProfileLabel(undefined), "Unknown");
assert.equal(mapProfileLabel(123), "Unknown");

// 拒绝原因 code 使用安全兜底
assert.equal(mapRejectReasonKey("below_draft_min"), "voice.local.vad_debug.reject_below_draft_min");
assert.equal(mapRejectReasonKey("strong_pause_owned_too_short"), "voice.local.vad_debug.reject_strong_pause_owned_too_short");
assert.equal(mapRejectReasonKey("strong_pause_voiced_too_short"), "voice.local.vad_debug.reject_strong_pause_voiced_too_short");
assert.equal(mapRejectReasonKey("request_already_reserved"), "voice.local.vad_debug.reject_request_already_reserved");
assert.equal(mapRejectReasonKey("terminal_takeover"), "voice.local.vad_debug.reject_terminal_takeover");
assert.equal(mapRejectReasonKey("unknown_code"), "voice.local.vad_debug.reject_unknown");
assert.equal(mapRejectReasonKey(null), "voice.local.vad_debug.reject_unknown");
assert.equal(mapRejectReasonKey(undefined), "voice.local.vad_debug.reject_unknown");
assert.equal(mapRejectReasonKey(123), "voice.local.vad_debug.reject_unknown");

// 候选不被错误显示为 committed Draft
const acceptedCandidate = mapCandidateStatus({
    boundary_sample: 48000,
    quiet_start_sample: 47000,
    reason: "natural_silence",
    voiced_samples: 32000,
    quiet_samples: 8000,
    accepted: true,
    reject_reason: null,
});
assert.equal(acceptedCandidate.accepted, true);
assert.equal(acceptedCandidate.rejectReasonKey, null);

const rejectedCandidate = mapCandidateStatus({
    boundary_sample: 48000,
    quiet_start_sample: 47000,
    reason: "natural_silence",
    voiced_samples: 32000,
    quiet_samples: 8000,
    accepted: false,
    reject_reason: "below_draft_min",
});
assert.equal(rejectedCandidate.accepted, false);
assert.equal(rejectedCandidate.rejectReasonKey, "voice.local.vad_debug.reject_below_draft_min");

// 候选缺字段安全回退
assert.equal(mapCandidateStatus(null), null);
assert.equal(mapCandidateStatus(undefined), null);
assert.equal(mapCandidateStatus({}), null);
assert.equal(mapCandidateStatus({boundary_sample: "abc"}), null);

// 请求状态映射
const draftReq = mapRequestStatus({
    request_id: 1,
    audio_range_start: 0,
    audio_range_end: 80000,
    revision: 1,
    state: "running",
}, "draft");
assert.equal(draftReq.kind, "draft");
assert.equal(draftReq.requestId, 1);
assert.equal(draftReq.rangeStart, 0);
assert.equal(draftReq.rangeEnd, 80000);
assert.equal(draftReq.revision, 1);
assert.equal(draftReq.state, "running");

assert.equal(mapRequestStatus(null, "draft"), null);
assert.equal(mapRequestStatus(undefined, "draft"), null);
assert.equal(mapRequestStatus({}, "draft").requestId, 0);
assert.equal(mapRequestStatus({}, "draft").state, "unknown");

// 完整 trace 映射——空数据安全
assert.equal(mapCoordinatorTrace(null), null);
assert.equal(mapCoordinatorTrace(undefined), null);
assert.equal(mapCoordinatorTrace({}), null);
const emptyTrace = mapCoordinatorTrace({profile: "PreviewDraft"});
assert.equal(emptyTrace.profile, "PreviewDraft");
assert.equal(emptyTrace.previewWindowMs, 0);
assert.equal(emptyTrace.overloaded, false);
assert.equal(emptyTrace.candidate, null);
assert.equal(emptyTrace.runningDraft, null);
assert.equal(emptyTrace.pendingDraft, null);
assert.equal(emptyTrace.runningPreview, null);
assert.equal(emptyTrace.pendingPreview, null);
assert.equal(emptyTrace.committedSpans, 0);

// 完整 trace 映射——异常数值不产生 NaN
const badNumTrace = mapCoordinatorTrace({
    profile: "PreviewDraft",
    preview_window_ms: Number.NaN,
    preview_refresh_ms: Infinity,
    draft_min_s: "abc",
    strong_pause_ms: undefined,
    sample_rate: null,
    captured_audio_end: {},
    draft_committed_audio_end: Number.NaN,
    draft_reserved_audio_end: Number.NaN,
    backlog_samples: Number.NaN,
    backlog_limit_samples: Number.NaN,
    overloaded: "yes",
    closing: 1,
    committed_spans: [],
    drain_preview_count: {},
});
assert.equal(badNumTrace.previewWindowMs, 0);
assert.equal(badNumTrace.previewRefreshMs, 0);
assert.equal(badNumTrace.draftMinS, 0);
assert.equal(badNumTrace.strongPauseMs, 0);
assert.equal(badNumTrace.sampleRate, 0);
assert.equal(badNumTrace.capturedAudioEnd, 0);
assert.equal(badNumTrace.overloaded, true);
assert.equal(badNumTrace.closing, true);
assert.equal(badNumTrace.committedSpans, 0);
assert.equal(badNumTrace.drainPreviewCount, 0);

// ── renderCoordinatorTrace 渲染测试 ──

// 重新设置 document（renderVadDebugResult 测试后已删除）
globalThis.document = {createElement: fakeNode, createElementNS: (_ns, name) => fakeNode(name)};

// null trace 渲染空状态
const emptyContainer = fakeNode("container");
renderCoordinatorTrace(null, emptyContainer, key => key);
assert.equal(emptyContainer.children.length, 1, "null trace renders empty state");
assert.equal(emptyContainer.children[0].className, "voice-coordinator-trace-empty");

// 完整 trace 渲染概览网格 + 候选 + 请求槽
const traceContainer = fakeNode("container");
const fullTrace = {
    profile: "PreviewDraft",
    preview_window_ms: 3000,
    preview_refresh_ms: 700,
    draft_min_s: 5,
    strong_pause_ms: 700,
    sample_rate: 16000,
    captured_audio_end: 160000,
    draft_committed_audio_end: 80000,
    draft_reserved_audio_end: 120000,
    backlog_samples: 80000,
    backlog_limit_samples: 384000,
    overloaded: false,
    closing: false,
    candidate: {
        boundary_sample: 120000,
        quiet_start_sample: 119000,
        reason: "natural_silence",
        voiced_samples: 32000,
        quiet_samples: 8000,
        accepted: true,
        reject_reason: null,
    },
    running_draft: {
        request_id: 3,
        audio_range_start: 80000,
        audio_range_end: 160000,
        revision: 2,
        state: "running",
    },
    pending_draft: null,
    running_preview: {
        request_id: 7,
        audio_range_start: 120000,
        audio_range_end: 160000,
        revision: 5,
        state: "running",
    },
    pending_preview: null,
    committed_spans: 2,
    drain_preview_count: 1,
};
renderCoordinatorTrace(fullTrace, traceContainer, key => key);
// 概览网格 + 候选 + 2行请求槽 = 4 个子元素
assert.ok(traceContainer.children.length >= 3, "full trace renders grid, candidate, and request sections");
// 第一个子元素是概览网格
const grid = traceContainer.children[0];
assert.equal(grid.className, "voice-coordinator-trace-grid");

console.log("voice-vad-debug.test.mjs: all assertions passed");
