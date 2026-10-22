//! 长截图开发诊断与显式回放导出。默认关闭，不写日志、不持久化截图像素。

import {SCROLL_DECISION_SCHEMA_VERSION} from './tracker.js';
import {screenshotSaveReplayFile} from '../../shared/api.js';
import {ss} from '../ss-state.js';
import {positionScrollDiagnostics} from './preview.js';

const MAX_REPLAY_FRAMES = 256;
const MAX_REPLAY_BYTES = 256 * 1024 * 1024;
const MAX_TIMING_SAMPLES = 1024;
const MAX_TIMELINE_EVENTS = 2048;
const CALIBRATION_SCHEMA_VERSION = 4;
const TIMING_KEYS = [
    'preCaptureMs', 'settleMs', 'captureMs', 'queueMs', 'trackMs', 'commitMs', 'previewMs', 'workerRoundTripMs',
    'totalLatencyMs', 'lastWheelToPreviewMs',
];

export function scrollDiagnosticsEnabled() {
    // URL 覆盖：dev 一次性调试用，优先级最高
    if (new URLSearchParams(window.location.search).get('scrollDebug') === '1') return true;
    // 从 SQLite 配置库缓存读取（index.js loadScreenshot 时异步写入 ss.screenshotConfig）
    return ss.screenshotConfig.scrollDebug === true;
}

/** 分配诊断像素副本前先检查预算；预算满后继续保留元数据。 */
export function canRetainScrollReplayFrame(session, bytes) {
    return bytes > 0 && scrollDiagnosticsEnabled()
        && (session.scrollReplayFrames?.length || 0) < MAX_REPLAY_FRAMES
        && (session.scrollReplayBytes || 0) + bytes <= MAX_REPLAY_BYTES;
}

/// 配置读完后刷新面板可见性（index.js 的 get_config_section .then 回调调用）。
export function refreshDiagnosticsVisibility() {
    setPanelVisible(scrollDiagnosticsEnabled());
}

function setPanelVisible(visible) {
    visible = visible && ss.scrollSession?.scrollCapturePhase === 'capturing';
    document.getElementById('scroll-diagnostics')?.classList.toggle('hidden', !visible);
    if (visible) positionScrollDiagnostics();
}

export function resetScrollDiagnostics(session) {
    session.scrollLastDecision = null;
    session.scrollDecisions = [];
    session.scrollReplayFrames = [];
    session.scrollReplayBytes = 0;
    session.scrollTimingSamples = [];
    session.scrollTimeline = [];
    session.scrollDroppedTimingSamples = 0;
    session.scrollDroppedTimelineEvents = 0;
    session.scrollDiagnosticStartedAt = performance.now();
    setPanelVisible(scrollDiagnosticsEnabled());
    const text = document.getElementById('scroll-diagnostics-text');
    if (text) text.textContent = '等待首帧';
}

/** 只记录调度元数据；不包含窗口标题、输入文本或截图像素。 */
export function recordScrollEvent(session, kind, details = {}) {
    if (!scrollDiagnosticsEnabled()) return;
    session.scrollTimeline ??= [];
    session.scrollTimeline.push({
        atMs: Math.round(performance.now() - (session.scrollDiagnosticStartedAt ?? 0)),
        kind, generation: session.captureGeneration, ...details,
    });
    if (session.scrollTimeline.length > MAX_TIMELINE_EVENTS) {
        session.scrollTimeline.shift();
        session.scrollDroppedTimelineEvents = (session.scrollDroppedTimelineEvents || 0) + 1;
    }
}

