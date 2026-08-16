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

function dataUrl(source) {
  return 'data:text/javascript;base64,' + Buffer.from(source).toString('base64');
}

const stitchSource = await readFile(new URL('./stitch.js', import.meta.url), 'utf8');
const stitchUrl = dataUrl(stitchSource);
const trackerSource = (await readFile(new URL('./tracker.js', import.meta.url), 'utf8'))
  .replace("'./stitch.js'", JSON.stringify(stitchUrl));
const trackerUrl = dataUrl(trackerSource);
const {
  rememberScrollKeyframe, trackScrollFrame, transitionScrollTracking,
} = await import(trackerUrl);
const replaySource = (await readFile(new URL('./replay.js', import.meta.url), 'utf8'))
  .replace("'./stitch.js'", JSON.stringify(stitchUrl))
  .replace("'./tracker.js'", JSON.stringify(trackerUrl));
const { replayScrollSequence } = await import(dataUrl(replaySource));

function documentFrame(top, variant = null, width = 36, height = 90) {
  const image = new ImageData(width, height);
  for (let y = 0; y < height; y++) {
    const documentY = top + y;
    for (let x = 0; x < width; x++) {
      const i = (y * width + x) * 4;
      image.data[i] = (documentY * 17 + Math.floor(documentY / 7) * 29 + x * 3) & 255;
      image.data[i + 1] = (documentY * 7 + Math.floor(documentY / 11) * 43 + x * 11) & 255;
      image.data[i + 2] = (documentY * 13 + Math.floor(documentY / 17) * 61 + x * 5) & 255;
      image.data[i + 3] = 255;
      if (variant != null && y < 10 && x >= 8 && x < 28) {
        image.data[i] = (variant * 83 + x * 5) & 255;
        image.data[i + 1] = (variant * 47 + x * 9) & 255;
        image.data[i + 2] = (variant * 29 + x * 13) & 255;
      }
    }
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

let retainedKeyframes = [];
for (let top = 0; top < 70; top++) {
  retainedKeyframes = rememberScrollKeyframe(retainedKeyframes, documentFrame(top, null, 8, 12), top * 10);
}
assert.equal(retainedKeyframes.length, 64, '极长页面关键帧必须受统一硬上限约束');
assert.equal(retainedKeyframes[0].top, 0, '空间抽样必须保留页面起点');
assert.equal(retainedKeyframes.at(-1).top, 690, '空间抽样必须保留页面远端');

const fixtureNames = [
  'continuous-down',
  'return-to-top',
  'fast-loss-recovery',
  'repeated-texture',
  'local-animation',
  'no-overlap',
  'scroll-bottom',
];
for (const fixtureName of fixtureNames) {
  const fixture = JSON.parse(await readFile(
    new URL(`./fixtures/scroll-replay/${fixtureName}.json`, import.meta.url),
    'utf8',
  ));
  const sequence = fixture.frames.map((item) => ({
    frame: fixture.generator === 'repeating'
      ? repeatingFrame(item.top)
      : documentFrame(item.top, fixture.generator === 'animated-document' ? item.variant : null),
    expectedDirection: item.direction,
  }));
  const firstRun = replayScrollSequence(sequence);
  const secondRun = replayScrollSequence(sequence);
  assert.deepEqual(firstRun.decisions, secondRun.decisions, `${fixture.name} 回放必须确定`);
  fixture.frames.forEach((expected, index) => {
    const actual = firstRun.decisions[index];
    assert.equal(actual.accepted, expected.accepted, `${fixture.name} frame ${index} accepted`);
    assert.equal(actual.reason, expected.reason, `${fixture.name} frame ${index} reason`);
    if (expected.accepted) {
      assert.equal(actual.candidateTop, expected.confirmedTop, `${fixture.name} frame ${index} top`);
    } else {
      assert.equal(actual.positionDelta, 0, `${fixture.name} frame ${index} 不得推进确认位置`);
      assert.equal(actual.appendRange, null, `${fixture.name} frame ${index} 不得追加像素`);
    }
  });
}

const positionedFrames = [];
for (let top = 0; top <= 900; top += 45) {
  positionedFrames.push({ image: documentFrame(top), top });
}
const lostState = {
  frames: positionedFrames,
  keyframes: [],
  lastFrame: documentFrame(900),
  currentTop: 900,
  trackingState: 'lost',
  lostFrameCount: 7,
  pendingJump: null,
};
const pending = trackScrollFrame(lostState, documentFrame(400), { expectedDirection: -1 });
assert.equal(pending.decision.reason, 'pending-confirmation');
assert.equal(pending.decision.candidateTop, 400);
assert.equal(pending.decision.accepted, false, '内容分区大跨度召回首帧不得直接推进坐标');
const mismatchedConfirmation = trackScrollFrame(
  { ...lostState, trackingState: 'tracking', pendingJump: pending.pendingJump },
  documentFrame(500),
  { expectedDirection: 0 },
);
assert.equal(mismatchedConfirmation.decision.accepted, false);
assert.equal(mismatchedConfirmation.decision.reason, 'ambiguous');
assert.equal(
  mismatchedConfirmation.nextTop,
  lostState.currentTop,
  '确认帧通过另一条匹配路径得到不同位置时不得绕过 pending 门禁',
);
const confirmed = trackScrollFrame(
  { ...lostState, pendingJump: pending.pendingJump },
  documentFrame(400),
  { expectedDirection: 0 },
);
assert.equal(confirmed.decision.accepted, true, '同一区域连续确认后才接受大跨度召回');
assert.equal(confirmed.decision.confirmation, 'confirmed');
assert.equal(confirmed.nextTop, 400);

const trackingFailure = {
  decision: { accepted: false, reason: 'no-overlap', source: 'adjacent', confirmation: 'none' },
  match: { status: 'no-match' },
};
const recovering = transitionScrollTracking({
  trackingState: 'tracking', lostFrameCount: 0,
}, trackingFailure);
assert.deepEqual(recovering, {
  trackingState: 'recovering', lostFrameCount: 1, becameLost: false,
}, '单帧失配只进入 recovering，不应立刻显示 lost');
const lost = transitionScrollTracking(recovering, trackingFailure);
assert.deepEqual(lost, {
  trackingState: 'lost', lostFrameCount: 2, becameLost: true,
}, '连续失配才进入 lost，并且只在转换瞬间触发提示');
const productionSessionLost = transitionScrollTracking({
  scrollTrackingState: 'recovering', scrollLostFrameCount: 1,
}, trackingFailure);
assert.deepEqual(productionSessionLost, {
  trackingState: 'lost', lostFrameCount: 2, becameLost: true,
}, '生产 session 的 scroll* 字段必须与离线回放状态得到同一转换结果');
const waitingConfirmation = transitionScrollTracking(lost, {
  decision: {
    accepted: false, reason: 'pending-confirmation', source: 'keyframe-global',
    confirmation: 'required',
  },
  match: { status: 'unchanged' },
});
assert.deepEqual(waitingConfirmation, {
  trackingState: 'lost', lostFrameCount: 2, becameLost: false,
}, '可靠候选等待确认不应继续累计失败次数');

const farFrames = [];
for (let top = 0; top <= 3960; top += 90) {
  farFrames.push({ image: documentFrame(top), top });
}
let farKeyframes = rememberScrollKeyframe([], farFrames[0].image, 0);
farKeyframes = rememberScrollKeyframe(farKeyframes, documentFrame(3900), 3900);
const farLostState = {
  frames: farFrames,
  keyframes: farKeyframes,
  lastFrame: documentFrame(3900),
  currentTop: 3900,
  trackingState: 'lost',
  lostFrameCount: 8,
  pendingJump: null,
};
const farReturnPending = trackScrollFrame(
  farLostState,
  documentFrame(0),
  { expectedDirection: 1 },
);
assert.equal(farReturnPending.decision.reason, 'pending-confirmation');
assert.equal(farReturnPending.decision.candidateTop, 0);
assert.equal(
  farReturnPending.decision.calibration.relocalizedOverlap.status,
  'consistent',
  '跨越整张长图回到起点时，应以已确认像素复核候选而不是按距离拒绝',
);
const farReturnConfirmed = trackScrollFrame(
  { ...farLostState, pendingJump: farReturnPending.pendingJump },
  documentFrame(0),
  { expectedDirection: 0 },
);
assert.equal(farReturnConfirmed.decision.accepted, true);
assert.equal(farReturnConfirmed.decision.confirmation, 'confirmed');
assert.equal(farReturnConfirmed.nextTop, 0);

const confirmedFrame = documentFrame(0);
const falseAdjacentState = {
  frames: [{ image: confirmedFrame, top: 0 }],
  keyframes: rememberScrollKeyframe([], confirmedFrame, 0),
  // 像素上像 -10，坐标却仍是 0：模拟重复纹理产生的可信相邻伪匹配。
  lastFrame: documentFrame(-10),
  currentTop: 0,
  trackingState: 'tracking',
  lostFrameCount: 0,
  pendingJump: null,
};
const overlapRecovered = trackScrollFrame(
  falseAdjacentState,
  documentFrame(-20),
  { expectedDirection: -1 },
);
// 相邻伪匹配与已确认像素冲突时，重定位不得在已确认范围外凭空推断位置。
// 候选 -20 在 bounds [0, 90] 之外，重定位无法找到安全候选，因此拒绝。
// 这验证了安全检查不会让重复纹理的伪匹配绕过门禁。
assert.equal(overlapRecovered.decision.accepted, false);
assert.equal(overlapRecovered.decision.reason, 'position-conflict');
assert.equal(overlapRecovered.decision.calibration.adjacent.shift, -10);
assert.equal(overlapRecovered.decision.calibration.positionedOverlap.status, 'conflict');
assert.equal(overlapRecovered.decision.calibration.relocalizedOverlap, null,
  '候选在已确认范围外时重定位不得凭空返回候选');

const poisonedKeyframeState = {
  ...falseAdjacentState,
  keyframes: rememberScrollKeyframe([], documentFrame(-10), 0),
};
const poisonedKeyframeRecovered = trackScrollFrame(
  poisonedKeyframeState,
  documentFrame(-20),
  { expectedDirection: -1 },
);
assert.equal(
  poisonedKeyframeRecovered.decision.accepted,
  false,
  '与确认内容冲突的关键帧候选不得绕过门禁接受同一伪位置',
);
assert.equal(poisonedKeyframeRecovered.decision.reason, 'position-conflict');
assert.equal(poisonedKeyframeRecovered.decision.calibration.positionedOverlap.status, 'conflict');

const lostPoisonedKeyframeState = {
  frames: [{ image: documentFrame(0), top: 0 }],
  keyframes: rememberScrollKeyframe([], documentFrame(30), 0),
  lastFrame: documentFrame(0),
  currentTop: 0,
  trackingState: 'lost',
  lostFrameCount: 2,
  pendingJump: null,
};
const lostPoisonedRecovery = trackScrollFrame(
  lostPoisonedKeyframeState,
  documentFrame(40),
  { expectedDirection: 0 },
);
assert.equal(
  lostPoisonedRecovery.decision.accepted,
  false,
  'lost 后的重定位候选与已确认像素冲突时，连续确认也不得接受',
);
assert.equal(lostPoisonedRecovery.decision.reason, 'position-conflict');
assert.equal(lostPoisonedRecovery.decision.calibration.relocalizedOverlap.status, 'conflict');

const farFalseAdjacentState = {
  ...falseAdjacentState,
  frames: [
    { image: documentFrame(0), top: 0 },
    { image: documentFrame(90), top: 90 },
  ],
  currentTop: 90,
};
const farPending = trackScrollFrame(
  farFalseAdjacentState,
  documentFrame(-20),
  { expectedDirection: -1 },
);
assert.equal(farPending.decision.accepted, false);
assert.equal(farPending.decision.reason, 'pending-confirmation');
assert.equal(farPending.decision.candidateTop, -20);
const farConfirmed = trackScrollFrame(
  { ...farFalseAdjacentState, pendingJump: farPending.pendingJump },
  documentFrame(-20),
  { expectedDirection: 0 },
);
assert.equal(farConfirmed.decision.accepted, true);
assert.equal(farConfirmed.decision.reason, 'relocalized');
assert.equal(farConfirmed.decision.confirmation, 'confirmed');
assert.equal(farConfirmed.decision.candidateTop, -20);

console.log('scroll replay tests passed');
