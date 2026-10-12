// 0.25.28 指南 tab DOM/正文契约测试：
// 1) buildToc 的产物结构必须与 settings-guide.css 的选择器成对——
//    .guide-toc（pointer-events:none 的 sticky 行程轨道）下必须有且仅有
//    .guide-toc-inner 包裹层承载链接。
//    回归背景：buildToc 曾漏掉 inner 包裹层，链接成为轨道直接子元素，
//    pointer-events 继承为 none（点击无效）且样式选择器全部落空（原生蓝链）；
//    浏览器预览因手写了 inner 而未能暴露该漂移，故以此测试锁契约。
// 2) bodyMarkdown 必须从首个编号章节「\n## 1.」起切片——中英两份文档
//    （docs/guide.md / guide.en.md）的文档头（H1+引言+目录/Contents）都在
//    应用内有 tab chrome 与悬浮目录替代，保留会同屏重复；切片语言无关。
import assert from "node:assert/strict";

// Mock window/document：guide.js 的 import 链（shared/tauri.js）触达 window，
// buildToc 触达 document.createElement
globalThis.window = globalThis.window || {};
globalThis.window.__TAURI__ = {
    core: {invoke: async () => ({})},
    event: {listen: async () => ({unlisten: () => {}})},
};

function fakeElement(tag) {
    return {
        tagName: tag,
        className: "",
        children: [],
        attributes: {},
        textContent: "",
        href: "",
        setAttribute(key, value) {
            this.attributes[key] = value;
        },
        appendChild(child) {
            this.children.push(child);
            return child;
        },
    };
}

globalThis.document = {
    createElement: (tag) => fakeElement(tag),
};

const {buildToc, bodyMarkdown} = await import("./guide.js");

const headings = [
    {id: "1-%E4%B8%89%E5%88%86%E9%92%9F%E4%B8%8A%E6%89%8B", textContent: "1. 三分钟上手"},
    {id: "9-%E4%BD%A0%E5%8F%AF%E8%83%BD%E4%B8%8D%E7%9F%A5%E9%81%93%E7%9A%84%E9%80%9F%E8%A7%88", textContent: "9. 你可能不知道的速览"},
];

{
    const nav = buildToc(headings);

    assert.equal(nav.tagName, "nav");
    assert.equal(nav.className, "guide-toc");
    assert.equal(nav.attributes["aria-label"], "指南目录");

    const inners = nav.children.filter((c) => c.className === "guide-toc-inner");
    assert.equal(inners.length, 1, "必须有且仅有一个 .guide-toc-inner 包裹层（样式与 pointer-events:auto 都挂在它下面）");
    assert.equal(nav.children.length, 1, "轨道上除 inner 包裹层外不得有其他直接子元素");

    const links = inners[0].children;
    assert.equal(links.length, headings.length);
    assert.deepEqual(links.map((a) => a.href), headings.map((h) => "#" + h.id), "href 必须直接复用 Cherry 的编码 id");
    assert.deepEqual(links.map((a) => a.textContent), headings.map((h) => h.textContent));
}

{
    // 空标题清单：不应抛异常，产出空 inner
    const nav = buildToc([]);
    assert.equal(nav.className, "guide-toc");
    assert.equal(nav.children[0].className, "guide-toc-inner");
    assert.equal(nav.children[0].children.length, 0);
}

// bodyMarkdown：文档头（H1+引言+目录/Contents）必须被裁掉，正文从「## 1.」起
{
    const zh = [
        "# Blink 丝滑小妙招",
        "",
        "> 引言块",
        "",
        "---",
        "",
        "## 目录",
        "",
        "1. [三分钟上手](#1-三分钟上手)",
        "",
        "---",
        "",
        "## 1. 三分钟上手",
        "",
        "正文",
    ].join("\n");
    assert.ok(bodyMarkdown(zh).startsWith("## 1. 三分钟上手"), "中文版应从「## 1.」章节起渲染");

    const en = [
        "# Blink Tips & Tricks",
        "",
        "> Intro blockquote",
        "",
        "---",
        "",
        "## Contents",
        "",
        "1. [Quick start](#1-quick-start)",
        "",
        "---",
        "",
        "## 1. Quick start",
        "",
        "Body",
    ].join("\n");
    assert.ok(bodyMarkdown(en).startsWith("## 1. Quick start"), "EN doc should render from \"## 1.\"");

    // 「## 10.」不得被误认成「## 1.」（indexOf 语义验证）
    const onlyTen = "## 10. 快捷键总表";
    assert.equal(bodyMarkdown(onlyTen), onlyTen, "无编号 1 章节时原文返回，不得切在 10 上");
}
