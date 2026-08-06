#!/usr/bin/env node
/**
 * Bundle Tiptap into a single IIFE file for Blink's sticky notes IR editor.
 *
 * Usage:
 *   cargo xtask tiptap
 *   # or directly: node xtask/scripts/bundle-tiptap.js
 *
 * Output:
 *   frontend/vendor/tiptap.bundle.min.js  (~428KB raw / 136KB gzip)
 *
 * This script:
 *   1. Creates a temp build directory under target/tiptap-build/
 *   2. npm installs @tiptap/core, @tiptap/pm, @tiptap/starter-kit, @tiptap/markdown
 *   3. Bundles them via esbuild into a single IIFE (global name: BlinkTiptap)
 *   4. Injects version watermark at the top of the output
 *   5. Writes to frontend/vendor/tiptap.bundle.min.js
 *
 * Rationale (see phases/0.18 §3.2 spike-A):
 * - Cherry Markdown does not support IR (Instant Rendering).
 * - Tiptap provides IR via inputRules + @tiptap/markdown serialization layer.
 * - Tiptap has no official UMD build; we self-bundle with esbuild (same nature as
 *   cherry-markdown.stream.min.js being a pre-built artifact — runtime zero build).
 * - This does NOT violate the "no bundler" rule (spec-frontend §1.1): the build
 *   step happens here, the runtime loads a static .js file.
 */

"use strict";

const { existsSync, mkdirSync, writeFileSync, rmSync, readFileSync } = require("fs");
const { join, resolve } = require("path");
const { execSync } = require("child_process");

// ── 配置 ─────────────────────────────────────────────────────────────────────

// Tiptap 版本（四包同版）。升级前先 review release notes。
// https://github.com/ueberdosis/tiptap/releases
const TIPTAP_VERSION = "3.29.2";

// 需要安装的包
const PACKAGES = [
  `@tiptap/core@${TIPTAP_VERSION}`,
  `@tiptap/pm@${TIPTAP_VERSION}`,
  `@tiptap/starter-kit@${TIPTAP_VERSION}`,
  `@tiptap/markdown@${TIPTAP_VERSION}`,
  `@tiptap/extension-task-list@${TIPTAP_VERSION}`,
  `@tiptap/extension-task-item@${TIPTAP_VERSION}`,
];

// esbuild 版本（与 spike-A 验证时一致）
const ESBUILD_VERSION = "^0.28.0";

// ── 路径 ─────────────────────────────────────────────────────────────────────

// 从 __dirname 上溯两级（xtask/scripts/ → xtask/ → repo root）
const REPO_ROOT = resolve(__dirname, "..", "..");
const BUILD_DIR = join(REPO_ROOT, "target", "tiptap-build");
const OUT_FILE = join(REPO_ROOT, "frontend", "vendor", "tiptap.bundle.min.js");

// ── 入口文件 ─────────────────────────────────────────────────────────────────

// entry.mjs — 导出 Tiptap IR 编辑所需的最小 API 表面
//   window.BlinkTiptap.Editor       — Editor 类
//   window.BlinkTiptap.StarterKit   — 基础扩展包（含 inputRules：# → H1 等）
//   window.BlinkTiptap.Markdown      — Markdown 扩展（contentType:'markdown' + getMarkdown()）
const ENTRY_CONTENT = `// Entry for esbuild IIFE bundling — exports become window.BlinkTiptap.*
export { Editor } from "@tiptap/core";
export { default as StarterKit } from "@tiptap/starter-kit";
export { Markdown } from "@tiptap/markdown";
export { default as TaskList } from "@tiptap/extension-task-list";
export { default as TaskItem } from "@tiptap/extension-task-item";
`;

// ── 构建逻辑 ─────────────────────────────────────────────────────────────────

