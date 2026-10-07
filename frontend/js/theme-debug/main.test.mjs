/**
 * contrast.js — WCAG 对比度纯函数测试（0.25.11 主题调试台）。
 *
 * 验证：
 * 1. rgb()/rgba()/#hex 解析与合成正确。
 * 2. 对比度数值与 WCAG 定义一致（已知锚点：黑/白 = 21，同色 = 1）。
 * 3. 0.25.11 light 主题状态色锚点达标（green/warning/yellow/accent ≥4.5），
 *    dark 基线不回退——token 值改动跑这里先红。
 * 4. verdict 判级阈值。
 */

import {describe, test} from "node:test";
import assert from "node:assert";

const {parseColor, relativeLuminance, compositeOver, contrastRatio, verdict, AA_NORMAL, AA_LARGE} =
    await import("./contrast.js");

describe("parseColor — 颜色字面量解析", () => {
    test("rgb/rgba/#hex 基础解析", () => {
        assert.deepStrictEqual(parseColor("rgb(255, 0, 0)"), [255, 0, 0, 1]);
        assert.deepStrictEqual(parseColor("rgba(0, 0, 255, 0.5)"), [0, 0, 255, 0.5]);
        assert.deepStrictEqual(parseColor("#ff0000"), [255, 0, 0, 1]);
        assert.deepStrictEqual(parseColor("#f00"), [255, 0, 0, 1]);
        assert.deepStrictEqual(parseColor("#ff000080"), [255, 0, 0, 128 / 255]);
    });

    test("非法输入返回 null", () => {
        assert.strictEqual(parseColor(""), null);
        assert.strictEqual(parseColor("red"), null);
        assert.strictEqual(parseColor("var(--text)"), null);
        assert.strictEqual(parseColor(null), null);
    });
});

describe("contrastRatio — WCAG 对比度", () => {
    test("锚点：黑白 21:1，同色 1:1", () => {
        assert.strictEqual(contrastRatio("#000000", "#ffffff"), 21);
        assert.strictEqual(contrastRatio("#181825", "#181825"), 1);
    });

    test("黑上白 vs 白上黑 对称", () => {
        assert.strictEqual(
            contrastRatio("#ffffff", "#000000"),
            contrastRatio("#000000", "#ffffff"),
        );
    });

    test("alpha 前景经 compositeOver 后可参与计算", () => {
        const flat = compositeOver("rgba(137, 180, 250, 0.15)", "#1e1e2e");
        assert.match(flat, /^rgb\(/);
        assert.ok(contrastRatio(flat, "#1e1e2e") < 2, "15% 淡蓝合到深底不应产生高对比");
    });

    test("0.25.11 light 主题状态色锚点：正文场景全部 ≥4.5", () => {
        const bg = "#eff1f5";
        for (const [name, fg] of [
            ["text-dim", "#696c82"],
            ["accent", "#1d62ec"],
            ["green", "#317b21"],
            ["warning", "#b94908"],
            ["yellow", "#976014"],
        ]) {
            const ratio = contrastRatio(fg, bg);
            assert.ok(ratio >= AA_NORMAL, `${name} ${fg} 对 ${bg} = ${ratio?.toFixed(2)}，须 ≥4.5`);
        }
    });

    test("dark 基线不回退：正文与状态色 ≥4.5", () => {
        const bg = "#1e1e2e";
        for (const fg of ["#cdd6f4", "#a6adc8", "#89b4fa", "#a6e3a1", "#f38ba8", "#fab387", "#e5c07b"]) {
            const ratio = contrastRatio(fg, bg);
            assert.ok(ratio >= AA_NORMAL, `${fg} 对 ${bg} = ${ratio?.toFixed(2)}，dark 基线须 ≥4.5`);
        }
    });

    test("error-item 修复锚点：--yellow 浅色下不再隐形（旧 fallback #e5c07b 仅 1.53）", () => {
        const ratio = contrastRatio("#976014", "#eff1f5");
        assert.ok(ratio > 3 * contrastRatio("#e5c07b", "#eff1f5"), "新 yellow 至少应为旧 fallback 的 3 倍对比");
    });
});

describe("verdict — AA 判级", () => {
    test("阈值 4.5 / 3.0", () => {
        assert.strictEqual(AA_NORMAL, 4.5);
        assert.strictEqual(AA_LARGE, 3.0);
        assert.strictEqual(verdict(21), "pass");
        assert.strictEqual(verdict(4.5), "pass");
        assert.strictEqual(verdict(4.4), "large");
        assert.strictEqual(verdict(3.0), "large");
        assert.strictEqual(verdict(2.9), "fail");
        assert.strictEqual(verdict(null), "fail");
    });
});

describe("relativeLuminance — 相对亮度", () => {
    test("黑白锚点", () => {
        assert.strictEqual(relativeLuminance("#000000"), 0);
        assert.ok(Math.abs(relativeLuminance("#ffffff") - 1) < 1e-9);
    });
});
