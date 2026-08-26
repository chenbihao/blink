/**
 * 进程状态渲染纯函数测试（0.22.5 H1）。
 *
 * 测试覆盖：
 * 1. ProcessStateDto 各状态正确渲染（stopped/starting/running/stopping/exited）
 * 2. running 状态带 pid 正确显示
 * 3. 未知 shape fail closed 显示 "unknown"
 * 4. stopped 字符串不抛异常（旧 serde enum shape 不再使用）
 * 5. null/undefined 进程状态 fail closed
 * 6. processClass 各状态正确映射 CSS class
 *
 * 这些纯函数从 local-engine-card.js 提取到 local-engine-process.js，
 * 使其可在 Node.js 测试环境中测试，不依赖 DOM。
 */

import assert from "node:assert/strict";
import {processDisplay, processClass} from "./local-engine-process.js";

let testCount = 0;
let passCount = 0;

function test(name, fn) {
    testCount++;
    try {
        fn();
        passCount++;
        console.log(`  ✓ ${name}`);
    } catch (e) {
        console.error(`  ✗ ${name}`);
        console.error(`    ${e.message}`);
        console.error(`    ${e.stack?.split("\n")[1]?.trim() || ""}`);
        throw e;
    }
}

// ── 1. ProcessStateDto 各状态正确渲染 ──────────────────────────────────────

test("processDisplay: stopped → 'stopped'", () => {
    assert.equal(processDisplay({state: "stopped"}), "stopped");
});

test("processDisplay: starting → 'starting'", () => {
    assert.equal(processDisplay({state: "starting"}), "starting");
});

test("processDisplay: running with pid → 'running (pid=1234)'", () => {
    assert.equal(processDisplay({state: "running", pid: 1234}), "running (pid=1234)");
});

test("processDisplay: running without pid → 'running'", () => {
    assert.equal(processDisplay({state: "running"}), "running");
});

test("processDisplay: stopping → 'stopping'", () => {
    assert.equal(processDisplay({state: "stopping"}), "stopping");
});

test("processDisplay: exited → 'exited'", () => {
    assert.equal(processDisplay({state: "exited", reason: "exit code 1"}), "exited");
});

// ── 2. 未知 shape fail closed 显示 "unknown" ────────────────────────────────

test("processDisplay: unknown state → 'unknown' (fail closed)", () => {
    assert.equal(processDisplay({state: "unknown_state"}), "unknown");
});

test("processDisplay: state is not string → 'unknown'", () => {
    assert.equal(processDisplay({state: 123}), "unknown");
});

test("processDisplay: null → 'unknown'", () => {
    assert.equal(processDisplay(null), "unknown");
});

test("processDisplay: undefined → 'unknown'", () => {
    assert.equal(processDisplay(undefined), "unknown");
});

test("processDisplay: empty object → 'unknown'", () => {
    assert.equal(processDisplay({}), "unknown");
});

test("processDisplay: string (旧 serde shape) → 'unknown' (不抛异常)", () => {
    // 旧 serde enum shape: "stopped" 是字符串
    // 旧代码执行 "stopped" in process 会抛 TypeError（字符串不是对象）
    // 新代码检查 typeof object，字符串返回 "unknown"
    assert.equal(processDisplay("stopped"), "unknown");
    assert.equal(processDisplay("running"), "unknown");
});

// ── 3. processClass 各状态正确映射 CSS class ─────────────────────────────────

test("processClass: stopped → 'status-unknown'", () => {
    assert.equal(processClass({state: "stopped"}), "status-unknown");
});

test("processClass: running → 'status-available'", () => {
    assert.equal(processClass({state: "running", pid: 1234}), "status-available");
});

test("processClass: starting → 'status-warning'", () => {
    assert.equal(processClass({state: "starting"}), "status-warning");
});

test("processClass: stopping → 'status-warning'", () => {
    assert.equal(processClass({state: "stopping"}), "status-warning");
});

test("processClass: exited → 'status-unavailable'", () => {
    assert.equal(processClass({state: "exited", reason: "crashed"}), "status-unavailable");
});

test("processClass: unknown state → 'status-unknown' (fail closed)", () => {
    assert.equal(processClass({state: "whatever"}), "status-unknown");
});

test("processClass: null → 'status-unknown'", () => {
    assert.equal(processClass(null), "status-unknown");
});

test("processClass: string (旧 serde shape) → 'status-unknown' (不抛异常)", () => {
    assert.equal(processClass("stopped"), "status-unknown");
    assert.equal(processClass("running"), "status-unknown");
});

// ── 4. 真实 ProcessStateDto shape 验证 ───────────────────────────────────────

test("真实 ProcessStateDto shape: stopped 不含 pid/reason", () => {
    const process = {state: "stopped"};
    assert.equal(processDisplay(process), "stopped");
    assert.equal(processClass(process), "status-unknown");
    // 验证不存在 pid/reason 字段（serde skip_serializing_if = Option::is_none）
    assert.ok(!("pid" in process));
    assert.ok(!("reason" in process));
});

test("真实 ProcessStateDto shape: running 含 pid", () => {
    const process = {state: "running", pid: 4242};
    assert.equal(processDisplay(process), "running (pid=4242)");
    assert.equal(processClass(process), "status-available");
});

test("真实 ProcessStateDto shape: exited 含 reason", () => {
    const process = {state: "exited", reason: "exit code 1"};
    assert.equal(processDisplay(process), "exited");
    assert.equal(processClass(process), "status-unavailable");
});

// ── 汇总 ──────────────────────────────────────────────────────────────────────

console.log(`\n${passCount}/${testCount} tests passed`);
if (passCount !== testCount) {
    process.exit(1);
}
console.log("local-engine-process tests passed");
