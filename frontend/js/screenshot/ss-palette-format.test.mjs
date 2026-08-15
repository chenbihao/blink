//! ss-palette 格式化函数测试（P4-1 去复制化）。
//!
//! 运行：node --test frontend/js/screenshot/ss-palette-format.test.mjs
//!
//! 测试范围：
//! - rgbToHex（RGB → HEX 字符串）
//! - hexToHslString（HEX → HSL 展示字符串，仅格式化用途）
//! - formatOutput（hex/rgb/hsl/list 格式化）
//! - formatAsCssVariables（角色色 → CSS 变量声明）
//! - formatPaletteColors（auto/list/css 模式）
//!
//! P4-1：纯函数已提取至 `palette-format.js`，测试直接 import 同一模块，
//! 不再复制代码。消除"源码改了测试忘了改"的维护风险。

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import {
  rgbToHex,
  hexToHslString,
  formatOutput,
  formatAsCssVariables,
  formatPaletteColors,
} from "./palette-format.js";

// ── 测试 ──────────────────────────────────────────────────────────────

describe("rgbToHex", () => {
  test("pure red", () => {
    assert.equal(rgbToHex(255, 0, 0), "#FF0000");
  });

  test("pure green", () => {
    assert.equal(rgbToHex(0, 255, 0), "#00FF00");
  });

  test("pure blue", () => {
    assert.equal(rgbToHex(0, 0, 255), "#0000FF");
  });

  test("uppercase output", () => {
    assert.equal(rgbToHex(255, 255, 255), "#FFFFFF");
    assert.equal(rgbToHex(0, 0, 0), "#000000");
  });

  test("arbitrary color", () => {
    assert.equal(rgbToHex(123, 67, 201), "#7B43C9");
  });

  test("clamps out-of-range values", () => {
    assert.equal(rgbToHex(-1, 300, 128), "#00FF80");
  });

  test("rounds float values", () => {
    assert.equal(rgbToHex(127.4, 127.5, 127.6), "#7F8080");
  });
});

describe("hexToHslString", () => {
  test("pure red → hsl(0, 100%, 50%)", () => {
    assert.equal(hexToHslString("#FF0000"), "hsl(0, 100%, 50%)");
  });

  test("pure green → hsl(120, 100%, 50%)", () => {
    assert.equal(hexToHslString("#00FF00"), "hsl(120, 100%, 50%)");
  });

  test("pure blue → hsl(240, 100%, 50%)", () => {
    assert.equal(hexToHslString("#0000FF"), "hsl(240, 100%, 50%)");
  });

  test("white → hsl(0, 0%, 100%)", () => {
    assert.equal(hexToHslString("#FFFFFF"), "hsl(0, 0%, 100%)");
  });

  test("black → hsl(0, 0%, 0%)", () => {
    assert.equal(hexToHslString("#000000"), "hsl(0, 0%, 0%)");
  });

  test("mid gray → zero saturation", () => {
    const hsl = hexToHslString("#808080");
    assert.ok(hsl.startsWith("hsl("));
    // 饱和度应为 0%
    assert.match(hsl, /, 0%, /);
  });
});

describe("formatOutput", () => {
  const colors = ["#FF0000", "#00FF00", "#0000FF"];

  test("hex format joins with newlines", () => {
    assert.equal(formatOutput(colors, "hex"), "#FF0000\n#00FF00\n#0000FF");
  });

  test("list format joins with newlines", () => {
    assert.equal(formatOutput(colors, "list"), "#FF0000\n#00FF00\n#0000FF");
  });

  test("rgb format converts each color", () => {
    assert.equal(
      formatOutput(colors, "rgb"),
      "rgb(255, 0, 0)\nrgb(0, 255, 0)\nrgb(0, 0, 255)"
    );
  });

  test("hsl format converts each color", () => {
    assert.equal(
      formatOutput(colors, "hsl"),
      "hsl(0, 100%, 50%)\nhsl(120, 100%, 50%)\nhsl(240, 100%, 50%)"
    );
  });

  test("unknown format defaults to hex join", () => {
    assert.equal(formatOutput(colors, "unknown"), "#FF0000\n#00FF00\n#0000FF");
  });

  test("empty array produces empty string", () => {
    assert.equal(formatOutput([], "hex"), "");
  });

  test("single color no trailing newline", () => {
    assert.equal(formatOutput(["#FF0000"], "hex"), "#FF0000");
  });
});

