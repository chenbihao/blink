/**
 * 丝滑小妙招 Tab（0.25.28）
 *
 * 内容真源是 docs/guide.md（中文）与 docs/guide.en.md（英文翻译）：后端
 * 编译期 include_str! 双文档嵌入，本 tab 一次性拉取（get_guide_markdown）
 * 后按当前语言用 Cherry 渲染进 #guide-body（容器模式），语言切换
 * （onLangChange）时用内存里的原文重渲染正文与目录。
 *
 * Cherry vendor 脚本由 settings.html 底部统一加载（about tab 同源依赖），
 * 无需懒加载；主题适配见 components/cherry.css，布局见 settings-guide.css。
 *
 * 应用内裁掉文档头（H1/引言/目录章节）+ 右侧悬浮目录（sticky 跟随滚动、
 * 高亮当前章节）；文档头只服务在线阅读，职责划分见 bodyMarkdown 注释。
 */
import {invoke} from "../../shared/tauri.js";
import {initMarkdown, renderMarkdown} from "../../shared/markdown.js";
import {t, getLang, onLangChange} from "../../i18n/index.js";

/** 一次性加载标志：指南是编译期常量，加载成功/失败后不再重试 */
let rendered = false;

/** 拉取到的双语原文（GuideDoc），语言切换重渲染时复用 */
let doc = null;

/** 当前悬浮目录（语言切换重渲染时整体替换） */
let tocNav = null;

/** 当前目录跟踪的标题集（与 tocNav 同步更新，供单例滚动监听读取） */
let tocHeadings = [];

/**
 * 初始化丝滑小妙招 Tab。
 * 首次激活（点击 tab 按钮，含 open_settings_tab 的 eval click）时加载渲染。
 */
export function initGuideTab() {
    const tabBtn = document.querySelector('.tab[data-tab="guide"]');
    if (!tabBtn) return;
    tabBtn.addEventListener("click", loadGuide);
    // 兜底：设置窗打开时本 tab 已是激活态（如上次隐藏前停留在此页）
    if (tabBtn.classList.contains("active")) loadGuide();
    // 锚点统一委托在 panel 上：正文里的 Cherry 标题锚点与侧边悬浮目录的
    // 跳转链接走同一条定位逻辑。markdown.js 的 addBlankTarget 给所有 <a>
    // 补了 target="_blank"，页内锚点必须在默认动作（外开浏览器）前拦截。
    // Cherry 生成的标题 id 是百分号编码形态（href 与 id 同构），故先按
    // 原始 fragment 查，解码形态兜底（vendor 行为变化时仍可用）。
    const panel = document.getElementById("guide");
    if (panel) {
        panel.addEventListener("click", (e) => {
            const anchor = e.target.closest('a[href^="#"]');
            if (!anchor) return;
            const rawId = anchor.hash.slice(1);
            let target = document.getElementById(rawId);
            if (!target) {
                try {
                    target = document.getElementById(decodeURIComponent(rawId));
                } catch {
                    // 非法编码序列，放弃定位
                }
            }
            if (!target) return;
            e.preventDefault();
            target.scrollIntoView({behavior: "smooth", block: "start"});
            // 鼠标点击跳转后把焦点还回去：链接持焦会让目录卡的 :focus-within
            // 持续匹配，鼠标离开也不收起。键盘激活（Enter，detail=0）保留
            // 焦点——卡片对键盘用户保持可见是有意的 a11y 行为。
            if (e.detail > 0 && document.activeElement === anchor) {
                anchor.blur();
            }
        });
    }
}

/**
 * 应用内渲染前裁掉文档头部。真源文档开头的「H1 + 引言引用块」服务 GitHub
 * 在线阅读（含中英互切链接），「## 目录 / ## Contents」章节服务在线版页内
 * 跳转（GitHub 没有悬浮目录）；设置页 tab 内这两者的职责由 tab chrome
 * （panel h2/lede/meta 行）与右侧悬浮目录（buildToc）承担，保留会同屏重复。
 * 约定：两者必须完整位于首个编号章节「## 1.」之前；中英两份文档都使用
 * 数字编号章节，故按「\n## 1.」切片与语言无关。
 */
export function bodyMarkdown(markdown) {
    const start = markdown.indexOf("\n## 1.");
    return start === -1 ? markdown : markdown.slice(start + 1);
}

