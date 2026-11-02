//! 录制编辑器「最近录制」列表（0.28.23 §5.7.4）。
//!
//! 纯静态前端：分页摘要（`gif_recent_sessions`，不解析完整索引）+ 可见项
//! 按需缩略图（`gif_session_thumbnail`）；打开/删除为显式操作——当前编辑
//! 会话不可从列表删除，在途导出时切换须确认后显式取消。
//!
//! 工厂注入依赖（DOM/invoke/确认框/i18n），不做顶层初始化——便于单测。

import {formatSeconds} from './timeline-view.js';

/** 单页条数（后端 clamp 1..50）。 */
const PAGE_SIZE = 20;
/** 滚动接近底部时加载下一页的提前量（px）。 */
const LOAD_MORE_THRESHOLD = 120;

/**
 * 纯函数：摘要项的元信息行（时长 · 尺寸 · fps · 帧数 · 占用）。
 * @param {{totalMs:number, width:number, height:number, fps:number, frameCount:number, spoolBytes:number}} s
 * @returns {string}
 */
export function itemMetaText(s) {
    const kb = s.spoolBytes < 1024 * 1024
        ? `${Math.max(1, Math.round(s.spoolBytes / 1024))} KB`
        : `${(s.spoolBytes / (1024 * 1024)).toFixed(1)} MB`;
    return `${formatSeconds(s.totalMs || 0)} · ${s.width}×${s.height} · ${s.fps}fps · ${s.frameCount} 帧 · ${kb}`;
}

