//! 截图预选框视觉层。
//!
//! 窗口与控件命中共用同一个 DOM 元素，因此跨层级切换时可以继承上一帧的
//! 几何位置并连续形变；kind 只负责语义配色，不再用两个元素交叉淡入淡出。

const HIDE_DELAY_MS = 120;

let hintEl = null;
let hintOwner = null;
let hintPresented = false;
let hintHideTimer = 0;

function ensureHint() {
  if (hintEl) return hintEl;
  hintEl = document.createElement('div');
  hintEl.id = 'preselection-hint';
  hintEl.className = 'preselection-hint preselection-hint--window';
  document.body.appendChild(hintEl);
  return hintEl;
}

/**
 * 显示或移动统一预选框。
 *
 * 真正隐藏前收到新的 show 时会取消淡出，并从当前屏幕位置继续形变，避免
 * window -> control -> window 在同一 mousemove 内发生瞬移。
 */
export function showPreselectionHint(rect, kind, title = '') {
  const el = ensureHint();
  if (hintHideTimer) {
    clearTimeout(hintHideTimer);
    hintHideTimer = 0;
  }

  const wasHidden = !hintPresented;
  if (wasHidden) el.style.transition = 'none';

  el.classList.toggle('preselection-hint--window', kind === 'window');
  el.classList.toggle('preselection-hint--control', kind === 'control');
  el.style.left = `${rect.x}px`;
  el.style.top = `${rect.y}px`;
  el.style.width = `${rect.w}px`;
  el.style.height = `${rect.h}px`;
  el.style.visibility = 'visible';
  el.style.opacity = '1';
  el.title = title;
  hintOwner = kind;

  if (wasHidden) {
    // 首次出现只淡入，不从默认 (0,0) 滑入；后续层级切换沿用同一几何轨迹。
    el.offsetHeight;
    el.style.transition = '';
    hintPresented = true;
  }
}

/** 仅当调用方仍拥有预选框时淡出，防止旧层级隐藏掉刚切换的新层级。 */
export function hidePreselectionHint(owner) {
  if (!hintEl || !hintPresented || (owner && hintOwner !== owner)) return;

  hintEl.style.opacity = '0';
  if (hintHideTimer) clearTimeout(hintHideTimer);
  hintHideTimer = setTimeout(() => {
    hintHideTimer = 0;
    if (!hintEl || hintEl.style.opacity !== '0') return;
    hintEl.style.visibility = 'hidden';
    hintPresented = false;
    hintOwner = null;
  }, HIDE_DELAY_MS);
}

/** overlay 关闭时立即复位，不播放退场动画。 */
export function resetPreselectionHint() {
  if (hintHideTimer) {
    clearTimeout(hintHideTimer);
    hintHideTimer = 0;
  }
  hintOwner = null;
  hintPresented = false;
  if (!hintEl) return;
  hintEl.style.transition = 'none';
  hintEl.style.opacity = '0';
  hintEl.style.visibility = 'hidden';
  hintEl.style.transition = '';
}
