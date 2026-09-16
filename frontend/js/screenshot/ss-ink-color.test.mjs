//! 原文字色采样（sampleOriginalInkColorFromPixels）测试（0.23.x）。
//!
//! 覆盖嵌字字色匹配的核心行为：
//! 1. 白底黑字 / 白底彩字 → 采出接近原文字的颜色
//! 2. 纯色无字 / 候选过少 → null（回退黑白）
//! 3. 低对比文字（#eee on white）→ null；合法浅灰（#999）→ 保留
//! 4. solid 白底策略 + 深底白字 → null（防白底画白字）
//! 5. rect 越界钳制 / 空输入 → 不崩、正确返回

import {describe, test} from 'node:test';
import assert from 'node:assert';

// Mock window before importing production modules (ss-state → api.js → tauri.js)
globalThis.window = globalThis;
globalThis.document = {
    getElementById: () => null,
    createElement: () => ({
        style: {},
        setAttribute() {
        },
        removeAttribute() {
        },
        addEventListener() {
        },
        appendChild() {
        },
    }),
    body: {
        appendChild() {
        },
        contains: () => false,
        classList: {add() {
        }, remove() {
        }},
    },
    addEventListener() {
    },
    removeEventListener() {
    },
    querySelector: () => null,
    querySelectorAll: () => [],
};

const {sampleOriginalInkColorFromPixels, sampleOriginalInkColorsFromPixels,
    sampleAverageBackgroundColorFromPixels, chooseStableTranslatedInk,
    buildInkSegments, quantizeInkColors, wcagContrastRatio,
    countSolidInkClusters, isCodeLikeInkLayout,
    mergeTranslatedInkFragments, alignInkColorsToWords} = await import('./annotation-engine.js');

/** 构造 ImageData 样例。fill(x, y) → [r, g, b]。 */
function makeImage(w, h, fill) {
    const data = new Uint8ClampedArray(w * h * 4);
    for (let y = 0; y < h; y++) {
        for (let x = 0; x < w; x++) {
            const [r, g, b] = fill(x, y);
            const i = (y * w + x) * 4;
            data[i] = r;
            data[i + 1] = g;
            data[i + 2] = b;
            data[i + 3] = 255;
        }
    }
    return {data, width: w, height: h};
}

/** 在图上画实心矩形"字块"（字形核心区的代理，采样器只关心像素簇）。 */
function fillRect(img, x0, y0, x1, y1, [r, g, b]) {
    for (let y = y0; y <= y1; y++) {
        for (let x = x0; x <= x1; x++) {
            const i = (y * img.width + x) * 4;
            img.data[i] = r;
            img.data[i + 1] = g;
            img.data[i + 2] = b;
        }
    }
}

const WHITE = [255, 255, 255];
const RECT = {x: 0, y: 0, w: 100, h: 40};

describe('wcagContrastRatio', () => {
    test('黑白对比度 = 21，#999 对白底 ≈ 2.85', () => {
        assert.ok(Math.abs(wcagContrastRatio({r: 0, g: 0, b: 0}, {r: 255, g: 255, b: 255}) - 21) < 0.01);
        const ratio = wcagContrastRatio({r: 153, g: 153, b: 153}, {r: 255, g: 255, b: 255});
        assert.ok(Math.abs(ratio - 2.85) < 0.05, `实际 ${ratio}`);
    });
});

describe('sampleAverageBackgroundColorFromPixels — 对称稳健背景采样', () => {
    const parse = (css) => css.match(/\d+/g).slice(0, 3).map(Number);

    test('深底白字亮边污染环带 → 仍采出深色背景', () => {
        const img = makeImage(60, 40, () => [24, 42, 31]);
        // 模拟白字/抗锯齿溢出到文字框外围环带。
        fillRect(img, 16, 7, 23, 9, [245, 245, 245]);
        fillRect(img, 35, 30, 42, 32, [180, 220, 205]);
        const bg = sampleAverageBackgroundColorFromPixels(img, {x: 12, y: 10, w: 36, h: 20});
        const [r, g, b] = parse(bg);
        assert.ok(r < 60 && g < 70 && b < 60, `应保留深色背景，实际 ${bg}`);
    });

    test('浅底深字暗边污染环带 → 仍采出浅色背景', () => {
        const img = makeImage(60, 40, () => [238, 234, 226]);
        fillRect(img, 16, 7, 23, 9, [15, 15, 15]);
        fillRect(img, 35, 30, 42, 32, [70, 45, 30]);
        const bg = sampleAverageBackgroundColorFromPixels(img, {x: 12, y: 10, w: 36, h: 20});
        const [r, g, b] = parse(bg);
        assert.ok(r > 210 && g > 205 && b > 195, `应保留浅色背景，实际 ${bg}`);
    });

    test('空输入与完全无外围环带 → null', () => {
        assert.strictEqual(sampleAverageBackgroundColorFromPixels(null, RECT), null);
        const img = makeImage(20, 20, () => WHITE);
        assert.strictEqual(sampleAverageBackgroundColorFromPixels(img, {x: 0, y: 0, w: 20, h: 20}), null);
    });
});

