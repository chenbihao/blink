import {test} from 'node:test';
import assert from 'node:assert/strict';
import {capturePluginDisclosure, restorePluginDisclosure} from './plugin-disclosure.js';

function fixture(id, open = false, groups = []) {
    const section = {open};
    const details = groups.map(([name, open]) => ({dataset: {group: name}, open}));
    const card = {dataset: {pluginId: id}, querySelector: () => section, querySelectorAll: () => details};
    return {section, details, card};
}
const container = (...cards) => ({querySelectorAll: () => cards.map(c => c.card)});

test('configuration refresh preserves each plugin and credential group disclosure, including explicit collapse', () => {
    const previous = fixture('builtin.translate', true, [['Provider A', true], ['Provider B', false]]);
    const saved = capturePluginDisclosure(container(previous));
    const fresh = fixture('builtin.translate', false, [['Provider A', false], ['Provider B', true], ['New', false]]);
    const unrelated = fixture('other', false, [['Provider A', false]]);
    restorePluginDisclosure(container(unrelated, fresh), saved);
    assert.equal(fresh.section.open, true);
    assert.deepEqual(fresh.details.map(d => d.open), [true, false, false]);
    assert.equal(unrelated.section.open, false);
    fresh.section.open = false;
    fresh.details[0].open = false;
    const closed = capturePluginDisclosure(container(fresh));
    const next = fixture('builtin.translate', true, [['Provider A', true]]);
    restorePluginDisclosure(container(next), closed);
    assert.equal(next.section.open, false);
    assert.equal(next.details[0].open, false);
});
