#!/usr/bin/env python3
"""Golden corpus 生成脚本。

使用 Pillow 程序化生成覆盖各种子集的 OCR 测试图片。
不依赖任何外部素材，所有图片使用 CC0-1.0 许可。

字体确定性：
- 记录实际字体路径/字体名称/hash
- 找不到支持 CJK/日文的字体时必须失败
- 不得回退到 Pillow default font 后生成空框并继续

用法：
    python generate_corpus.py --output ./testdata/ocr/ppocrv6/
"""

import argparse
import hashlib
import json
import os
import sys

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError:
    print("[FATAL] Pillow 未安装。请先 pip install pillow", file=sys.stderr)
    sys.exit(1)


# ── 字体确定性管理 ──

_FONT_REGISTRY = {}


def _font_hash(filepath):
    """计算字体文件的 SHA-256。"""
    try:
        with open(filepath, "rb") as f:
            return hashlib.sha256(f.read()).hexdigest()[:16]
    except Exception:
        return "unknown"


def _check_cjk_font(font_path):
    """检查字体是否支持 CJK 字符（通过尝试渲染中文字符）。"""
    try:
        font = ImageFont.truetype(font_path, 20)
        # 尝试渲染中文字符
        img = Image.new("RGB", (100, 30), (255, 255, 255))
        draw = ImageDraw.Draw(img)
        draw.text((10, 5), "你好", fill=(0, 0, 0), font=font)
        # 检查是否有非白像素（不是空框）
        pixels = img.load()
        has_content = False
        for x in range(img.width):
            for y in range(img.height):
                if pixels[x, y] != (255, 255, 255):
                    has_content = True
                    break
            if has_content:
                break
        return has_content
    except Exception:
        return False


def find_font(size, bold=False, mono=False):
    """查找可用的字体文件。

    必须找到支持 CJK 的字体，否则失败。
    不得回退到 Pillow default font。
    """
    cache_key = (size, bold, mono)
    if cache_key in _FONT_REGISTRY:
        return _FONT_REGISTRY[cache_key]

    candidates = []

    if mono:
        candidates = [
            "C:/Windows/Fonts/consola.ttf",
            "C:/Windows/Fonts/cour.ttf",
            "C:/Windows/Fonts/lucon.ttf",
        ]
    elif bold:
        candidates = [
            "C:/Windows/Fonts/msyhbd.ttc",
            "C:/Windows/Fonts/segoeuib.ttf",
            "C:/Windows/Fonts/simhei.ttf",
        ]
    else:
        candidates = [
            "C:/Windows/Fonts/msyh.ttc",
            "C:/Windows/Fonts/segoeui.ttf",
            "C:/Windows/Fonts/simsun.ttc",
            "C:/Windows/Fonts/msyhbd.ttc",
        ]

    for path in candidates:
        if os.path.exists(path):
            try:
                font = ImageFont.truetype(path, size)
                _FONT_REGISTRY[cache_key] = font
                _FONT_REGISTRY[f"{cache_key}_path"] = path
                _FONT_REGISTRY[f"{cache_key}_hash"] = _font_hash(path)
                return font
            except Exception:
                continue

    # 不允许回退到 default font
    print(f"[FATAL] 找不到可用的字体文件。已尝试: {candidates}", file=sys.stderr)
    raise FileNotFoundError(f"没有可用的字体文件")


def find_cjk_font(size):
    """查找支持 CJK 的字体。必须通过 CJK 渲染检查。"""
    candidates = [
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/msyhbd.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
    ]

    for path in candidates:
        if os.path.exists(path):
            if _check_cjk_font(path):
                font = ImageFont.truetype(path, size)
                _FONT_REGISTRY[("cjk", size)] = font
                _FONT_REGISTRY[("cjk", size, "path")] = path
                _FONT_REGISTRY[("cjk", size, "hash")] = _font_hash(path)
                return font

    print(f"[FATAL] 找不到支持 CJK 的字体。已尝试: {candidates}", file=sys.stderr)
    raise FileNotFoundError("没有支持 CJK 的字体")


