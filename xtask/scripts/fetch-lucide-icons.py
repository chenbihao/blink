#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Fetch Lucide icons and build a local SVG sprite.

Usage:
    cargo xtask icons
    # 或直接：python xtask/scripts/fetch-lucide-icons.py <repo-root>

Output:
    frontend/assets/icons/sprite.svg      SVG <symbol> collection, one per icon
    frontend/assets/icons/manifest.json   { version, generatedAt, icons: [names] }

Rationale:
- Lucide is fetched at *build-time*, embedded into the frontend bundle.
- Runtime is fully offline — no network dep, no CDN. Consistent with Blink's
  "local-first" principle (0.10.8 §11.2 方案 3).
- Re-run this script when adding new icons (append to ICON_LIST below).

Requires: Python 3.8+, standard library only (urllib).
"""

import json
import re
import sys
import urllib.request
from datetime import datetime
from pathlib import Path

# ── 配置 ───────────────────────────────────────────────────────────────────────

# 锁定 Lucide 版本 —— 更新前先 review release notes 中的 breaking changes。
# 参考：https://github.com/lucide-icons/lucide/releases
# 注意：Lucide 1.x 起 tag 不带 `v` 前缀（0.x 时代带 `v`），下方 URL 直接拼 tag。
LUCIDE_VERSION = "1.25.0"

# 需要的图标名（对应 lucide.dev/icons/{name}）。新增图标追加到这里再跑脚本。
# 分组仅供人工可读，脚本不依赖分组。
ICON_LIST = [
    # 内置动作（settings tab · plugins tab · BUILTIN_ACTION_ICONS 映射）
    "settings",         # open_settings ⚙️
    "lock",             # lock 🔒
    "power",            # shutdown ⏻
    "refresh-cw",       # restart 🔁
    "moon",             # sleep 🌙
    "eraser",           # clear_history 🧹
    "log-out",          # exit_blink 🚪
    "file-text",        # open_logs 📄
    "folder-open",      # open_data_dir 🗂️
    "external-link",    # open_url 🔗
    "folder",           # open_path 📁 / 文件搜索 (settings.html)
    "folder-search",    # reveal_in_explorer 🔍

    # 插件（PLUGIN_ICONS 映射）
    "globe",            # builtin.ip / context 语言 🌐 / 🌍
    "volume-2",         # builtin.echo 🔊
    "sparkles",         # builtin.ai 🤖 (亦可挪作 AI 通用符)
    "languages",        # builtin.translate 📝
    "cloud-sun",        # builtin.weather 🌤️

    # 设置页 extension-icon
    "shield",           # context.js 敏感应用 🛡
    "spotlight",        # context.js 环境感知（0.11.8：从 globe 换成 spotlight，语义更精准）
    "ghost",            # settings.html Ghost 触发规则（0.11.8：从 zap 换成 ghost，与"Ghost"命名呼应）
    "audio-lines",      # settings.html 语音输入 card（0.11.8）
    "search",           # settings.html 应用搜索 🔍
    "calculator",       # settings.html 计算器 🧮
    "terminal",         # settings.html Python 🐍
    "zap",              # settings.html 通用 ⚡
    "brain",            # settings.html AI 🧠
    "lightbulb",        # settings.html Autosuggest 💡
    "command",          # settings.html 快捷键 ⌘

    # 截图 overlay 工具按钮
    "scan-text",        # OCR 🔍
    "pin",              # 钉图 📌
    "bookmark-check",   # 翻译并 pin（0.18.1：书签+勾选 = 翻译确认后钉住）
    "save",             # 保存为文件 💾
    "grid-3x3",         # 马赛克（0.11.8：从 emoji 🗯 迁移，格子点阵最像马赛克视觉）
    "spray-can",        # 涂抹（0.11.8：从 emoji ▦ 迁移，喷罐涂抹感）
    "aperture",          # 高斯模糊（旧，保留兼容）
    "paintbrush",       # 涂抹 + 高斯模糊（0.15.11：画笔涂抹感更直观）
    "pipette",            # 取色按钮（0.15.12：吸管图标）
    "gallery-horizontal", # 长截图（旧，保留兼容）
    "gallery-vertical",   # 长截图（0.15.7：纵向画廊图标）
    "gallery-vertical-end", # 长截图自动滚动
    "rectangle-vertical",   # 长截图方向-纵向
    "rectangle-horizontal", # 长截图方向-横向
    "hand-grab",             # 拖拽语义（保留兼容）
    "pencil-sparkles",       # 长截图编辑长图
    "stamp",            # 水印（0.11.8-c）——印章语义
    "ticket-slash",     # 荧光笔（0.11.8-c）——斜线穿越条状代表笔画
    "move-up-right",    # 箭头（0.11.8-d）——替代 emoji ➘
    "pencil",           # 铅笔（0.11.8-d）——替代 emoji ✎
    "square",           # 矩形（0.11.8-e）——替代 emoji ▭
    "circle",           # 椭圆（0.11.8-f）——替代 ellipse（后者线条太细，不匹配 stroke=2）
    "undo-2",           # 撤销（0.11.8-f）——替代 ↶
    "redo-2",           # 重做（0.11.8-f）——替代 ↷
    "check",            # 复制确认（0.11.8-f，绿色 tool-btn-primary）——替代 ✓
    "x",                # 取消（0.11.8-f，红色 #btn-cancel）——替代 ✕
    "mouse-pointer-2",  # 选取工具（0.11.10-a）——工具栏默认工具
    "menu",             # 面板召唤（0.11.10-e）——工具栏右侧[≡]按钮
    "eye-off",          # 便签管理隐藏（0.16.10）——隐藏便签动作
    "eye",              # 便签管理重新显示到桌面
    "chevron-down",     # 供应商预设折叠指示
    "trash-2",          # 便签管理删除（0.17.7）——移入回收站
    "rotate-ccw",       # 便签管理恢复（0.17.7）——从回收站恢复

    # AI 确认徽章
    "triangle-alert",   # AI 需确认 ⚠  （Lucide 1.x 从 alert-triangle 重命名）

    # AI 对话能力 tab 图标
    "copy",             # 复制 MCP 配置
    "plus",             # 添加 Server 按钮
    "server",           # MCP Server 暴露能力 section
    "plug",             # MCP 外部工具 section
    "puzzle",           # Skill 约定式 section
    "message-square",   # 对话 section
    "database",         # 记忆策略 section

    # MD 工具栏图标（0.18 review：md-toolbar.js 从内联 SVG 迁移到 Lucide sprite）
    "list",             # 无序列表
    "list-ordered",     # 有序列表
    "list-checks",      # 任务清单
    "quote",            # 引用
    "code",             # 代码块
]

# ── 路径 ───────────────────────────────────────────────────────────────────────

# 由 xtask 传入 repo root，或从 __file__ 上溯三级（xtask/scripts/ → xtask/ → root）
ROOT = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent.parent
OUT_DIR = ROOT / "frontend" / "assets" / "icons"
SPRITE_FILE = OUT_DIR / "sprite.svg"
MANIFEST_FILE = OUT_DIR / "manifest.json"
LICENSE_FILE = OUT_DIR / "LICENSE.lucide.txt"

RAW_URL = "https://raw.githubusercontent.com/lucide-icons/lucide/{ver}/icons/{name}.svg"

# Lucide ISC 授权原文（release 说明附带；打包到 assets 目录同侧，运行期也可看到）
LICENSE_TEXT = """ISC License

Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2022 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2022.

