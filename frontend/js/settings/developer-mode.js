//! 设置页调试入口显隐。仅当前窗口会话记住，不修改任何诊断/日志开关。
import {t, onLangChange} from '../i18n/index.js';

const STORAGE_KEY = 'blink.settings.developerMode';
const REQUIRED_TAPS = 7;
const MAX_TAP_GAP_MS = 1500;
const controllers = new WeakMap();

export function initDeveloperMode({documentRef = document, windowRef = window,
    now = () => performance.now()} = {}) {
    if (controllers.has(documentRef)) return controllers.get(documentRef);
    const version = documentRef.getElementById('about-version');
    const status = documentRef.getElementById('developer-mode-status');
    const hide = documentRef.getElementById('developer-mode-hide');
    const controls = documentRef.getElementById('developer-mode-controls');
    if (!version || !status || !hide) return null;

    let enabled = false, taps = 0, lastTapAt = null;
    let tapTimer = null;
    let messageKey = '', remaining = REQUIRED_TAPS;
    // 清理早期实现的持久化值，不将其迁移到新会话。
    try { windowRef.localStorage.removeItem(STORAGE_KEY); }
    catch (error) { console.warn('[settings] 无法清理旧调试入口显隐偏好', error); }
    try { enabled = windowRef.sessionStorage.getItem(STORAGE_KEY) === 'true'; }
    catch (error) { console.warn('[settings] 无法读取当前会话的调试入口状态', error); }

    const render = () => {
        documentRef.documentElement.dataset.developerMode = String(enabled);
        status.textContent = messageKey ? t(messageKey, {remaining}) : '';
        if (controls) controls.hidden = !messageKey && !enabled;
    };
    const resetTaps = () => {
        if (tapTimer !== null) windowRef.clearTimeout(tapTimer);
        tapTimer = null;
        taps = 0; lastTapAt = null; remaining = REQUIRED_TAPS;
    };
    const save = () => {
        try { windowRef.sessionStorage.setItem(STORAGE_KEY, String(enabled)); }
        catch (error) { console.warn('[settings] 无法保存调试入口会话状态，本次页面内仍生效', error); }
    };
    version.addEventListener('click', () => {
        if (enabled) { messageKey = 'about.developer.enabled'; render(); return; }
        const at = now();
        if (lastTapAt === null || at - lastTapAt > MAX_TAP_GAP_MS || at < lastTapAt) resetTaps();
        lastTapAt = at;
        remaining = REQUIRED_TAPS - ++taps;
        if (remaining === 0) {
            enabled = true;
            messageKey = 'about.developer.enabled';
            resetTaps();
            save();
            console.info('[settings] 调试入口已显示');
        } else {
            messageKey = remaining <= 3 ? 'about.developer.remaining' : '';
            if (tapTimer !== null) windowRef.clearTimeout(tapTimer);
            tapTimer = windowRef.setTimeout(() => {
                resetTaps();
                messageKey = '';
                render();
            }, MAX_TAP_GAP_MS);
        }
        render();
    });
    version.addEventListener('keydown', event => {
        if (event.repeat && (event.key === 'Enter' || event.key === ' ')) event.preventDefault();
    });
    hide.addEventListener('click', () => {
        enabled = false;
        messageKey = 'about.developer.hidden';
        resetTaps();
        save();
        render();
        // 当前按钮即将隐藏，把键盘焦点归还版本号。
        version.focus();
        console.info('[settings] 调试入口已隐藏');
    });
    windowRef.addEventListener('blur', () => {
        resetTaps();
        messageKey = enabled ? 'about.developer.enabled' : '';
        render();
    });
    onLangChange(render);
    messageKey = enabled ? 'about.developer.enabled' : '';
    render();
    const controller = {get enabled() { return enabled; }};
    controllers.set(documentRef, controller);
    return controller;
}
