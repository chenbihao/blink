//! 长截图纵向拼接的纯算法层。
//!
//! 不依赖 DOM / Tauri，便于用合成 ImageData 做独立验证。

// 至少保留约 22% 视口重叠。低于该值时少量横线/吸顶栏就可能主导 SAD，
// 应转入关键帧恢复，而不是把九牛一毛的重叠当成可靠局部位移。
const DEFAULT_MAX_SHIFT_RATIO = 0.78;
const DEFAULT_MATCH_THRESHOLD = 22;
const DEFAULT_UNCHANGED_THRESHOLD = 2.5;
const DEFAULT_PROBE_WIDTH = 48;
const DEFAULT_PROBE_HEIGHT = 64;
const DEFAULT_POSITIONED_OVERLAP_THRESHOLD = 8;
// 真实录制中 198 / 283px 的常见手动步进仍保留约 30% 重叠；20% 足以让细节
// tile 复核工作，同时会拒绝只剩一两行、无法可靠辨别重复纹理的极端跳转。
const DEFAULT_MIN_POSITIONED_OVERLAP_RATIO = 0.2;
const DEFAULT_DETAIL_TILE_SIZE = 12;
const DEFAULT_DETAIL_LUMA_RANGE = 10;
const DEFAULT_DETAIL_TILE_MISMATCH_THRESHOLD = 8;
const DEFAULT_DETAIL_MISMATCH_RATIO = 0.32;

function sampledSad(prevFrame, currFrame, shift, sampleRows = 24, sampleCols = 28) {
  const w = Math.min(prevFrame.width, currFrame.width);
  const h = Math.min(prevFrame.height, currFrame.height);
  const overlap = h - shift;
  if (w <= 0 || overlap <= 8) return Infinity;

  const prev = prevFrame.data;
  const curr = currFrame.data;
  const prevStride = prevFrame.width * 4;
  const currStride = currFrame.width * 4;
  // 避开最上方常见的吸顶栏，并覆盖中下部内容。
  const marginY = Math.min(
    Math.floor(overlap / 4),
    Math.max(16, Math.floor(h * 0.18)),
  );
  const usableH = Math.max(1, overlap - marginY * 2);
  let sad = 0;
  let samples = 0;

  for (let sy = 0; sy < sampleRows; sy++) {
    const y = marginY + Math.min(usableH - 1, Math.floor((sy + 0.5) * usableH / sampleRows));
    const prevY = y + shift;
    for (let sx = 0; sx < sampleCols; sx++) {
      const x = Math.min(w - 1, Math.floor((sx + 0.5) * w / sampleCols));
      const pi = prevY * prevStride + x * 4;
      const ci = y * currStride + x * 4;
      sad += Math.abs(prev[pi] - curr[ci]);
      sad += Math.abs(prev[pi + 1] - curr[ci + 1]);
      sad += Math.abs(prev[pi + 2] - curr[ci + 2]);
      samples += 3;
    }
  }
  return samples > 0 ? sad / samples : Infinity;
}

/**
 * 估算相邻两帧的纵向位移。
 * shift > 0 表示视口向下移动，shift < 0 表示视口向上移动。
 * expectedDirection 可用滚轮意图限定搜索方向，避免重复纹理在反向产生伪匹配。
 */
export function estimateVerticalShift(prevFrame, currFrame, options = {}) {
  if (!prevFrame || !currFrame ||
      prevFrame.width !== currFrame.width || prevFrame.height !== currFrame.height) {
    return { status: 'no-match', shift: 0, score: Infinity };
  }

  const h = prevFrame.height;
  const sampleRows = options.sampleRows ?? 24;
  const sampleCols = options.sampleCols ?? 28;
  const unchangedThreshold = options.unchangedThreshold ?? DEFAULT_UNCHANGED_THRESHOLD;
  const sameScore = sampledSad(prevFrame, currFrame, 0, sampleRows, sampleCols);
  if (sameScore <= unchangedThreshold) {
    return { status: 'unchanged', shift: 0, score: sameScore };
  }

  const maxShift = Math.min(
    h - 9,
    options.maxShift ?? Math.max(1, Math.floor(h * DEFAULT_MAX_SHIFT_RATIO)),
  );
  const expectedDirection = Math.sign(options.expectedDirection ?? 0);
  let bestShift = 0;
  let bestScore = Infinity;
  let bestRank = Infinity;
  const candidates = [];
  // 默认把滚轮方向作为重复纹理中的优先级；采集链路可启用 strictDirection，
  // 防止“向上滚动”被相似内容解释成向下追加。
  const directions = expectedDirection === 0
    ? [1, -1]
    : (options.strictDirection ? [expectedDirection] : [expectedDirection, -expectedDirection]);
  for (const direction of directions) {
    for (let distance = 1; distance <= maxShift; distance++) {
      const score = direction > 0
        ? sampledSad(prevFrame, currFrame, distance, sampleRows, sampleCols)
        : sampledSad(currFrame, prevFrame, distance, sampleRows, sampleCols);
      const rank = score * (expectedDirection !== 0 && direction !== expectedDirection ? 1.08 : 1);
      if (options.rejectAmbiguous) candidates.push({ shift: direction * distance, score });
      if (rank < bestRank) {
        bestRank = rank;
        bestScore = score;
        bestShift = direction * distance;
      }
    }
  }

  const matchThreshold = options.matchThreshold ?? DEFAULT_MATCH_THRESHOLD;
  const improvementRatio = options.improvementRatio ?? 0.8;
  const rankedCandidates = [...candidates].sort((a, b) => a.score - b.score);
  const second = rankedCandidates.find((candidate) => candidate.shift !== bestShift);
  // 必须既达到绝对阈值，又明显优于“没滚动”的对齐，避免动画/光标闪烁误判为滚动。
  if (bestScore > matchThreshold || bestScore >= sameScore * improvementRatio) {
    return {
      status: 'no-match',
      reason: bestScore > matchThreshold ? 'low-confidence' : 'no-overlap',
      shift: 0,
      candidateShift: bestShift,
      score: bestScore,
      secondScore: second?.score,
      sameScore,
    };
  }
  if (options.rejectAmbiguous) {
    const ambiguityDistance = options.ambiguityDistance ?? Math.max(12, h * 0.12);
    const ambiguityRatio = options.ambiguityRatio ?? 1.12;
    const ambiguityDelta = options.ambiguityDelta ?? 1.5;
    const rival = candidates.find((candidate) => (
      Math.abs(candidate.shift - bestShift) >= ambiguityDistance
      && candidate.score <= bestScore * ambiguityRatio + ambiguityDelta
    ));
    if (rival) {
      return {
        status: 'no-match',
        reason: 'ambiguous',
        shift: 0,
        candidateShift: bestShift,
        score: bestScore,
        secondScore: rival.score,
        sameScore,
        rivalShift: rival.shift,
        rivalScore: rival.score,
      };
    }
  }
  return {
    status: 'matched', shift: bestShift, score: bestScore, secondScore: second?.score, sameScore,
  };
}

