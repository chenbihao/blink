//! 主题调试台（0.25.11）。
//!
//! 单页验收全部主题：语义 token 色板、WCAG 对比度体检、真实窗口组件样本。
//! 纯静态可独立运行（浏览器直开或 dev server），不依赖 Tauri 后端；
//! 主题切换复用 shared/theme.js（auto 跟随系统，其余透传 data-theme）。

import { applyTheme } from "../shared/theme.js";
import { iconHTML, ensureSpriteLoaded } from "../shared/icon.js";
import { renderKey, renderCombo } from "../shared/kbd.js";
import { contrastRatio, compositeOver, verdict } from "./contrast.js";
import { THEME_CATALOG } from "./themes.js";

/** 色板展示的语义 token（顺序即展示顺序，按 token 体系分组注释）。 */
const SWATCH_TOKENS = [
    // 背景
    "--bg", "--bg-sidebar", "--bg-deep",
    // 表面
    "--surface", "--surface-2",
    // 文字
    "--text", "--text-sub", "--text-dim", "--text-faint",
    // 强调
    "--accent", "--accent-bg", "--on-accent",
    // 状态
    "--green", "--green-bg", "--red", "--red-bg", "--red-border",
    "--warning", "--warning-bg", "--warning-border", "--yellow",
    // 其他
    "--shadow", "--shadow-strong", "--border-glow", "--danger-hover-bg",
];

/** 对比度体检的前景/背景对。[前景 token, 背景 token, 场景说明, 判级基准] */
const CONTRAST_PAIRS = [
    ["--text", "--bg", "正文 / 页面底", "normal"],
    ["--text", "--surface", "正文 / 卡片底", "normal"],
    ["--text-sub", "--bg", "次要文本 / 页面底", "normal"],
    ["--text-sub", "--surface", "次要文本 / 卡片底", "normal"],
    ["--text-dim", "--bg", "弱化文本 / 页面底", "normal"],
    ["--text-dim", "--surface", "弱化文本 / 卡片底", "normal"],
    ["--text-faint", "--bg", "ghost 提示（刻意弱）", "large"],
    ["--accent", "--bg", "强调/链接文字", "normal"],
    ["--green", "--bg", "成功/计算结果文字", "normal"],
    ["--red", "--bg", "错误文字", "normal"],
    ["--warning", "--bg", "警告文字", "normal"],
    ["--yellow", "--bg", "错误项文字（error-item）", "normal"],
    ["--on-accent", "--accent", "accent 实底上的文字", "normal"],
    ["--text", "--bg-deep", "正文 / 输入区底", "normal"],
];

const computedRoot = () => getComputedStyle(document.documentElement);

/** 读 token 计算值；未定义返回 null（调试台自身也暴露 token 断供）。 */
function tokenValue(name) {
    const v = computedRoot().getPropertyValue(name).trim();
    return v || null;
}

/* ── 主题切换 ─────────────────────────────────────────────────────────────── */

function buildThemeSelect(select) {
    const groups = [];
    for (const item of THEME_CATALOG) {
        let bucket = groups.find((g) => g.label === item.group);
        if (!bucket) {
            bucket = { label: item.group, items: [] };
            groups.push(bucket);
        }
        bucket.items.push(item);
    }
    for (const group of groups) {
        const optgroup = document.createElement("optgroup");
        optgroup.label = group.label;
        for (const { value, label } of group.items) {
            const option = document.createElement("option");
            option.value = value;
            option.textContent = label;
            optgroup.appendChild(option);
        }
        select.appendChild(optgroup);
    }
}

/* ── 对比度体检 ───────────────────────────────────────────────────────────── */

function verdictBadge(v, ratio) {
    const cls = v === "pass" ? "td-verdict-pass" : v === "large" ? "td-verdict-large" : "td-verdict-fail";
    const text = v === "pass" ? "✓ AA" : v === "large" ? "△ 仅大字/UI" : "✗ 不达标";
    return `<span class="td-verdict ${cls}">${text}</span> <span class="td-contrast-mono">${ratio.toFixed(2)}:1</span>`;
}

