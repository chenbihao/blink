#!/usr/bin/env node
// 一次构建后按 Cargo package cwd 有界并行运行，保留 doctest 和前端测试。
import {dirname} from 'node:path';
import {fileURLToPath} from 'node:url';
import {performance} from 'node:perf_hooks';
import {mapLimit, runCaptured, requirePassed, testJobs} from './test-support.mjs';

const ROOT = fileURLToPath(new URL('../../', import.meta.url));
const scope = ['--workspace', ...['blink-plugin-ip', 'blink-plugin-translate', 'blink-plugin-weather', 'xtask']
    .flatMap(name => ['--exclude', name])];

async function main() {
    const start = performance.now();
    const jobs = testJobs(3);
    const support = await runCaptured(process.execPath, [fileURLToPath(new URL('./test-support.test.mjs', import.meta.url))], {cwd: ROOT});
    requirePassed(support, 'runner 自测');
    const metadata = await runCaptured('cargo', ['metadata', '--no-deps', '--format-version=1'], {cwd: ROOT});
    requirePassed(metadata, 'Cargo metadata');
    const packages = new Map(JSON.parse(metadata.stdout).packages.map(pkg => [pkg.id, pkg]));
    const build = await runCaptured('cargo', ['test', ...scope, '--no-run', '--message-format=json'], {
        cwd: ROOT, timeoutMs: 600_000, onStderr: chunk => process.stderr.write(chunk),
    });
    requirePassed(build, '测试构建');
    const artifacts = new Map();
    for (const line of build.stdout.trim().split(/\r?\n/)) {
        if (!line) continue;
        const message = JSON.parse(line);
        if (message.reason === 'compiler-message' && message.message.rendered) {
            process.stderr.write(message.message.rendered);
        }
        if (message.reason !== 'compiler-artifact' || !message.profile.test || !message.executable) continue;
        const pkg = packages.get(message.package_id);
        if (!pkg) throw new Error(`找不到测试包: ${message.package_id}`);
        artifacts.set(message.executable, {name: `${pkg.name}/${message.target.name}`, executable: message.executable, cwd: dirname(pkg.manifest_path)});
    }
    if (!artifacts.size) throw new Error('没有发现 Rust 测试产物，拒绝空测试集');
    const unitStart = performance.now();
    const unitRun = mapLimit([...artifacts.values()], jobs, async target => {
        const result = await runCaptured(target.executable, ['--quiet'], {cwd: target.cwd});
        console.log(`${result.passed ? '✓' : '✗'} ${target.name} (${result.elapsed}ms)`);
        if (!result.passed) console.error(result.stdout, result.stderr);
        else console.log(result.stdout.trim().split(/\r?\n/).find(line => line.startsWith('test result:')) ?? result.stdout.trim());
        return result;
    }).then(units => ({units, unitMs: performance.now() - unitStart}));
    // 构建完成后各测试组互相独立；共同等待，任一组失败仍收集其它组结果。
    const [unitResult, docs, frontend] = await Promise.all([
        unitRun,
        runCaptured('cargo', ['test', ...scope, '--doc', '--no-fail-fast'], {
            cwd: ROOT, timeoutMs: 600_000, onStderr: chunk => process.stderr.write(chunk),
        }),
        runCaptured(process.execPath, ['frontend/run-tests.mjs'], {cwd: ROOT, timeoutMs: 600_000}),
    ]);
    const {units, unitMs} = unitResult;
    console.log(docs.stdout);
    if (!docs.passed) console.error(docs.stderr);
    console.log(frontend.stdout);
    if (!frontend.passed) console.error(frontend.stderr);
    console.log(`耗时：构建 ${(build.elapsed / 1000).toFixed(2)}s / Rust 单测 ${(unitMs / 1000).toFixed(2)}s (${jobs} 并行) / doctest ${(docs.elapsed / 1000).toFixed(2)}s / 前端 ${(frontend.elapsed / 1000).toFixed(2)}s / 总计 ${((performance.now() - start) / 1000).toFixed(2)}s（构建后各组并行）`);
    if (units.some(result => !result.passed) || !docs.passed || !frontend.passed) process.exitCode = 1;
}

main().catch(error => { console.error(error); process.exitCode = 1; });