/**
 * 把完整帧压成小型 RGBA 灰度指纹。指纹仍沿用 ImageData 形状，因此可直接复用
 * 纵向位移估算器；只用于候选召回，最终位置必须回到全分辨率帧确认。
 */
export function createGrayFingerprint(frame, maxWidth = DEFAULT_PROBE_WIDTH,
  maxHeight = DEFAULT_PROBE_HEIGHT) {
  if (!frame?.data || frame.width <= 0 || frame.height <= 0) return null;
  const scale = Math.min(1, maxWidth / frame.width, maxHeight / frame.height);
  const width = Math.max(1, Math.round(frame.width * scale));
  const height = Math.max(1, Math.round(frame.height * scale));
  const result = new ImageData(width, height);
  for (let y = 0; y < height; y++) {
    const sourceY = Math.min(frame.height - 1, Math.floor((y + 0.5) * frame.height / height));
    for (let x = 0; x < width; x++) {
      const sourceX = Math.min(frame.width - 1, Math.floor((x + 0.5) * frame.width / width));
      const sourceIndex = (sourceY * frame.width + sourceX) * 4;
      const gray = Math.round(
        frame.data[sourceIndex] * 0.299
        + frame.data[sourceIndex + 1] * 0.587
        + frame.data[sourceIndex + 2] * 0.114,
      );
      const targetIndex = (y * width + x) * 4;
      result.data[targetIndex] = gray;
      result.data[targetIndex + 1] = gray;
      result.data[targetIndex + 2] = gray;
      result.data[targetIndex + 3] = 255;
    }
  }
  return result;
}

/**
 * 关键帧的不可变精配参考：仅横向降采样，纵向保留逐像素分辨率。
 * 相比保存完整 RGBA 帧显著省内存，同时不会把纵向位移量化到十几像素。
 */
export function createVerticalReference(frame, maxWidth = 96) {
  if (!frame?.data || frame.width <= 0 || frame.height <= 0) return null;
  const width = Math.max(1, Math.min(frame.width, Math.floor(maxWidth)));
  const result = new ImageData(width, frame.height);
  for (let y = 0; y < frame.height; y++) {
    for (let x = 0; x < width; x++) {
      const sourceX = Math.min(frame.width - 1, Math.floor((x + 0.5) * frame.width / width));
      const sourceIndex = (y * frame.width + sourceX) * 4;
      const gray = Math.round(
        frame.data[sourceIndex] * 0.299
        + frame.data[sourceIndex + 1] * 0.587
        + frame.data[sourceIndex + 2] * 0.114,
      );
      const targetIndex = (y * width + x) * 4;
      result.data[targetIndex] = gray;
      result.data[targetIndex + 1] = gray;
      result.data[targetIndex + 2] = gray;
      result.data[targetIndex + 3] = 255;
    }
  }
  return result;
}

/** 从非重叠定位片段中，只重建指定视口；缺任意一行就拒绝返回半成品。 */
export function extractPositionedViewport(frames, top, height) {
  if (!frames?.length || !Number.isFinite(top) || height <= 0) return null;
  const first = frames.find((frame) => frame?.image);
  if (!first) return null;
  const width = first.image.width;
  const result = new ImageData(width, height);
  const filledRows = new Uint8Array(height);
  const rowBytes = width * 4;
  for (const frame of frames) {
    if (!frame?.image || frame.image.width !== width) continue;
    const overlapTop = Math.max(top, frame.top);
    const overlapBottom = Math.min(top + height, frame.top + frame.image.height);
    for (let documentY = overlapTop; documentY < overlapBottom; documentY++) {
      const targetY = Math.round(documentY - top);
      const sourceY = Math.round(documentY - frame.top);
      if (targetY < 0 || targetY >= height || sourceY < 0
          || sourceY >= frame.image.height || filledRows[targetY]) continue;
      const sourceStart = sourceY * rowBytes;
      result.data.set(
        frame.image.data.subarray(sourceStart, sourceStart + rowBytes),
        targetY * rowBytes,
      );
      filledRows[targetY] = 1;
    }
  }
  return filledRows.every((filled) => filled === 1) ? result : null;
}

