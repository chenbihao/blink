import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';

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
    commitTrackedFrame,
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
    validatePositionedOverlap,
} = await import(
'data:text/javascript;base64,'
+ Buffer.from(await readFile(new URL('./stitch.js', import.meta.url))).toString('base64')
    );

function withFixedBlocks(frame) {
    const result = new ImageData(new Uint8ClampedArray(frame.data), frame.width, frame.height);
    for (const y of [0, 72]) {
        for (let dy = 0; dy < 8; dy++) {
            for (let x = 8; x < 16; x++) {
                const index = ((y + dy) * result.width + x) * 4;
                const bright = (x + dy) % 2 === 0 ? 250 : 20;
                result.data[index] = bright;
                result.data[index + 1] = 40;
                result.data[index + 2] = 255 - bright;
                result.data[index + 3] = 255;
            }
        }
    }
    return result;
}

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

function sparseTextFrame(top, width = 120, height = 90) {
    const image = new ImageData(width, height);
    for (let y = 0; y < height; y++) {
        const documentY = top + y;
        const line = Math.floor(documentY / 12);
        const row = ((documentY % 12) + 12) % 12;
        const textEnd = 12 + ((line % 3) + 3) % 3 * 3;
        for (let x = 0; x < width; x++) {
            const i = (y * width + x) * 4;
            const textPixel = (row === 2 || row === 3) && x >= 4 && x < textEnd;
            image.data[i] = textPixel ? 185 : 20;
            image.data[i + 1] = textPixel ? 155 : 20;
            image.data[i + 2] = textPixel ? 210 : 20;
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
    estimateVerticalShift(first, downward, {expectedDirection: 1}).shift,
    17,
    '应识别向下滚动',
);
assert.equal(
    estimateVerticalShift(first, upward, {expectedDirection: -1}).shift,
    -9,
    '应识别向上滚动',
);
assert.equal(
    estimateVerticalShift(first, upward, {expectedDirection: 1}).shift,
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

// ── 0.24.10 回归 ────────────────────────────────────────────────

// 精搜 skip bug 回归：构造“粗搜最佳落在真位移 +1”的分数地形。
// prev 采样区内若干行被替换为上一行内容（模拟固定层/部分渲染差异），
// 使 SAD(9) 因局部完美行低于 SAD(5)；真位移 8（≡0 mod 4）在原 skip
// 规则（跳 ≡0 mod 4 的点）下永远不被精搜测试，永久差一像素。
{
    const skipPrev = documentFrame(0);
    const pristine = documentFrame(0);
    const rowBytes = skipPrev.width * 4;
    for (let row = 59; row <= 65; row++) {
        skipPrev.data.set(
            pristine.data.subarray((row - 1) * rowBytes, row * rowBytes),
            row * rowBytes,
        );
    }
    assert.equal(
        estimateVerticalShift(skipPrev, documentFrame(8), {expectedDirection: 1}).shift,
        8,
        '精搜必须能测试 ≡0 (mod 4) 的真位移（skip bug 回归）',
    );
}

// 空白页 unchanged 卡死回归：稀疏细线、低对比、窄文字使全局均分 ≤ 2.5。
// 行位置/长度由行号哈希决定（非周期），保证真实位移是唯一对齐。
// 默认路径维持 unchanged 短路（探针/重定位不换管线）；robustScoring 存在
// 足量行变化行，应继续搜索并找出真实位移。
function sparseScrollFrame(top, width = 120, height = 240, contrast = 40,
                           inkWidth = 12, textBits = 5) {
    const image = new ImageData(width, height);
    const darkValue = 250 - contrast;
    for (let y = 0; y < height; y++) {
        const documentY = top + y;
        const line = Math.floor(documentY / 4);
        const row = ((documentY % 4) + 4) % 4;
        const lineHash = Math.imul(line, 2654435761) >>> 0;
        const hasText = row <= 1 && (lineHash & 7) < textBits;
        const xStart = 8 + ((lineHash >>> 8) % (width - inkWidth - 16));
        for (let x = 0; x < width; x++) {
            const i = (y * width + x) * 4;
            const dark = hasText && x >= xStart && x < xStart + inkWidth;
            image.data[i] = dark ? darkValue : 250;
            image.data[i + 1] = dark ? darkValue + 2 : 250;
            image.data[i + 2] = dark ? darkValue - 2 : 250;
            image.data[i + 3] = 255;
        }
    }
    return image;
}
{
    const sparsePrev = sparseScrollFrame(0);
    const sparseNext = sparseScrollFrame(70);
    assert.equal(
        estimateVerticalShift(sparsePrev, sparseNext, {expectedDirection: 1}).status,
        'unchanged',
        '默认路径维持 unchanged 短路（探针/重定位不换管线）',
    );
    // 卡死边界的稀疏内容信息贫乏，匹配成败取决于局部纹理运气；断言语义
    // 锁定为“不得再被判为没滚动”（应尝试匹配），精确位移由下方脱离边界
    // 的用例与 tracker 绝对位置复核共同保证。
    const sparseRobust = estimateVerticalShift(sparsePrev, sparseNext, {
        expectedDirection: 1,
        strictDirection: true,
        rejectAmbiguous: true,
        robustScoring: true,
    });
    assert.notEqual(
        sparseRobust.status,
        'unchanged',
        '稀疏内容滚动不得被误判为没滚动（应尝试匹配）',
    );
    // 对比度/墨宽更高（全局均分高于 unchanged 阈值、脱离信息贫乏带）的
    // 稀疏页面，鲁棒管线应精确恢复真实位移。
    const readablePrev = sparseScrollFrame(0, 120, 240, 120, 16, 6);
    const readableNext = sparseScrollFrame(70, 120, 240, 120, 16, 6);
    assert.equal(
        estimateVerticalShift(readablePrev, readableNext, {
            expectedDirection: 1,
            strictDirection: true,
            rejectAmbiguous: true,
            robustScoring: true,
        }).shift,
        70,
        '稀疏内容应恢复真实位移',
    );
}

// 固定层 + 鲁棒管线：吸顶/置底/悬浮块同时存在时精确位移。
{
    const overlayMatch = estimateVerticalShift(
        withFixedBlocks(documentFrame(0)),
        withFixedBlocks(documentFrame(20)),
        {expectedDirection: 1, robustScoring: true},
    );
    assert.equal(overlayMatch.status, 'matched');
    assert.equal(overlayMatch.shift, 20, '视口固定块不得破坏鲁棒管线的位移精度');
}

const captures = [
    {image: documentFrame(40), top: 0},
    {image: documentFrame(57), top: 17},
    {image: documentFrame(40), top: 0}, // 回滚到已捕获的重复区域
    {image: documentFrame(31), top: -9},
];
const composite = compositePositionedFrames(captures);
assert.equal(composite.top, -9);
assert.equal(composite.bottom, 107);
assert.equal(composite.image.height, 116, '回滚不应重复增加长图高度');
assert.deepEqual(
    planPositionedIncrement({top: 0, bottom: 300}, 80, 90),
    {edge: 'inside', rowCount: 0},
    '回到已有内容只能更新定位，不得伪装成新增拼接',
);
assert.deepEqual(
    planPositionedIncrement({top: 0, bottom: 300}, -20, 90),
    {edge: 'top', startRow: 0, rowCount: 20, targetTop: -20},
    '越过上边界时只提交新暴露的顶部行',
);

const longCaptures = [];
const keyframes = [];
for (let top = 0; top <= 900; top += 45) {
    const image = documentFrame(top);
    longCaptures.push({image, top});
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

const confirmedDocument = [{image: documentFrame(0), top: 0}];
const overlapConsistent = validatePositionedOverlap(
    confirmedDocument,
    documentFrame(-20),
    -20,
);
assert.equal(overlapConsistent.status, 'consistent', '真实绝对位置应通过已确认内容复核');
const overlapConflict = validatePositionedOverlap(
    confirmedDocument,
    documentFrame(-20),
    -10,
);
assert.equal(overlapConflict.status, 'conflict', '伪相邻位置不得与已确认内容矛盾');
assert.ok(overlapConflict.score > overlapConflict.threshold);
const narrowOverlapConsistent = validatePositionedOverlap(
    [{image: documentFrame(0, 36, 120), top: 0}],
    documentFrame(90, 36, 120),
    90,
    {tileSize: 6},
);
assert.equal(
    narrowOverlapConsistent.status,
    'consistent',
    '约 25% 的真实重叠仍应进入细节复核，覆盖常见手动滚轮步进',
);
const tooNarrowOverlap = validatePositionedOverlap(
    [{image: documentFrame(0, 36, 120), top: 0}],
    documentFrame(100, 36, 120),
    100,
);
assert.equal(tooNarrowOverlap.status, 'insufficient', '低于 20% 的重叠不得作为绝对证据');
const sparseBackgroundConflict = validatePositionedOverlap(
    [{image: sparseTextFrame(0), top: 0}],
    sparseTextFrame(-20),
    -10,
);
assert.ok(
    sparseBackgroundConflict.score <= sparseBackgroundConflict.threshold,
    '暗色稀疏文本用均匀 RGB 平均时应能复现假阴性前提',
);
assert.equal(
    sparseBackgroundConflict.status,
    'conflict',
    '细节 tile 必须识别被大面积暗色背景稀释的文本错位',
);
assert.ok(sparseBackgroundConflict.mismatchRatio > sparseBackgroundConflict.detailMismatchRatio);
const fixedOverlap = validatePositionedOverlap(
    [{image: withFixedBlocks(documentFrame(0)), top: 0}],
    withFixedBlocks(documentFrame(20)),
    20,
);
assert.equal(fixedOverlap.status, 'consistent', '少量视口固定块不得误伤正确文档位置');

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
    estimateVerticalShift(documentFrame(450), documentFrame(20), {expectedDirection: -1}).status,
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
    estimateVerticalShift(documentFrame(0), documentFrame(75), {expectedDirection: 1}).status,
    'no-match',
    '低于最小可靠重叠的局部帧不得凭少量横线继续推进坐标',
);

const recoveredAfterLost = relocalizeFromKeyframes(
    longCaptures,
    keyframes,
    documentFrame(700),
    500,
    -1,
    {trackingLost: true},
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
    {trackingLost: true},
);
assert.equal(contentRecovered?.top, 400, '关键帧缺失时应能从已拼接内容分区恢复位置');
assert.equal(contentRecovered?.scope, 'content');

const veryLongCaptures = [];
for (let top = 0; top <= 7200; top += 45) {
    veryLongCaptures.push({image: documentFrame(top), top});
}
assert.equal(
    relocalizeFromPositionedContent(
        veryLongCaptures,
        documentFrame(400),
        7200,
        -1,
        {trackingLost: true},
    )?.top,
    400,
    '有界分区召回仍应覆盖超过 48 个视口锚点的超长内容',
);

const repeatingCaptures = [];
for (let top = 0; top <= 300; top += 30) {
    repeatingCaptures.push({image: repeatingFrame(top), top});
}
assert.equal(
    relocalizeFromPositionedContent(
        repeatingCaptures,
        repeatingFrame(10),
        150,
        0,
        {trackingLost: true},
    ),
    null,
    '重复分区存在多个近似位置时不得强行恢复',
);

assert.equal(
    selectRelocalizationCandidate([
        {top: 40, score: 3.1},
        {top: 260, score: 3.2},
    ], 90),
    null,
    '重复内容形成两个远距离近似候选时应拒绝猜测',
);
assert.equal(
    selectRelocalizationCandidate([
        {top: 40, score: 3.1},
        {top: 42, score: 3.0},
    ], 90)?.top,
    42,
    '同一位置的微小抖动候选应合并而不是误判为歧义',
);

const fixedPrevious = withFixedBlocks(documentFrame(0));
const fixedCurrent = withFixedBlocks(documentFrame(20));
const fixedCommit = commitTrackedFrame(
    [{image: fixedPrevious, top: 0}],
    fixedPrevious,
    fixedCurrent,
    {
        decision: {accepted: true},
        placement: {edge: 'bottom', startRow: 70, rowCount: 20, targetTop: 90},
        nextTop: 20,
        match: {status: 'matched', shift: 20},
        relocalized: null,
    },
);
assert.ok(fixedCommit.fixedTileCount >= 2, '应识别顶部和底部的视口固定块');
const fixedComposite = compositePositionedFrames(fixedCommit.frames);
assert.equal(fixedComposite.height, 110);
const rgbAt = (image, x, y) => Array.from(
    image.data.subarray((y * image.width + x) * 4, (y * image.width + x) * 4 + 3),
);
assert.deepEqual(
    rgbAt(fixedComposite.image, 8, 20),
    rgbAt(documentFrame(0), 8, 20),
    '吸顶元素的新副本应由上一帧的真实文档像素擦除',
);
assert.deepEqual(
    rgbAt(fixedComposite.image, 8, 72),
    rgbAt(documentFrame(0), 8, 72),
    '底部悬浮元素的旧副本应由新版重叠区覆盖',
);
assert.notDeepEqual(
    rgbAt(fixedComposite.image, 8, 92),
    rgbAt(documentFrame(20), 8, 72),
    '最终视口只保留一个最新的底部悬浮元素',
);

const upwardFixedCurrent = withFixedBlocks(documentFrame(-20));
const upwardFixedCommit = commitTrackedFrame(
    [{image: fixedPrevious, top: 0}],
    fixedPrevious,
    upwardFixedCurrent,
    {
        decision: {accepted: true},
        placement: {edge: 'top', startRow: 0, rowCount: 20, targetTop: -20},
        nextTop: -20,
        match: {status: 'matched', shift: -20},
        relocalized: null,
    },
);
assert.ok(upwardFixedCommit.fixedTileCount >= 2, '向上滚动也应识别视口固定块');
const upwardFixedComposite = compositePositionedFrames(upwardFixedCommit.frames);
assert.equal(upwardFixedComposite.height, 110);
assert.deepEqual(
    rgbAt(upwardFixedComposite.image, 8, 20),
    rgbAt(documentFrame(0), 8, 0),
    '向上扩展后，上一帧吸顶元素的旧副本应由真实文档像素覆盖',
);
assert.deepEqual(
    rgbAt(upwardFixedComposite.image, 8, 72),
    rgbAt(documentFrame(-20), 8, 72),
    '向上滚动时当前帧底部悬浮元素应由上一帧对齐内容擦除',
);

console.log('scroll stitch tests passed');