function renderContrastTable(table) {
    const rows = CONTRAST_PAIRS.map(([fgTok, bgTok, usage, level]) => {
        let fg = tokenValue(fgTok);
        const bg = tokenValue(bgTok);
        if (!fg || !bg) {
            return `<tr><td><code>${fgTok}</code> / <code>${bgTok}</code></td><td>${usage}</td><td colspan="2" class="td-verdict td-verdict-fail">token 缺失</td></tr>`;
        }
        // 半透明前景（如 accent-bg）先合成到背景再算，避免 alpha 扰动
        if (fg.startsWith("rgba")) fg = compositeOver(fg, bg);
        const ratio = contrastRatio(fg, bg);
        const v = verdict(ratio);
        // 判级基准 large 的对（ghost 提示）允许 large 算过
        const effective = level === "large" && v === "large" ? "pass" : v;
        return `<tr>
            <td><span class="td-contrast-fg"><span class="td-contrast-dot" style="background: ${fg}"></span><code>${fgTok}</code> → <code>${bgTok}</code></span></td>
            <td>${usage}</td>
            <td>${verdictBadge(effective === "pass" ? "pass" : v, ratio)}</td>
        </tr>`;
    });
    table.innerHTML = `<thead><tr><th>组合</th><th>场景</th><th>结论</th></tr></thead><tbody>${rows.join("")}</tbody>`;
}

/* ── token 色板 ───────────────────────────────────────────────────────────── */