def get_font_info():
    """获取已使用的字体信息（用于 manifest 记录）。"""
    info = {}
    for key in _FONT_REGISTRY:
        if isinstance(key, tuple) and len(key) == 2:
            path = _FONT_REGISTRY.get(key + ("path",))
            hash_val = _FONT_REGISTRY.get(key + ("hash",))
            if path:
                info[str(key)] = {
                    "path": path,
                    "hash": hash_val,
                }
        elif isinstance(key, tuple) and len(key) == 3 and key[2] == "path":
            base_key = key[:2]
            path = _FONT_REGISTRY[key]
            hash_val = _FONT_REGISTRY.get(key[:2] + ("hash",))
            if str(base_key) not in info:
                info[str(base_key)] = {
                    "path": path,
                    "hash": hash_val,
                }
    return info


def make_image(width, height, bg_color, text_color):
    """创建基础图片。"""
    img = Image.new("RGB", (width, height), bg_color)
    return img, ImageDraw.Draw(img)


def draw_text_lines(draw, lines, x, y, font, fill, line_height):
    """绘制多行文本。"""
    for line in lines:
        draw.text((x, y), line, fill=fill, font=font)
        y += line_height


def generate_chinese(out_dir):
    """生成中文子集。"""
    font = find_cjk_font(28)
    # basic-1
    img, draw = make_image(400, 120, (255, 255, 255), (0, 0, 0))
    draw_text_lines(draw, ["你好世界", "这是测试文本"], 20, 20, font, (0, 0, 0), 40)
    img.save(os.path.join(out_dir, "chinese", "basic-1.png"))

    # basic-2
    img, draw = make_image(400, 60, (255, 255, 255), (0, 0, 0))
    font = find_cjk_font(24)
    draw.text((20, 15), "欢迎使用Blink启动器", fill=(0, 0, 0), font=font)
    img.save(os.path.join(out_dir, "chinese", "basic-2.png"))

    # basic-3
    img, draw = make_image(200, 160, (255, 255, 255), (0, 0, 0))
    font = find_cjk_font(22)
    draw_text_lines(draw, ["设置", "系统", "关于", "帮助"], 20, 15, font, (0, 0, 0), 35)
    img.save(os.path.join(out_dir, "chinese", "basic-3.png"))


def generate_english(out_dir):
    """生成英文子集。"""
    # basic-1
    img, draw = make_image(400, 120, (255, 255, 255), (0, 0, 0))
    font = find_font(28, mono=True)
    draw_text_lines(draw, ["Hello World", "This is a test"], 20, 20, font, (0, 0, 0), 40)
    img.save(os.path.join(out_dir, "english", "basic-1.png"))

    # basic-2
    img, draw = make_image(400, 60, (255, 255, 255), (0, 0, 0))
    font = find_font(24, mono=True)
    draw.text((20, 15), "Settings System About Help", fill=(0, 0, 0), font=font)
    img.save(os.path.join(out_dir, "english", "basic-2.png"))

    # basic-3
    img, draw = make_image(500, 50, (255, 255, 255), (0, 0, 0))
    font = find_font(20, mono=True)
    draw.text((10, 12), "The quick brown fox jumps over the lazy dog", fill=(0, 0, 0), font=font)
    img.save(os.path.join(out_dir, "english", "basic-3.png"))


def generate_japanese(out_dir):
    """生成日文子集。"""
    font = find_cjk_font(28)
    # basic-1
    img, draw = make_image(400, 120, (255, 255, 255), (0, 0, 0))
    draw_text_lines(draw, ["こんにちは世界", "これはテストです"], 20, 20, font, (0, 0, 0), 40)
    img.save(os.path.join(out_dir, "japanese", "basic-1.png"))

    # basic-2
    img, draw = make_image(200, 160, (255, 255, 255), (0, 0, 0))
    font = find_cjk_font(22)
    draw_text_lines(draw, ["設定", "システム", "バージョン"], 20, 15, font, (0, 0, 0), 35)
    img.save(os.path.join(out_dir, "japanese", "basic-2.png"))