function sampledPositionedOverlapSad(frames, currFrame, candidateTop,
  overlapTop, overlapRows, sampleRows = 24, sampleCols = 28) {
  const ordered = frames.filter((item) => item?.image).sort((a, b) => a.top - b.top);
  if (!ordered.length || overlapRows <= 8) return Infinity;
  const width = Math.min(currFrame.width, ordered[0].image.width);
  if (width <= 0) return Infinity;
  const marginY = Math.min(
    Math.floor(overlapRows / 4),
    Math.max(16, Math.floor(overlapRows * 0.18)),
  );
  const usableHeight = Math.max(1, overlapRows - marginY * 2);
  let total = 0;
  let samples = 0;
  let segmentIndex = 0;
  for (let sy = 0; sy < sampleRows; sy++) {
    const offsetY = marginY + Math.min(
      usableHeight - 1,
      Math.floor((sy + 0.5) * usableHeight / sampleRows),
    );
    const documentY = overlapTop + offsetY;
    while (segmentIndex < ordered.length
        && documentY >= ordered[segmentIndex].top + ordered[segmentIndex].image.height) {
      segmentIndex++;
    }
    const confirmed = ordered[segmentIndex];
    const confirmedY = Math.floor(documentY - (confirmed?.top ?? 0));
    const capturedY = Math.floor(documentY - candidateTop);
    if (!confirmed || documentY < confirmed.top
        || confirmedY < 0 || confirmedY >= confirmed.image.height
        || capturedY < 0 || capturedY >= currFrame.height) return Infinity;
    for (let sx = 0; sx < sampleCols; sx++) {
      const x = Math.min(width - 1, Math.floor((sx + 0.5) * width / sampleCols));
      const confirmedIndex = (confirmedY * confirmed.image.width + x) * 4;
      const capturedIndex = (capturedY * currFrame.width + x) * 4;
      total += Math.abs(confirmed.image.data[confirmedIndex] - currFrame.data[capturedIndex]);
      total += Math.abs(confirmed.image.data[confirmedIndex + 1] - currFrame.data[capturedIndex + 1]);
      total += Math.abs(confirmed.image.data[confirmedIndex + 2] - currFrame.data[capturedIndex + 2]);
      samples += 3;
    }
  }
  return samples ? total / samples : Infinity;
}

function measureDetailedPositionedOverlap(frames, currFrame, candidateTop,
  overlapTop, overlapRows, options = {}) {
  const ordered = frames.filter((item) => item?.image).sort((a, b) => a.top - b.top);
  if (!ordered.length || overlapRows <= 0) return null;
  const width = Math.min(currFrame.width, ordered[0].image.width);
  const tileSize = options.tileSize ?? DEFAULT_DETAIL_TILE_SIZE;
  const detailLumaRange = options.detailLumaRange ?? DEFAULT_DETAIL_LUMA_RANGE;
  const tileMismatchThreshold = options.tileMismatchThreshold
    ?? DEFAULT_DETAIL_TILE_MISMATCH_THRESHOLD;
  const rowSources = [];
  let sourceIndex = 0;
  for (let offsetY = 0; offsetY < overlapRows; offsetY++) {
    const documentY = overlapTop + offsetY;
    while (sourceIndex < ordered.length
        && documentY >= ordered[sourceIndex].top + ordered[sourceIndex].image.height) {
      sourceIndex++;
    }
    const source = ordered[sourceIndex];
    const sourceY = Math.floor(documentY - (source?.top ?? 0));
    if (!source || documentY < source.top || sourceY < 0 || sourceY >= source.image.height) {
      return null;
    }
    rowSources.push({ image: source.image, sourceY });
  }
  let totalTileCount = 0;
  let detailTileCount = 0;
  let mismatchedTileCount = 0;
  let detailScoreTotal = 0;
  for (let tileY = 0; tileY < overlapRows; tileY += tileSize) {
    const tileHeight = Math.min(tileSize, overlapRows - tileY);
    for (let tileX = 0; tileX < width; tileX += tileSize) {
      const tileWidth = Math.min(tileSize, width - tileX);
      let confirmedMin = 255;
      let confirmedMax = 0;
      let capturedMin = 255;
      let capturedMax = 0;
      let sad = 0;
      let samples = 0;
      for (let dy = 0; dy < tileHeight; dy++) {
        const documentY = overlapTop + tileY + dy;
        const confirmed = rowSources[tileY + dy];
        const capturedY = Math.floor(documentY - candidateTop);
        if (!confirmed || capturedY < 0 || capturedY >= currFrame.height) return null;
        for (let dx = 0; dx < tileWidth; dx++) {
          const x = tileX + dx;
          const confirmedIndex = (confirmed.sourceY * confirmed.image.width + x) * 4;
          const capturedIndex = (capturedY * currFrame.width + x) * 4;
          const confirmedLuma = confirmed.image.data[confirmedIndex] * 0.299
            + confirmed.image.data[confirmedIndex + 1] * 0.587
            + confirmed.image.data[confirmedIndex + 2] * 0.114;
          const capturedLuma = currFrame.data[capturedIndex] * 0.299
            + currFrame.data[capturedIndex + 1] * 0.587
            + currFrame.data[capturedIndex + 2] * 0.114;
          confirmedMin = Math.min(confirmedMin, confirmedLuma);
          confirmedMax = Math.max(confirmedMax, confirmedLuma);
          capturedMin = Math.min(capturedMin, capturedLuma);
          capturedMax = Math.max(capturedMax, capturedLuma);
          sad += Math.abs(confirmed.image.data[confirmedIndex] - currFrame.data[capturedIndex]);
          sad += Math.abs(
            confirmed.image.data[confirmedIndex + 1] - currFrame.data[capturedIndex + 1],
          );
          sad += Math.abs(
            confirmed.image.data[confirmedIndex + 2] - currFrame.data[capturedIndex + 2],
          );
          samples += 3;
        }
      }
      totalTileCount++;
      const informative = Math.max(
        confirmedMax - confirmedMin,
        capturedMax - capturedMin,
      ) >= detailLumaRange;
      if (!informative || samples === 0) continue;
      const tileScore = sad / samples;
      detailTileCount++;
      detailScoreTotal += tileScore;
      if (tileScore > tileMismatchThreshold) mismatchedTileCount++;
    }
  }
  return {
    totalTileCount,
    detailTileCount,
    detailCoverage: totalTileCount
      ? Math.round(detailTileCount / totalTileCount * 1000) / 1000 : 0,
    detailScore: detailTileCount
      ? Math.round(detailScoreTotal / detailTileCount * 1000) / 1000 : null,
    mismatchedTileCount,
    mismatchRatio: detailTileCount
      ? Math.round(mismatchedTileCount / detailTileCount * 1000) / 1000 : null,
    tileSize,
    detailLumaRange,
    tileMismatchThreshold,
  };
}

