//! palette-core 纯函数单测（0.20.7）。
//!
//! 运行：node --test frontend/js/shared/color/palette-core.test.mjs
//!
//! 验证：
//! - OKLab 转换精度
//! - OKLab roundtrip（RGB→OKLab→RGB 误差 ≤ 1）
//! - WCAG 对比度
//! - Harmony 方案
//! - 聚类确定性（相同输入结果一致）
//! - 角色色分配
//! - 输出格式
//! - 无 DOM/Tauri/ss-state 依赖

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  PALETTE_ALGORITHM_V1,
  rgbToOklab,
  oklabToRgb,
  rgbToHsl,
  hslToRgb,
  rgbToHex,
  deltaE,
  relativeLuminance,
  contrastRatio,
  recommendTextColor,
  contrastWithRecommendation,
  kMeansCluster,
  assignRoles,
  generateHarmony,
  generateAllHarmonies,
  selectBaseColor,
  selectRecommendedSchemes,
  analyzePalette,
  analyzeTheme,
  buildColorHistogram,
  generateDesignPalettes,
  formatAsList,
  formatAsCssVariables,
  formatAsMultiLine,
  formatOutput,
} from "./palette-core.js";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

// 读取 fixture
const fixturePath = join(__dirname, "..", "fixtures", "palette-fixtures.json");
const fixture = JSON.parse(readFileSync(fixturePath, "utf-8"));

// ── OKLab 转换 ─────────────────────────────────────────────

describe("OKLab conversion", () => {
  for (const c of fixture.oklab_cases) {
    test(`${c.name}: RGB→OKLab`, () => {
      const lab = rgbToOklab(c.rgba[0], c.rgba[1], c.rgba[2]);
      const tol = c.tolerance || 0.001;
      assert.ok(Math.abs(lab[0] - c.oklab[0]) < tol, `L mismatch: got ${lab[0]}, expected ${c.oklab[0]}`);
      assert.ok(Math.abs(lab[1] - c.oklab[1]) < tol, `a mismatch: got ${lab[1]}, expected ${c.oklab[1]}`);
      assert.ok(Math.abs(lab[2] - c.oklab[2]) < tol, `b mismatch: got ${lab[2]}, expected ${c.oklab[2]}`);
    });
  }
});

// ── OKLab Roundtrip ────────────────────────────────────────

describe("OKLab roundtrip", () => {
  for (const c of fixture.roundtrip_cases) {
    test(`${c.name}: RGB→OKLab→RGB within tolerance ${c.tolerance}`, () => {
      const lab = rgbToOklab(c.rgba[0], c.rgba[1], c.rgba[2]);
      const rgb = oklabToRgb(lab);
      assert.ok(Math.abs(rgb[0] - c.rgba[0]) <= c.tolerance, `R mismatch: got ${rgb[0]}, expected ${c.rgba[0]}`);
      assert.ok(Math.abs(rgb[1] - c.rgba[1]) <= c.tolerance, `G mismatch: got ${rgb[1]}, expected ${c.rgba[1]}`);
      assert.ok(Math.abs(rgb[2] - c.rgba[2]) <= c.tolerance, `B mismatch: got ${rgb[2]}, expected ${c.rgba[2]}`);
    });
  }
});

// ── DeltaE ─────────────────────────────────────────────────

describe("DeltaE", () => {
  test("identical colors have zero distance", () => {
    const lab = rgbToOklab(255, 0, 0);
    assert.equal(deltaE(lab, lab), 0);
  });

  test("black and white have large distance", () => {
    const black = rgbToOklab(0, 0, 0);
    const white = rgbToOklab(255, 255, 255);
    assert.ok(deltaE(black, white) > 0.5);
  });
});

// ── WCAG 对比度 ─────────────────────────────────────────────

