import assert from "node:assert/strict";
import {CONTEXT_WINDOW_STOPS, MAX_OUTPUT_TOKEN_STOPS, formatCapacityStop, nearestStopIndex} from "./capacity-scale.js";

assert.equal(MAX_OUTPUT_TOKEN_STOPS[nearestStopIndex(MAX_OUTPUT_TOKEN_STOPS, 9000)], 8192);
assert.equal(MAX_OUTPUT_TOKEN_STOPS.at(-1), 262144);
assert.equal(CONTEXT_WINDOW_STOPS[nearestStopIndex(CONTEXT_WINDOW_STOPS, 128000)], 131072);
assert.equal(formatCapacityStop(32768), "32K");
assert.equal(formatCapacityStop(1048576), "1M");
assert.equal(formatCapacityStop(2097152), "2M");
console.log("capacity-scale tests passed");
