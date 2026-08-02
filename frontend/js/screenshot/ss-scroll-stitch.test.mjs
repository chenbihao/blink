import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

globalThis.ImageData = class ImageData {
  constructor(dataOrWidth, widthOrHeight, maybeHeight) {
    if (typeof dataOrWidth === 'number') {
      this.width = dataOrWidth;
      this.height = widthOrHeight;
      this.data = new Uint8ClampedArray(this.width * this.height * 4);
    } else {
      this.data = dataOrWidth;
      this.width = widthOrHeight;
      this.height = maybeHeight;
    }
  }
};

const {
  compositePositionedFrames,
  createGrayFingerprint,
  createPositionedProbe,
  createVerticalReference,
  estimateVerticalShift,
  extractPositionedViewport,
  planPositionedIncrement,
  relocalizeFromKeyframes,
  relocalizeFromPositionedContent,
  selectRelocalizationCandidate,
} = await import(
  'data:text/javascript;base64,'
  + Buffer.from(await readFile(new URL('./ss-scroll-stitch.js', import.meta.url))).toString('base64')
);

function documentFrame(top, width = 36, height = 90) {
  const image = new ImageData(width, height);
  for (let y = 0; y < height; y++) {
    const documentY = top + y;
    for (let x = 0; x < width; x++) {
      const i = (y * width + x) * 4;
      image.data[i] = (documentY * 17 + Math.floor(documentY / 7) * 29 + x * 3) & 255;
      image.data[i + 1] = (documentY * 7 + Math.floor(documentY / 11) * 43 + x * 11) & 255;
      image.data[i + 2] = (documentY * 13 + Math.floor(documentY / 17) * 61 + x * 5) & 255;
      image.data[i + 3] = 255;
    }
  }
  return image;
}

function unrelatedFrame(seed, width = 36, height = 90) {
  const image = new ImageData(width, height);
  let value = seed >>> 0;
  for (let i = 0; i < image.data.length; i += 4) {
    value = (Math.imul(value, 1664525) + 1013904223) >>> 0;
    image.data[i] = value & 255;
    image.data[i + 1] = (value >>> 8) & 255;
    image.data[i + 2] = (value >>> 16) & 255;
    image.data[i + 3] = 255;
  }
  return image;
}

function repeatingFrame(top, width = 36, height = 90, period = 30) {
  const image = new ImageData(width, height);
  for (let y = 0; y < height; y++) {
    const repeatedY = ((top + y) % period + period) % period;
    for (let x = 0; x < width; x++) {
      const i = (y * width + x) * 4;
      image.data[i] = (repeatedY * 19 + x * 7) & 255;
      image.data[i + 1] = (repeatedY * 11 + x * 13) & 255;
      image.data[i + 2] = (repeatedY * 5 + x * 17) & 255;
      image.data[i + 3] = 255;
    }
  }
  return image;
}

const first = documentFrame(40);
const downward = documentFrame(57);
const upward = documentFrame(31);
assert.equal(
  estimateVerticalShift(first, downward, { expectedDirection: 1 }).shift,
  17,
  '应识别向下滚动',
);
assert.equal(
  estimateVerticalShift(first, upward, { expectedDirection: -1 }).shift,
  -9,
  '应识别向上滚动',
);
assert.equal(
  estimateVerticalShift(first, upward, { expectedDirection: 1 }).shift,
  -9,
  '穿透期间用户反向时应以画面实际位移为准',
);
assert.notEqual(
  estimateVerticalShift(first, upward, {
    expectedDirection: 1,
    strictDirection: true,
  }).shift,
  -9,
  '采集链路启用严格方向后，不得接受反方向位移',
);
assert.equal(
  estimateVerticalShift(repeatingFrame(0), repeatingFrame(10), {
    expectedDirection: 1,
    strictDirection: true,
    rejectAmbiguous: true,
  }).status,
  'no-match',
  '多个远距离位移同样匹配时应拒绝重复纹理',
);

const captures = [
  { image: documentFrame(40), top: 0 },
  { image: documentFrame(57), top: 17 },
  { image: documentFrame(40), top: 0 }, // 回滚到已捕获的重复区域
  { image: documentFrame(31), top: -9 },
];
const composite = compositePositionedFrames(captures);
assert.equal(composite.top, -9);
assert.equal(composite.bottom, 107);
assert.equal(composite.image.height, 116, '回滚不应重复增加长图高度');
assert.deepEqual(
  planPositionedIncrement({ top: 0, bottom: 300 }, 80, 90),
  { edge: 'inside', rowCount: 0 },
  '回到已有内容只能更新定位，不得伪装成新增拼接',
);
assert.deepEqual(
  planPositionedIncrement({ top: 0, bottom: 300 }, -20, 90),
  { edge: 'top', startRow: 0, rowCount: 20, targetTop: -20 },
  '越过上边界时只提交新暴露的顶部行',
);