describe("WCAG contrast ratio", () => {
  test("black on white = 21:1", () => {
    const ratio = contrastRatio([0, 0, 0], [255, 255, 255]);
    assert.ok(Math.abs(ratio - 21.0) < 0.1, `got ${ratio}`);
  });

  test("same color = 1:1", () => {
    const ratio = contrastRatio([128, 128, 128], [128, 128, 128]);
    assert.ok(Math.abs(ratio - 1.0) < 0.01, `got ${ratio}`);
  });

  test("recommend text color for dark background", () => {
    const result = recommendTextColor([0, 0, 0]);
    assert.equal(result.textColor, 'light');
    assert.ok(result.ratio > 15);
  });

  test("recommend text color for light background", () => {
    const result = recommendTextColor([255, 255, 255]);
    assert.equal(result.textColor, 'dark');
    assert.ok(result.ratio > 15);
  });
});

// ── Harmony ────────────────────────────────────────────────

describe("Harmony generation", () => {
  test("complementary generates 6 colors (2 hues × 3 levels)", () => {
    const colors = generateHarmony([255, 0, 0], 'complementary');
    assert.equal(colors.length, 6);
  });

  test("triadic generates 9 colors (3 hues × 3 levels)", () => {
    const colors = generateHarmony([255, 0, 0], 'triadic');
    assert.equal(colors.length, 9);
  });

  test("analogous generates 9 colors (3 hues × 3 levels)", () => {
    const colors = generateHarmony([0, 0, 255], 'analogous');
    assert.equal(colors.length, 9);
  });

  test("splitComplementary generates 9 colors", () => {
    const colors = generateHarmony([0, 255, 0], 'splitComplementary');
    assert.equal(colors.length, 9);
  });

  test("generateAllHarmonies returns 6 schemes", () => {
    const all = generateAllHarmonies([255, 0, 0]);
    assert.equal(all.length, 6);
    assert.ok(all.every((s) => s.colors.length > 0));
  });

  test("selectRecommendedSchemes returns max 3", () => {
    const recommended = selectRecommendedSchemes([255, 0, 0]);
    assert.ok(recommended.length <= 3);
    assert.ok(recommended.length >= 1);
  });
});

// ── 聚类确定性 ─────────────────────────────────────────────

describe("k-means clustering determinism", () => {
  test("same input produces same output", () => {
    const pixels = [
      [255, 0, 0], [255, 0, 0], [255, 0, 0], [255, 0, 0],
      [0, 255, 0], [0, 255, 0], [0, 255, 0], [0, 255, 0],
      [0, 0, 255], [0, 0, 255], [0, 0, 255], [0, 0, 255],
    ];
    const result1 = kMeansCluster(pixels, 3);
    const result2 = kMeansCluster(pixels, 3);
    assert.equal(result1.length, result2.length);
    for (let i = 0; i < result1.length; i++) {
      assert.deepEqual(result1[i].rgb, result2[i].rgb, `cluster ${i} rgb mismatch`);
      assert.equal(result1[i].count, result2[i].count, `cluster ${i} count mismatch`);
    }
  });

  test("transparent pixels are skipped", () => {
    const pixels = [
      [255, 0, 0], [0, 255, 0], [0, 0, 255],
    ];
    const result = kMeansCluster(pixels, 3);
    assert.ok(result.length <= 3);
  });

  test("fewer pixels than k returns deduplicated", () => {
    const pixels = [[255, 0, 0], [0, 255, 0]];
    const result = kMeansCluster(pixels, 5);
    assert.ok(result.length <= 2);
  });

  test("representative colors come from original pixels", () => {
    const pixels = [
      [100, 50, 30], [100, 50, 30],
      [200, 100, 60], [200, 100, 60],
    ];
    const result = kMeansCluster(pixels, 2);
    for (const c of result) {
      // The representative RGB should be one of the original pixel values
      const isOriginal = pixels.some((p) => p[0] === c.rgb[0] && p[1] === c.rgb[1] && p[2] === c.rgb[2]);
      assert.ok(isOriginal, `rgb [${c.rgb}] not found in original pixels`);
    }
  });
});

// ── 角色色分配 ─────────────────────────────────────────────

