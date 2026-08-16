//! 颜色解析模块测试（0.20.3）。
//!
//! 运行：node --test frontend/js/shared/color.test.mjs
//!
//! 读取 Rust/JS 共享 fixture `color-literals.json`，
//! 验证 JS parse() 对所有 case 的 RGBA 与 canonical 输出完全一致。
//! Rust 侧（domain/color.rs tests）读取同一份 fixture 验证一致性。

import {describe, test} from "node:test";
import assert from "node:assert/strict";
import {readFileSync} from "node:fs";
import {fileURLToPath} from "node:url";
import {dirname, join} from "node:path";

import {parse} from "./color.js";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

// 读取共享 fixture
const fixturePath = join(__dirname, "fixtures", "color-literals.json");
const fixture = JSON.parse(readFileSync(fixturePath, "utf-8"));

describe("color parse - fixture consistency", () => {
    for (const c of fixture.cases) {
        // 非法输入
        if (c.rgba === null) {
            test(`illegal: "${c.input}"`, () => {
                assert.equal(parse(c.input), null);
            });
            continue;
        }

        // 合法输入
        test(`valid: "${c.input}" (${c.kind})`, () => {
            const result = parse(c.input);
            assert.notEqual(result, null, `expected parse("${c.input}") to return a result`);

            // RGBA8 检查
            assert.equal(result.rgba.r, c.rgba.r, `r mismatch for "${c.input}"`);
            assert.equal(result.rgba.g, c.rgba.g, `g mismatch for "${c.input}"`);
            assert.equal(result.rgba.b, c.rgba.b, `b mismatch for "${c.input}"`);
            assert.equal(result.rgba.a, c.rgba.a, `a mismatch for "${c.input}"`);

            // Canonical HEX 检查
            assert.equal(result.hex, c.hex, `hex mismatch for "${c.input}"`);

            // Canonical RGB 检查
            assert.equal(result.rgb, c.rgb, `rgb mismatch for "${c.input}"`);

            // Canonical HSL 检查
            assert.equal(result.hsl, c.hsl, `hsl mismatch for "${c.input}"`);

            // Alpha 浮点检查（允许 1e-5 误差）
            assert.ok(
                Math.abs(result.alpha - c.alpha) < 1e-5,
                `alpha mismatch for "${c.input}": got ${result.alpha}, expected ${c.alpha}`
            );
        });
    }
});

describe("color parse - edge cases", () => {
    test("empty string returns null", () => {
        assert.equal(parse(""), null);
    });

    test("whitespace only returns null", () => {
        assert.equal(parse("   "), null);
    });

    test("CSS named color 'red' returns null", () => {
        assert.equal(parse("red"), null);
    });

    test("app name 'calc' returns null", () => {
        assert.equal(parse("calc"), null);
    });

    test("app name 'settings' returns null", () => {
        assert.equal(parse("settings"), null);
    });

    test("plain English returns null", () => {
        assert.equal(parse("hello world"), null);
    });
});

describe("color canonical output", () => {
    test("hex is uppercase", () => {
        const r = parse("#ff0000");
        assert.equal(r.hex, "#FF0000");
    });

    test("hex8 includes alpha when < 255", () => {
        const r = parse("#ff0000aa");
        assert.equal(r.hex, "#FF0000AA");
    });

    test("hex6 when alpha = 255 even from hex8", () => {
        const r = parse("#ff0000ff");
        assert.equal(r.hex, "#FF0000");
    });

    test("rgb without alpha when fully opaque", () => {
        const r = parse("#ff0000");
        assert.equal(r.rgb, "rgb(255, 0, 0)");
    });

    test("rgb with alpha when semi-transparent", () => {
        const r = parse("rgba(255, 0, 0, 0.5)");
        assert.equal(r.rgb, "rgb(255, 0, 0, 0.502)");
    });

    test("hsl without alpha when fully opaque", () => {
        const r = parse("hsl(0, 100%, 50%)");
        assert.equal(r.hsl, "hsl(0, 100%, 50%)");
    });

    test("hsl with alpha when semi-transparent", () => {
        const r = parse("hsla(0, 100%, 50%, 0.5)");
        assert.equal(r.hsl, "hsl(0, 100%, 50%, 0.502)");
    });
});

describe("color HSL hue normalization", () => {
    test("360 degrees = 0 degrees", () => {
        const r = parse("hsl(360, 100%, 50%)");
        assert.equal(r.rgba.r, 255);
        assert.equal(r.rgba.g, 0);
        assert.equal(r.rgba.b, 0);
    });

    test("720 degrees wraps to 0", () => {
        const r = parse("hsl(720, 100%, 50%)");
        assert.equal(r.rgba.r, 255);
        assert.equal(r.rgba.g, 0);
        assert.equal(r.rgba.b, 0);
    });

    test("-120 degrees wraps to 240", () => {
        const r = parse("hsl(-120, 100%, 50%)");
        assert.equal(r.rgba.r, 0);
        assert.equal(r.rgba.g, 0);
        assert.equal(r.rgba.b, 255);
    });
});

describe("color float channel rounding", () => {
    test("rgb(127.5, 0, 0) rounds to 128", () => {
        const r = parse("rgb(127.5, 0, 0)");
        assert.equal(r.rgba.r, 128);
    });
});

describe("color percentage channels", () => {
    test("rgb(100%, 0%, 0%) = rgb(255, 0, 0)", () => {
        const r = parse("rgb(100%, 0%, 0%)");
        assert.equal(r.rgba.r, 255);
    });

    test("rgb(50%, 50%, 50%) = rgb(128, 128, 128)", () => {
        const r = parse("rgb(50%, 50%, 50%)");
        assert.equal(r.rgba.r, 128);
        assert.equal(r.rgba.g, 128);
        assert.equal(r.rgba.b, 128);
    });
});