export function recordScrollDiagnostic(session, frame, decision, metadata = {}) {
    session.scrollLastDecision = decision;
    if (!scrollDiagnosticsEnabled()) return;
    session.scrollTimingSamples ??= [];
    session.scrollTimingSamples.push({decision, timing: metadata.timing || null,
        tracking: metadata.tracking || null, calibration: decision.calibration || null});
    if (session.scrollTimingSamples.length > MAX_TIMING_SAMPLES) {
        session.scrollTimingSamples.shift();
        session.scrollDroppedTimingSamples = (session.scrollDroppedTimingSamples || 0) + 1;
    }
    recordScrollEvent(session, 'frame-decision', {
        accepted: decision.accepted, reason: decision.reason,
        wheelVersion: metadata.wheelVersion ?? null, finalizing: metadata.finalizing === true,
        timing: metadata.timing || null,
    });
    session.scrollDecisions.push(decision);
    if (session.scrollDecisions.length > MAX_REPLAY_FRAMES) session.scrollDecisions.shift();
    const frameBytes = frame?.data?.byteLength || 0;
    if (frame && canRetainScrollReplayFrame(session, frameBytes)) {
        session.scrollReplayFrames.push({
            frame,
            discontinuity: metadata.discontinuity === true,
            trackingReset: metadata.trackingReset === true,
            capturedAtMs: Math.round(metadata.capturedAt ?? performance.now()),
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
        text.title = text.textContent;
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
        tracking: {transitions: {}, becameLostCount: 0},
        positionedOverlap: {
            checkedCount: 0,
            consistentCount: 0,
            conflictCount: 0,
            insufficientCount: 0,
            insufficientDetailCount: 0,
            unavailableCount: 0,
        },
        confidence: {count: 0, min: null, max: null, average: null},
        timing: Object.fromEntries(TIMING_KEYS.map((key) => [key, {
            count: 0, min: null, max: null, average: null, p50: null, p95: null,
        }])),
    };
    let confidenceTotal = 0;
    const timingTotals = Object.fromEntries(TIMING_KEYS.map((key) => [key, 0]));
    const timingValues = Object.fromEntries(TIMING_KEYS.map((key) => [key, []]));
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
            if (!Number.isFinite(value) || value < 0) continue;
            const stats = summary.timing[key];
            stats.count++;
            stats.min = stats.min == null ? value : Math.min(stats.min, value);
            stats.max = stats.max == null ? value : Math.max(stats.max, value);
            timingTotals[key] += value;
            timingValues[key].push(value);
        }
    }
    if (summary.confidence.count) {
        summary.confidence.average = Math.round(
            confidenceTotal / summary.confidence.count * 1000,
        ) / 1000;
    }
    for (const key of TIMING_KEYS) {
        const stats = summary.timing[key];
        if (stats.count) {
            stats.average = Math.round(timingTotals[key] / stats.count * 10) / 10;
            const values = timingValues[key].sort((a, b) => a - b);
            stats.p50 = values[Math.ceil(values.length * 0.5) - 1];
            stats.p95 = values[Math.ceil(values.length * 0.95) - 1];
        }
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
    // 导出过程可跨窗口关闭/复用；只消费启动时的自有快照，不再追读 session。
    const capturedFrames = [...session.scrollReplayFrames];
    const timingSamples = [...(session.scrollTimingSamples || session.scrollReplayFrames)];
    const timeline = [...(session.scrollTimeline || [])];
    const environment = {
        userAgent: globalThis.navigator?.userAgent || null,
        devicePixelRatio: window.devicePixelRatio || null,
        bandWidth: session.scrollBandW, bandHeight: session.scrollBandH,
        renderScale: ss.canvas?.getBoundingClientRect?.().width > 0
            ? ss.canvas.width / ss.canvas.getBoundingClientRect().width : null,
        capture: session.continuousCapture?.diagnostics() || null,
    };
    const droppedTimingSamples = session.scrollDroppedTimingSamples || 0;
    const droppedTimelineEvents = session.scrollDroppedTimelineEvents || 0;
    const directoryName = `blink-scroll-${new Date().toISOString().replaceAll(':', '-').replaceAll('.', '-')}`;
    const frames = [];
    for (let index = 0; index < capturedFrames.length; index++) {
        const captured = capturedFrames[index];
        const file = `frame-${String(index).padStart(4, '0')}.png`;
        const png = new Uint8Array(await (await imageDataToPngBlob(captured.frame)).arrayBuffer());
        await screenshotSaveReplayFile(directoryName, file, png);
        frames.push({
            file,
            capturedAtMs: captured.capturedAtMs,
            expectedDirection: captured.expectedDirection,
            discontinuity: captured.discontinuity === true,
            trackingReset: captured.trackingReset === true,
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
        environment,
        calibrationSummary: buildCalibrationSummary(capturedFrames),
        timingSummary: buildCalibrationSummary(timingSamples).timing,
        timingSampleCount: timingSamples.length,
        droppedTimingSamples,
        timeline, droppedTimelineEvents,
        frames,
    };
    const manifestBytes = new TextEncoder().encode(JSON.stringify(manifest, null, 2));
    const directory = await screenshotSaveReplayFile(
        directoryName,
        'manifest.json',
        manifestBytes,
    );
    return {count: frames.length, directory};
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