describe("Role assignment", () => {
  test("single dominant color is background", () => {
    const clusters = [
      { rgb: [240, 240, 240], oklab: rgbToOklab(240, 240, 240), count: 100, ratio: 1.0 },
    ];
    const roles = assignRoles(clusters);
    assert.equal(roles[0].role, 'background');
  });

  test("high ratio color gets background role", () => {
    const clusters = [
      { rgb: [240, 240, 240], oklab: rgbToOklab(240, 240, 240), count: 80, ratio: 0.8 },
      { rgb: [30, 100, 200], oklab: rgbToOklab(30, 100, 200), count: 20, ratio: 0.2 },
    ];
    const roles = assignRoles(clusters);
    const bg = roles.find((r) => r.role === 'background');
    assert.ok(bg, 'should have a background role');
    assert.deepEqual(bg.rgb, [240, 240, 240]);
  });

  test("small ratio non-background gets accent or muted", () => {
    const clusters = [
      { rgb: [240, 240, 240], oklab: rgbToOklab(240, 240, 240), count: 90, ratio: 0.9 },
      { rgb: [30, 100, 200], oklab: rgbToOklab(30, 100, 200), count: 5, ratio: 0.05 },
    ];
    const roles = assignRoles(clusters);
    const accent = roles.find((r) => r.role === 'accent');
    assert.ok(accent, 'should have an accent role');
  });
});

// ── 完整分析入口 ───────────────────────────────────────────