describe("formatAsCssVariables", () => {
  test("single role produces clean variable", () => {
    const roles = [{ hex: "#FF0000", role: "background" }];
    assert.equal(formatAsCssVariables(roles), "  --blink-color-background: #FF0000;");
  });

  test("multiple distinct roles", () => {
    const roles = [
      { hex: "#FFFFFF", role: "background" },
      { hex: "#FF0000", role: "accent" },
      { hex: "#333333", role: "foreground" },
    ];
    assert.equal(
      formatAsCssVariables(roles),
      "  --blink-color-background: #FFFFFF;\n  --blink-color-accent: #FF0000;\n  --blink-color-foreground: #333333;"
    );
  });

  test("duplicate roles get numeric suffix", () => {
    const roles = [
      { hex: "#FF0000", role: "accent" },
      { hex: "#00FF00", role: "accent" },
      { hex: "#0000FF", role: "accent" },
    ];
    assert.equal(
      formatAsCssVariables(roles),
      "  --blink-color-accent: #FF0000;\n  --blink-color-accent-2: #00FF00;\n  --blink-color-accent-3: #0000FF;"
    );
  });

  test("mixed unique and duplicate roles", () => {
    const roles = [
      { hex: "#FFFFFF", role: "background" },
      { hex: "#FF0000", role: "accent" },
      { hex: "#00FF00", role: "accent" },
      { hex: "#333333", role: "foreground" },
    ];
    assert.equal(
      formatAsCssVariables(roles),
      "  --blink-color-background: #FFFFFF;\n  --blink-color-accent: #FF0000;\n  --blink-color-accent-2: #00FF00;\n  --blink-color-foreground: #333333;"
    );
  });

  test("empty roles produces empty string", () => {
    assert.equal(formatAsCssVariables([]), "");
  });
});

describe("formatPaletteColors", () => {
  // 模拟 getColorFormat：测试中固定为 hex
  const getColorFormat = () => "hex";

  const mockPaletteResult = {
    roles: [
      { hex: "#FFFFFF", role: "background", rgb: [255, 255, 255] },
      { hex: "#FF0000", role: "accent", rgb: [255, 0, 0] },
    ],
  };

  test("list mode joins with newlines", () => {
    const colors = ["#FFFFFF", "#FF0000"];
    assert.equal(formatPaletteColors(colors, "list", getColorFormat, mockPaletteResult.roles), "#FFFFFF\n#FF0000");
  });

  test("css mode uses role names from paletteResult", () => {
    const colors = ["#FFFFFF", "#FF0000"];
    const result = formatPaletteColors(colors, "css", getColorFormat, mockPaletteResult.roles);
    assert.equal(
      result,
      "  --blink-color-background: #FFFFFF;\n  --blink-color-accent: #FF0000;"
    );
  });

  test("css mode assigns fallback names for unknown colors", () => {
    const colors = ["#123456"]; // 不在 paletteResult.roles 中
    const result = formatPaletteColors(colors, "css", getColorFormat, mockPaletteResult.roles);
    assert.equal(result, "  --blink-color-selected-1: #123456;");
  });

  test("auto mode falls through to formatOutput(hex)", () => {
    const colors = ["#FF0000"];
    assert.equal(formatPaletteColors(colors, "auto", getColorFormat, mockPaletteResult.roles), "#FF0000");
  });

  test("undefined mode defaults to auto", () => {
    const colors = ["#FF0000", "#00FF00"];
    assert.equal(
      formatPaletteColors(colors, undefined, getColorFormat, mockPaletteResult.roles),
      "#FF0000\n#00FF00"
    );
  });

  test("css mode with empty paletteResult uses fallback names", () => {
    const colors = ["#FF0000", "#00FF00"];
    const result = formatPaletteColors(colors, "css", getColorFormat, null);
    assert.equal(
      result,
      "  --blink-color-selected-1: #FF0000;\n  --blink-color-selected-2: #00FF00;"
    );
  });
});
