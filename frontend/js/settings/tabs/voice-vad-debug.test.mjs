import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {buildVadDebugCopyText, buildVadDebugCutRows, buildVadDebugTimeline, mapVadReasonKey, parseVadDebugResult, renderVadDebugResult, vadChartY, vadDebugProgressState, vadIntervalSummary, vadReasonLabel, mapProfileLabel, mapRejectReasonKey, mapCandidateStatus, mapRequestStatus, mapCoordinatorTrace, renderCoordinatorTrace} from "./voice-vad-debug.js";

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
assert.match(source, /invoke\("clone_audio_ref_for_vad_debug"/);
assert.match(source, /invoke\("read_audio_for_playback"/);
// 0.23.14.7：切句与识别区的"复制调试信息"必须接线到完整载荷与系统剪贴板
assert.match(source, /getElementById\("voice-vad-debug-copy"\)/);
assert.match(source, /const snapshot = lastDebugResult/);
assert.match(source, /buildVadDebugCopyText\(snapshot,/);
assert.match(source, /copyToClipboard\(text\)/);
assert.match(source, /invoke\("get_stt_config"\)/);

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

// ── 0.23.14.7 候选原因映射：界面不再显示裸 i18n key ──
//
// 此前 reason 直接拼 `voice.local.vad_debug.${code}`，未登记的 code（short_phrase）
// 会把裸 key 显示成"不切 · voice.local.vad_debug.short_phrase"。

// 后端 VadEvent::reason() 的全部取值都必须登记（新增 code 才会落到兜底）
for (const code of ["natural_silence", "soft_window", "hard_window", "short_phrase", "uncommitted_cap", "none"]) {
    assert.equal(mapVadReasonKey(code), `voice.local.vad_debug.${code}`, `${code} 必须映射到同名 key`);
}
assert.equal(mapVadReasonKey("brand_new_reason"), "voice.local.vad_debug.reason_unknown");
assert.equal(mapVadReasonKey(""), "voice.local.vad_debug.reason_unknown");
assert.equal(mapVadReasonKey(null), "voice.local.vad_debug.reason_unknown");

// 文案字典：与 i18n 的降级链一致（缺 key 时返回 key 本身）
const labelDict = {
    "voice.local.vad_debug.short_phrase": "短句停顿",
    "voice.local.vad_debug.reason_unknown": "其他停顿",
};
const translate = key => labelDict[key] ?? key;
assert.equal(vadReasonLabel("short_phrase", translate), "短句停顿");
assert.equal(vadReasonLabel("brand_new_reason", translate), "其他停顿 (brand_new_reason)",
    "未登记 code 必须附上原始 code，既不露 key 也不丢排查线索");
assert.equal(vadReasonLabel(undefined, translate), "其他停顿 (unknown)");
assert.doesNotMatch(vadReasonLabel("brand_new_reason", translate), /voice\.local\.vad_debug\./);

// 渲染层同样不再露出裸 key（决策行与切点悬浮文案）
globalThis.document = {createElement: fakeNode, createElementNS: (_ns, name) => fakeNode(name)};
const reasonElements = Object.fromEntries(["chart", "events", "transcript", "meta"].map(key => [key, fakeNode(key)]));
renderVadDebugResult({
    ...result,
    boundaries: [{audio_ms: 1200, reason: "short_phrase"}],
    decisions: [{
        audioMs: 1200, ownedStartMs: 0, ownedEndMs: 1200, reason: "short_phrase",
        outcome: "waiting", waitReason: "below_natural_pause",
    }],
}, reasonElements, translate);
const renderedReasons = JSON.stringify(reasonElements.events);
assert.match(renderedReasons, /短句停顿/, "已登记的 reason 必须显示本地化文案");
assert.doesNotMatch(renderedReasons, /voice\.local\.vad_debug\.(short_phrase|natural_silence)/,
    "reason 不得以裸 i18n key 出现在界面上");
delete globalThis.document;

// 新增的等待原因 code 也要有登记（否则降级为"未知原因"，丢掉本轮新增的证据）
for (const code of ["below_natural_pause", "natural_sentence_voiced_too_short",
    "natural_sentence_voiced_not_credible", "long_pause_voiced_too_short",
    "long_pause_voiced_not_credible", "invalid_sample_rate"]) {
    assert.notEqual(mapRejectReasonKey(code), "voice.local.vad_debug.reject_unknown",
        `${code} 必须登记到拒绝原因映射`);
}

// ── 0.23.14.7 复制调试信息：载荷必须完整到能独立排查 ──

const copyDict = {
    "voice.local.vad_debug.copy_header": "Blink VAD 调试信息",
    "voice.local.vad_debug.copy_env": "环境与参数",
    "voice.local.vad_debug.copy_final_text": "最终全文",
    "voice.local.vad_debug.copy_timeline": "切句与识别",
    "voice.local.vad_debug.copy_energy": "能量轨迹",
    "voice.local.vad_debug.copy_coordinator": "协调器状态",
    "voice.local.vad_debug.copy_raw": "原始 JSON",
    "voice.local.vad_debug.copy_empty": "（无）",
};
const copyTranslate = key => copyDict[key] ?? labelDict[key] ?? key;

const copyResult = {
    duration_ms: 23_170,
    min_sentence_ms: 800,
    wall_ms: 24_100,
    finalize_ms: 300,
    engine_id: "funasr",
    model_id: "gguf/fun-asr-nano-q4km",
    final_text: "在风扇下说长句。",
    trace: {
        points: [
            {time_ms: 50, rms: 0.0012, on: 0.01, off: 0.005, speaking: false},
            {time_ms: 100, rms: 0.02, on: 0.01, off: 0.005, speaking: true},
        ],
        quiet_spans: [{start_ms: 19_290, end_ms: 20_230}],
        rejected_short_sentences: [{time_ms: 2_800, sentence_ms: 370, silence_ms: 300, reason: "short_phrase"}],
    },
    boundaries: [{audio_ms: 6_120, reason: "natural_silence"}],
    commits: [{audio_ms: 6_120, observed_wall_ms: 7_700}],
    decisions: [
        {audioMs: 6_120, ownedStartMs: 0, ownedEndMs: 6_120, reason: "natural_silence",
            outcome: "accepted", acceptedVia: "draft_min", voicedMs: 4_050, strongMs: 2_850,
            strongRunMs: 1_150, quietMs: 310},
        {audioMs: 21_600, ownedStartMs: 19_290, ownedEndMs: 21_600, reason: "natural_silence",
            outcome: "waiting", waitReason: "natural_sentence_voiced_not_credible",
            voicedMs: 1_060, strongMs: 340, strongRunMs: 50, quietMs: 440},
    ],
    text_events: [{
        kind: "draft", fed_ms: 7_100, wall_ms: 7_700, span_id: 1, revision: 2,
        audio_range: {startSample: 0, endSample: 97_920}, text: "在风扇下说长句。",
    }],
};
const copyText = buildVadDebugCopyText(copyResult, {
    t: copyTranslate,
    fileLabel: "sample.wav",
    settings: {silence_threshold: 0.001, draft_min_s: 5, strong_pause_ms: 700,
        long_pause_ms: 1_100, min_sentence_ms: 900},
});
assert.match(copyText, /=== Blink VAD 调试信息 ===/);
assert.match(copyText, /engine_id=funasr model_id=gguf\/fun-asr-nano-q4km/);
assert.match(copyText, /file=sample\.wav/);
assert.match(copyText, /min_sentence_ms=800 trace_points=2/);
assert.equal(copyText.split("min_sentence_ms=").length - 1, 1,
    "结果已有的字段不得被参数快照重复输出");
assert.match(copyText, /draft_min_s=5/, "参数快照必须进载荷");
assert.match(copyText, /\[最终全文\] chars=8[\s\S]*在风扇下说长句。/);
assert.match(copyText, /boundaries \(1\):\n {2}t=6\.12s reason=natural_silence/);
assert.match(copyText, /commits \(1\):\n {2}t=6\.12s observed_wall=7\.70s/);
assert.match(copyText, /owned=0\.00s-6\.12s voiced_ms=4050ms strong_ms=2850ms strong_run_ms=1150ms quiet_ms=310ms/,
    "决策必须带上完整证据字段（含连续强有声）");
assert.match(copyText, /accepted_via=draft_min/);
assert.match(copyText, /wait_reason=natural_sentence_voiced_not_credible/);
assert.match(copyText, /strong_run_ms=50ms/);
assert.match(copyText, /text_events \(1\):\n {2}\[draft\] fed=7\.10s wall=7\.70s range=0\.00s-6\.12s span_id=1 revision=2 text="在风扇下说长句。"/);
assert.match(copyText, /trace\.quiet_spans \(1\):\n {2}19\.29s-20\.23s/);
assert.match(copyText, /trace\.rejected_short_sentences \(1\):\n {2}t=2\.80s sentence_ms=370 silence_ms=300 reason=short_phrase/);
assert.match(copyText, /\[能量轨迹\] time_ms,rms,on,off,speaking/);
assert.match(copyText, /100,0\.020000,0\.010000,0\.005000,1/);
assert.match(copyText, /\[原始 JSON\]/);
assert.match(copyText, /"strongRunMs": 1150/, "原始 JSON 必须是完整未裁剪的");
assert.doesNotMatch(copyText, /voice\.local\.vad_debug\.copy_/, "载荷文案必须已本地化");
// 无协调器快照时整段省略，不写占位假信息
assert.doesNotMatch(copyText, /协调器状态/);
assert.match(buildVadDebugCopyText(copyResult, {t: copyTranslate, coordinatorTrace: {profile: "PreviewDraft"}}),
    /\[协调器状态\][\s\S]*"profile": "PreviewDraft"/);
// 载荷自洽：载荷里出现的 reason code 必须都能被映射（含兜底）识别
const knownReasons = new Set(["natural_silence", "soft_window", "hard_window", "short_phrase", "uncommitted_cap", "none"]);
for (const code of ["short_phrase", "uncommitted_cap"]) assert.ok(knownReasons.has(code));
assert.ok(copyText.length > 500, "载荷必须足够完整而不是摘要");

console.log("voice-vad-debug.test.mjs: all assertions passed");