def generate_mixed(out_dir):
    """生成混排子集。"""
    # cjk-en-1
    img, draw = make_image(300, 120, (255, 255, 255), (0, 0, 0))
    font = find_cjk_font(24)
    draw_text_lines(draw, ["温度25度", "湿度60%"], 20, 20, font, (0, 0, 0), 40)
    img.save(os.path.join(out_dir, "mixed", "cjk-en-1.png"))

    # cjk-en-2
    img, draw = make_image(400, 60, (255, 255, 255), (0, 0, 0))
    font = find_font(22, mono=True)
    draw.text((20, 15), "API key: abc123", fill=(0, 0, 0), font=font)
    img.save(os.path.join(out_dir, "mixed", "cjk-en-2.png"))


def generate_vertical(out_dir):
    """生成竖排子集。"""
    font = find_cjk_font(28)
    # vert-1 (Japanese vertical)
    img, draw = make_image(120, 200, (255, 255, 255), (0, 0, 0))
    cols = ["縦書き", "テスト", "日本語"]
    for col_idx, col_text in enumerate(cols):
        x = 90 - col_idx * 30
        for row_idx, ch in enumerate(col_text):
            y = 10 + row_idx * 35
            draw.text((x, y), ch, fill=(0, 0, 0), font=font)
    img.save(os.path.join(out_dir, "vertical", "vert-1.png"))

    # vert-2 (Chinese vertical)
    img, draw = make_image(120, 200, (255, 255, 255), (0, 0, 0))
    cols = ["垂直排列", "中文测试"]
    for col_idx, col_text in enumerate(cols):
        x = 90 - col_idx * 30
        for row_idx, ch in enumerate(col_text):
            y = 10 + row_idx * 35
            draw.text((x, y), ch, fill=(0, 0, 0), font=font)
    img.save(os.path.join(out_dir, "vertical", "vert-2.png"))


def generate_small_font(out_dir):
    """生成小字号子集。"""
    # small-1
    img, draw = make_image(300, 50, (255, 255, 255), (0, 0, 0))
    font = find_font(12, mono=True)
    draw.text((10, 15), "Small font text 12px", fill=(0, 0, 0), font=font)
    img.save(os.path.join(out_dir, "small-font", "small-1.png"))

    # small-2
    img, draw = make_image(200, 40, (255, 255, 255), (0, 0, 0))
    font = find_cjk_font(12)
    draw.text((10, 10), "小字号文字测试", fill=(0, 0, 0), font=font)
    img.save(os.path.join(out_dir, "small-font", "small-2.png"))


def generate_light_ui(out_dir):
    """生成浅色 UI 子集。"""
    bg = (245, 245, 245)
    text = (30, 30, 30)

    # light-1
    img, draw = make_image(200, 120, bg, text)
    font = find_font(20, mono=True)
    draw_text_lines(draw, ["Settings", "Preferences", "About"], 20, 15, font, text, 30)
    img.save(os.path.join(out_dir, "light-ui", "light-1.png"))

    # light-2
    img, draw = make_image(200, 120, bg, text)
    font = find_cjk_font(20)
    draw_text_lines(draw, ["设置", "偏好设置", "关于"], 20, 15, font, text, 30)
    img.save(os.path.join(out_dir, "light-ui", "light-2.png"))


def generate_dark_ui(out_dir):
    """生成深色 UI 子集。"""
    bg = (30, 30, 30)
    text = (230, 230, 230)

    # dark-1
    img, draw = make_image(200, 120, bg, text)
    font = find_font(20, mono=True)
    draw_text_lines(draw, ["Settings", "Preferences", "About"], 20, 15, font, text, 30)
    img.save(os.path.join(out_dir, "dark-ui", "dark-1.png"))

    # dark-2
    img, draw = make_image(200, 120, bg, text)
    font = find_cjk_font(20)
    draw_text_lines(draw, ["设置", "偏好设置", "关于"], 20, 15, font, text, 30)
    img.save(os.path.join(out_dir, "dark-ui", "dark-2.png"))