function renderSwatches(container) {
    const cells = SWATCH_TOKENS.map((name) => {
        const value = tokenValue(name);
        const safe = value ? value.replace(/"/g, "&quot;") : "transparent";
        return `<div class="td-swatch">
            <div class="td-swatch-color" style="background: ${safe}"></div>
            <div class="td-swatch-meta">
                <span class="td-swatch-name">${name}</span>
                <span class="td-swatch-value">${value ?? "未定义"}</span>
            </div>
        </div>`;
    });
    container.innerHTML = cells.join("");
}

/* ── 主窗结果列表模拟（result-item.css 真实类名与结构）────────────────────── */

function buildResultsMock(list) {
    const items = [
        { cls: "active", name: "记事本", desc: "打开应用" },
        { name: "Visual Studio Code", desc: "打开应用" },
        { cls: "calc-result", text: "= 1024 × 768 = 786432" },
        { cls: "error-item", text: "插件 weather 执行失败：网络请求超时 (500ms)" },
        {
            cls: "ai-item error-item",
            badge: true,
            name: "AI 会话出错",
        },
        { cls: "context-aware", name: "打开链接", desc: "https://example.com/very/long/path", arg: true },
        { name: "weather.zip", desc: "D:\\Downloads\\weather.zip" },
    ];
    for (const item of items) {
        const li = document.createElement("li");
        if (item.cls) li.className = item.cls;
        if (item.badge) {
            const badge = document.createElement("span");
            badge.className = "ai-icon-badge";
            badge.textContent = "AI";
            li.appendChild(badge);
        }
        if (item.text) {
            li.textContent = item.text;
        } else {
            const body = document.createElement("div");
            body.className = "item-body";
            const name = document.createElement("span");
            name.className = "item-name";
            name.textContent = item.name;
            body.appendChild(name);
            if (item.desc) {
                const desc = document.createElement("span");
                desc.className = item.arg ? "item-desc item-desc-arg" : "item-desc";
                desc.textContent = item.desc;
                body.appendChild(desc);
            }
            li.appendChild(body);
            const num = document.createElement("span");
            num.className = "item-badge";
            num.textContent = "1";
            li.appendChild(num);
        }
        list.appendChild(li);
    }
}

/* ── 状态栏 / 建议条 / 瞬时反馈模拟 ───────────────────────────────────────── */

function buildStatusbarMock(statusbar) {
    const left = document.createElement("div");
    left.className = "hint-left";
    const primary = document.createElement("span");
    primary.className = "hint-primary";
    primary.appendChild(renderKey("↑"));
    primary.appendChild(renderKey("↓"));
    primary.appendChild(document.createTextNode(" 选择 "));
    primary.appendChild(renderKey("Enter"));
    primary.appendChild(document.createTextNode(" 打开 · "));
    primary.appendChild(renderCombo("Alt+1"));
    primary.appendChild(document.createTextNode(" 直达"));
    left.appendChild(primary);
    const right = document.createElement("div");
    right.className = "hint-right";
    right.appendChild(renderCombo("Alt+A"));
    const label = document.createElement("span");
    label.className = "chord-label";
    label.textContent = "翻译";
    right.appendChild(label);
    statusbar.appendChild(left);
    statusbar.appendChild(right);
}

function buildSuggestionMock(bar) {
    const rows = [
        { slot: "primary", icon: "languages", text: "翻译并搜索", origin: "建议", key: renderKey("Tab") },
        { slot: "secondary", icon: "sparkles", text: "询问 AI", origin: "AI", key: renderCombo("Shift+Tab") },
    ];
    for (const { slot, icon, text, origin, key } of rows) {
        const row = document.createElement("button");
        row.type = "button";
        row.className = `suggestion-row slot-${slot}`;
        const iconEl = document.createElement("span");
        iconEl.className = "suggestion-icon";
        iconEl.innerHTML = iconHTML(icon);
        row.appendChild(iconEl);
        const textEl = document.createElement("span");
        textEl.className = "suggestion-text";
        textEl.appendChild(document.createTextNode(text));
        const originEl = document.createElement("span");
        originEl.className = "suggestion-origin";
        originEl.textContent = origin;
        textEl.appendChild(document.createTextNode(" · "));
        textEl.appendChild(originEl);
        row.appendChild(textEl);
        key.classList.add("suggestion-key");
        row.appendChild(key);
        bar.appendChild(row);
    }
}

/* ── 对话窗口模拟（bubble.css / tool-card.css 真实类名）───────────────────── */

function buildChatMock(container) {
    const user = document.createElement("div");
    user.className = "chat-msg chat-msg-user";
    user.textContent = "帮我把这段代码改成浅色主题友好";
    container.appendChild(user);

    const assistant = document.createElement("div");
    assistant.className = "chat-msg chat-msg-assistant";
    assistant.innerHTML = `
        <p>好的，下面是示例代码：</p>
        <pre><code class="hljs language-js"><span class="hljs-keyword">const</span> <span class="hljs-title">theme</span> = <span class="hljs-string">"light"</span>;
<span class="hljs-keyword">if</span> (theme === <span class="hljs-string">"light"</span>) { <span class="hljs-built_in">console</span>.<span class="hljs-title">log</span>(<span class="hljs-string">"浅色模式"</span>); } <span class="hljs-comment">// 达标 4.5:1</span></code></pre>
        <div class="chat-tool-card">
            <div class="chat-tool-card-header">
                <span class="chat-tool-card-name">web_search</span>
                <span class="chat-tool-card-duration">1.2s</span>
            </div>
            <div class="chat-tool-detail"><pre>搜索完成，共 5 条结果</pre></div>
        </div>
        <p>以上就是修改建议，<strong>注意对比度</strong>要达到 WCAG AA 标准。</p>
        <div class="chat-msg-footer"><span class="chat-msg-model">glm-4.7-flash</span></div>`;
    container.appendChild(assistant);
}

/* ── 组件样本 ─────────────────────────────────────────────────────────────── */

function buildComponentSamples(container) {
    container.innerHTML = "";

    const block = (label, inner) => {
        const el = document.createElement("div");
        el.className = "td-samples-block";
        const labelEl = document.createElement("span");
        labelEl.className = "td-block-label";
        labelEl.textContent = label;
        el.appendChild(labelEl);
        const row = document.createElement("div");
        row.className = "td-sample-row";
        row.append(...inner);
        el.appendChild(row);
        container.appendChild(el);
    };

    const el = (html) => {
        const t = document.createElement("template");
        t.innerHTML = html.trim();
        return t.content.firstElementChild;
    };

    block("按钮", [
        el(`<button class="btn">次要</button>`),
        el(`<button class="btn btn-primary">主要</button>`),
        el(`<button class="btn btn-danger">危险</button>`),
        el(`<button class="btn btn-small">小按钮</button>`),
    ]);

    block("输入", [
        el(`<input class="input-wide" placeholder="输入以搜索…" />`),
        el(`<input class="input-small" value="180" />`),
    ]);

    block("勾选与开关", [
        el(`<label class="checkbox"><input type="checkbox" checked /><span class="checkmark"></span><span class="label-text">开机自启</span></label>`),
        el(`<label class="checkbox"><input type="checkbox" /><span class="checkmark"></span><span class="label-text">未勾选</span></label>`),
        el(`<label class="switch"><input type="checkbox" checked /><span class="slider"></span></label>`),
        el(`<label class="radio radio-inline"><input type="radio" name="td-radio" checked /><span class="radio-dot"></span><span class="label-text">云端</span></label>`),
        el(`<label class="radio radio-inline"><input type="radio" name="td-radio" /><span class="radio-dot"></span><span class="label-text">本地</span></label>`),
    ]);

    const kbdBlock = document.createElement("div");
    kbdBlock.className = "td-sample-row";
    kbdBlock.appendChild(renderCombo("Alt+A"));
    kbdBlock.appendChild(renderKey("Enter"));
    kbdBlock.appendChild(renderCombo("Ctrl+Shift+P"));
    block("键帽（kbd.css 真源）", [kbdBlock]);

    block("徽章与 chip", [
        el(`<span class="chip">剪贴板</span>`),
        el(`<span class="status-badge status-available">可用</span>`),
        el(`<span class="status-badge status-unavailable">不可用</span>`),
    ]);

    const spinner = el(`<span class="spinner spinner-md" aria-label="加载中"></span>`);
    block("加载", [spinner]);
}

/* ── 装配 ─────────────────────────────────────────────────────────────────── */

async function init() {
    const select = document.getElementById("td-theme-select");
    const current = document.getElementById("td-current");
    const contrastTable = document.getElementById("td-contrast-table");
    const swatches = document.getElementById("td-swatches");

    buildThemeSelect(select);

    const refresh = () => {
        const attr = document.documentElement.getAttribute("data-theme");
        current.textContent = attr ? `data-theme="${attr}"` : "data-theme 未设（回落 :root 深色）";
        renderContrastTable(contrastTable);
        renderSwatches(swatches);
    };

    select.addEventListener("change", () => {
        applyTheme(select.value);
        refresh();
    });

    try {
        await ensureSpriteLoaded();
    } catch {
        // 浏览器直开 file:// 时 sprite 可能取不到；图标位留空不阻塞主题验证
    }

    buildResultsMock(document.getElementById("results"));
    const statusbar = document.getElementById("statusbar");
    statusbar.classList.add("visible");
    buildStatusbarMock(statusbar);
    const suggestionBar = document.getElementById("suggestion-bar");
    suggestionBar.classList.add("visible");
    buildSuggestionMock(suggestionBar);
    buildChatMock(document.getElementById("td-chat"));
    buildComponentSamples(document.getElementById("td-components"));

    // 初始主题：URL ?theme= 优先，否则 auto（跟随系统）
    const requested = new URLSearchParams(location.search).get("theme");
    const initial = requested && THEME_CATALOG.some((t) => t.value === requested) ? requested : "auto";
    select.value = initial;
    applyTheme(initial);
    refresh();
}

init();