describe('sampleOriginalInkColorFromPixels — 原文字色采样', () => {
    test('白底黑字 → 采出近黑字色', () => {
        const img = makeImage(100, 40, () => WHITE);
        fillRect(img, 10, 15, 40, 25, [10, 10, 10]);
        const ink = sampleOriginalInkColorFromPixels(img, RECT, 'rgba(255, 255, 255, 0.95)');
        assert.match(ink, /^rgb\(\d+, \d+, \d+\)$/);
        const [r, g, b] = ink.match(/\d+/g).map(Number);
        assert.ok(r <= 40 && g <= 40 && b <= 40, `应接近黑色，实际 ${ink}`);
    });

    test('白底蓝字（链接色）→ 采出蓝系字色而非黑白', () => {
        const img = makeImage(100, 40, () => WHITE);
        fillRect(img, 10, 15, 40, 25, [30, 90, 200]);
        const ink = sampleOriginalInkColorFromPixels(img, RECT, 'rgba(255, 255, 255, 0.95)');
        const [r, g, b] = ink.match(/\d+/g).map(Number);
        assert.ok(b - r > 100, `应为蓝系，实际 ${ink}`);
    });

    test('纯色无字 → null', () => {
        const img = makeImage(100, 40, () => WHITE);
        assert.strictEqual(sampleOriginalInkColorFromPixels(img, RECT, null), null);
    });

    test('文字像素低于面积阈值（2%）→ null', () => {
        const img = makeImage(100, 40, () => WHITE);
        fillRect(img, 50, 20, 51, 21, [10, 10, 10]); // 4 px ≈ 0.1%
        assert.strictEqual(sampleOriginalInkColorFromPixels(img, RECT, null), null);
    });

    test('低对比文字（#eee on white）→ null', () => {
        const img = makeImage(100, 40, () => WHITE);
        fillRect(img, 10, 15, 40, 25, [238, 238, 238]);
        assert.strictEqual(sampleOriginalInkColorFromPixels(img, RECT, null), null);
    });

    test('合法浅灰文字（#999 on white，对比度 2.85 ≥ 2.5）→ 保留灰', () => {
        const img = makeImage(100, 40, () => WHITE);
        fillRect(img, 10, 15, 40, 25, [153, 153, 153]);
        const ink = sampleOriginalInkColorFromPixels(img, RECT, null);
        const [r, g, b] = ink.match(/\d+/g).map(Number);
        assert.ok(Math.abs(r - 153) <= 10 && r === g && g === b, `应保留灰系，实际 ${ink}`);
    });

    test('solid 白底策略 + 原图为深底白字 → null（防白底画白字）', () => {
        const img = makeImage(100, 40, () => [40, 40, 40]);
        fillRect(img, 10, 15, 40, 25, [250, 250, 250]);
        const ink = sampleOriginalInkColorFromPixels(img, RECT, 'rgba(255, 255, 255, 0.92)');
        assert.strictEqual(ink, null);
    });

    test('rect 越界自动钳制到裁剪区', () => {
        const img = makeImage(60, 30, () => WHITE);
        fillRect(img, 2, 10, 30, 20, [200, 40, 40]);
        const ink = sampleOriginalInkColorFromPixels(img, {x: -10, y: -5, w: 120, h: 60}, null);
        const [r, b] = ink.match(/\d+/g).map(Number);
        assert.ok(r - b > 80, `应采出红系，实际 ${ink}`);
    });

    test('空输入与空 rect → null', () => {
        assert.strictEqual(sampleOriginalInkColorFromPixels(null, RECT, null), null);
        assert.strictEqual(sampleOriginalInkColorFromPixels(makeImage(10, 10, () => WHITE), {
            x: 5,
            y: 5,
            w: 0,
            h: 0,
        }, null), null);
    });

    // ── charRects 逐字符框收紧采样 ────────────────────────────────

    test('charRects 收紧采样：行内干扰色被几何排除', () => {
        const img = makeImage(100, 40, () => WHITE);
        fillRect(img, 10, 15, 40, 25, [30, 90, 200]);   // 蓝色文字
        fillRect(img, 60, 15, 90, 25, [200, 40, 40]);   // 行内干扰(图标/下划线，离白底更远)

        // 无 charRects：候选池=整行，核心取距离最远的红 → 采出干扰色
        const inkNo = sampleOriginalInkColorFromPixels(img, RECT, null);
        let [r, , b] = inkNo.match(/\d+/g).map(Number);
        assert.ok(r - b > 80, `无约束应采出红系干扰色，实际 ${inkNo}`);

        // charRects 只框住蓝色文字 → 干扰被几何排除，采出蓝
        const ink = sampleOriginalInkColorFromPixels(img, RECT, null, [{x: 8, y: 13, w: 35, h: 14}]);
        [r, , b] = ink.match(/\d+/g).map(Number);
        assert.ok(b - r > 80, `应采出蓝系字色，实际 ${ink}`);
    });

    test('charRects 全部越界 → 回退整行采样', () => {
        const img = makeImage(100, 40, () => WHITE);
        fillRect(img, 10, 15, 40, 25, [10, 10, 10]);
        const ink = sampleOriginalInkColorFromPixels(img, RECT, null, [{x: 500, y: 500, w: 20, h: 20}]);
        const [r] = ink.match(/\d+/g).map(Number);
        assert.ok(r <= 40, `应回退整行采出黑字，实际 ${ink}`);
    });

    test('charRects 框内全是背景（错位）→ null', () => {
        const img = makeImage(100, 40, () => WHITE);
        fillRect(img, 10, 15, 40, 25, [200, 40, 40]);
        const ink = sampleOriginalInkColorFromPixels(img, RECT, null, [{x: 70, y: 2, w: 10, h: 8}]);
        assert.strictEqual(ink, null);
    });
});

