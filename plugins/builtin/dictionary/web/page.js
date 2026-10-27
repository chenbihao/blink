//! 离线词典插件页面（0.28.4）——详情浏览/复制/转翻译。
//!
//! 数据全部经宿主 SDK `Blink.invoke("dictionary_lookup"/"dictionary_search_zh")`
//! 走 Capability（owner 复核）；无直接文件/网络访问。词库状态由 tool 结果
//! 的 data.status 表达（not_installed/corrupt 如实展示修复指引）。

const input = document.getElementById("dict-input");
const clearBtn = document.getElementById("dict-clear");
const body = document.getElementById("dict-body");
const statusEl = document.getElementById("dict-status");

let epoch = 0; // 竞态丢弃：连续输入/切词时旧响应不再渲染
let currentWord = "";

const ELLIPSIS = "…";

function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
}

function escText(s) {
    return String(s ?? "");
}

function truncate(s, n) {
    const t = escText(s).trim();
    return t.length <= n ? t : t.slice(0, n) + ELLIPSIS;
}

/** 字面 \n 分行渲染（构建期保真的多义项）。 */
function senseLines(raw) {
    return escText(raw).split("\\n").map(l => l.trim()).filter(Boolean);
}

function setStatus(text) {
    statusEl.textContent = text || "";
}

/** 初始/清空空态（含 Lucide 线性图标，与 index.html 静态结构同构）。 */
const SEARCH_ICON_SVG = '<svg class="dict-empty-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></svg>';

function emptyState() {
    const wrap = el("div", "dict-empty");
    wrap.innerHTML = SEARCH_ICON_SVG;
    wrap.appendChild(el("span", null, "输入要查的词"));
    return wrap;
}

function renderEntry(entry, dictServiceDesc) {
    const wrap = el("div", "dict-entry");

    const head = el("div", "dict-entry-head");
    head.appendChild(el("span", "dict-entry-word", entry.display || entry.word));
    if (entry.phonetic) head.appendChild(el("span", "dict-entry-phonetic", `/${entry.phonetic}/`));
    const badges = el("span", "dict-entry-badges");
    if (entry.oxford) badges.appendChild(el("span", "dict-badge dict-badge-badge", "牛津3000"));
    if (entry.collins) badges.appendChild(el("span", "dict-badge dict-badge-badge", "柯林斯 " + "★".repeat(Math.min(entry.collins, 5))));
    if (entry.tag) {
        for (const tag of escText(entry.tag).split(/\s+/).filter(Boolean)) {
            badges.appendChild(el("span", "dict-badge", tag));
        }
    }
    head.appendChild(badges);
    wrap.appendChild(head);

    if (entry.lemma_of) {
        wrap.appendChild(el("div", "dict-lemma", entry.lemma_of));
    }

    const transLines = senseLines(entry.translation);
    if (transLines.length) {
        const section = el("div", "dict-section");
        section.appendChild(el("div", "dict-section-title", "释义"));
        for (const line of transLines) section.appendChild(el("div", "dict-sense", line));
        wrap.appendChild(section);
    }

    const defLines = senseLines(entry.definition);
    if (defLines.length) {
        const section = el("div", "dict-section");
        section.appendChild(el("div", "dict-section-title", "English"));
        for (const line of defLines) section.appendChild(el("div", "dict-sense", line));
        wrap.appendChild(section);
    }

    if (Array.isArray(entry.forms) && entry.forms.length) {
        const section = el("div", "dict-section");
        section.appendChild(el("div", "dict-section-title", "词形"));
        const forms = el("div", "dict-forms");
        for (const f of entry.forms) {
            const chip = el("span", "dict-form-chip");
            chip.append(f.form + " ");
            chip.appendChild(el("span", null, formKindZh(f.kind)));
            forms.appendChild(chip);
        }
        section.appendChild(forms);
        wrap.appendChild(section);
    }

    if (Array.isArray(entry.etymology) && entry.etymology.length) {
        const section = el("div", "dict-section");
        section.appendChild(el("div", "dict-section-title", "词源"));
        for (const ety of entry.etymology) {
            const card = el("div", "dict-ety-card");
            const line = el("div");
            line.appendChild(el("span", "dict-ety-root", ety.root));
            if (ety.meaning) line.appendChild(el("span", "dict-ety-meta", `${ety.meaning}${ety.origin ? " · " + ety.origin : ""}`));
            if (ety.class) line.appendChild(el("span", "dict-ety-meta", ety.class));
            card.appendChild(line);
            section.appendChild(card);
        }
        wrap.appendChild(section);
    }

    // 转翻译入口在主窗快查（未收录项 Route 回写"翻译 xxx"）——插件页面无法
    // 调其他插件的 tool（宿主只放行本插件 manifest 声明的 tool），这里提供
    // 复制原词方便用户自行粘贴到翻译。
    const actions = el("div", "dict-entry-actions");
    const copyBtn = el("button", "dict-btn", "复制释义");
    copyBtn.addEventListener("click", async () => {
        try {
            await Blink.copy(transLines.join("\n"));
            copyBtn.textContent = "已复制";
            setTimeout(() => (copyBtn.textContent = "复制释义"), 1500);
        } catch (e) {
            console.error("copy failed:", e);
        }
    });
    actions.appendChild(copyBtn);
    const copyWordBtn = el("button", "dict-btn", "复制原词");
    copyWordBtn.addEventListener("click", async () => {
        try {
            await Blink.copy(entry.display || entry.word);
            copyWordBtn.textContent = "已复制";
            setTimeout(() => (copyWordBtn.textContent = "复制原词"), 1500);
        } catch (e) {
            console.error("copy failed:", e);
        }
    });
    actions.appendChild(copyWordBtn);
    wrap.appendChild(actions);

    if (dictServiceDesc) setStatus(dictServiceDesc);
    return wrap;
}