def generate_medium(out_dir):
    """生成 1440p 截图替代。"""
    img, draw = make_image(2560, 1440, (255, 255, 255), (0, 0, 0))

    font_large = find_font(32, bold=True)
    font_med = find_font(24, mono=True)
    font_cjk = find_cjk_font(24)

    lines = [
        ("This is a 1440p screenshot replacement for benchmark.", font_med),
        ("It contains multiple lines of text", font_med),
        ("at various sizes and positions.", font_med),
        ("The goal is to test OCR performance", font_med),
        ("on realistic screen content.", font_med),
        ("中文测试：你好世界", font_cjk),
        ("日本語テスト：こんにちは", font_cjk),
        ("Mixed: API key abc123 Temperature 25C", font_med),
    ]

    y = 100
    for text, font in lines:
        draw.text((100, y), text, fill=(0, 0, 0), font=font)
        y += 50

    img.save(os.path.join(out_dir, "medium", "medium-1.png"))


def generate_dpi(out_dir):
    """生成不同 DPI 子集。"""
    # dpi-100 (96 DPI = 100%)
    img, draw = make_image(300, 50, (255, 255, 255), (0, 0, 0))
    font = find_font(16, mono=True)
    draw.text((10, 15), "100% DPI test", fill=(0, 0, 0), font=font)
    img.save(os.path.join(out_dir, "dpi", "dpi-100-1.png"))

    # dpi-150 (144 DPI = 150%)
    img, draw = make_image(450, 75, (255, 255, 255), (0, 0, 0))
    font = find_font(24, mono=True)
    draw.text((15, 22), "150% DPI test", fill=(0, 0, 0), font=font)
    img.save(os.path.join(out_dir, "dpi", "dpi-150-1.png"))

    # dpi-200 (192 DPI = 200%)
    img, draw = make_image(600, 100, (255, 255, 255), (0, 0, 0))
    font = find_font(32, mono=True)
    draw.text((20, 30), "200% DPI test", fill=(0, 0, 0), font=font)
    img.save(os.path.join(out_dir, "dpi", "dpi-200-1.png"))