describe('sampleOriginalInkColorsFromPixels — 逐字符字色', () => {
    test('两个不同色字符 → charInks 分别采出对应色', () => {
        const img = makeImage(120, 40, () => WHITE);
        fillRect(img, 8, 15, 28, 25, [30, 90, 200]);    // 字0: 蓝
        fillRect(img, 48, 15, 68, 25, [200, 40, 40]);   // 字1: 红
        const charRects = [
            {x: 6, y: 13, w: 25, h: 14},
            {x: 46, y: 13, w: 25, h: 14},
        ];
        const s = sampleOriginalInkColorsFromPixels(img, RECT, null, charRects);
        assert.ok(s && s.ink, '行级字色应采出');
        assert.strictEqual(s.charInks.length, 2);
        const [c0r, , c0b] = s.charInks[0].match(/\d+/g).map(Number);
        assert.ok(c0b - c0r > 80, `字0应为蓝系，实际 ${s.charInks[0]}`);
        const [c1r, , c1b] = s.charInks[1].match(/\d+/g).map(Number);
        assert.ok(c1r - c1b > 80, `字1应为红系，实际 ${s.charInks[1]}`);
    });

    test('非收紧路径(无 charRects) → charInks 为 null', () => {
        const img = makeImage(100, 40, () => WHITE);
        fillRect(img, 10, 15, 40, 25, [10, 10, 10]);
        const s = sampleOriginalInkColorsFromPixels(img, RECT, null);
        assert.ok(s && s.ink);
        assert.strictEqual(s.charInks, null);
    });

    test('候选过少的窄字符 → charInks 该位为 null(继承整行)', () => {
        const img = makeImage(120, 40, () => WHITE);
        fillRect(img, 8, 12, 28, 26, [30, 90, 200]);    // 字0: 大块蓝，候选充足
        fillRect(img, 48, 18, 49, 18, [30, 90, 200]);   // 字1: 仅 2 个候选像素(< 3)
        const s = sampleOriginalInkColorsFromPixels(img, RECT, null, [
            {x: 6, y: 10, w: 25, h: 18},
            {x: 46, y: 16, w: 6, h: 5},
        ]);
        assert.ok(s && s.ink, '行级字色仍应采出');
        const [r, , b] = s.charInks[0].match(/\d+/g).map(Number);
        assert.ok(b - r > 80, `字0应正常立色，实际 ${s.charInks[0]}`);
        assert.strictEqual(s.charInks[1], null, '候选不足的窄字符应继承整行字色');
    });

    test('徽章场景：行内双背景(内绿底+外围黄底) → 字色采黑而非混绿', () => {
        const img = makeImage(100, 40, () => [235, 225, 170]);   // 外围黄底
        fillRect(img, 20, 10, 80, 30, [170, 205, 130]);          // 绿色徽章
        fillRect(img, 28, 16, 37, 26, [20, 20, 20]);             // 字0 黑色字形
        fillRect(img, 58, 16, 67, 26, [20, 20, 20]);             // 字1 黑色字形
        const s = sampleOriginalInkColorsFromPixels(img, RECT, null, [
            {x: 25, y: 12, w: 16, h: 18},
            {x: 55, y: 12, w: 16, h: 18},
        ]);
        assert.ok(s && s.ink, '行级字色应采出');
        const [lr, , lb] = s.ink.match(/\d+/g).map(Number);
        assert.ok(lr <= 90 && lb <= 90, `行级字色应为黑系，实际 ${s.ink}`);
        for (const c of s.charInks) {
            const [r, , b] = c.match(/\d+/g).map(Number);
            assert.ok(r <= 90 && b <= 90, `逐字符应为黑系，实际 ${c}`);
        }
    });

    test('装饰线横穿文字行(框内外都有绿) → 框内绿被背景簇排除，字色纯橙', () => {
        const img = makeImage(120, 40, () => WHITE);
        fillRect(img, 10, 12, 30, 28, [230, 140, 60]);   // 字0 橙
        fillRect(img, 60, 12, 80, 28, [230, 140, 60]);   // 字1 橙
        fillRect(img, 0, 19, 119, 21, [120, 170, 80]);   // 绿线横穿整行(含框内外)
        const s = sampleOriginalInkColorsFromPixels(img, RECT, null, [
            {x: 8, y: 10, w: 25, h: 20},
            {x: 58, y: 10, w: 25, h: 20},
        ]);
        assert.ok(s && s.ink, '行级字色应采出');
        for (const c of s.charInks) {
            const [r, g, b] = c.match(/\d+/g).map(Number);
            assert.ok(r > 150 && g > 90 && g < 190 && b < 120, `应为纯橙而非绿混色，实际 ${c}`);
        }
    });

    test('框外装饰与文字同色(橙色火花+橙字) → 文字色不被误当背景排除', () => {
        const img = makeImage(120, 40, () => WHITE);
        fillRect(img, 10, 12, 30, 28, [230, 140, 60]);   // 字0 橙(框内)
        fillRect(img, 60, 12, 80, 28, [230, 140, 60]);   // 字1 橙(框内)
        fillRect(img, 40, 2, 52, 8, [230, 140, 60]);     // 橙色火花(框外)
        const s = sampleOriginalInkColorsFromPixels(img, RECT, null, [
            {x: 8, y: 10, w: 25, h: 20},
            {x: 58, y: 10, w: 25, h: 20},
        ]);
        assert.ok(s && s.ink, '行级字色应采出');
        for (const c of s.charInks) {
            const [r, , b] = c.match(/\d+/g).map(Number);
            assert.ok(r > 150 && b < 120, `应为橙系，实际 ${c}`);
        }
    });

    test('装饰线只在框内穿过 → 桶内取主导色而非混色均值', () => {
        const img = makeImage(120, 40, () => WHITE);
        fillRect(img, 10, 12, 30, 28, [230, 140, 60]);   // 字0 橙(大块,主导)
        fillRect(img, 9, 18, 32, 24, [120, 170, 80]);    // 绿线只穿过字0框内
        const s = sampleOriginalInkColorsFromPixels(img, RECT, null, [
            {x: 8, y: 10, w: 25, h: 20},
        ]);
        assert.ok(s && s.ink);
        const [r, g, b] = s.charInks[0].match(/\d+/g).map(Number);
        const dist = Math.hypot(r - 230, g - 140, b - 60);
        assert.ok(dist < 20, `应取主导的橙而非混色均值，实际 ${s.charInks[0]}`);
    });

    test('主背景在框内天然占多 → 不被内外占比规则否决(防整行隐形)', () => {
        const img = makeImage(100, 40, () => [245, 238, 220]);   // 米色主背景
        fillRect(img, 15, 6, 35, 32, [40, 35, 30]);              // 大字形0(深棕)
        fillRect(img, 62, 6, 82, 32, [40, 35, 30]);              // 大字形1(深棕)
        fillRect(img, 0, 17, 4, 23, [110, 150, 70]);             // 细绿线只经过框外间隙
        fillRect(img, 48, 17, 51, 23, [110, 150, 70]);
        fillRect(img, 95, 17, 99, 23, [110, 150, 70]);
        const s = sampleOriginalInkColorsFromPixels(img, RECT, null, [
            {x: 5, y: 2, w: 42, h: 36},
            {x: 52, y: 2, w: 42, h: 36},
        ]);
        assert.ok(s && s.ink, '行级字色应采出');
        for (const c of s.charInks) {
            const [r, , b] = c.match(/\d+/g).map(Number);
            assert.ok(r <= 90 && b <= 90, `应为深色字形而非背景米色，实际 ${c}`);
        }
    });
});

