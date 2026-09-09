import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";

import {
    formatAudioTranscriptionIdentity,
    parseAudioTranscriptionCapability,
} from "./voice-file-transcribe.js";

const parsed = parseAudioTranscriptionCapability({
    kind: "items",
    items: [{
        data: {
            text: "result",
            engine_id: "funasr",
            model_id: "model-a",
            duration_ms: 1250,
            no_speech: false,
        },
    }],
});
assert.deepEqual(parsed, {
    text: "result",
    engineId: "funasr",
    modelId: "model-a",
    durationMs: 1250,
    noSpeech: false,
});
assert.equal(formatAudioTranscriptionIdentity(parsed), "funasr · model-a · 1.3s");
assert.throws(() => parseAudioTranscriptionCapability({kind: "done"}));
assert.throws(() => parseAudioTranscriptionCapability({kind: "items", items: [{data: {}}]}));

const voiceSource = await readFile(new URL("./voice.js", import.meta.url), "utf8");
assert.match(voiceSource, /invoke\("pick_audio_file"\)/);
assert.match(voiceSource, /invoke\("transcribe_audio_file", \{audioRef\}\)/);
assert.doesNotMatch(voiceSource, /__TAURI__/);

const html = await readFile(new URL("../../../settings.html", import.meta.url), "utf8");
for (const id of [
    "voice-file-transcribe-btn",
    "voice-file-transcribe-status",
    "voice-file-transcribe-output",
    "voice-file-transcribe-meta",
]) {
    assert.match(html, new RegExp(`id="${id}"`));
}
