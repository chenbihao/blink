//! tauri.js IPC 错误归一化 / 用户文案测试
//!
//! 覆盖：
//! 1. normalizeError：结构化 CommandError 透传 + 旧字符串错误包装
//! 2. commandErrorText：IPC 投影类别前缀剥离 + 兜底文案 + 字符串错误

import {describe, test} from 'node:test';
import assert from 'node:assert';

// tauri.js 模块加载时会给 window.alert/confirm/prompt 打补丁，先 mock window
globalThis.window = globalThis;

const {normalizeError, commandErrorText} = await import('./tauri.js');

describe('normalizeError — IPC 错误归一化', () => {
    test('结构化 CommandError 透传 code/message/retryable', () => {
        const err = {code: 'invalid_state', message: '状态错误: 环境未安装', retryable: false};
        const n = normalizeError(err);
        assert.strictEqual(n.code, 'invalid_state');
        assert.strictEqual(n.message, '状态错误: 环境未安装');
        assert.strictEqual(n.retryable, false);
    });

    test('旧字符串错误包装为 unknown_error', () => {
        const n = normalizeError('boom');
        assert.strictEqual(n.code, 'unknown_error');
        assert.strictEqual(n.message, 'boom');
        assert.strictEqual(n.retryable, false);
    });
});

describe('commandErrorText — CommandError → 用户可读文案', () => {
    test('剥离「状态错误:」前缀，保留可行动说明', () => {
        const err = normalizeError({
            code: 'invalid_state',
            message: '状态错误: OCR ONNX 环境未安装，请在设置页「引擎」中安装 PaddleOCR 环境',
        });
        assert.strictEqual(
            commandErrorText(err, '识别失败'),
            '识别失败：OCR ONNX 环境未安装，请在设置页「引擎」中安装 PaddleOCR 环境',
        );
    });

    test('剥离「数据无效:」等其他类别前缀', () => {
        assert.strictEqual(
            commandErrorText({code: 'invalid_data', message: '数据无效: PNG header 非法'}, '识别失败'),
            '识别失败：PNG header 非法',
        );
        assert.strictEqual(
            commandErrorText({code: 'internal_error', message: '内部错误: 磁盘已满'}, '识别失败'),
            '识别失败：磁盘已满',
        );
    });

    test('无类别前缀的 message 原样拼接', () => {
        // Backend 类错误（start_failed 等）message 投影时无前缀
        assert.strictEqual(
            commandErrorText({code: 'start_failed', message: 'port in use'}, '识别失败'),
            '识别失败：port in use',
        );
    });

    test('无 message / 空对象 → 退回兜底文案（默认「操作失败」）', () => {
        assert.strictEqual(commandErrorText(null), '操作失败');
        assert.strictEqual(commandErrorText({}), '操作失败');
        assert.strictEqual(commandErrorText({code: 'cancelled'}), '操作失败');
    });

    test('自定义兜底文案 + 字符串错误', () => {
        assert.strictEqual(
            commandErrorText('翻译服务超时', '翻译失败'),
            '翻译失败：翻译服务超时',
        );
    });

    test('message 与兜底文案相同时不重复拼接', () => {
        assert.strictEqual(commandErrorText({message: '识别失败'}, '识别失败'), '识别失败');
    });
});
