import assert from "node:assert/strict";
import {createEarlyStreamBuffer} from "./early-stream-buffer.js";

const buffer = createEarlyStreamBuffer();
buffer.begin("conv-a");
assert.equal(buffer.capture({request_id: 7, conversation_id: "conv-b"}), false);
assert.equal(buffer.capture({request_id: 7, conversation_id: "conv-a", chunk: {kind: "error"}}), true);
assert.equal(buffer.capture({request_id: 6, conversation_id: "conv-a", chunk: {kind: "done"}}), true);
assert.deepEqual(buffer.resolve(7).map((event) => event.chunk.kind), ["error"]);
buffer.begin("conv-a");
buffer.capture({request_id: 8, conversation_id: "conv-a"});
buffer.clear();
assert.deepEqual(buffer.resolve(8), []);
console.log("early-stream-buffer tests passed");