function formKindZh(kind) {
    return {
        p: "过去式", d: "过去分词", i: "现在分词", "3": "第三人称单数",
        r: "比较级", t: "最高级", s: "复数",
    }[kind] || "词形";
}

function renderCandidates(word, candidates) {
    const wrap = el("div");
    wrap.appendChild(el("div", "dict-empty", `「${word}」未收录`));
    if (!candidates?.length) {
        const nf = el("div", "dict-notfound");
        nf.appendChild(el("div", null, "词库中没有这个词"));
        // 转翻译入口在主窗快查（Route 回写"翻译 xxx"）；页面提供复制原词。
        nf.appendChild(el("div", "dict-notfound-hint", `可在主窗输入「翻译 ${word}」显式转翻译，或检查拼写`));
        const btn = el("button", "dict-btn dict-btn-primary", "复制原词");
        btn.addEventListener("click", async () => {
            try {
                await Blink.copy(word);
                btn.textContent = "已复制";
                setTimeout(() => (btn.textContent = "复制原词"), 1500);
            } catch (e) {
                console.error("copy failed:", e);
            }
        });
        nf.appendChild(btn);
        wrap.appendChild(nf);
        return wrap;
    }
    for (const c of candidates) {
        const row = el("div", "dict-candidate");
        row.appendChild(el("span", "dict-candidate-word", c.word));
        row.appendChild(el("span", "dict-candidate-translation", truncate(c.translation, 40)));
        row.addEventListener("click", () => {
            input.value = c.word;
            void query(c.word);
        });
        wrap.appendChild(row);
    }
    return wrap;
}