describe('quantizeInkColors — 逐字符色归一化', () => {
    test('同色微差波动归一为同一色', () => {
        const out = quantizeInkColors(['rgb(200, 45, 50)', 'rgb(190, 55, 45)', 'rgb(215, 35, 40)']);
        assert.strictEqual(new Set(out).size, 1, `应收敛为一个色，实际 ${out.join(',')}`);
    });

    test('红与黑距离远，保持两簇', () => {
        const out = quantizeInkColors([
            'rgb(200, 45, 50)', 'rgb(20, 20, 20)', 'rgb(205, 40, 45)', 'rgb(25, 25, 25)',
        ]);
        assert.strictEqual(new Set(out).size, 2);
    });

    test('null 位原样保留(继承整行色)', () => {
        const out = quantizeInkColors(['rgb(200, 45, 50)', null]);
        assert.strictEqual(out[1], null);
        assert.match(out[0], /^rgb\(/);
    });
});

describe('chooseStableTranslatedInk — 译文整行主色', () => {
    const WHITE_INK = 'rgb(245, 245, 245)';
    const BLUE = 'rgb(30, 90, 200)';
    const FALLBACK_LIGHT = '#f5f5f5';

    test('逐字符青绿黄噪声无共识 → 回退背景对比色，不生成彩色译文', () => {
        const picked = chooseStableTranslatedInk({
            ink: 'rgb(70, 160, 120)',
            charInks: ['rgb(30, 170, 190)', 'rgb(70, 180, 80)', 'rgb(205, 190, 70)', 'rgb(35, 130, 175)'],
        }, FALLBACK_LIGHT, 'rgba(20, 35, 24, 0.95)');
        assert.strictEqual(picked, FALLBACK_LIGHT);
    });

    test('白字为行级稳定中性色，即使少数字符受壁纸污染也保持白色', () => {
        const picked = chooseStableTranslatedInk({
            ink: WHITE_INK,
            charInks: [WHITE_INK, WHITE_INK, 'rgb(50, 160, 90)', WHITE_INK],
        }, FALLBACK_LIGHT, 'rgba(20, 35, 24, 0.95)');
        assert.match(picked, /^rgb\(245, 245, 245\)$/);
    });

    test('十六进制黑白回退色可参与亮度与对比度判断', () => {
        const picked = chooseStableTranslatedInk({
            ink: '#f5f5f5',
            charInks: null,
        }, '#111', 'rgba(20, 35, 24, 0.95)');
        assert.strictEqual(picked, '#f5f5f5');
    });

    test('高覆盖、高一致性的真实蓝字 → 保留蓝色整行主色', () => {
        const picked = chooseStableTranslatedInk({
            ink: BLUE,
            charInks: [BLUE, BLUE, BLUE, BLUE],
        }, '#111', 'rgba(255, 255, 255, 0.95)');
        assert.match(picked, /rgb\(30, 90, 200\)/);
    });

    test('白底绿字仅两个字符但颜色一致 → 保留绿色', () => {
        const green = 'rgb(0, 150, 45)';
        const picked = chooseStableTranslatedInk({
            ink: green,
            charInks: [green, green],
        }, '#111', 'rgba(255, 255, 255, 0.95)');
        assert.strictEqual(picked, green);
    });

    test('黑底蓝字仅两个字符但颜色一致 → 保留蓝色', () => {
        const blue = 'rgb(40, 100, 230)';
        const picked = chooseStableTranslatedInk({
            ink: blue,
            charInks: [blue, blue],
        }, '#f5f5f5', 'rgba(0, 0, 0, 0.95)');
        assert.strictEqual(picked, blue);
    });

    test('单字符彩字但核心颜色高度一致 → 保留颜色', () => {
        const blue = 'rgb(40, 100, 230)';
        const picked = chooseStableTranslatedInk({
            ink: blue,
            charInks: [blue],
            inkConfidence: 0.9,
        }, '#f5f5f5', 'rgba(0, 0, 0, 0.95)');
        assert.strictEqual(picked, blue);
    });

    test('无逐字符框但整行核心色高度一致 → 保留彩色', () => {
        const green = 'rgb(0, 150, 45)';
        const picked = chooseStableTranslatedInk({
            ink: green,
            charInks: null,
            inkConfidence: 0.9,
        }, '#111', 'rgba(255, 255, 255, 0.95)');
        assert.strictEqual(picked, green);
    });

    test('无逐字符框且核心颜色分散 → 彩色回退', () => {
        const picked = chooseStableTranslatedInk({
            ink: 'rgb(40, 160, 90)',
            charInks: null,
            inkConfidence: 0.55,
        }, '#f5f5f5', 'rgba(20, 35, 24, 0.95)');
        assert.strictEqual(picked, '#f5f5f5');
    });

    test('彩色候选覆盖率不足 → 回退', () => {
        const picked = chooseStableTranslatedInk({
            ink: BLUE,
            charInks: [BLUE, BLUE, null, null, null],
        }, '#111', 'rgba(255, 255, 255, 0.95)');
        assert.strictEqual(picked, '#111');
    });

    test('对比度低于 2.5 的灰色译文 → 回退高对比色', () => {
        const gray = 'rgb(190, 190, 190)';
        const picked = chooseStableTranslatedInk({
            ink: gray,
            charInks: [gray, gray, gray, gray],
        }, '#111', 'rgba(255, 255, 255, 0.95)');
        assert.strictEqual(picked, '#111');
    });
});

describe('buildInkSegments — 译文分段着色', () => {
    const BLUE = 'rgb(30, 90, 200)';
    const RED = 'rgb(200, 40, 40)';
    const BLACK = 'rgb(10, 10, 10)';

    test('混合色行 → 按比例映射为两段，覆盖无缺口', () => {
        const segs = buildInkSegments(6, 'hello world', [BLUE, BLUE, BLUE, RED, RED, RED], BLACK);
        assert.strictEqual(segs.length, 2);
        assert.strictEqual(segs[0].text, 'hello ');
        assert.strictEqual(segs[0].color, BLUE);
        assert.strictEqual(segs[1].text, 'world');
        assert.strictEqual(segs[1].color, RED);
        assert.strictEqual(segs[0].text.length + segs[1].text.length, 11, '分段必须覆盖全文本');
    });

    test('全行同色 → null（走整行快路径）', () => {
        assert.strictEqual(buildInkSegments(3, 'abc', [BLUE, BLUE, BLUE], BLACK), null);
    });

    test('charInks 为 null/空 → null', () => {
        assert.strictEqual(buildInkSegments(3, 'abc', null, BLACK), null);
        assert.strictEqual(buildInkSegments(3, 'abc', [], BLACK), null);
    });

    test('null 字符色继承整行字色，与相邻同色合并', () => {
        // fallback(整行字色)与字符同色 → 合并为一段 → null 走快路径
        assert.strictEqual(buildInkSegments(4, 'abcd', [BLUE, null, BLUE, BLUE], BLUE), null);
        // fallback 与字符异色 → null 位独立成段(用整行色绘制),三段覆盖
        const segs = buildInkSegments(4, 'abcd', [BLUE, null, BLUE, BLUE], BLACK);
        assert.strictEqual(segs.length, 3);
        assert.strictEqual(segs[1].color, BLACK);
        assert.strictEqual(segs.reduce((n, s) => n + s.text.length, 0), 4);
    });

    test('译文被截断时分段仍完整覆盖显示文本', () => {
        const segs = buildInkSegments(6, 'hel', [BLUE, BLUE, BLUE, RED, RED, RED], BLACK);
        assert.strictEqual(segs.reduce((n, s) => n + s.text.length, 0), 3);
    });
});

// ── 译文模式代码行分段着色（isCodeLikeInkLayout / mergeTranslatedInkFragments）──

describe('countSolidInkClusters — 扎实结构簇统计', () => {
    const BLUE = 'rgb(60, 120, 220)';   // 亮度 ≈ 117，真语法色
    const PURPLE = 'rgb(170, 100, 190)'; // 亮度 ≈ 130
    const GRAY = 'rgb(100, 105, 110)';  // 亮度 ≈ 104
    const WHITE = 'rgb(255, 255, 255)'; // 亮度 255，近白主体
    const DARK_BG = {r: 25, g: 26, b: 28};

    test('双结构簇 → 计 2', () => {
        const inks = [BLUE, BLUE, BLUE, BLUE, PURPLE, PURPLE, PURPLE, PURPLE];
        assert.strictEqual(countSolidInkClusters(inks, DARK_BG).length, 2);
    });

    test('近白簇被排除（白色文字主体不是结构色）', () => {
        const inks = [WHITE, WHITE, WHITE, WHITE, BLUE, BLUE, BLUE, BLUE];
        assert.strictEqual(countSolidInkClusters(inks, DARK_BG).length, 1);
    });

    test('占比低于 15% 的碎片簇被排除', () => {
        // 8 字符里只有 1 个 RED（12.5% < 15%）
        const inks = [BLUE, BLUE, BLUE, BLUE, BLUE, BLUE, BLUE, 'rgb(200, 40, 40)'];
        assert.strictEqual(countSolidInkClusters(inks, DARK_BG).length, 1);
    });

    test('对背景不可读的簇被排除', () => {
        // 暗色簇对深背景 WCAG < 2.5
        const inks = [BLUE, BLUE, BLUE, BLUE, 'rgb(35, 36, 38)', 'rgb(35, 36, 38)',
            'rgb(35, 36, 38)', 'rgb(35, 36, 38)'];
        assert.strictEqual(countSolidInkClusters(inks, DARK_BG).length, 1);
    });

    test('null 字符不计入占比分母', () => {
        // 有效 6 字符 = 3 蓝 3 紫，各占 50%
        const inks = [BLUE, BLUE, BLUE, null, PURPLE, PURPLE, PURPLE, null];
        assert.strictEqual(countSolidInkClusters(inks, DARK_BG).length, 2);
    });

    test('空输入 / 全 null → 空数组', () => {
        assert.deepStrictEqual(countSolidInkClusters([], DARK_BG), []);
        assert.deepStrictEqual(countSolidInkClusters([null, null], DARK_BG), []);
        assert.deepStrictEqual(countSolidInkClusters(null, DARK_BG), []);
    });
});

describe('isCodeLikeInkLayout — 代码特征行判定', () => {
    const BLUE = 'rgb(60, 120, 220)';
    const PURPLE = 'rgb(170, 100, 190)';
    const WHITE = 'rgb(255, 255, 255)';
    const DARK_BG_CSS = 'rgb(25, 26, 28)';

    const makeSampled = (charInks) => ({ink: charInks[0] || null, charInks, inkConfidence: 0.8});

    test('高覆盖 + 双结构簇 → true（代码行）', () => {
        const sampled = makeSampled([BLUE, BLUE, BLUE, BLUE, PURPLE, PURPLE, PURPLE, PURPLE]);
        assert.strictEqual(isCodeLikeInkLayout(sampled, DARK_BG_CSS), true);
    });

    test('覆盖率低（大量 null）→ false', () => {
        const sampled = makeSampled([BLUE, null, null, null, PURPLE, null, null, null]);
        assert.strictEqual(isCodeLikeInkLayout(sampled, DARK_BG_CSS), false);
    });

    test('单结构簇 → false（无分段意义）', () => {
        const sampled = makeSampled([BLUE, BLUE, BLUE, BLUE, BLUE, BLUE, BLUE, BLUE]);
        assert.strictEqual(isCodeLikeInkLayout(sampled, DARK_BG_CSS), false);
    });

    test('白主体 + 单彩簇（图标/白字彩边形态）→ false', () => {
        const sampled = makeSampled([WHITE, WHITE, WHITE, WHITE, WHITE, BLUE, BLUE, BLUE]);
        assert.strictEqual(isCodeLikeInkLayout(sampled, DARK_BG_CSS), false);
    });

    test('结构簇超过 3 → false（杂色花斑风险）', () => {
        const sampled = makeSampled([
            'rgb(60, 120, 220)', 'rgb(60, 120, 220)',          // 蓝
            'rgb(170, 100, 190)', 'rgb(170, 100, 190)',        // 紫
            'rgb(200, 120, 60)', 'rgb(200, 120, 60)',          // 橙
            'rgb(80, 180, 90)', 'rgb(80, 180, 90)',            // 绿
        ]);
        assert.strictEqual(isCodeLikeInkLayout(sampled, DARK_BG_CSS), false);
    });

    test('charInks 为空 / 背景不可解析 → false', () => {
        assert.strictEqual(isCodeLikeInkLayout(makeSampled([]), DARK_BG_CSS), false);
        assert.strictEqual(isCodeLikeInkLayout(makeSampled([BLUE, BLUE, PURPLE, PURPLE]), ''), false);
        assert.strictEqual(isCodeLikeInkLayout(null, DARK_BG_CSS), false);
    });
});

describe('mergeTranslatedInkFragments — 碎片段合并', () => {
    const RED = 'rgb(200, 40, 40)';
    const DARKRED = 'rgb(160, 60, 60)';  // 与 RED 色距 ≈ 57 < 120
    const GREEN = 'rgb(40, 180, 60)';    // 与 RED 色距 ≈ 245 > 120

    test('孤立短段并入色距近的邻段', () => {
        // 暗红短段夹在两个红长段之间 → 并入（合并后剩 2 段）
        const segs = mergeTranslatedInkFragments([
            {text: 'abcdef', color: RED},
            {text: 'x', color: DARKRED},
            {text: 'ghijkl', color: RED},
        ]);
        assert.strictEqual(segs.length, 2);
        // 短段并入左邻（色距相等时取左），文本随合并移动
        assert.strictEqual(segs[0].text, 'abcdefx');
        assert.strictEqual(segs[0].color, RED);
        assert.strictEqual(segs[1].text, 'ghijkl');
    });

    test('与两邻段色距都大的短段保留（真语法色）', () => {
        const segs = mergeTranslatedInkFragments([
            {text: 'abcdef', color: RED},
            {text: 'x', color: GREEN},
            {text: 'ghijkl', color: RED},
        ]);
        assert.strictEqual(segs.length, 3, '绿色短段是真语法色，不应被合并');
    });

    test('占比达 8% 的段不合并', () => {
        // 短段 3 字符 / 总 27 = 11% ≥ 8%
        const segs = mergeTranslatedInkFragments([
            {text: 'abcdefghijkl', color: RED},
            {text: 'mno', color: DARKRED},
            {text: 'pqrstuvwxyz', color: RED},
        ]);
        assert.strictEqual(segs.length, 3);
    });

    test('迭代合并：多个碎片段逐一收敛', () => {
        const segs = mergeTranslatedInkFragments([
            {text: 'abcdefghijklmnopqrst', color: RED},
            {text: 'x', color: DARKRED},
            {text: 'y', color: DARKRED},
            {text: 'uvwxyz', color: RED},
        ]);
        assert.strictEqual(segs.length, 2);
        assert.strictEqual(segs[0].text, 'abcdefghijklmnopqrstxy');
    });

    test('边界：空 / 单段原样返回', () => {
        assert.strictEqual(mergeTranslatedInkFragments(null), null);
        assert.strictEqual(mergeTranslatedInkFragments([]), null);
        const single = [{text: 'abc', color: RED}];
        assert.strictEqual(mergeTranslatedInkFragments(single).length, 1);
    });
});

describe('alignInkColorsToWords — 词单元染色对齐', () => {
    const BLUE = 'rgb(60, 120, 220)';
    const RED = 'rgb(200, 40, 40)';
    const GRAYISH = 'rgb(120, 128, 130)'; // 蓝的采样噪声变体（距离 < 60）

    test('词内颜色抖动统一为主色（camelCase 不拆）', () => {
        // "chatPrompt" 中段两个字符采样发灰 → 整词统一回蓝
        const out = alignInkColorsToWords('chatPrompt',
            [BLUE, BLUE, GRAYISH, GRAYISH, BLUE, BLUE, BLUE, BLUE, BLUE]);
        assert.strictEqual(out.length, 9);
        for (const c of out) assert.strictEqual(c, BLUE);
    });

    test('snake_case 整体一个染色单元', () => {
        const out = alignInkColorsToWords('chat_prompt',
            [RED, GRAYISH, RED, RED, RED, GRAYISH, RED, RED, RED, RED, RED]);
        for (const c of out) assert.strictEqual(c, RED);
    });

    test('不同词保持各自颜色（词边界不被抹掉）', () => {
        const out = alignInkColorsToWords('foo bar',
            [BLUE, BLUE, BLUE, null, RED, RED, RED]);
        assert.strictEqual(out[0], BLUE);
        assert.strictEqual(out[2], BLUE);
        assert.strictEqual(out[4], RED);
    });

    test('标点自成单元，不影响相邻词', () => {
        // "id:" → id 为蓝单元，":" 为独立单元
        const out = alignInkColorsToWords('id:',
            [BLUE, GRAYISH, RED]);
        assert.strictEqual(out[0], BLUE);
        assert.strictEqual(out[1], BLUE);
        assert.strictEqual(out[2], RED);
    });

    test('全 null 词保留 null（继承整行色）', () => {
        const out = alignInkColorsToWords('foo bar',
            [null, null, null, BLUE, BLUE, BLUE, BLUE]);
        assert.strictEqual(out[0], null);
        assert.strictEqual(out[2], null);
        assert.strictEqual(out[4], BLUE);
    });

    test('边界：空输入原样返回', () => {
        assert.strictEqual(alignInkColorsToWords('', []), null);
        assert.strictEqual(alignInkColorsToWords(null, null), null);
        // 单字符输入返回内容一致的新数组
        assert.deepStrictEqual(alignInkColorsToWords('a', [BLUE]), [BLUE]);
    });
});