const longCaptures = [];
const keyframes = [];
for (let top = 0; top <= 900; top += 45) {
  const image = documentFrame(top);
  longCaptures.push({ image, top });
  keyframes.push({
    top,
    probe: createGrayFingerprint(image),
    reference: createVerticalReference(image),
  });
}
const rebuilt = extractPositionedViewport(longCaptures, 135, 90);
assert.ok(rebuilt, '应能从定位片段按需重建完整视口');
assert.deepEqual(rebuilt.data, documentFrame(135).data);
assert.deepEqual(
  createPositionedProbe(longCaptures, 135, 90).data,
  createGrayFingerprint(documentFrame(135)).data,
  '分区粗召回应直接从已提交片段采样出等价指纹',
);

const boundedReference = createVerticalReference(documentFrame(0, 240, 180));
assert.equal(boundedReference.width, 96, '精配参考必须限制横向内存');
assert.equal(boundedReference.height, 180, '精配参考必须保留纵向逐像素定位精度');

const recovered = relocalizeFromKeyframes(
  longCaptures,
  keyframes,
  documentFrame(20),
  450,
  -1,
);
assert.equal(
  estimateVerticalShift(documentFrame(450), documentFrame(20), { expectedDirection: -1 }).status,
  'no-match',
  '该用例必须真实覆盖相邻帧已无重叠的路径',
);
assert.equal(recovered?.top, 20, '相邻帧完全失配后应从全局上方关键帧恢复');
assert.equal(recovered?.scope, 'global', '附近关键帧失败后才扩大到全局索引');

const nearbyRecovered = relocalizeFromKeyframes(
  longCaptures,
  keyframes,
  documentFrame(400),
  450,
  -1,
);
assert.equal(nearbyRecovered?.top, 400, '回滚到附近已捕获区域时应从附近关键帧恢复');
assert.equal(nearbyRecovered?.scope, 'nearby');

assert.equal(
  estimateVerticalShift(documentFrame(0), documentFrame(75), { expectedDirection: 1 }).status,
  'no-match',
  '低于最小可靠重叠的局部帧不得凭少量横线继续推进坐标',
);

const recoveredAfterLost = relocalizeFromKeyframes(
  longCaptures,
  keyframes,
  documentFrame(700),
  500,
  -1,
  { trackingLost: true },
);
assert.equal(
  recoveredAfterLost?.top,
  700,
  'lost 后真实位置可位于陈旧 currentTop 任一侧，恢复搜索不得再硬套滚轮方向',
);

const corruptedCaptures = longCaptures.map((capture, index) => ({
  image: index === 0 ? capture.image : unrelatedFrame(1000 + index),
  top: capture.top,
}));
assert.equal(
  relocalizeFromKeyframes(
    corruptedCaptures,
    keyframes,
    documentFrame(400),
    450,
    -1,
  )?.top,
  400,
  '精配必须使用与 probe 同源的不可变关键帧，而不是多帧重建画面',
);

const wrongDirection = relocalizeFromKeyframes(
  longCaptures,
  keyframes,
  documentFrame(20),
  450,
  1,
);
assert.equal(wrongDirection, null, '全局恢复不得明显逆着滚动方向跳转');
assert.equal(
  relocalizeFromKeyframes(longCaptures, keyframes, unrelatedFrame(20260802), 450, 1),
  null,
  '完全无视觉重叠时不得凭空推断新位置',
);

const contentRecovered = relocalizeFromPositionedContent(
  longCaptures,
  documentFrame(400),
  900,
  -1,
  { trackingLost: true },
);
assert.equal(contentRecovered?.top, 400, '关键帧缺失时应能从已拼接内容分区恢复位置');
assert.equal(contentRecovered?.scope, 'content');

const veryLongCaptures = [];
for (let top = 0; top <= 7200; top += 45) {
  veryLongCaptures.push({ image: documentFrame(top), top });
}
assert.equal(
  relocalizeFromPositionedContent(
    veryLongCaptures,
    documentFrame(400),
    7200,
    -1,
    { trackingLost: true },
  )?.top,
  400,
  '有界分区召回仍应覆盖超过 48 个视口锚点的超长内容',
);

const repeatingCaptures = [];
for (let top = 0; top <= 300; top += 30) {
  repeatingCaptures.push({ image: repeatingFrame(top), top });
}
assert.equal(
  relocalizeFromPositionedContent(
    repeatingCaptures,
    repeatingFrame(10),
    150,
    0,
    { trackingLost: true },
  ),
  null,
  '重复分区存在多个近似位置时不得强行恢复',
);

assert.equal(
  selectRelocalizationCandidate([
    { top: 40, score: 3.1 },
    { top: 260, score: 3.2 },
  ], 90),
  null,
  '重复内容形成两个远距离近似候选时应拒绝猜测',
);
assert.equal(
  selectRelocalizationCandidate([
    { top: 40, score: 3.1 },
    { top: 42, score: 3.0 },
  ], 90)?.top,
  42,
  '同一位置的微小抖动候选应合并而不是误判为歧义',
);

console.log('ss-scroll-stitch tests passed');
