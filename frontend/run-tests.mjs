#!/usr/bin/env node
// 0.21.12: 统一前端测试 runner
//
// 递归发现 frontend/js/**/*.test.mjs，路径排序保证执行顺序稳定。
// 每个测试文件独立执行，单个失败不中止后续文件。
// 任意测试失败、超时、被 signal 终止或 runner 自身异常时，最终退出码非零。
// 未发现任何测试文件时失败，避免空测试集假绿。

import {execFileSync} from 'node:child_process';
import {readdirSync, statSync} from 'node:fs';
import {join, relative, sep} from 'node:path';
import {fileURLToPath} from 'node:url';
import {performance} from 'node:perf_hooks';

const ROOT = fileURLToPath(new URL('.', import.meta.url));
const TEST_DIR = join(ROOT, 'js');

const DEFAULT_TIMEOUT_MS = 60_000;

/**
 * 递归收集所有 *.test.mjs 文件，返回排序后的绝对路径数组。
 */
function discoverTestFiles(dir) {
    const results = [];

    function walk(current) {
        let entries;
        try {
            entries = readdirSync(current);
        } catch {
            return;
        }
        for (const entry of entries) {
            const fullPath = join(current, entry);
            let stat;
            try {
                stat = statSync(fullPath);
            } catch {
                continue;
            }
            if (stat.isDirectory()) {
                walk(fullPath);
            } else if (stat.isFile() && entry.endsWith('.test.mjs')) {
                results.push(fullPath);
            }
        }
    }

    walk(dir);
    // 排序保证执行顺序稳定（跨平台路径分隔符归一化后排序）
    results.sort((a, b) => {
        const na = a.split(sep).join('/');
        const nb = b.split(sep).join('/');
        return na < nb ? -1 : na > nb ? 1 : 0;
    });
    return results;
}

/**
 * 独立执行单个测试文件，返回结果对象。
 * 用 execFileSync 在子进程中执行，隔离 failure/crash。
 */
function runTestFile(filePath, timeoutMs) {
    const relativePath = relative(ROOT, filePath).split(sep).join('/');
    const start = performance.now();
    try {
        execFileSync(process.execPath, [filePath], {
            cwd: ROOT,
            timeout: timeoutMs,
            stdio: ['ignore', 'pipe', 'pipe'],
            encoding: 'utf8',
            maxBuffer: 10 * 1024 * 1024,
        });
        const elapsed = Math.round(performance.now() - start);
        return {file: relativePath, passed: true, elapsed};
    } catch (error) {
        const elapsed = Math.round(performance.now() - start);
        let stdout = '';
        let stderr = '';
        if (error.stdout) stdout = error.stdout.toString();
        if (error.stderr) stderr = error.stderr.toString();

        // 超时
        if (error.signal === 'SIGTERM' || error.code === 'ETIMEDOUT') {
            return {
                file: relativePath,
                passed: false,
                elapsed,
                timedOut: true,
                stdout,
                stderr,
            };
        }

        // 被其他 signal 终止
        if (error.signal) {
            return {
                file: relativePath,
                passed: false,
                elapsed,
                signal: error.signal,
                stdout,
                stderr,
            };
        }

        // 非零退出码
        return {
            file: relativePath,
            passed: false,
            elapsed,
            exitCode: error.status ?? 'unknown',
            stdout,
            stderr,
        };
    }
}

function main() {
    const files = discoverTestFiles(TEST_DIR);

    if (files.length === 0) {
        console.error('❌ 未发现任何测试文件 (*.test.mjs)，拒绝假绿。');
        process.exit(1);
    }

    console.log(`🧪 发现 ${files.length} 个测试文件\n`);

    const results = [];
    for (const file of files) {
        const relativePath = relative(ROOT, file).split(sep).join('/');
        process.stdout.write(`▶ ${relativePath} ... `);
        const result = runTestFile(file, DEFAULT_TIMEOUT_MS);
        if (result.passed) {
            console.log(`✓ (${result.elapsed}ms)`);
        } else if (result.timedOut) {
            console.log(`⏱ 超时 (${result.elapsed}ms)`);
        } else if (result.signal) {
            console.log(`💥 signal=${result.signal} (${result.elapsed}ms)`);
        } else {
            console.log(`✗ exit=${result.exitCode} (${result.elapsed}ms)`);
        }
        results.push(result);
    }

    const failures = results.filter((r) => !r.passed);

    console.log('');
    console.log('─'.repeat(60));

    if (failures.length === 0) {
        console.log(`✅ 全部通过：${results.length} 个测试文件`);
        process.exit(0);
    }

    console.error(`❌ ${failures.length}/${results.length} 个测试文件失败：\n`);
    for (const f of failures) {
        let detail = '';
        if (f.timedOut) detail = '超时';
        else if (f.signal) detail = `signal=${f.signal}`;
        else detail = `exit=${f.exitCode}`;
        console.error(`  • ${f.file} (${detail})`);
        // 输出最后几行 stderr/stdout 帮助定位，但避免把完整 stdout 重放一遍
        const tail = (f.stderr || f.stdout || '').trim().split('\n').slice(-8).join('\n');
        if (tail) {
            console.error(`    ${tail.split('\n').join('\n    ')}`);
        }
        console.error('');
    }

    process.exit(1);
}

main();
