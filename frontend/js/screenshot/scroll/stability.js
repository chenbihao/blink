//! 长截图画面稳定检测的纯算法层。
//!
//! 探针由 Rust 端降采样为灰度字节。用差值的 65 分位而非全图均值，允许视频、
//! 光标等局部动态区域持续变化；真正滚动通常会让大多数内容像素一起改变。

const DEFAULT_STABLE_QUANTILE = 0.65;
const DEFAULT_STABLE_THRESHOLD = 3;

// M5 优化：复用 differences 缓冲区，避免每帧 new Uint8Array + GC
let _diffBuffer = null;

export function probeMotionScore(previous, current, quantile = DEFAULT_STABLE_QUANTILE) {
  if (!previous || !current || previous.length === 0 || previous.length !== current.length) {
    return Infinity;
  }
  const len = previous.length;
  // M5 优化：复用缓冲区，仅在实际需要扩容时分配
  if (!_diffBuffer || _diffBuffer.length < len) {
    _diffBuffer = new Uint8Array(len);
  }
  const differences = _diffBuffer.subarray(0, len);
  for (let i = 0; i < len; i++) {
    differences[i] = Math.abs(previous[i] - current[i]);
  }
  differences.sort();
  const index = Math.min(
    differences.length - 1,
    Math.max(0, Math.floor((differences.length - 1) * quantile)),
  );
  return differences[index];
}

export function isProbeStable(previous, current, options = {}) {
  const threshold = options.threshold ?? DEFAULT_STABLE_THRESHOLD;
  const quantile = options.quantile ?? DEFAULT_STABLE_QUANTILE;
  const score = probeMotionScore(previous, current, quantile);
  return { stable: score <= threshold, score };
}
