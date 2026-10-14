//! 对选择快照执行操作；文本组合与资源批量互相独立，旧任务不清理新选择。
import {refKey, supportsTextOperation} from './clipboard-entry.js';

export function createClipboardBatch({selection, snapshot, active, refresh, reload, remove, api, feedback, dismiss, t}) {
    const busy = new Set();
    const current = task => active() && selection.isCurrent(task);
    const reportError = error => feedback(t('clipboard.operation_failed', {message: error?.message || String(error)}), {type: 'error'});

    async function textOperation(operation, task = snapshot()) {
        if (!current(task) || task.revision !== selection.getSelectionRevision()) return;
        if (!supportsTextOperation(task.refs)) {
            feedback(t('clipboard.choose_typed_action'), {type: 'info'});
            return;
        }
        if (busy.has(operation)) { feedback(t('clipboard.operation_busy'), {type: 'info'}); return; }
        busy.add(operation);
        const gen = selection.beginCopy();
        const valid = () => current(task) && selection.isCopyStillValid(gen);
        feedback(t('clipboard.loading_count', {count: task.refs.length}), {type: 'info', durationMs: 0});
        try {
            const ids = task.refs.map(ref => ref.id);
            const rows = await api.getClipboardTextBatch(ids);
            if (!valid()) return;
            const byId = new Map(rows.map(row => [row.id, row.text]));
            const texts = ids.map(id => byId.get(id));
            if (texts.some(text => typeof text !== 'string')) throw new Error(t('clipboard.item_unavailable'));
            const combined = texts.join('\n');
            if (operation === 'copy') await api.copyToClipboard(combined);
            else if (operation === 'paste') await api.pasteToInput(combined);
            else if (operation === 'edit') await api.openContentEditor({body: combined,
                title: t('clipboard.merged_title'), source: {kind: 'clipboard_item', itemRef: null}});
            else if (operation === 'sticky') {
                const note = await api.createStickyNote(combined);
                if (!valid()) return;
                await api.showStickyWindow(note.id, true);
            } else throw new Error('Unknown clipboard text operation');
            if (operation === 'copy' || operation === 'paste') for (const id of ids) {
                api.recordClipboardHit(id).catch(error => console.warn('[clipboard] record hit failed', error));
            }
            if (!valid()) return;
            const unchanged = task.revision === selection.getSelectionRevision();
            selection.complete(task, task.refs.map(refKey));
            refresh();
            if (operation === 'copy') {
                feedback(t('clipboard.copied_count', {count: texts.length}), {type: 'success', durationMs: 2000});
                reload();
            } else {
                dismiss();
                if (unchanged) api.hideWindow();
            }
        } catch (error) {
            if (valid()) reportError(error);
        } finally { busy.delete(operation); }
    }

    async function resourceOperation(operation, task = snapshot()) {
        if (!task.refs.length || !current(task) || task.revision !== selection.getSelectionRevision()) return;
        if (busy.has(operation)) { feedback(t('clipboard.operation_busy'), {type: 'info'}); return; }
        busy.add(operation);
        feedback(t('clipboard.working_count', {count: task.refs.length}), {type: 'info', durationMs: 0});
        try {
            // 后端持有完整执行生命周期，主窗隐藏/复用只影响 UI 回流，不中断批量操作。
            const outcomes = await api.manageClipboardEntries(operation, task.refs);
            if (!current(task)) return;
            const byKey = new Map(outcomes.map(outcome => [refKey(outcome.entry), outcome]));
            const succeeded = task.refs.filter(ref => byKey.get(refKey(ref))?.success === true);
            const failed = task.refs.length - succeeded.length;
            selection.complete(task, succeeded.map(refKey));
            if (operation === 'delete') remove(succeeded);
            refresh();
            feedback(t(operation === 'delete' ? 'clipboard.delete_result' : 'clipboard.pin_result',
                {success: succeeded.length, failed}), {type: failed ? 'error' : 'success', durationMs: failed ? 5000 : 2000});
        } catch (error) { if (current(task)) reportError(error); }
        finally { busy.delete(operation); }
    }
    return {textOperation, resourceOperation};
}
