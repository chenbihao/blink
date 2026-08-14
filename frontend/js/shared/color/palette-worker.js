//! 0.20.7：配色分析 Worker。
//!
//! Worker 协议携带 version + epoch，旧 epoch 的结果自动丢弃。
//! 异常时只对同一轮整图直方图同步降级（回主线程分析）。
//!
//! 消息协议：
//! 主线程 → Worker: { type: 'analyze-histogram', version, epoch, histogram, width, height }
//! Worker → 主线程: { type: 'result', version, epoch, result }
//! Worker → 主线程: { type: 'error', version, epoch, message }

import {
  analyzePaletteHistogram,
  PALETTE_ALGORITHM_V1,
} from './palette-core.js';

/**
 * Worker 消息处理。
 * @param {MessageEvent} e
 */
self.onmessage = function (e) {
  const msg = e.data;
  if (!msg || msg.type !== 'analyze-histogram') return;
  const { version, epoch, width, height } = msg;

  // 版本校验
  if (version !== PALETTE_ALGORITHM_V1.WORKER_VERSION) {
    self.postMessage({
      type: 'error',
      version,
      epoch,
      message: `worker version mismatch: expected ${PALETTE_ALGORITHM_V1.WORKER_VERSION}, got ${version}`,
    });
    return;
  }

  try {
    const result = analyzePaletteHistogram(msg.histogram, width, height);
    self.postMessage({
      type: 'result',
      version,
      epoch,
      result,
    });
  } catch (err) {
    self.postMessage({
      type: 'error',
      version,
      epoch,
      message: err?.message || String(err),
    });
  }
};