/**
 * 用已确认的长图像素复核一个候选绝对位置。
 *
 * 相邻帧匹配只能证明“两张截图的某些纹理相似”；候选与已确认范围有足够重叠时，
 * 这里再验证“这些像素在同一文档坐标上也一致”。后者冲突时不得提交候选。
 */
export function validatePositionedOverlap(frames, currFrame, candidateTop, options = {}) {
  const bounds = positionedFrameBounds(frames);
  if (!bounds || !currFrame?.data || !Number.isFinite(candidateTop)) {
    return { status: 'unavailable', score: null, threshold: null, overlapRows: 0, overlapRatio: 0 };
  }
  const overlapTop = Math.max(bounds.top, candidateTop);
  const overlapBottom = Math.min(bounds.bottom, candidateTop + currFrame.height);
  const overlapRows = Math.max(0, Math.round(overlapBottom - overlapTop));
  const overlapRatio = overlapRows / Math.max(1, currFrame.height);
  const minOverlapRatio = options.minOverlapRatio ?? DEFAULT_MIN_POSITIONED_OVERLAP_RATIO;
  const threshold = options.threshold ?? DEFAULT_POSITIONED_OVERLAP_THRESHOLD;
  if (overlapRows < 9 || overlapRatio < minOverlapRatio) {
    return {
      status: 'insufficient', score: null, threshold, overlapRows,
      overlapRatio: Math.round(overlapRatio * 1000) / 1000,
    };
  }
  const score = sampledPositionedOverlapSad(
    frames,
    currFrame,
    candidateTop,
    overlapTop,
    overlapRows,
  );
  if (!Number.isFinite(score)) {
    return {
      status: 'unavailable', score: null, threshold, overlapRows,
      overlapRatio: Math.round(overlapRatio * 1000) / 1000,
    };
  }
  const detail = measureDetailedPositionedOverlap(
    frames,
    currFrame,
    candidateTop,
    overlapTop,
    overlapRows,
    options,
  );
  if (!detail) {
    return {
      status: 'unavailable', score: null, threshold, overlapRows,
      overlapRatio: Math.round(overlapRatio * 1000) / 1000,
    };
  }
  const minimumDetailTiles = options.minimumDetailTiles
    ?? Math.max(8, Math.ceil(detail.totalTileCount * 0.02));
  const detailMismatchRatio = options.detailMismatchRatio ?? DEFAULT_DETAIL_MISMATCH_RATIO;
  const enoughDetail = detail.detailTileCount >= minimumDetailTiles;
  const detailConflict = enoughDetail
    && detail.mismatchRatio > detailMismatchRatio;
  return {
    status: !enoughDetail
      ? 'insufficient-detail'
      : (score <= threshold && !detailConflict ? 'consistent' : 'conflict'),
    score: Number.isFinite(score) ? score : null,
    threshold,
    overlapRows,
    overlapRatio: Math.round(overlapRatio * 1000) / 1000,
    ...detail,
    minimumDetailTiles,
    detailMismatchRatio,
  };
}

/**
 * 直接从已提交的非重叠片段采样视口指纹，避免为了粗召回反复重建整张 RGBA 视口。
 * 任一采样行缺失时拒绝返回，防止空洞被误当作黑色纹理参与匹配。
 */