/** 纯函数：摘要项的时间标题（本地时间 `MM-dd HH:mm:ss`）。 */
export function itemTimeText(createdAtMs) {
    const d = new Date(createdAtMs);
    if (Number.isNaN(d.getTime())) return '';
    const p = n => String(n).padStart(2, '0');
    return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** 纯函数：状态徽章 key 列表（异常中断 / 已保存编辑）。 */
export function itemBadges(s) {
    const badges = [];
    if (s.hasDraft) badges.push('draft');
    if (s.state === 'recording') badges.push('abnormal');
    return badges;
}

/**
 * @param {object} deps
 * @param {() => Promise<any>} deps.invoke 与 shared/tauri 同签名的 invoke 封装
 * @param {(key:string, params?:object) => string} deps.t i18n t()
 * @param {(message:string) => Promise<boolean>} deps.confirm 确认对话框
 * @param {() => void} deps.onOpened 打开会话后回调（编辑器切回编辑视图）
 * @param {() => string|null} deps.currentSessionId 当前编辑会话（不可删除/标记）
 */
export function createRecentList(deps) {
    const {invoke, t, confirm, onOpened, currentSessionId} = deps;
    let items = [];
    let total = 0;
    let loading = false;

    const els = {
        pane: document.getElementById('recent-pane'),
        list: document.getElementById('recent-list'),
        empty: document.getElementById('recent-empty'),
        loading: document.getElementById('recent-loading'),
        back: document.getElementById('btn-back-edit'),
        refresh: document.getElementById('btn-recent-refresh'),
    };

    async function load(reset) {
        if (loading) return;
        loading = true;
        els.loading?.classList.remove('hidden');
        try {
            const offset = reset ? 0 : items.length;
            const res = await invoke('gif_recent_sessions', {offset, limit: PAGE_SIZE});
            if (reset) items = [];
            // 追加去重（后端分页窗口内新录制可能挤位）：按 id 过滤已存在项
            const seen = new Set(items.map(i => i.id));
            for (const it of (res?.items || [])) {
                if (!seen.has(it.id)) items.push(it);
            }
            total = res?.total ?? items.length;
            render();
        } catch (err) {
            console.error('[gif-editor] 最近录制加载失败', err);
        } finally {
            loading = false;
            els.loading?.classList.add('hidden');
        }
    }

    function render() {
        const list = els.list;
        if (!list) return;
        list.textContent = '';
        els.empty?.classList.toggle('hidden', items.length > 0);
        for (const s of items) list.appendChild(buildItem(s));
        const back = els.back;
        if (back) back.classList.toggle('hidden', currentSessionId() == null);
    }

    function buildItem(s) {
        const current = currentSessionId() === s.id;
        const article = document.createElement('article');
        article.className = 'recent-item';
        article.dataset.id = s.id;

        const thumb = document.createElement('div');
        thumb.className = 'recent-thumb';
        const img = document.createElement('img');
        img.alt = '';
        img.loading = 'lazy';
        thumb.appendChild(img);
        // 缩略图失败静默占位（列表不因单帧解码失败阻塞）
        void invoke('gif_session_thumbnail', {sessionId: s.id})
            .then(r => {
                if (article.isConnected && r?.pngBase64) {
                    img.src = `data:image/png;base64,${r.pngBase64}`;
                }
            })
            .catch(() => {});

        const info = document.createElement('div');
        info.className = 'recent-info';
        const name = document.createElement('div');
        name.className = 'recent-name';
        name.textContent = itemTimeText(s.createdAt);
        const meta = document.createElement('div');
        meta.className = 'recent-meta';
        meta.textContent = itemMetaText(s);
        info.appendChild(name);
        info.appendChild(meta);
        for (const badge of itemBadges(s)) {
            const b = document.createElement('span');
            b.className = `recent-badge badge-${badge}`;
            b.textContent = badge === 'draft' ? t('gif.recent.draft') : t('gif.recent.abnormal');
            info.appendChild(b);
        }
        if (current) {
            const b = document.createElement('span');
            b.className = 'recent-badge badge-current';
            b.textContent = t('gif.recent.current');
            info.appendChild(b);
        }

        const actions = document.createElement('div');
        actions.className = 'recent-item-actions';
        const btnOpen = document.createElement('button');
        btnOpen.type = 'button';
        btnOpen.className = 'btn';
        btnOpen.textContent = t('gif.recent.open');
        btnOpen.addEventListener('click', () => openSession(s));
        const btnDelete = document.createElement('button');
        btnDelete.type = 'button';
        btnDelete.className = 'btn btn-danger';
        btnDelete.textContent = t('gif.recent.delete');
        // 当前编辑会话不可删除（先切换）；其余显式确认后删（含编辑进度）
        btnDelete.disabled = current;
        btnDelete.title = current ? t('gif.recent.current') : '';
        btnDelete.addEventListener('click', () => deleteSession(s));
        actions.appendChild(btnOpen);
        actions.appendChild(btnDelete);

        article.appendChild(thumb);
        article.appendChild(info);
        article.appendChild(actions);
        return article;
    }

    /** 打开会话：当前会话在途导出时先确认并显式取消，不静默取消。 */
    async function openSession(s) {
        if (currentSessionId() && currentSessionId() !== s.id) {
            try {
                const job = await invoke('gif_export_status', {sessionId: currentSessionId(), jobId: null});
                if (job && !['done', 'cancelled', 'failed'].includes(job.stage)) {
                    const okToCancel = await confirm(t('gif.recent.exportBusyMsg'));
                    if (!okToCancel) return;
                    await invoke('gif_export_cancel', {sessionId: currentSessionId()});
                }
            } catch {
                // 状态查询失败不阻断切换（后端 gif_open_session 仍有 busy 保护）
            }
        }
        try {
            await invoke('gif_open_session', {sessionId: s.id});
            onOpened();
        } catch (err) {
            console.error('[gif-editor] 打开录制失败', err);
            // gif_export_busy：提示用户先在导出面板完成/取消，不自动重试
            if (err?.code === 'gif_export_busy') {
                void confirm(t('gif.recent.exportBusyRetry'));
            }
        }
    }

    async function deleteSession(s) {
        const ok = await confirm(t('gif.recent.deleteConfirm'));
        if (!ok) return;
        try {
            await invoke('gif_discard_session', {sessionId: s.id});
        } catch (err) {
            console.error('[gif-editor] 删除录制失败', err);
            return;
        }
        items = items.filter(i => i.id !== s.id);
        total = Math.max(0, total - 1);
        render();
    }

    function bind() {
        els.list?.addEventListener('scroll', () => {
            const el = els.list;
            if (!el || loading || items.length >= total) return;
            if (el.scrollTop + el.clientHeight + LOAD_MORE_THRESHOLD >= el.scrollHeight) {
                void load(false);
            }
        });
        els.refresh?.addEventListener('click', () => void load(true));
    }

    return {
        bind,
        refresh: () => load(true),
    };
}
