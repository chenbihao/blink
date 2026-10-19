import assert from 'node:assert/strict';
import {test} from 'node:test';
import {renderCompletionGhost} from './completion-ghost-view.js';

class Element {
    constructor(tag) { this.tagName = tag; this.children = []; this.textContent = ''; }
    appendChild(child) { this.children.push(child); }
    replaceChildren(...children) { this.children = children; }
}
globalThis.document = {createElement: tag => new Element(tag)};

test('完整引擎触发词仍显示可采纳键帽，清空时释放', () => {
    const typed = new Element('span');
    const suggest = new Element('span');
    assert.equal(renderCompletionGhost('bili', {display:''}, typed, suggest, 'Tab'), true);
    assert.equal(typed.textContent, 'bili');
    assert.equal(suggest.children.length, 1);
    assert.equal(suggest.children[0].tagName, 'kbd');
    assert.equal(suggest.children[0].className, 'kbd kbd-quiet');
    assert.equal(suggest.children[0].textContent, 'Tab');
    assert.equal(renderCompletionGhost('', null, typed, suggest, 'Tab'), false);
    assert.equal(typed.textContent, '');
    assert.equal(suggest.children.length, 0);
});

test('部分触发词同时展示补全和配置的采纳键', () => {
    const typed = new Element('span');
    const suggest = new Element('span');
    renderCompletionGhost('bil', {display:'bili'}, typed, suggest, 'ArrowRight');
    assert.equal(typed.textContent, 'bil');
    assert.equal(suggest.children[0].textContent, ' → bili');
    assert.equal(suggest.children[1].tagName, 'kbd');
    assert.equal(suggest.children[1].textContent, '→');
});