function createPositionedProbeFromOrdered(ordered, top, height, maxWidth, maxHeight) {
  if (!ordered?.length || !Number.isFinite(top) || height <= 0) return null;
  const first = ordered[0];
  if (!first) return null;
  const sourceWidth = first.image.width;
  const scale = Math.min(1, maxWidth / sourceWidth, maxHeight / height);
  const width = Math.max(1, Math.round(sourceWidth * scale));
  const probeHeight = Math.max(1, Math.round(height * scale));
  const result = new ImageData(width, probeHeight);
  let segmentIndex = 0;
  for (let y = 0; y < probeHeight; y++) {
    const documentY = top + Math.min(height - 1, Math.floor((y + 0.5) * height / probeHeight));
    while (segmentIndex < ordered.length
        && documentY >= ordered[segmentIndex].top + ordered[segmentIndex].image.height) {
      segmentIndex++;
    }
    const resolved = ordered[segmentIndex];
    if (!resolved || documentY < resolved.top
        || documentY >= resolved.top + resolved.image.height
        || resolved.image.width !== sourceWidth) return null;
    const sourceY = Math.floor(documentY - resolved.top);
    for (let x = 0; x < width; x++) {
      const sourceX = Math.min(sourceWidth - 1, Math.floor((x + 0.5) * sourceWidth / width));
      const sourceIndex = (sourceY * sourceWidth + sourceX) * 4;
      const gray = Math.round(
        resolved.image.data[sourceIndex] * 0.299
        + resolved.image.data[sourceIndex + 1] * 0.587
        + resolved.image.data[sourceIndex + 2] * 0.114,
      );
      const targetIndex = (y * width + x) * 4;
      result.data[targetIndex] = gray;
      result.data[targetIndex + 1] = gray;
      result.data[targetIndex + 2] = gray;
      result.data[targetIndex + 3] = 255;
    }
  }
  return result;
}

export function createPositionedProbe(frames, top, height,
  maxWidth = DEFAULT_PROBE_WIDTH, maxHeight = DEFAULT_PROBE_HEIGHT) {
  const ordered = frames?.filter((item) => item?.image).sort((a, b) => a.top - b.top);
  return createPositionedProbeFromOrdered(ordered, top, height, maxWidth, maxHeight);
}

/**
 * 从已通过全分辨率确认的候选中选唯一位置。相距很远的两个候选若分数接近，
 * 说明页面存在重复纹理，此时宁可暂停也不猜测。
 */
export function selectRelocalizationCandidate(candidates, viewportHeight, options = {}) {
  if (!candidates?.length) return null;
  const positionTolerance = options.positionTolerance ?? 3;
  const ambiguityDistance = options.ambiguityDistance ?? Math.max(12, viewportHeight * 0.12);
  const ambiguityRatio = options.ambiguityRatio ?? 1.12;
  const ambiguityDelta = options.ambiguityDelta ?? 1.5;
  const deduplicated = [];
  for (const candidate of [...candidates].sort((a, b) => a.score - b.score)) {
    const duplicate = deduplicated.find((item) => Math.abs(item.top - candidate.top) <= positionTolerance);
    if (!duplicate) deduplicated.push(candidate);
  }
  const best = deduplicated[0];
  const rival = deduplicated.find((candidate) => (
    Math.abs(candidate.top - best.top) >= ambiguityDistance
    && candidate.score <= best.score * ambiguityRatio + ambiguityDelta
  ));
  return rival ? null : best;
}

/**
 * 相邻帧失配后的方向约束重定位。先用灰度关键帧召回少量候选，再从已捕获片段
 * 重建对应全分辨率视口做二次确认；不会在无重叠的新区域凭空推断位置。
 */
