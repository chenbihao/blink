import assert from "node:assert/strict";
import {fetchedContextWindow, formatContextWindowLabel} from "./model-meta.js";

assert.equal(
    fetchedContextWindow([{id: "deepseek-chat", context_window: 65536}], "deepseek-chat"),
    65536,
);
assert.equal(fetchedContextWindow(["legacy-model"], "legacy-model"), null);
assert.equal(fetchedContextWindow([], "manual-model"), null);
assert.equal(formatContextWindowLabel(65536), "66K");
assert.equal(formatContextWindowLabel(128000), "128K");
assert.equal(formatContextWindowLabel(1000000), "1M");

console.log("model-meta tests passed");
