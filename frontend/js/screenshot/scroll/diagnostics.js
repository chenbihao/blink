//! 长截图开发诊断与显式回放导出。默认关闭，不写日志、不持久化截图像素。

import { SCROLL_DECISION_SCHEMA_VERSION } from './tracker.js';
import { screenshotSaveReplayFile } from '../../shared/api.js';

const MAX_REPLAY_FRAMES = 256;
const MAX_REPLAY_BYTES = 256 * 1024 * 1024;
const CALIBRATION_SCHEMA_VERSION = 3;
const TIMING_KEYS = [
  'settleMs', 'captureMs', 'trackMs', 'commitMs', 'previewMs', 'totalLatencyMs',
];

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
      timing: metadata.timing || null,
      tracking: metadata.tracking || null,
      decision,
      calibration: decision.calibration || null,
    });
    session.scrollReplayBytes += frameBytes;
  }
  const text = document.getElementById('scroll-diagnostics-text');
  if (text) {
    const score = decision.bestScore == null ? '—' : decision.bestScore.toFixed(2);
    const confidence = Math.round(decision.confidence * 100);
    const latency = metadata.timing?.totalLatencyMs;
    const timingText = Number.isFinite(latency) ? ` · ${Math.round(latency)}ms` : '';
    text.textContent = `top ${decision.candidateTop ?? '—'} · ${decision.source} · ${decision.reason} · ${confidence}% · score ${score}${timingText}`;
  }
}

function incrementCounter(target, key) {
  const normalized = key || 'unknown';
  target[normalized] = (target[normalized] || 0) + 1;
}

export function buildCalibrationSummary(frames) {
  const summary = {
    schemaVersion: CALIBRATION_SCHEMA_VERSION,
    frameCount: frames.length,
    acceptedCount: 0,
    rejectedCount: 0,
    reasons: {},
    sources: {},
    tracking: { transitions: {}, becameLostCount: 0 },
    positionedOverlap: {
      checkedCount: 0,
      consistentCount: 0,
      conflictCount: 0,
      insufficientCount: 0,
      insufficientDetailCount: 0,
      unavailableCount: 0,
    },
    confidence: { count: 0, min: null, max: null, average: null },
    timing: Object.fromEntries(TIMING_KEYS.map((key) => [key, {
      count: 0, min: null, max: null, average: null,
    }])),
  };
  let confidenceTotal = 0;
  const timingTotals = Object.fromEntries(TIMING_KEYS.map((key) => [key, 0]));
  for (const captured of frames) {
    const decision = captured.decision || {};
    if (decision.accepted) summary.acceptedCount++;
    else summary.rejectedCount++;
    incrementCounter(summary.reasons, decision.reason);
    incrementCounter(summary.sources, decision.source);
    const before = captured.tracking?.before;
    const after = captured.tracking?.after;
    if (before && after) incrementCounter(summary.tracking.transitions, `${before}->${after}`);
    if (captured.tracking?.becameLost) summary.tracking.becameLostCount++;
    if (Number.isFinite(decision.confidence)) {
      summary.confidence.count++;
      confidenceTotal += decision.confidence;
      summary.confidence.min = summary.confidence.min == null
        ? decision.confidence : Math.min(summary.confidence.min, decision.confidence);
      summary.confidence.max = summary.confidence.max == null
        ? decision.confidence : Math.max(summary.confidence.max, decision.confidence);
    }
    const status = captured.calibration?.positionedOverlap?.status;
    const statusCounter = status === 'insufficient-detail'
      ? 'insufficientDetailCount' : `${status}Count`;
    if (status && statusCounter in summary.positionedOverlap) {
      summary.positionedOverlap[statusCounter]++;
    }
    if (status === 'consistent' || status === 'conflict') {
      summary.positionedOverlap.checkedCount++;
    }
    for (const key of TIMING_KEYS) {
      const value = captured.timing?.[key];
      if (!Number.isFinite(value)) continue;
      const stats = summary.timing[key];
      stats.count++;
      stats.min = stats.min == null ? value : Math.min(stats.min, value);
      stats.max = stats.max == null ? value : Math.max(stats.max, value);
      timingTotals[key] += value;
    }
  }
  if (summary.confidence.count) {
    summary.confidence.average = Math.round(
      confidenceTotal / summary.confidence.count * 1000,
    ) / 1000;
  }
  for (const key of TIMING_KEYS) {
    const stats = summary.timing[key];
    if (stats.count) stats.average = Math.round(timingTotals[key] / stats.count * 10) / 10;
  }
  return summary;
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

/** 只有开发者主动点击后，才把可能含敏感信息的帧写入 Blink 本地日志目录。 */
export async function exportScrollReplay(session) {
  if (!scrollDiagnosticsEnabled()) throw new Error('长截图诊断未开启');
  if (!session.scrollReplayFrames.length) throw new Error('当前没有可导出的采集帧');
  const directoryName = `blink-scroll-${new Date().toISOString().replaceAll(':', '-').replaceAll('.', '-')}`;
  const frames = [];
  for (let index = 0; index < session.scrollReplayFrames.length; index++) {
    const captured = session.scrollReplayFrames[index];
    const file = `frame-${String(index).padStart(4, '0')}.png`;
    const png = new Uint8Array(await (await imageDataToPngBlob(captured.frame)).arrayBuffer());
    await screenshotSaveReplayFile(directoryName, file, png);
    frames.push({
      file,
      capturedAtMs: captured.capturedAtMs,
      expectedDirection: captured.expectedDirection,
      settle: captured.settle,
      timing: captured.timing || null,
      tracking: captured.tracking || null,
      expectedDecision: captured.decision,
      calibration: captured.calibration || null,
    });
  }
  const manifest = {
    format: 'blink-scroll-replay',
    version: 1,
    decisionSchemaVersion: SCROLL_DECISION_SCHEMA_VERSION,
    createdAt: new Date().toISOString(),
    frameCount: frames.length,
    calibrationSummary: buildCalibrationSummary(session.scrollReplayFrames),
    frames,
  };
  const manifestBytes = new TextEncoder().encode(JSON.stringify(manifest, null, 2));
  const directory = await screenshotSaveReplayFile(
    directoryName,
    'manifest.json',
    manifestBytes,
  );
  return { count: frames.length, directory };
}

export function bindScrollDiagnostics(session, showHint) {
  setPanelVisible(scrollDiagnosticsEnabled());
  const button = document.getElementById('scroll-diagnostics-export');
  if (!button || button.dataset.bound === '1') return;
  button.dataset.bound = '1';
  button.addEventListener('click', async () => {
    try {
      const result = await exportScrollReplay(session);
      showHint?.(`已导出 ${result.count} 帧到日志目录 scroll-replays`);
    } catch (error) {
      if (error?.name !== 'AbortError') showHint?.(error?.message || '回放导出失败');
    }
  });
}