export function relocalizeFromKeyframes(frames, keyframes, currFrame, currentTop,
  expectedDirection = 0, options = {}) {
  if (!currFrame || !keyframes?.length) return null;
  const bounds = positionedFrameBounds(frames);
  const currentProbe = createGrayFingerprint(currFrame);
  const currentReference = createVerticalReference(currFrame);
  if (!bounds || !currentProbe || !currentReference) return null;
  // lost 后 currentTop 已是陈旧坐标，本次 wheel 方向仅描述未知物理位置上的运动，
  // 不能再据此判断候选位于 currentTop 哪一侧。
  const direction = options.trackingLost ? 0 : Math.sign(expectedDirection);
  const directionTolerance = options.directionTolerance ?? 3;
  const coarseDirectionTolerance = options.coarseDirectionTolerance
    ?? currFrame.height * 0.08;
  const maxFullCandidates = options.maxFullCandidates ?? 8;
  const nearbyCount = options.nearbyCount ?? 5;
  const ordered = [...keyframes].sort(
    (a, b) => Math.abs(a.top - currentTop) - Math.abs(b.top - currentTop),
  );

  const search = (anchors) => {
    const coarse = [];
    for (const anchor of anchors) {
      if (!anchor?.probe) continue;
      const probeMatch = estimateVerticalShift(anchor.probe, currentProbe, {
        expectedDirection: direction,
        maxShift: currentProbe.height - 9,
        matchThreshold: options.probeMatchThreshold ?? 26,
        sampleRows: options.probeSampleRows ?? 10,
        sampleCols: options.probeSampleCols ?? 12,
      });
      if (probeMatch.status !== 'matched' && probeMatch.status !== 'unchanged') continue;
      const scaledShift = probeMatch.shift * currFrame.height / currentProbe.height;
      const coarseTop = anchor.top + scaledShift;
      if (direction > 0 && coarseTop < currentTop - coarseDirectionTolerance) continue;
      if (direction < 0 && coarseTop > currentTop + coarseDirectionTolerance) continue;
      if (coarseTop + currFrame.height <= bounds.top || coarseTop >= bounds.bottom) continue;
      coarse.push({ anchor, coarseTop, score: probeMatch.score });
    }

    const coarsePositionTolerance = options.coarsePositionTolerance
      ?? Math.max(4, currFrame.height / currentProbe.height * 2);
    const distinctCoarse = [];
    for (const candidate of coarse.sort((a, b) => a.score - b.score)) {
      if (!distinctCoarse.some(
        (item) => Math.abs(item.coarseTop - candidate.coarseTop) <= coarsePositionTolerance,
      )) distinctCoarse.push(candidate);
    }

    const confirmed = [];
    for (const candidate of distinctCoarse.slice(0, maxFullCandidates)) {
      // reference 与 probe 必须来自同一张不可变关键帧。旧数据/纯算法调用没有
      // reference 时才回退到已提交片段重建，避免动态内容造成粗配成功、精配失败。
      const reference = candidate.anchor.reference
        || extractPositionedViewport(frames, candidate.anchor.top, currFrame.height);
      if (!reference) continue;
      const target = candidate.anchor.reference ? currentReference : currFrame;
      const match = estimateVerticalShift(reference, target, {
        expectedDirection: direction,
        maxShift: Math.max(1, Math.floor(currFrame.height * DEFAULT_MAX_SHIFT_RATIO)),
        matchThreshold: options.referenceMatchThreshold ?? 18,
        improvementRatio: options.referenceImprovementRatio ?? 0.72,
        strictDirection: direction !== 0,
        rejectAmbiguous: true,
      });
      if (match.status !== 'matched' && match.status !== 'unchanged') continue;
      const top = candidate.anchor.top + match.shift;
      if (direction > 0 && top < currentTop - directionTolerance) continue;
      if (direction < 0 && top > currentTop + directionTolerance) continue;
      if (top + currFrame.height <= bounds.top || top >= bounds.bottom) continue;
      confirmed.push({ top: Math.round(top), score: match.score, match, anchorTop: candidate.anchor.top });
    }
    return selectRelocalizationCandidate(confirmed, currFrame.height, options);
  };

  // 先以附近锚点为顺序入口，但仍把全局粗候选纳入最终歧义判断，避免附近的
  // 重复列表项以“高分假阳性”抢先返回。全分辨率复核仍只发生在少量候选上。
  const selected = search(ordered);
  if (!selected) return null;
  const nearbyTops = ordered.slice(0, nearbyCount).map((anchor) => anchor.top);
  const scope = nearbyTops.some((top) => Math.abs(top - selected.anchorTop) <= 3)
    ? 'nearby'
    : 'global';
  return { ...selected, scope };
}

/**
 * 关键帧召回失败后的有界内容分区召回。粗阶段只采样少量视口指纹，精阶段才
 * 重建原分辨率候选；候选唯一性仍复用 selectRelocalizationCandidate 守门。
 */
export function relocalizeFromPositionedContent(frames, currFrame, currentTop,
  expectedDirection = 0, options = {}) {
  if (!currFrame) return null;
  const bounds = positionedFrameBounds(frames);
  const currentProbe = createGrayFingerprint(currFrame);
  const currentReference = createVerticalReference(currFrame);
  if (!bounds || !currentProbe || !currentReference || bounds.height < currFrame.height) return null;
  const minTop = bounds.top;
  const maxTop = bounds.bottom - currFrame.height;
  const direction = options.trackingLost ? 0 : Math.sign(expectedDirection);
  const orderedFrames = frames.filter((item) => item?.image).sort((a, b) => a.top - b.top);
  const step = Math.max(12, Math.floor(currFrame.height * (options.partitionStepRatio ?? 0.45)));
  const anchors = new Set([minTop, maxTop, Math.max(minTop, Math.min(maxTop, currentTop))]);
  for (let top = minTop; top <= maxTop; top += step) anchors.add(top);
  const prioritized = [...anchors].sort((a, b) => {
    if (direction > 0) {
      const ap = a >= currentTop ? 0 : 1;
      const bp = b >= currentTop ? 0 : 1;
      if (ap !== bp) return ap - bp;
    } else if (direction < 0) {
      const ap = a <= currentTop ? 0 : 1;
      const bp = b <= currentTop ? 0 : 1;
      if (ap !== bp) return ap - bp;
    }
    return Math.abs(a - currentTop) - Math.abs(b - currentTop);
  }).slice(0, options.maxPartitions ?? 256);

  const coarse = [];
  for (const anchorTop of prioritized) {
    const anchorProbe = createPositionedProbeFromOrdered(
      orderedFrames,
      anchorTop,
      currFrame.height,
      DEFAULT_PROBE_WIDTH,
      DEFAULT_PROBE_HEIGHT,
    );
    if (!anchorProbe) continue;
    const probeMatch = estimateVerticalShift(anchorProbe, currentProbe, {
      maxShift: currentProbe.height - 9,
      matchThreshold: options.probeMatchThreshold ?? 26,
      sampleRows: options.probeSampleRows ?? 10,
      sampleCols: options.probeSampleCols ?? 12,
      rejectAmbiguous: true,
    });
    if (probeMatch.status !== 'matched' && probeMatch.status !== 'unchanged') continue;
    const scaledShift = probeMatch.shift * currFrame.height / currentProbe.height;
    const top = Math.round(anchorTop + scaledShift);
    if (top < minTop || top > maxTop) continue;
    coarse.push({ anchorTop, top, score: probeMatch.score });
  }

  const coarseTolerance = Math.max(4, Math.floor(step * 0.35));
  const distinct = [];
  for (const candidate of coarse.sort((a, b) => a.score - b.score)) {
    if (!distinct.some((item) => Math.abs(item.top - candidate.top) <= coarseTolerance)) {
      distinct.push(candidate);
    }
  }
  const confirmed = [];
  for (const candidate of distinct.slice(0, options.maxFullCandidates ?? 8)) {
    const viewport = extractPositionedViewport(frames, candidate.top, currFrame.height);
    const reference = createVerticalReference(viewport);
    if (!reference) continue;
    const match = estimateVerticalShift(reference, currentReference, {
      maxShift: Math.max(1, Math.floor(currFrame.height * 0.2)),
      matchThreshold: options.referenceMatchThreshold ?? 18,
      improvementRatio: options.referenceImprovementRatio ?? 0.72,
      rejectAmbiguous: true,
    });
    if (match.status !== 'matched' && match.status !== 'unchanged') continue;
    const top = Math.round(candidate.top + match.shift);
    if (top < minTop || top > maxTop) continue;
    confirmed.push({
      top,
      score: match.score,
      match,
      anchorTop: candidate.anchorTop,
      partitionTop: candidate.top,
    });
  }
  const selected = selectRelocalizationCandidate(confirmed, currFrame.height, options);
  return selected ? { ...selected, scope: 'content' } : null;
}

