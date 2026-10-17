import assert from 'node:assert/strict';
import {test} from 'node:test';
import {TestElement} from '../settings/tabs/weather-settings-test-helpers.mjs';

globalThis.window ??= {};
const feedback = await import('./transient-feedback.js');
const {showActionError} = await import('./action-error.js');

test('真实操作错误进入可见反馈，后续消息替换并重新计算关闭时间', t => {
    t.mock.timers.enable({apis:['setTimeout']});
    const previous = globalThis.document;
    const view = new TestElement('hidden');
    globalThis.document = {getElementById: () => view};
    t.after(() => {feedback.dismiss(); globalThis.document = previous;});
    feedback.init();
    showActionError('save_clipboard_file_as_image', {code:'invalid_state', message:'图片展开后超过 256MiB 转存上限'});
    assert.equal(view.classList.contains('hidden'), false);
    assert.equal(view.role,'alert');
    assert.ok(view.textContent.includes('256MiB'));
    t.mock.timers.tick(3000);
    showActionError('pin_clipboard_image', {code:'invalid_state', message:'已保存，钉图失败'});
    t.mock.timers.tick(1000);
    assert.equal(view.classList.contains('hidden'), false);
    assert.ok(view.textContent.includes('钉图失败'));
    t.mock.timers.tick(2500);
    assert.equal(view.classList.contains('hidden'),true);
});
