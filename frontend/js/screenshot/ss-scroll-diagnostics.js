//! 长截图开发诊断与显式回放导出。默认关闭，不写日志、不持久化截图像素。

import { SCROLL_DECISION_SCHEMA_VERSION } from './ss-scroll-tracker.js';

const MAX_REPLAY_FRAMES = 256;
const MAX_REPLAY_BYTES = 256 * 1024 * 1024;

export function scrollDiagnosticsEnabled() {
  if (new URLSearchParams(window.location.search).get('scrollDebug') === '1') return true;
  try {
    return window.localStorage.getItem('blink.scrollDebug') === '1';
  } catch {
    return false;
  }
}

function setPanelVisible(visible) {
  document.getElementById('scroll-diagnostics')?.classList.toggle('hidden', !visible);
}

export function resetScrollDiagnostics(session) {
  session.scrollLastDecision = null;
  session.scrollDecisions = [];
  session.scrollReplayFrames = [];
  session.scrollReplayBytes = 0;
  setPanelVisible(scrollDiagnosticsEnabled());
  const text = document.getElementById('scroll-diagnostics-text');
  if (text) text.textContent = '等待首帧';
}

export function recordScrollDiagnostic(session, frame, decision, metadata = {}) {
  session.scrollLastDecision = decision;
  if (!scrollDiagnosticsEnabled()) return;
  session.scrollDecisions.push(decision);
  if (session.scrollDecisions.length > MAX_REPLAY_FRAMES) session.scrollDecisions.shift();
  const frameBytes = frame?.data?.byteLength || 0;
  if (session.scrollReplayFrames.length < MAX_REPLAY_FRAMES
      && session.scrollReplayBytes + frameBytes <= MAX_REPLAY_BYTES) {
    session.scrollReplayFrames.push({
      frame,
      capturedAtMs: Math.round(performance.now()),
      expectedDirection: decision.expectedDirection,
      settle: metadata.settle || null,
      decision,
    });
    session.scrollReplayBytes += frameBytes;
  }
  const text = document.getElementById('scroll-diagnostics-text');
  if (text) {
    const score = decision.bestScore == null ? '—' : decision.bestScore.toFixed(2);
    const confidence = Math.round(decision.confidence * 100);
    text.textContent = `top ${decision.candidateTop ?? '—'} · ${decision.source} · ${decision.reason} · ${confidence}% · score ${score}`;
  }
}

async function imageDataToPngBlob(image) {
  const canvas = document.createElement('canvas');
  canvas.width = image.width;
  canvas.height = image.height;
  canvas.getContext('2d').putImageData(image, 0, 0);
  return new Promise((resolve, reject) => canvas.toBlob(
    (blob) => blob ? resolve(blob) : reject(new Error('PNG 编码失败')),
    'image/png',
  ));
}

async function writeFile(directory, name, data) {
  const handle = await directory.getFileHandle(name, { create: true });
  const writable = await handle.createWritable();
  try {
    await writable.write(data);
  } finally {
    await writable.close();
  }
}

/** 只有开发者主动点击并选择本地目录后，才导出可能含敏感信息的帧。 */
export async function exportScrollReplay(session) {
  if (!scrollDiagnosticsEnabled()) throw new Error('长截图诊断未开启');
  if (!window.showDirectoryPicker) throw new Error('当前 WebView 不支持目录选择');
  if (!session.scrollReplayFrames.length) throw new Error('当前没有可导出的采集帧');
  const selectedDirectory = await window.showDirectoryPicker({ mode: 'readwrite' });
  const directoryName = `blink-scroll-${new Date().toISOString().replaceAll(':', '-').replaceAll('.', '-')}`;
  const directory = await selectedDirectory.getDirectoryHandle(directoryName, { create: true });
  const frames = [];
  for (let index = 0; index < session.scrollReplayFrames.length; index++) {
    const captured = session.scrollReplayFrames[index];
    const file = `frame-${String(index).padStart(4, '0')}.png`;
    await writeFile(directory, file, await imageDataToPngBlob(captured.frame));
    frames.push({
      file,
      capturedAtMs: captured.capturedAtMs,
      expectedDirection: captured.expectedDirection,
      settle: captured.settle,
      expectedDecision: captured.decision,
    });
  }
  const manifest = {
    format: 'blink-scroll-replay',
    version: 1,
    decisionSchemaVersion: SCROLL_DECISION_SCHEMA_VERSION,
    createdAt: new Date().toISOString(),
    frameCount: frames.length,
    frames,
  };
  await writeFile(directory, 'manifest.json', JSON.stringify(manifest, null, 2));
  return frames.length;
}

export function bindScrollDiagnostics(session, showHint) {
  setPanelVisible(scrollDiagnosticsEnabled());
  const button = document.getElementById('scroll-diagnostics-export');
  if (!button || button.dataset.bound === '1') return;
  button.dataset.bound = '1';
  button.addEventListener('click', async () => {
    try {
      const count = await exportScrollReplay(session);
      showHint?.(`已导出 ${count} 帧长截图回放`);
    } catch (error) {
      if (error?.name !== 'AbortError') showHint?.(error?.message || '回放导出失败');
    }
  });
}