Permission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby granted, provided that the above copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
"""

# ── 抓取 + 提取 ────────────────────────────────────────────────────────────────

# 匹配 <svg ... > ... </svg>，提取属性块与内部路径。
# Lucide SVG 结构固定：<svg xmlns=".." width=24 height=24 viewBox="0 0 24 24" ..>
#   <path .../>
#   <path .../>
#   ...
# </svg>
SVG_RE = re.compile(r"<svg\b([^>]*)>(.*?)</svg>", re.DOTALL)
VIEWBOX_RE = re.compile(r'viewBox="([^"]+)"')


def fetch_icon(name: str) -> str:
    url = RAW_URL.format(ver=LUCIDE_VERSION, name=name)
    print(f"  fetch {name} ← {url}")
    try:
        with urllib.request.urlopen(url, timeout=15) as resp:
            if resp.status != 200:
                raise RuntimeError(f"HTTP {resp.status}")
            return resp.read().decode("utf-8")
    except Exception as e:
        raise RuntimeError(f"failed to fetch {name}: {e}") from e


def svg_to_symbol(name: str, svg: str) -> str:
    """
    Convert `<svg viewBox="0 0 24 24">...children...</svg>` to
    `<symbol id="icon-{name}" viewBox="0 0 24 24">...children...</symbol>`.

    Lucide's <svg> attrs (width/height/stroke/fill/...) are dropped —
    those go on the *consumer* <svg> at render time (via icon.css).
    Only viewBox is preserved (essential for scaling).
    """
    m = SVG_RE.search(svg)
    if not m:
        raise RuntimeError(f"malformed SVG for {name}")
    attrs, inner = m.group(1), m.group(2).strip()
    vb = VIEWBOX_RE.search(attrs)
    viewbox = vb.group(1) if vb else "0 0 24 24"
    return f'<symbol id="icon-{name}" viewBox="{viewbox}">{inner}</symbol>'


def build_sprite() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)

    print(f"[fetch-lucide-icons] Lucide v{LUCIDE_VERSION}, {len(ICON_LIST)} icons")
    print(f"[fetch-lucide-icons] output → {OUT_DIR}")

    symbols = []
    fetched = []
    seen: set[str] = set()
    for name in ICON_LIST:
        if name in seen:
            print(f"  skip duplicate: {name}")
            continue
        seen.add(name)
        svg = fetch_icon(name)
        symbols.append(svg_to_symbol(name, svg))
        fetched.append(name)

    # 一次性输出 sprite —— 用 hidden svg 承载 <symbol>，注入 body 后由 <use> 引用。
    # xmlns:xlink 用于兼容旧浏览器（Blink 只用 Chromium，可省，但保留无害）。
    sprite = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<svg xmlns="http://www.w3.org/2000/svg" '
        'xmlns:xlink="http://www.w3.org/1999/xlink" '
        'style="display:none" aria-hidden="true">\n'
        + "\n".join(symbols)
        + "\n</svg>\n"
    )

    SPRITE_FILE.write_text(sprite, encoding="utf-8", newline="\n")
    print(f"[fetch-lucide-icons] wrote {SPRITE_FILE.relative_to(ROOT)} ({len(fetched)} symbols)")

    manifest = {
        "lucideVersion": LUCIDE_VERSION,
        "generatedAt": datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ"),
        "icons": sorted(fetched),
    }
    MANIFEST_FILE.write_text(
        json.dumps(manifest, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
        newline="\n",
    )
    print(f"[fetch-lucide-icons] wrote {MANIFEST_FILE.relative_to(ROOT)}")

    LICENSE_FILE.write_text(LICENSE_TEXT, encoding="utf-8", newline="\n")
    print(f"[fetch-lucide-icons] wrote {LICENSE_FILE.relative_to(ROOT)}")


if __name__ == "__main__":
    try:
        build_sprite()
    except Exception as e:
        print(f"[fetch-lucide-icons] FAILED: {e}", file=sys.stderr)
        sys.exit(1)
