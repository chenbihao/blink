import test from 'node:test';
import assert from 'node:assert/strict';
import {ExportJobState, exportPath, isTerminal, progressView} from './export-state.js';
const snapshot = (jobId, sequence, stage = 'encoding', sessionId = 'session') => ({jobId, sequence, stage, sessionId, format: 'mp4', framesDone: 5, framesTotal: 10});
test('格式切换同步后缀，保留中文目录和空格', () => {
    assert.equal(exportPath('C:\\中文 空格\\片段.GIF', 'mp4'), 'C:\\中文 空格\\片段.mp4');
    assert.equal(exportPath('C:\\片段.mp4', 'gif'), 'C:\\片段.gif');
    assert.equal(exportPath('', 'mp4'), '');
});
test('仅明确任务可接收事件，迟到序列与旧会话不能污染当前任务', () => {
    const state = new ExportJobState(); state.reset('session');
    assert.equal(state.accept(snapshot('old', 8)), false);
    state.attach('new');
    assert.equal(state.accept(snapshot('new', 2)), true);
    assert.equal(state.accept(snapshot('new', 1)), false);
    assert.equal(state.accept(snapshot('old', 9)), false);
    assert.equal(state.accept(snapshot('new', 3, 'encoding', 'other')), false);
    assert.equal(state.snapshot.sequence, 2);
});
test('查询可恢复丢失的开始/终态，终态不可被迟到进度或取消修改', () => {
    const state = new ExportJobState(); state.reset('session');
    assert.equal(state.accept(snapshot('one', 10, 'done'), true), true);
    assert.equal(state.running, false);
    assert.equal(state.accept(snapshot('one', 11, 'cancelled')), false);
    assert.equal(state.accept(snapshot('two', 1), true), true);
    assert.equal(state.running, true);
    state.reset('other'); assert.equal(state.accept(snapshot('two', 12)), false);
});
test('GIF 和 MP4 采用不同准备与编码进度，验证与提交未被提前报完成', () => {
    assert.equal(progressView({...snapshot('one', 1), stage:'preparing'}).text, '准备视频…');
    assert.equal(progressView({...snapshot('one', 1), stage:'preparing', format:'gif'}).text, '准备调色板…');
    assert.equal(progressView(snapshot('one',1)).value, 42.5);
    assert.equal(progressView({...snapshot('one',1), format:'gif'}).value, 65);
    assert.ok(progressView({...snapshot('one',1), stage:'verifying'}).value < 100);
    assert.ok(progressView({...snapshot('one',1), stage:'committing'}).value < 100);
    assert.equal(isTerminal(snapshot('one',1,'done')), true);
});
