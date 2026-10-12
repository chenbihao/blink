import {execFile} from 'node:child_process';
import {performance} from 'node:perf_hooks';

export function testJobs(fallback) {
    const value = process.env.BLINK_TEST_JOBS;
    if (value === undefined) return fallback;
    const jobs = Number(value);
    if (!Number.isSafeInteger(jobs) || jobs < 1 || jobs > 32) {
        throw new Error('BLINK_TEST_JOBS 必须为 1–32 的整数');
    }
    return jobs;
}

/** 保持结果顺序，每项独立执行；worker 返回失败结果时仍收集其它项。 */
export async function mapLimit(items, jobs, worker) {
    const results = new Array(items.length);
    let next = 0;
    await Promise.all(Array.from({length: Math.min(jobs, items.length)}, async () => {
        while (next < items.length) {
            const index = next++;
            results[index] = await worker(items[index], index);
        }
    }));
    return results;
}

export function cleanCargoContext(environment) {
    const clean = {...environment};
    // cargo run 注入父包上下文；ring 等 build script 会追踪它们，导致与原生
    // cargo test 来回切换时重编译。保留 CARGO_HOME/TARGET_DIR、RUSTFLAGS 等构建配置。
    for (const key of Object.keys(clean)) {
        if (/^CARGO_(PKG_|MANIFEST_|CFG_)/.test(key)
            || ['CARGO_BIN_NAME', 'CARGO_CRATE_NAME', 'OUT_DIR'].includes(key)) delete clean[key];
    }
    return clean;
}

export function runCaptured(command, args, {cwd, timeoutMs = 120_000, onStderr} = {}) {
    const start = performance.now();
    return new Promise((resolve) => {
        const child = execFile(command, args, {
            cwd, timeout: timeoutMs, windowsHide: true,
            env: cleanCargoContext(process.env),
            encoding: 'utf8', maxBuffer: 32 * 1024 * 1024,
        }, (error, stdout, stderr) => resolve({
            passed: !error, elapsed: Math.round(performance.now() - start),
            exitCode: error?.code ?? 0, signal: error?.signal,
            timedOut: Boolean(error?.killed && error.code !== 'ERR_CHILD_PROCESS_STDIO_MAXBUFFER'), stdout, stderr,
        }));
        child.stdin.end();
        if (onStderr) child.stderr.on('data', onStderr);
    });
}

export function requirePassed(result, label) {
    if (!result.passed) {
        throw new Error(`${label} 失败 (${result.timedOut ? '超时' : result.signal ?? result.exitCode})\n${result.stderr}\n${result.stdout}`);
    }
}