/** 返回带绝对 top 的完整帧覆盖范围。 */
export function positionedFrameBounds(frames) {
  if (!frames || frames.length === 0) return null;
  let top = Infinity;
  let bottom = -Infinity;
  for (const frame of frames) {
    if (!frame?.image || !Number.isFinite(frame.top)) continue;
    top = Math.min(top, frame.top);
    bottom = Math.max(bottom, frame.top + frame.image.height);
  }
  return Number.isFinite(top) && bottom > top ? { top, bottom, height: bottom - top } : null;
}

/**
 * 规划一个已定位视口对当前已提交范围的唯一增量。定位落在已有范围内时明确
 * 返回 inside，调用方只能移动定位框，不能把“识别成功”误当成“内容已追加”。
 */
export function planPositionedIncrement(bounds, top, height) {
  if (!Number.isFinite(top) || height <= 0) return { edge: 'invalid', rowCount: 0 };
  if (!bounds) {
    return { edge: 'full', startRow: 0, rowCount: height, targetTop: top };
  }
  if (top < bounds.top) {
    return {
      edge: 'top',
      startRow: 0,
      rowCount: Math.min(height, bounds.top - top),
      targetTop: top,
    };
  }
  const bottom = top + height;
  if (bottom > bounds.bottom) {
    return {
      edge: 'bottom',
      startRow: Math.max(0, bounds.bottom - top),
      rowCount: Math.min(height, bottom - bounds.bottom),
      targetTop: bounds.bottom,
    };
  }
  return { edge: 'inside', rowCount: 0 };
}

/**
 * 合成带绝对位置的完整采集帧。同一行保留最早采到的像素，
 * 回滚到已有区域只更新视口位置，不会把重复内容再追加一次。
 */
export function compositePositionedFrames(frames) {
  const bounds = positionedFrameBounds(frames);
  if (!bounds) return null;
  const first = frames.find((frame) => frame?.image);
  const width = first.image.width;
  if (frames.some((frame) => frame?.image && frame.image.width !== width)) return null;

  const result = new ImageData(width, bounds.height);
  const filledRows = new Uint8Array(bounds.height);
  const rowBytes = width * 4;
  for (const frame of frames) {
    if (!frame?.image) continue;
    const targetTop = Math.round(frame.top - bounds.top);
    for (let sourceY = 0; sourceY < frame.image.height; sourceY++) {
      const targetY = targetTop + sourceY;
      if (targetY < 0 || targetY >= bounds.height || filledRows[targetY]) continue;
      const sourceStart = sourceY * rowBytes;
      result.data.set(
        frame.image.data.subarray(sourceStart, sourceStart + rowBytes),
        targetY * rowBytes,
      );
      filledRows[targetY] = 1;
    }
  }
  return { image: result, ...bounds };
}

/** 裁出 ImageData 的连续行，并把结果平移到 y=0。 */
export function extractRows(frame, startRow, rowCount) {
  const start = Math.max(0, Math.min(frame.height, Math.floor(startRow)));
  const count = Math.max(0, Math.min(frame.height - start, Math.floor(rowCount)));
  if (count === 0) return null;
  const rowBytes = frame.width * 4;
  const data = frame.data.slice(start * rowBytes, (start + count) * rowBytes);
  return new ImageData(new Uint8ClampedArray(data), frame.width, count);
}

function tileRgbSad(first, firstY, second, secondY, x, width, height) {
  let total = 0;
  let samples = 0;
  for (let dy = 0; dy < height; dy++) {
    for (let dx = 0; dx < width; dx++) {
      const ai = ((firstY + dy) * first.width + x + dx) * 4;
      const bi = ((secondY + dy) * second.width + x + dx) * 4;
      total += Math.abs(first.data[ai] - second.data[bi]);
      total += Math.abs(first.data[ai + 1] - second.data[bi + 1]);
      total += Math.abs(first.data[ai + 2] - second.data[bi + 2]);
      samples += 3;
    }
  }
  return samples ? total / samples : Infinity;
}

