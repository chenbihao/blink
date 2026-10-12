import assert from 'node:assert/strict';
import {mapLimit, runCaptured, requirePassed, cleanCargoContext} from './test-support.mjs';

const clean = cleanCargoContext({
    CARGO_PKG_NAME: 'xtask', CARGO_MANIFEST_DIR: '/parent', CARGO_CFG_TARGET_OS: 'windows', OUT_DIR: '/parent-out',
    CARGO_TARGET_DIR: '/target', CARGO_HOME: '/cargo', RUSTFLAGS: '-C opt-level=1', CARGO_BUILD_JOBS: '2',
});
assert.deepEqual(clean, {CARGO_TARGET_DIR: '/target', CARGO_HOME: '/cargo', RUSTFLAGS: '-C opt-level=1', CARGO_BUILD_JOBS: '2'});

let active = 0;
let maximum = 0;
const results = await mapLimit([30, 5, 10, 1], 2, async (delay, index) => {
    maximum = Math.max(maximum, ++active);
    await new Promise(resolve => setTimeout(resolve, delay));
    active--;
    return index;
});
assert.deepEqual(results, [0, 1, 2, 3]);
assert.equal(maximum, 2);
const outcomes = await mapLimit(['process.exit(7)', 'console.log("ok")'], 2,
    source => runCaptured(process.execPath, ['-e', source]));
assert.equal(outcomes[0].passed, false);
assert.equal(outcomes[0].exitCode, 7);
assert.throws(() => requirePassed(outcomes[0], 'fixture'), /失败/);
assert.equal(outcomes[1].passed, true);
const timeout = await runCaptured(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], {timeoutMs: 100});
assert.equal(timeout.passed, false);
assert.equal(timeout.timedOut, true);
console.log('测试 runner：环境隔离、并行上限、结果顺序、失败透传及超时通过');
