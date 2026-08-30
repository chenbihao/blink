import assert from "node:assert/strict";
import {
    appendLog,
    createInitialState,
    getEntry,
    mergeStatus,
    setLogHistory,
} from "./local-engine-state.js";
import {makeStatus, processState} from "./local-engine-fixtures.js";

function operationLog(seq = "1") {
    return {
        engine_id: "funasr",
        instance_id: "",
        operation_id: "install-model-old",
        seq,
        timestamp: "2026-08-30T14:59:45Z",
        level: "info",
        text: "paraformer-q8.gguf 校验通过",
    };
}

function instanceLog(seq = "1") {
    return {
        engine_id: "funasr",
        instance_id: "inst-current",
        operation_id: null,
        seq,
        timestamp: "2026-08-30T15:04:11Z",
        level: "info",
        text: "worker ready",
    };
}

let state = createInitialState();
state = appendLog(state, operationLog());
assert.equal(getEntry(state, "funasr").logs.length, 1);

state = mergeStatus(state, makeStatus({
    revision: "1",
    status: {process: processState.running(49988)},
}));
assert.deepEqual(
    getEntry(state, "funasr").logs,
    [],
    "运行态快照应清除上一轮模型操作日志",
);

state = setLogHistory(state, "funasr", [operationLog("2"), instanceLog()]);
assert.deepEqual(
    getEntry(state, "funasr").logs.map((log) => log.text),
    ["worker ready"],
    "运行态历史查询即使收到操作日志也只保留当前实例日志",
);

console.log("local-engine-running-logs: 2 passed");