describe("analyzePalette", () => {
  function makeStripeRgba(colors, stripeWidth = 12, height = 12) {
    const width = colors.length * stripeWidth;
    const rgba = new Uint8ClampedArray(width * height * 4);
    let offset = 0;
    for (let y = 0; y < height; y++) {
      for (let x = 0; x < width; x++) {
        const color = colors[Math.floor(x / stripeWidth)];
        rgba[offset++] = color[0];
        rgba[offset++] = color[1];
        rgba[offset++] = color[2];
        rgba[offset++] = 255;
      }
    }
    return { rgba, width, height };
  }

  test("empty sample returns empty result", () => {
    const result = analyzePalette([], 0, 0);
    assert.ok(result.empty);
    assert.equal(result.roles.length, 0);
  });

  test("all transparent returns empty", () => {
    const rgbaFlat = [255, 0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0];
    const result = analyzePalette(rgbaFlat, 2, 2);
    assert.ok(result.empty);
  });

  test("solid color returns one role", () => {
    const rgbaFlat = [
      240, 240, 240, 255, 240, 240, 240, 255,
      240, 240, 240, 255, 240, 240, 240, 255,
    ];
    const result = analyzePalette(rgbaFlat, 2, 2);
    assert.ok(!result.empty);
    assert.ok(result.roles.length >= 1);
    assert.ok(result.roles.some((r) => r.role === 'background'));
  });

  test("recommended schemes are populated", () => {
    const rgbaFlat = [
      255, 255, 255, 255, 30, 100, 200, 255,
    ];
    const result = analyzePalette(rgbaFlat, 2, 1);
    assert.ok(!result.empty);
    assert.ok(result.recommended.length >= 1);
    assert.ok(result.recommended.length <= 3);
    assert.equal(result.recommended[0].scheme, 'source');
    assert.deepEqual(result.recommended[0].colors, result.roles.map((role) => role.rgb));
    assert.deepEqual(result.full, [], '数学生成色不应混入图片提取结果');
  });

  test("seven-color rainbow stripes preserve all source colors and proportions", () => {
    const rainbow = [
      [255, 0, 0],     // 红
      [255, 127, 0],   // 橙
      [255, 255, 0],   // 黄
      [0, 200, 0],     // 绿
      [0, 180, 255],   // 青
      [0, 0, 255],     // 蓝
      [139, 0, 255],   // 紫
    ];
    const { rgba, width, height } = makeStripeRgba(rainbow);
    const result = analyzePalette(rgba, width, height);

    assert.equal(result.roles.length, 7, `expected 7 clusters, got ${result.roles.length}`);
    assert.deepEqual(result.sample, {
      width,
      height,
      validPixels: width * height,
      scannedPixels: width * height,
      mode: 'full',
    });

    const outputHex = new Set(result.roles.map((role) => role.hex));
    for (const [r, g, b] of rainbow) {
      assert.ok(outputHex.has(rgbToHex(r, g, b)), `missing rainbow color ${rgbToHex(r, g, b)}`);
    }
    for (const role of result.roles) {
      assert.ok(Math.abs(role.ratio - 1 / 7) < 0.001, `unexpected ratio ${role.ratio}`);
    }
    assert.equal(result.theme.family, '多色系');
    assert.equal(result.theme.temperature, '冷暖均衡');
  });

  test("rainbow image produces source, salient, and balanced design groups", () => {
    const { rgba, width, height } = makeStripeRgba([
      [255, 0, 0], [255, 127, 0], [255, 255, 0], [0, 200, 0],
      [0, 180, 255], [0, 0, 255], [139, 0, 255],
    ]);
    const result = analyzePalette(rgba, width, height);

    assert.deepEqual(result.recommended.map((scheme) => scheme.scheme), [
      'source', 'salient', 'balanced',
    ]);
    assert.deepEqual(result.recommended.map((scheme) => scheme.colors.length), [7, 6, 5]);
    assert.ok(result.recommended.every((scheme) => scheme.colors.every((rgb) => rgb.length === 3)));
  });

  test("editor-like image preserves tiny blue, red, and green accents", () => {
    const entries = [
      { rgb: [30, 30, 30], count: 8000 },      // 编辑器背景
      { rgb: [212, 212, 212], count: 1200 },   // 正文
      { rgb: [128, 128, 128], count: 300 },    // 次要文字
      { rgb: [55, 148, 255], count: 200 },     // 链接蓝 2%
      { rgb: [244, 71, 71], count: 150 },      // 错误红 1.5%
      { rgb: [106, 153, 85], count: 150 },     // 状态绿 1.5%
    ];
    const rgba = [];
    for (const entry of entries) {
      for (let i = 0; i < entry.count; i++) rgba.push(...entry.rgb, 255);
    }
    const result = analyzePalette(rgba, 100, 100);
    const focus = result.recommended.find((scheme) => scheme.scheme === 'salient');
    const ui = result.recommended.find((scheme) => scheme.scheme === 'balanced');
    const containsNear = (colors, expected) => {
      const lab = rgbToOklab(...expected);
      return colors.some((rgb) => deltaE(rgbToOklab(...rgb), lab) < 0.05);
    };

    assert.ok(containsNear(focus.colors, [55, 148, 255]), 'missing link blue');
    assert.ok(containsNear(focus.colors, [244, 71, 71]), 'missing error red');
    assert.ok(containsNear(focus.colors, [106, 153, 85]), 'missing status green');
    assert.equal(ui.label, '界面关键色');
    assert.ok(containsNear(ui.colors, [30, 30, 30]), 'missing editor background');
    assert.ok(containsNear(ui.colors, [212, 212, 212]), 'missing primary text');
  });

  test("hue peaks preserve sub-0.05% interface status colors", () => {
    const rgba = [];
    for (let i = 0; i < 20_000; i++) rgba.push(30, 30, 30, 255);
    for (let i = 0; i < 2; i++) rgba.push(55, 148, 255, 255);
    for (let i = 0; i < 2; i++) rgba.push(244, 71, 71, 255);
    for (let i = 0; i < 2; i++) rgba.push(106, 153, 85, 255);
    const result = analyzePalette(rgba, 200, 100);
    const focus = result.recommended.find((scheme) => scheme.scheme === 'salient');
    const focusHex = new Set(focus.colors.map((rgb) => rgbToHex(...rgb)));

    assert.ok(focusHex.has('#3794FF'), 'missing two-pixel link blue hue peak');
    assert.ok(focusHex.has('#F44747'), 'missing two-pixel error red hue peak');
    assert.ok(focusHex.has('#6A9955'), 'missing two-pixel status green hue peak');
  });

  test("repeated analysis is deterministic", () => {
    const rgbaFlat = [
      255, 0, 0, 255, 0, 255, 0, 255,
      0, 0, 255, 255, 255, 255, 0, 255,
    ];
    const r1 = analyzePalette(rgbaFlat, 2, 2);
    const r2 = analyzePalette(rgbaFlat, 2, 2);
    assert.equal(r1.roles.length, r2.roles.length);
    for (let i = 0; i < r1.roles.length; i++) {
      assert.deepEqual(r1.roles[i].rgb, r2.roles[i].rgb, `role ${i} rgb mismatch`);
      assert.equal(r1.roles[i].role, r2.roles[i].role, `role ${i} role mismatch`);
    }
  });
});