/**
 * 依据渲染后的 h2 构建右侧悬浮目录（窄窗由 settings-guide.css 收成
 * 贴右缘的悬浮小签，悬停/聚焦展开）。
 * 结构必须与 settings-guide.css 的选择器成对：.guide-toc 是不接收点击的
 * sticky 行程轨道（pointer-events:none 会被链接继承），.guide-toc-inner
 * 才是可见的 sticky 列表（样式与 pointer-events:auto 都挂在它下面）。
 * 链接 href 直接复用 Cherry 生成的编码 id，点击走 panel 上的统一锚点委托；
 * 滚动高亮由 trackTocActive 驱动。title 兜底英文长条目的省略号截断。
 * 导出仅供 guide-toc.test.mjs 锁 DOM 契约（曾因漏 inner 包裹层致真机
 * 目录不可点击且无样式，预览与产品 DOM 漂移）。
 */
export function buildToc(headings) {
    const nav = document.createElement("nav");
    nav.className = "guide-toc";
    nav.setAttribute("aria-label", t("guide.toc"));
    const inner = document.createElement("div");
    inner.className = "guide-toc-inner";
    for (const h of headings) {
        const a = document.createElement("a");
        a.href = "#" + h.id;
        a.textContent = h.textContent;
        a.title = h.textContent;
        inner.appendChild(a);
    }
    nav.appendChild(inner);
    return nav;
}

/** 滚动监听只绑一次的标志（语言切换重渲染只换 tocNav/tocHeadings） */
let tocScrollBound = false;

/** 以视口上缘下方 140px 为判线，高亮最后一个越线标题对应的目录项 */
function updateTocActive() {
    const panel = document.getElementById("guide");
    if (!panel || !panel.classList.contains("active")) return;
    let current = 0;
    tocHeadings.forEach((h, i) => {
        if (h.getBoundingClientRect().top <= 140) current = i;
    });
    const links = tocNav ? [...tocNav.querySelectorAll("a")] : [];
    links.forEach((a, i) => a.classList.toggle("active", i === current));
}

/**
 * 滚动高亮当前章节：rAF 合帧监听 .content 滚动；panel 未激活（停留在其他
 * tab）时跳过计算。重复调用只追加高亮刷新，不叠加监听器。
 */
function trackTocActive(content) {
    if (!tocScrollBound) {
        tocScrollBound = true;
        let ticking = false;
        content.addEventListener("scroll", () => {
            if (!ticking) {
                ticking = true;
                requestAnimationFrame(updateTocActive);
            }
        }, {passive: true});
    }
    updateTocActive();
}

/** 按当前语言渲染正文 + 目录 + 在线链接（语言切换时整体重跑） */
function renderGuide() {
    if (!doc) return;
    const body = document.getElementById("guide-body");
    const panel = document.getElementById("guide");
    const content = document.querySelector("main.content");
    const isEn = getLang() === "en";
    const versionEl = document.getElementById("guide-version");
    if (versionEl) versionEl.textContent = t("guide.version", {version: doc.version});
    const onlineEl = document.getElementById("guide-online");
    // .external-link 事件委托读 data-url，外部浏览器打开
    // （hidden 直到拿到真实 URL，防加载中点击走 openExternalUrl(undefined)）
    if (onlineEl) {
        onlineEl.dataset.url = isEn ? doc.online_url_en : doc.online_url;
        onlineEl.hidden = false;
    }
    renderMarkdown(bodyMarkdown(isEn ? doc.markdown_en : doc.markdown), {container: body});
    // 侧边悬浮目录：渲染完成后按实际 h2 构建，正文宽度由 CSS 在宽窗收窄
    if (panel && content) {
        tocNav?.remove();
        tocHeadings = [...body.querySelectorAll("h2")];
        tocNav = buildToc(tocHeadings);
        panel.appendChild(tocNav);
        trackTocActive(content);
    }
}

async function loadGuide() {
    if (rendered) return;
    rendered = true;
    initMarkdown();

    const body = document.getElementById("guide-body");
    try {
        doc = await invoke("get_guide_markdown");
        onLangChange(renderGuide);
        renderGuide();
    } catch (e) {
        console.error("[settings/guide] load failed:", e);
        if (body) {
            body.textContent = t("guide.error", {message: e?.message || String(e)});
        }
    }
}