function tileLumaRange(frame, y, x, width, height) {
  let minimum = 255;
  let maximum = 0;
  for (let dy = 0; dy < height; dy++) {
    for (let dx = 0; dx < width; dx++) {
      const index = ((y + dy) * frame.width + x + dx) * 4;
      const luma = frame.data[index] * 0.299
        + frame.data[index + 1] * 0.587
        + frame.data[index + 2] * 0.114;
      minimum = Math.min(minimum, luma);
      maximum = Math.max(maximum, luma);
    }
  }
  return maximum - minimum;
}

/**
 * 识别随视口固定的高细节小块。能从上一帧取到真实文档像素时，用它擦除本帧
 * 的固定层；底部新露出的固定层暂时保留，下一帧刷新重叠区时会自然覆盖旧副本。
 */
export function suppressFixedViewportLayers(previous, current, shift, options = {}) {
  if (!previous || !current || shift === 0
      || previous.width !== current.width || previous.height !== current.height) {
    return { frame: current, fixedTileCount: 0 };
  }
  const tileSize = options.tileSize ?? 8;
  const sameThreshold = options.sameThreshold ?? 1.5;
  const alignedThreshold = options.alignedThreshold ?? 8;
  const detailThreshold = options.detailThreshold ?? 12;
  const result = new ImageData(new Uint8ClampedArray(current.data), current.width, current.height);
  let fixedTileCount = 0;
  for (let y = 0; y < current.height; y += tileSize) {
    const tileHeight = Math.min(tileSize, current.height - y);
    for (let x = 0; x < current.width; x += tileSize) {
      const tileWidth = Math.min(tileSize, current.width - x);
      if (tileLumaRange(current, y, x, tileWidth, tileHeight) < detailThreshold) continue;
      const same = tileRgbSad(previous, y, current, y, x, tileWidth, tileHeight);
      if (same > sameThreshold) continue;
      const alignedY = y + shift;
      const hasAlignedSource = alignedY >= 0
        && alignedY + tileHeight <= previous.height;
      if (hasAlignedSource) {
        const aligned = tileRgbSad(previous, alignedY, current, y, x, tileWidth, tileHeight);
        if (aligned < alignedThreshold || aligned < same * 3 + 4) continue;
        const rowBytes = tileWidth * 4;
        for (let dy = 0; dy < tileHeight; dy++) {
          const sourceStart = ((alignedY + dy) * previous.width + x) * 4;
          const targetStart = ((y + dy) * current.width + x) * 4;
          result.data.set(previous.data.subarray(sourceStart, sourceStart + rowBytes), targetStart);
        }
      }
      fixedTileCount++;
    }
  }
  return { frame: result, fixedTileCount };
}

/** 用一个已确认的完整视口替换对应文档范围，并保留范围外的旧片段。 */
export function replacePositionedViewport(frames, frame, top) {
  const next = [];
  const bottom = top + frame.height;
  for (const item of frames || []) {
    if (!item?.image) continue;
    const itemBottom = item.top + item.image.height;
    if (itemBottom <= top || item.top >= bottom) {
      next.push(item);
      continue;
    }
    if (item.top < top) {
      const rows = extractRows(item.image, 0, top - item.top);
      if (rows) next.push({ image: rows, top: item.top });
    }
    if (itemBottom > bottom) {
      const rows = extractRows(item.image, bottom - item.top, itemBottom - bottom);
      if (rows) next.push({ image: rows, top: bottom });
    }
  }
  next.push({ image: frame, top });
  return next.sort((a, b) => a.top - b.top);
}

/** 生产采集与离线回放共用的提交策略。 */
export function commitTrackedFrame(frames, previousFrame, currentFrame, tracked) {
  const placement = tracked?.placement;
  if (!tracked?.decision?.accepted || !placement) {
    return { frames, committedFrame: currentFrame, addedRows: 0, fixedTileCount: 0 };
  }
  const canRefresh = previousFrame
    && !tracked.relocalized
    && tracked.match?.status === 'matched'
    && tracked.match.shift !== 0;
  if (canRefresh) {
    const fixed = suppressFixedViewportLayers(previousFrame, currentFrame, tracked.match.shift);
    if (fixed.fixedTileCount > 0) {
      return {
        frames: replacePositionedViewport(frames, fixed.frame, tracked.nextTop),
        committedFrame: fixed.frame,
        addedRows: placement.rowCount,
        fixedTileCount: fixed.fixedTileCount,
      };
    }
  }
  if (placement.rowCount <= 0) {
    return { frames, committedFrame: currentFrame, addedRows: 0, fixedTileCount: 0 };
  }
  const increment = extractRows(currentFrame, placement.startRow, placement.rowCount);
  return {
    frames: increment ? [...frames, { image: increment, top: placement.targetTop }] : frames,
    committedFrame: currentFrame,
    addedRows: increment?.height || 0,
    fixedTileCount: 0,
  };
}

/** 把“首帧 + 后续增量行”合成为完整长图。 */
export function compositeVerticalSegments(segments) {
  if (!segments || segments.length === 0) return null;
  const width = segments[0].width;
  if (segments.some((segment) => segment.width !== width)) return null;
  const totalHeight = segments.reduce((sum, segment) => sum + segment.height, 0);
  const result = new ImageData(width, totalHeight);
  let rowOffset = 0;
  for (const segment of segments) {
    result.data.set(segment.data, rowOffset * width * 4);
    rowOffset += segment.height;
  }
  return result;
}
