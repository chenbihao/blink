import assert from "node:assert/strict";
import {test} from "node:test";
import {TestElement} from "./weather-settings-test-helpers.mjs";

globalThis.window ??= {};
const {refreshCredentialGroups} = await import("./plugin-credentials.js");

test("collapsed credential summaries distinguish missing, partial and complete saved settings", () => {
    const card = new TestElement();
    const group = new TestElement("plugin-group");
    group.dataset.group = "Provider";
    const label = new TestElement();
    group.selectors.set(".plugin-group-status", label);
    card.selectors.set(".plugin-group", [group]);
    const secret = new TestElement();
    secret.dataset.configured = "false";
    card.selectors.set('.plugin-field[data-key="secret"]',secret);
    const schema = [{key:"id",group:"Provider"},{key:"secret",group:"Provider",secret:true}];
    refreshCredentialGroups(card,schema,{});
    const empty = label.textContent;
    refreshCredentialGroups(card,schema,{id:"app-id"});
    const partial = label.textContent;
    secret.dataset.configured = "true";
    refreshCredentialGroups(card,schema,{id:"app-id"});
    assert.notEqual(empty,partial);
    assert.notEqual(partial,label.textContent);
    assert.notEqual(empty,label.textContent);
});