function main() {
  console.log(`[bundle-tiptap] Tiptap v${TIPTAP_VERSION}, esbuild ${ESBUILD_VERSION}`);
  console.log(`[bundle-tiptap] build dir → ${BUILD_DIR}`);
  console.log(`[bundle-tiptap] output   → ${OUT_FILE}`);

  // 1. 准备构建目录
  prepareBuildDir();

  // 2. 写 package.json + entry.mjs
  writePackageJson();
  writeFileSync(join(BUILD_DIR, "entry.mjs"), ENTRY_CONTENT, "utf8");
  console.log("[bundle-tiptap] wrote entry.mjs");

  // 3. npm install
  console.log("[bundle-tiptap] npm install ...");
  execSync("npm install --silent --no-audit --no-fund", {
    cwd: BUILD_DIR,
    stdio: "inherit",
    env: { ...process.env },
  });
  console.log("[bundle-tiptap] npm install done");

  // 4. esbuild bundle
  const esbuild = require(join(BUILD_DIR, "node_modules", "esbuild"));
  const result = esbuild.buildSync({
    entryPoints: [join(BUILD_DIR, "entry.mjs")],
    bundle: true,
    format: "iife",
    globalName: "BlinkTiptap",
    minify: true,
    target: ["chrome89"], // WebView2 (Chromium 89+) 兼容
    write: false, // 返回内容而非直接写文件
    logLevel: "info",
  });

  if (result.errors.length > 0) {
    console.error("[bundle-tiptap] esbuild errors:");
    for (const e of result.errors) console.error("  " + e.text);
    process.exit(1);
  }

  // 5. 组装产物：版本水印 + bundle 代码
  const bundleCode = result.outputFiles[0].text;
  const watermark = makeWatermark();
  const output = watermark + "\n" + bundleCode;

  // 6. 写出
  mkdirSync(join(REPO_ROOT, "frontend", "vendor"), { recursive: true });
  writeFileSync(OUT_FILE, output, "utf8");

  const sizeKB = (Buffer.byteLength(output, "utf8") / 1024).toFixed(1);
  console.log(`[bundle-tiptap] wrote ${OUT_FILE.replace(REPO_ROOT + "\\", "")} (${sizeKB} KB)`);

  // 7. 清理构建目录（保留以便下次快速重建可跳过 install）
  // 不清理——target/ 本就在 .gitignore 中，且保留可加速重复构建
  console.log("[bundle-tiptap] done (build dir kept for faster re-runs)");
}

function prepareBuildDir() {
  if (!existsSync(BUILD_DIR)) {
    mkdirSync(BUILD_DIR, { recursive: true });
  }
}

function writePackageJson() {
  // 直接构造 dependencies 对象，避免 scoped 包名 split("@") 的歧义
  // PACKAGES 格式为 `@tiptap/core@3.29.2`，最后一个 `@` 后面是版本
  const deps = {};
  for (const p of PACKAGES) {
    const atIdx = p.lastIndexOf("@");
    // atIdx > 0 确保 skip 开头的 @（scoped 包名）
    const name = atIdx > 0 ? p.slice(0, atIdx) : p;
    const version = atIdx > 0 ? p.slice(atIdx + 1) : "*";
    deps[name] = version;
  }

  const pkg = {
    name: "tiptap-bundle-build",
    version: "0.0.0",
    private: true,
    description: "Temp build dir for Blink Tiptap IIFE bundling (not published)",
    type: "module",
    dependencies: deps,
    devDependencies: {
      esbuild: ESBUILD_VERSION,
    },
  };
  writeFileSync(join(BUILD_DIR, "package.json"), JSON.stringify(pkg, null, 2), "utf8");
  console.log("[bundle-tiptap] wrote package.json");
}

function makeWatermark() {
  const date = new Date().toISOString().slice(0, 10);
  return [
    "/*",
    ` * Blink Tiptap Bundle — self-packed IIFE for sticky note IR editor`,
    ` * Version: @tiptap/{core,pm,starter-kit,markdown} ${TIPTAP_VERSION}`,
    ` * Built:   ${date}`,
    ` * Tool:    esbuild ${ESBUILD_VERSION}, format=iife, globalName=BlinkTiptap`,
    ` * Source:  xtask/scripts/bundle-tiptap.js (cargo xtask tiptap)`,
    ` * License: MIT (@tiptap/core) + MIT (@tiptap/pm = ProseMirror)`,
    ` * DO NOT EDIT — regenerate via \`cargo xtask tiptap\``,
    " */",
  ].join("\n");
}

// ── 入口 ─────────────────────────────────────────────────────────────────────

try {
  main();
} catch (e) {
  console.error(`[bundle-tiptap] FAILED: ${e.message}`);
  console.error(e.stack);
  process.exit(1);
}
