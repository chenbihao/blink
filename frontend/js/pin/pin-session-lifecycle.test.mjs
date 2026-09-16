import {describe, test} from 'node:test';
import assert from 'node:assert';
import {readFileSync} from 'node:fs';

const html = readFileSync(new URL('../../pin.html', import.meta.url), 'utf8');

function bodyOf(name) {
    const start = html.indexOf(name);
    assert.ok(start >= 0, `找不到 ${name}`);
    const nextSection = html.indexOf('\n    // ──', start + name.length);
    return html.slice(start, nextSection >= 0 ? nextSection : html.length);
}

describe('Pin 复用会话隔离', () => {
    test('__blinkResetPin 与 __blinkClearPin 都先失效旧异步任务', () => {
        assert.match(bodyOf('window.__blinkResetPin = function'), /invalidatePinSession\(\)/);
        assert.match(bodyOf('window.__blinkClearPin = function'), /invalidatePinSession\(\)/);
    });

    test('OCR 与翻译在异步提交前校验会话代际', () => {
        assert.match(bodyOf('async function runPinOcr'), /generation !== pinSessionGeneration/);
        assert.match(bodyOf('async function translatePinImageOverlay'), /generation !== pinSessionGeneration/);
    });

    test('按 label 刷新携带当前图片 seq，前端入口也拒绝过期 seq', () => {
        assert.match(html, /screenshotPinRefreshByLabel\(translatedPng, myLabel, false, expectedSeq\)/);
        assert.match(bodyOf('window.__blinkRefreshPinImage = function'), /currentPinImageSeq\(\) !== expectedSeq/);
    });
});