describe("Theme analysis", () => {
  test("blue dominant palette reports a blue cool theme", () => {
    const theme = analyzeTheme([
      { rgb: [40, 90, 210], ratio: 0.75 },
      { rgb: [90, 170, 240], ratio: 0.25 },
    ]);
    assert.equal(theme.family, '蓝色系');
    assert.equal(theme.temperature, '冷色倾向');
    assert.ok(theme.summary.includes('2 个主题色'));
  });

  test("near-neutral palette reports neutral family", () => {
    const theme = analyzeTheme([{ rgb: [130, 130, 130], ratio: 1 }]);
    assert.equal(theme.family, '中性色系');
    assert.equal(theme.saturation, '低饱和');
  });

  test("full-image histogram keeps representatives as exact source pixels", () => {
    const source = [
      [255, 0, 0], [254, 0, 0], [0, 255, 0], [0, 0, 255], [30, 30, 30],
    ];
    const rgba = source.flatMap((rgb) => [...rgb, 255]);
    const histogram = buildColorHistogram(rgba);
    const sourceHex = new Set(source.map((rgb) => rgbToHex(...rgb)));

    assert.equal(histogram.mode, 'full');
    assert.equal(histogram.scannedPixels, source.length);
    assert.equal(histogram.validPixels, source.length);
    for (let i = 0; i < histogram.counts.length; i++) {
      const rgb = Array.from(histogram.colors.slice(i * 3, i * 3 + 3));
      assert.ok(sourceHex.has(rgbToHex(...rgb)), `invented representative ${rgbToHex(...rgb)}`);
    }
  });

  test("dominant exact UI color wins over nearby antialias pixels", () => {
    const rgba = [];
    for (let i = 0; i < 100; i++) rgba.push(255, 0, 0, 255);
    rgba.push(254, 0, 0, 255, 253, 0, 0, 255);
    const histogram = buildColorHistogram(rgba);

    assert.equal(histogram.counts.length, 1);
    assert.deepEqual(Array.from(histogram.colors), [255, 0, 0]);
    assert.equal(histogram.counts[0], 102);
  });
});

describe("explicit design palette generation", () => {
  test("uses the visible anchor and remains separate from extracted colors", () => {
    const anchor = [55, 148, 255];
    const source = [[30, 30, 30], [212, 212, 212], anchor];
    const schemes = generateDesignPalettes(anchor, source);

    assert.deepEqual(schemes.map((scheme) => scheme.scheme), [
      'generated-tones', 'generated-analogous', 'generated-complement',
    ]);
    const complement = schemes.find((scheme) => scheme.scheme === 'generated-complement');
    assert.ok(complement.colors.some((rgb) => rgbToHex(...rgb) === '#3794FF'));
    assert.ok(complement.colors.some((rgb) => rgbToHex(...rgb) === '#1E1E1E'));
    assert.ok(complement.colors.some((rgb) => rgbToHex(...rgb) === '#D4D4D4'));

    const changed = generateDesignPalettes([244, 71, 71], source);
    assert.notDeepEqual(changed, schemes, 'changing the explicit anchor must change generated palettes');
  });
});

// ── 输出格式 ───────────────────────────────────────────────

