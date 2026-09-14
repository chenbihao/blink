import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {buildVadDebugCutRows, buildVadDebugTimeline, parseVadDebugResult, renderVadDebugResult, vadChartY, vadDebugProgressState, vadIntervalSummary} from "./voice-vad-debug.js";

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
assert.deepEqual(timeline.map(entry => entry.kind), ["boundary", "boundary", "rejected"]);
assert.equal(timeline[0].confirmedText.text, "第一段定稿");
assert.deepEqual(timeline[1].previews.map(event => event.text), ["/sil", "第二段预览"]);
assert.equal(timeline[1].confirmedText.text, "第二段定稿");
assert.equal(timeline[2].previews[0].text, "短句预览");
assert.equal(timeline[2].recognitionStartMs, 4000);
const spanning = buildVadDebugTimeline({
    ...timelineResult,
    trace: {...timelineResult.trace, rejected_short_sentences: [{time_ms: 3000, sentence_ms: 370}]},
});
assert.equal(spanning[1].recognitionStartMs, 2000);
assert.equal(spanning[2].recognitionStartMs, 2000,
    "a rejected short sentence must not reset the recognition span of the later cut");
const unmatched = buildVadDebugTimeline({
    ...result,
    boundaries: [],
    commits: [],
    text_events: [{kind: "confirmed", fed_ms: 1400, wall_ms: 1400, text: "独立定稿"}],
});
assert.ok(unmatched.some(entry => entry.kind === "confirmed_text" && entry.confirmedText.text === "独立定稿"),
    "an unpaired confirmed result must remain visible");

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
assert.equal(elements.events.children.length, 4, "start and all three markers share one vertical timeline");
assert.equal(elements.transcript.textContent, "测试");
assert.ok(elements.events.children[1].children[1].children.some(node => node.className.includes("transcript")),
    "the cut and its text share one marker card");
delete globalThis.document;

const source = await readFile(new URL("./voice.js", import.meta.url), "utf8");
assert.match(source, /invoke\("pick_audio_file_for_vad_debug"\)/);
assert.match(source, /invoke\("debug_vad_audio_file", \{audioRef: picked\.audioRef, runId\}\)/);
assert.match(source, /listen\(EVENTS\.STT_VAD_DEBUG_PROGRESS/);
assert.match(source, /event\.payload\?\.runId === runId/);
assert.match(source, /invoke\("transcribe_audio_file", \{audioRef\}\)/);
