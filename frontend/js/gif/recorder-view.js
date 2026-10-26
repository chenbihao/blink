//! GIF 控制条渲染纯逻辑（0.28.7）——无顶层 DOM，node --test 可直接导入。

/** 有效时长 mm:ss.d 展示（暂停剔除后的后端真值）。 */
export function formatElapsed(ms) {
    const total = Math.max(0, ms);
    const m = Math.floor(total / 60_000);
    const s = Math.floor((total % 60_000) / 1000);
    const d = Math.floor((total % 1000) / 100);
    return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}.${d}`;
}

/** 按后端状态渲染控制条。纯函数（node --test 覆盖）。 */
export function renderState(state, dom) {
    const {countdown, recLine, recDot, recTime, recFrames, pauseBtn} = dom;
    switch (state.phase) {
        case 'countdown': {
            countdown.classList.remove('hidden');
            recLine.classList.add('hidden');
            countdown.textContent = String(Math.max(1, Math.ceil((state.countdownMs ?? 0) / 1000)));
            break;
        }
        case 'recording':
        case 'paused': {
            countdown.classList.add('hidden');
            recLine.classList.remove('hidden');
            recDot.classList.toggle('paused', state.phase === 'paused');
            recTime.textContent = formatElapsed(state.elapsedMs ?? 0);
            recFrames.textContent = `${state.frames ?? 0} 帧`;
            break;
        }
        default: {
            // idle / stopping / done：保持最后画面，窗口即将被后端关闭
            break;
        }
    }
    // 倒计时未开录，暂停无语义——禁用给出明确反馈（recording/paused 恢复）
    if (pauseBtn) {
        pauseBtn.disabled = state.phase === 'countdown';
    }
    // 暂停/继续图标切换（第一个 icon=pause，第二个=play）
    const icons = pauseBtn.querySelectorAll('.icon');
    if (icons.length === 2) {
        const paused = state.phase === 'paused';
        icons[0].classList.toggle('hidden', paused);
        icons[1].classList.toggle('hidden', !paused);
    }
}
