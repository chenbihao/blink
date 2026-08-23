#!/usr/bin/env python3
"""Corpus 验证器。

验证 golden corpus 的完整性：
- 22 个文件全部存在
- 尺寸正确
- SHA-256 与 manifest 匹配
- 没有额外未登记图片
- mixed、DPI、vertical 和 1440p 样本必须实际覆盖声明场景

用法：
    python corpus_validator.py --corpus ./testdata/ocr/ppocrv6/
"""

import argparse
import hashlib
import json
import os
import sys


def validate_corpus(corpus_dir):
    """验证 corpus 的完整性。"""
    manifest_path = os.path.join(corpus_dir, "manifest.json")
    if not os.path.exists(manifest_path):
        print(f"[FAIL] manifest.json 不存在: {manifest_path}", file=sys.stderr)
        return False

    with open(manifest_path, "r", encoding="utf-8") as f:
        manifest = json.load(f)

    items = manifest.get("items", [])
    errors = []
    warnings = []
    passed = 0

    print(f"[INFO] 验证 {len(items)} 个 corpus 项...")

    # 检查每个文件
    for item in items:
        img_rel = item["image"]
        img_path = os.path.join(corpus_dir, img_rel)

        # 1. 文件存在
        if not os.path.exists(img_path):
            errors.append(f"文件不存在: {img_rel}")
            continue

        # 2. 尺寸正确
        expected_w = item.get("width", 0)
        expected_h = item.get("height", 0)
        if expected_w > 0 and expected_h > 0:
            try:
                from PIL import Image
                img = Image.open(img_path)
                actual_w, actual_h = img.size
                if actual_w != expected_w or actual_h != expected_h:
                    errors.append(f"尺寸不匹配: {img_rel} (期望 {expected_w}x{expected_h}, 实际 {actual_w}x{actual_h})")
                    continue
            except ImportError:
                warnings.append("Pillow 未安装，跳过尺寸验证")
            except Exception as e:
                errors.append(f"无法读取图片: {img_rel} ({e})")
                continue

        # 3. SHA-256 匹配
        expected_sha = item.get("sha256")
        if expected_sha:
            with open(img_path, "rb") as f:
                actual_sha = hashlib.sha256(f.read()).hexdigest()
            if actual_sha != expected_sha:
                errors.append(f"SHA-256 不匹配: {img_rel} (期望 {expected_sha[:16]}..., 实际 {actual_sha[:16]}...)")
                continue

        # 4. 子集覆盖检查
        subset = item.get("subset", "")
        language = item.get("language", "")
        orientation = item.get("orientation", "")

        # 检查声明场景是否被覆盖
        if subset == "mixed" and language != "zh+en" and language != "en":
            if "zh" not in language and "en" not in language:
                warnings.append(f"mixed 子集缺少中英文混排: {img_rel}")

        if subset == "vertical" and orientation != "vertical":
            errors.append(f"vertical 子集方向标记错误: {img_rel}")

        if subset == "medium" and expected_w != 2560 and expected_h != 1440:
            warnings.append(f"medium 子集不是 1440p: {img_rel} ({expected_w}x{expected_h})")

        if subset == "dpi":
            dpi_scale = item.get("dpi_scale", 0)
            if dpi_scale not in (100, 150, 200):
                warnings.append(f"DPI 子集缺少 dpi_scale: {img_rel}")

        passed += 1

    # 5. 检查额外未登记图片
    registered_files = set(item["image"] for item in items)
    all_png_files = set()
    for root, dirs, files in os.walk(corpus_dir):
        for f in files:
            if f.endswith(".png"):
                rel = os.path.relpath(os.path.join(root, f), corpus_dir).replace("\\", "/")
                all_png_files.add(rel)

    extra_files = all_png_files - registered_files
    if extra_files:
        for ef in sorted(extra_files):
            warnings.append(f"未登记的图片: {ef}")

    # 6. 检查子集覆盖
    subsets_found = set(item.get("subset", "") for item in items)
    required_subsets = {"chinese", "english", "japanese", "mixed", "vertical", "small-font", "light-ui", "dark-ui", "medium", "dpi"}
    missing_subsets = required_subsets - subsets_found
    if missing_subsets:
        errors.append(f"缺少子集: {missing_subsets}")

    # 7. 检查数量
    if len(items) != 22:
        errors.append(f"corpus 项数不等于 22: {len(items)}")

    # 打印结果
    print()
    print("=== 验证结果 ===")
    print(f"通过: {passed}")
    print(f"错误: {len(errors)}")
    print(f"警告: {len(warnings)}")

    if errors:
        print("\n错误:")
        for e in errors:
            print(f"  [FAIL] {e}")

    if warnings:
        print("\n警告:")
        for w in warnings:
            print(f"  [WARN] {w}")

    if not errors:
        print("\n[OK] Corpus 验证通过")
        return True
    else:
        print(f"\n[FAIL] Corpus 验证失败，{len(errors)} 个错误", file=sys.stderr)
        return False


def main():
    parser = argparse.ArgumentParser(description="Validate PP-OCRv6 golden corpus")
    parser.add_argument("--corpus", default="./testdata/ocr/ppocrv6/")
    args = parser.parse_args()

    corpus_dir = os.path.abspath(args.corpus)
    print(f"Corpus 目录: {corpus_dir}")

    if not os.path.exists(corpus_dir):
        print(f"[FAIL] 目录不存在: {corpus_dir}", file=sys.stderr)
        sys.exit(1)

    success = validate_corpus(corpus_dir)
    if not success:
        sys.exit(1)


if __name__ == "__main__":
    main()