function renderReverse(term, hits) {
    const wrap = el("div");
    if (!hits?.length) {
        const nf = el("div", "dict-notfound");
        nf.appendChild(el("div", null, `「${term}」无对应词条`));
        nf.appendChild(el("div", "dict-notfound-hint", `基础汉英反查覆盖常用概念；长尾词可在主窗输入「翻译 ${term}」`));
        const btn = el("button", "dict-btn dict-btn-primary", "复制原词");
        btn.addEventListener("click", async () => {
            try {
                await Blink.copy(term);
                btn.textContent = "已复制";
                setTimeout(() => (btn.textContent = "复制原词"), 1500);
            } catch (e) {
                console.error("copy failed:", e);
            }
        });
        nf.appendChild(btn);
        wrap.appendChild(nf);
        return wrap;
    }
    const title = el("div", "dict-section-title", `「${term}」的英文候选`);
    wrap.appendChild(title);
    for (const h of hits) {
        const row = el("div", "dict-candidate");
        row.appendChild(el("span", "dict-candidate-word", h.word));
        row.appendChild(el("span", "dict-candidate-translation", truncate(h.translation, 40)));
        row.addEventListener("click", () => {
            input.value = h.word;
            void query(h.word);
        });
        wrap.appendChild(row);
    }
    return wrap;
}

function renderStateMessage(message, hint) {
    const nf = el("div", "dict-notfound");
    nf.appendChild(el("div", null, message));
    if (hint) nf.appendChild(el("div", "dict-notfound-hint", hint));
    return nf;
}

async function query(word) {
    const myEpoch = ++epoch;
    currentWord = word;
    clearBtn.hidden = !word;
    if (!word.trim()) {
        body.replaceChildren(emptyState());
        setStatus("");
        return;
    }
    body.replaceChildren(el("div", "dict-loading", "查询中…"));
    const isZh = /[\u4e00-\u9fff]/.test(word);
    const tool = isZh ? "dictionary_search_zh" : "dictionary_lookup";
    const arg = isZh ? {term: word} : {word};
    let r;
    try {
        r = await Blink.invoke(tool, arg);
    } catch (e) {
        if (myEpoch !== epoch) return;
        body.replaceChildren(el("div", "dict-error", `查询失败：${e?.message || e}`));
        return;
    }
    if (myEpoch !== epoch) return; // 过期响应丢弃
    if (!r?.ok) {
        body.replaceChildren(el("div", "dict-error", r?.error || "查询失败"));
        return;
    }
    setStatus(r.desc || "");

    if (isZh) {
        // 反查：kind = items（数组载荷）
        const hits = Array.isArray(r.items)
            ? r.items.map(i => i.data).filter(Boolean)
            : [];
        body.replaceChildren(renderReverse(word, hits));
        return;
    }

    // lookup：normalize(items) 把词条对象包成单元素列表
    const data = Array.isArray(r.items) && r.items.length
        ? r.items[0].data
        : null;
    if (!data || data.status) {
        if (data?.status === "not_installed") {
            body.replaceChildren(renderStateMessage(
                "离线词库未安装",
                "在 设置 → 插件 → 离线词典 → 数据包 安装后可用",
            ));
            return;
        }
        if (data?.status === "corrupt") {
            body.replaceChildren(renderStateMessage(
                "离线词库数据损坏",
                "在设置页重装数据包可修复",
            ));
            return;
        }
        if (data?.status === "not_found") {
            body.replaceChildren(renderCandidates(word, data.candidates));
            return;
        }
        body.replaceChildren(el("div", "dict-error", "查询失败"));
        return;
    }
    body.replaceChildren(renderEntry(data, r.desc));
}

// ── 事件绑定 ─────────────────────────────────────────────────────────────────

let debounceTimer = 0;

input.addEventListener("input", () => {
    ++epoch;
    clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => void query(input.value.trim()), 250);
});

input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
        e.preventDefault();
        clearTimeout(debounceTimer);
        void query(input.value.trim());
    } else if (e.key === "Escape") {
        Blink.close();
    }
});

clearBtn.addEventListener("click", () => {
    input.value = "";
    clearBtn.hidden = true;
    void query("");
    input.focus();
});

// ── 启动：ready/pull 初始 payload + 复用窗口 payload 事件 ────────────────────

Blink.init("builtin.dictionary").then(ctx => {
    const payload = ctx?.payload;
    if (payload?.word) {
        input.value = payload.word;
        void query(payload.word);
    }
    input.focus();
}).catch(e => {
    console.error("init failed:", e);
    input.focus();
});

Blink.onPayload((payload) => {
    if (payload?.word) {
        input.value = payload.word;
        void query(payload.word);
    }
});