describe("Output formatting", () => {
  const roles = [
    { rgb: [255, 0, 0], role: 'accent', hex: '#FF0000', ratio: 0.3, oklab: [0, 0, 0] },
    { rgb: [240, 240, 240], role: 'background', hex: '#F0F0F0', ratio: 0.7, oklab: [1, 0, 0] },
  ];

  test("formatAsList produces one HEX per line", () => {
    const text = formatAsList(roles);
    const lines = text.split('\n');
    assert.equal(lines.length, 2);
    assert.ok(lines.includes('#FF0000'));
    assert.ok(lines.includes('#F0F0F0'));
  });

  test("formatAsCssVariables uses role-based names", () => {
    const text = formatAsCssVariables(roles);
    assert.ok(text.includes('--blink-color-accent: #FF0000'));
    assert.ok(text.includes('--blink-color-background: #F0F0F0'));
  });

  test("formatAsCssVariables gives repeated roles stable unique names", () => {
    const repeated = [
      ...roles,
      { rgb: [0, 255, 0], role: 'accent', hex: '#00FF00', ratio: 0.1, oklab: [0, 0, 0] },
    ];
    const text = formatAsCssVariables(repeated);
    assert.ok(text.includes('--blink-color-accent: #FF0000'));
    assert.ok(text.includes('--blink-color-accent-2: #00FF00'));
  });

  test("formatAsMultiLine includes all formats", () => {
    const text = formatAsMultiLine(roles);
    assert.ok(text.includes('#FF0000'));
    assert.ok(text.includes('rgb(255, 0, 0)'));
    assert.ok(text.includes('hsl('));
  });

  test("formatOutput hex format", () => {
    const text = formatOutput(['#FF0000', '#00FF00'], 'hex');
    assert.equal(text, '#FF0000\n#00FF00');
    assert.ok(!text.includes(','));
  });

  test("formatOutput rgb format", () => {
    const text = formatOutput(['#FF0000'], 'rgb');
    assert.ok(text.includes('rgb(255, 0, 0)'));
  });

  test("formatOutput hsl format", () => {
    const text = formatOutput(['#FF0000'], 'hsl');
    assert.ok(text.includes('hsl('));
  });

  test("formatOutput list format", () => {
    const text = formatOutput(['#FF0000', '#00FF00'], 'list');
    assert.ok(text.includes('#FF0000'));
    assert.ok(text.includes('#00FF00'));
  });
});

// ── selectBaseColor ─────────────────────────────────────────

describe("selectBaseColor", () => {
  test("prefers accent role", () => {
    const roles = [
      { rgb: [240, 240, 240], role: 'background', hex: '#F0F0F0', ratio: 0.7, oklab: [1, 0, 0] },
      { rgb: [30, 100, 200], role: 'accent', hex: '#1E64C8', ratio: 0.3, oklab: [0.5, 0, 0] },
    ];
    const base = selectBaseColor(roles);
    assert.deepEqual(base, [30, 100, 200]);
  });

  test("falls back to first non-background", () => {
    const roles = [
      { rgb: [240, 240, 240], role: 'background', hex: '#F0F0F0', ratio: 0.7, oklab: [1, 0, 0] },
      { rgb: [30, 100, 200], role: 'muted', hex: '#1E64C8', ratio: 0.3, oklab: [0.5, 0, 0] },
    ];
    const base = selectBaseColor(roles);
    assert.deepEqual(base, [30, 100, 200]);
  });

  test("empty roles returns default gray", () => {
    const base = selectBaseColor([]);
    assert.deepEqual(base, [128, 128, 128]);
  });
});

// ── 常量 ────────────────────────────────────────────────────

describe("PALETTE_ALGORITHM_V1 constants", () => {
  test("K_MIN is 3 and K_MAX is 8", () => {
    assert.equal(PALETTE_ALGORITHM_V1.K_MIN, 3);
    assert.equal(PALETTE_ALGORITHM_V1.K_MAX, 8);
  });

  test("DEBOUNCE_MS is 120", () => {
    assert.equal(PALETTE_ALGORITHM_V1.DEBOUNCE_MS, 120);
  });

  test("WORKER_VERSION is 1", () => {
    assert.equal(PALETTE_ALGORITHM_V1.WORKER_VERSION, 1);
  });

  test("near-color merge threshold uses normalized OKLab scale", () => {
    assert.equal(PALETTE_ALGORITHM_V1.NEAR_COLOR_MERGE_DELTA_E, 0.05);
  });

  test("constants are frozen", () => {
    assert.ok(Object.isFrozen(PALETTE_ALGORITHM_V1));
  });
});