def main():
    parser = argparse.ArgumentParser(description="Generate PP-OCRv6 golden corpus")
    parser.add_argument("--output", default="./testdata/ocr/ppocrv6/")
    args = parser.parse_args()

    out = args.output

    # 创建子目录
    subdirs = [
        "chinese", "english", "japanese", "mixed",
        "vertical", "small-font", "light-ui", "dark-ui",
        "medium", "dpi",
    ]
    for sd in subdirs:
        os.makedirs(os.path.join(out, sd), exist_ok=True)

    # 检查 CJK 字体可用性
    print("检查 CJK 字体可用性...")
    try:
        find_cjk_font(20)
        print("  CJK 字体: OK")
    except FileNotFoundError as e:
        print(f"[FATAL] {e}", file=sys.stderr)
        sys.exit(1)

    print("Generating golden corpus...")
    generate_chinese(out)
    print("  chinese: OK")
    generate_english(out)
    print("  english: OK")
    generate_japanese(out)
    print("  japanese: OK")
    generate_mixed(out)
    print("  mixed: OK")
    generate_vertical(out)
    print("  vertical: OK")
    generate_small_font(out)
    print("  small-font: OK")
    generate_light_ui(out)
    print("  light-ui: OK")
    generate_dark_ui(out)
    print("  dark-ui: OK")
    generate_medium(out)
    print("  medium: OK")
    generate_dpi(out)
    print("  dpi: OK")

    # 生成 manifest（包含字体信息和文件 hash）
    font_info = get_font_info()
    manifest = {
        "version": "1.0",
        "license": "CC0-1.0",
        "fonts": font_info,
        "items": [
            {"image": "chinese/basic-1.png", "expected_text": "你好世界\n这是测试文本", "subset": "chinese", "language": "zh", "orientation": "horizontal", "width": 400, "height": 120},
            {"image": "chinese/basic-2.png", "expected_text": "欢迎使用Blink启动器", "subset": "chinese", "language": "zh", "orientation": "horizontal", "width": 400, "height": 60},
            {"image": "chinese/basic-3.png", "expected_text": "设置\n系统\n关于\n帮助", "subset": "chinese", "language": "zh", "orientation": "horizontal", "width": 200, "height": 160},
            {"image": "english/basic-1.png", "expected_text": "Hello World\nThis is a test", "subset": "english", "language": "en", "orientation": "horizontal", "width": 400, "height": 120},
            {"image": "english/basic-2.png", "expected_text": "Settings System About Help", "subset": "english", "language": "en", "orientation": "horizontal", "width": 400, "height": 60},
            {"image": "english/basic-3.png", "expected_text": "The quick brown fox jumps over the lazy dog", "subset": "english", "language": "en", "orientation": "horizontal", "width": 500, "height": 50},
            {"image": "japanese/basic-1.png", "expected_text": "こんにちは世界\nこれはテストです", "subset": "japanese", "language": "ja", "orientation": "horizontal", "width": 400, "height": 120},
            {"image": "japanese/basic-2.png", "expected_text": "設定\nシステム\nバージョン", "subset": "japanese", "language": "ja", "orientation": "horizontal", "width": 200, "height": 160},
            {"image": "mixed/cjk-en-1.png", "expected_text": "温度25度\n湿度60%", "subset": "mixed", "language": "zh+en", "orientation": "horizontal", "width": 300, "height": 120},
            {"image": "mixed/cjk-en-2.png", "expected_text": "API key: abc123", "subset": "mixed", "language": "en", "orientation": "horizontal", "width": 400, "height": 60},
            {"image": "vertical/vert-1.png", "expected_text": "縦書き\nテスト\n日本語", "subset": "vertical", "language": "ja", "orientation": "vertical", "width": 120, "height": 200},
            {"image": "vertical/vert-2.png", "expected_text": "垂直排列\n中文测试", "subset": "vertical", "language": "zh", "orientation": "vertical", "width": 120, "height": 200},
            {"image": "small-font/small-1.png", "expected_text": "Small font text 12px", "subset": "small-font", "language": "en", "orientation": "horizontal", "width": 300, "height": 50},
            {"image": "small-font/small-2.png", "expected_text": "小字号文字测试", "subset": "small-font", "language": "zh", "orientation": "horizontal", "width": 200, "height": 40},
            {"image": "light-ui/light-1.png", "expected_text": "Settings\nPreferences\nAbout", "subset": "light-ui", "language": "en", "orientation": "horizontal", "width": 200, "height": 120},
            {"image": "light-ui/light-2.png", "expected_text": "设置\n偏好设置\n关于", "subset": "light-ui", "language": "zh", "orientation": "horizontal", "width": 200, "height": 120},
            {"image": "dark-ui/dark-1.png", "expected_text": "Settings\nPreferences\nAbout", "subset": "dark-ui", "language": "en", "orientation": "horizontal", "width": 200, "height": 120},
            {"image": "dark-ui/dark-2.png", "expected_text": "设置\n偏好设置\n关于", "subset": "dark-ui", "language": "zh", "orientation": "horizontal", "width": 200, "height": 120},
            {"image": "medium/medium-1.png", "expected_text": "This is a 1440p screenshot replacement for benchmark.\nIt contains multiple lines of text\nat various sizes and positions.\nThe goal is to test OCR performance\non realistic screen content.\n中文测试：你好世界\n日本語テスト：こんにちは\nMixed: API key abc123 Temperature 25C", "subset": "medium", "language": "en+zh+ja", "orientation": "horizontal", "width": 2560, "height": 1440},
            {"image": "dpi/dpi-100-1.png", "expected_text": "100% DPI test", "subset": "dpi", "language": "en", "orientation": "horizontal", "width": 300, "height": 50, "dpi_scale": 100},
            {"image": "dpi/dpi-150-1.png", "expected_text": "150% DPI test", "subset": "dpi", "language": "en", "orientation": "horizontal", "width": 450, "height": 75, "dpi_scale": 150},
            {"image": "dpi/dpi-200-1.png", "expected_text": "200% DPI test", "subset": "dpi", "language": "en", "orientation": "horizontal", "width": 600, "height": 100, "dpi_scale": 200},
        ],
    }

    # 计算每个文件的 SHA-256
    for item in manifest["items"]:
        img_path = os.path.join(out, item["image"])
        if os.path.exists(img_path):
            with open(img_path, "rb") as f:
                item["sha256"] = hashlib.sha256(f.read()).hexdigest()

    manifest_path = os.path.join(out, "manifest.json")
    with open(manifest_path, "w", encoding="utf-8") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=2)

    print(f"\nDone! Corpus generated at: {out}")
    print(f"Manifest: {manifest_path} ({len(manifest['items'])} items)")


if __name__ == "__main__":
    main()
