#!/usr/bin/env python3
"""ocr_rect.py 归一化语义测试（0.22.6.1）。

覆盖 PaddleOCR 越界 rect 修复的全部约定场景：
1. polygon 右边界越过图片；
2. polygon 左上为负数；
3. word box 超出所属 line；
4. word box 完全在图片外；
5. 边缘取整导致 x+w 比 width 大 1；
6. clamp 后零宽/零高；
7. 多个有效和无效 line/word 混合；
8. 归一化结果通过 Rust parse_rect_strict 同等规则的严格校验，
   并与提交到 testdata/ 的 fixture 逐字段一致（Rust 端测试消费同一 fixture
   走 map_paddleocr_response 真实严格校验）。

运行：
    python resources/ocr/paddleocr/test_ocr_rect.py

纯 stdlib——不加载 FastAPI/PIL/numpy。
"""

import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from ocr_rect import (  # noqa: E402
    clamp_edges_to_image,
    extract_results,
    make_rect_from_box,
    make_rect_from_poly,
    make_word_rect,
)

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
FIXTURE_DIR = os.path.join(REPO_ROOT, "testdata", "ocr", "ppocrv6", "fixtures")
RAW_FIXTURE = os.path.join(FIXTURE_DIR, "rect_raw_out_of_bounds.json")
NORMALIZED_FIXTURE = os.path.join(FIXTURE_DIR, "rect_normalized_out_of_bounds.json")

# 与服务端 envelope 一致（Rust mapper 契约字段）
ENGINE_NAME = "paddleocr"
MODEL_ID = "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec"
MODEL_REVISION = "ppocrv6-tiny"
REQUEST_ID = "fixture-rect-oob-1"


def build_response(page_data, image_width, image_height):
    """走生产映射构造与 blink_ocr_server.py 相同 envelope 的响应 JSON。"""
    lines = []
    words = []
    extract_results(page_data, lines, words, image_width, image_height, warn=lambda _m: None)
    return {
        "request_id": REQUEST_ID,
        "engine": ENGINE_NAME,
        "model_id": MODEL_ID,
        "model_revision": MODEL_REVISION,
        "image_width": image_width,
        "image_height": image_height,
        "lines": lines,
        "words": [{k: v for k, v in w.items() if k != "_native"} for w in words],
    }


def assert_rect_in_image(rect, image_width, image_height, ctx):
    """Rust parse_rect_strict 的同等严格规则：非负、正宽高、不越界。"""
    assert rect["x"] >= 0, f"{ctx}: x={rect['x']} 为负"
    assert rect["y"] >= 0, f"{ctx}: y={rect['y']} 为负"
    assert rect["w"] > 0, f"{ctx}: w={rect['w']} 非正"
    assert rect["h"] > 0, f"{ctx}: h={rect['h']} 非正"
    assert rect["x"] + rect["w"] <= image_width, (
        f"{ctx}: x+w={rect['x'] + rect['w']} 超出 image_width={image_width}"
    )
    assert rect["y"] + rect["h"] <= image_height, (
        f"{ctx}: y+h={rect['y'] + rect['h']} 超出 image_height={image_height}"
    )


def assert_bidirectional_consistency(resp):
    """line.word_indices 与 word.line_index 双向一致 + 恰好引用一次。"""
    lines = resp["lines"]
    words = resp["words"]
    ref_count = [0] * len(words)
    for line_idx, line in enumerate(lines):
        seen = set()
        for idx in line["word_indices"]:
            assert 0 <= idx < len(words), f"word_indices[{idx}] 越界"
            assert idx not in seen, f"word[{idx}] 被 line[{line_idx}] 重复引用"
            seen.add(idx)
            assert words[idx]["line_index"] == line_idx, (
                f"双向一致失败：word[{idx}].line_index={words[idx]['line_index']} "
                f"但被 line[{line_idx}] 引用"
            )
            ref_count[idx] += 1
    for idx, count in enumerate(ref_count):
        assert count == 1, f"word[{idx}] 被引用 {count} 次（应恰好 1 次）"


class RectNormalizationTest(unittest.TestCase):
    IMAGE_W, IMAGE_H = 1188, 800  # 0.22 现象中的 1188px 宽图片

    def test_polygon_right_edge_beyond_image(self):
        # polygon 右边界 1259 超出 image_width=1188（现象复现）
        rect = make_rect_from_poly(
            [[100, 10], [1259, 10], [1259, 50], [100, 50]], self.IMAGE_W, self.IMAGE_H
        )
        self.assertIsNotNone(rect)
        assert_rect_in_image(rect, self.IMAGE_W, self.IMAGE_H, "right-oob")
        self.assertEqual(rect, {"x": 100, "y": 10, "w": 1088, "h": 40})

    def test_polygon_top_left_negative(self):
        # polygon 左上为负数 → clamp 到 0
        rect = make_rect_from_poly(
            [[-12.5, -3.2], [400, -3.2], [400, 60], [-12.5, 60]], self.IMAGE_W, self.IMAGE_H
        )
        self.assertIsNotNone(rect)
        assert_rect_in_image(rect, self.IMAGE_W, self.IMAGE_H, "negative")
        self.assertEqual(rect, {"x": 0, "y": 0, "w": 400, "h": 60})

    def test_rounding_x_plus_w_overflow(self):
        # 边缘取整：min=0.6 → round=1；max=1188.4 → round=1188。
        # 旧实现 round(min)+round(max-min) = 1 + 1188 = 1189 > 1188 越界 1px。
        rect = make_rect_from_poly(
            [[0.6, 0], [1188.4, 0], [1188.4, 30], [0.6, 30]], self.IMAGE_W, self.IMAGE_H
        )
        self.assertIsNotNone(rect)
        assert_rect_in_image(rect, self.IMAGE_W, self.IMAGE_H, "rounding")
        self.assertEqual(rect["x"] + rect["w"], 1188, "取整后 x+w 必须恰好等于 image_width")

    def test_polygon_completely_outside_image(self):
        # 完全在图片右侧之外 → 交集为空 → None
        rect = make_rect_from_poly(
            [[1300, 10], [1500, 10], [1500, 50], [1300, 50]], self.IMAGE_W, self.IMAGE_H
        )
        self.assertIsNone(rect)
        # 完全在上方之外 → None
        rect = make_rect_from_poly(
            [[10, -100], [200, -100], [200, -20], [10, -20]], self.IMAGE_W, self.IMAGE_H
        )
        self.assertIsNone(rect)

    def test_zero_size_after_clamp(self):
        # clamp 后零宽：x1 == x2 == image_width 边界
        self.assertIsNone(
            make_rect_from_box([self.IMAGE_W, 10, self.IMAGE_W, 50], self.IMAGE_W, self.IMAGE_H)
        )
        # 负宽高输入（box 方向颠倒）由 min/max 归一
        rect = make_rect_from_box([300, 100, 100, 10], self.IMAGE_W, self.IMAGE_H)
        self.assertEqual(rect, {"x": 100, "y": 10, "w": 200, "h": 90})
        # 非法点集 → None
        self.assertIsNone(make_rect_from_poly([], self.IMAGE_W, self.IMAGE_H))
        self.assertIsNone(make_rect_from_box(None, self.IMAGE_W, self.IMAGE_H))
        self.assertIsNone(make_rect_from_box([1, 2], self.IMAGE_W, self.IMAGE_H))
        self.assertIsNone(make_rect_from_poly([["a", "b"]], self.IMAGE_W, self.IMAGE_H))

    def test_word_box_outside_image_falls_back_to_line(self):
        line = {"x": 100, "y": 10, "w": 500, "h": 40}
        # word box 完全在图片外
        rect, native = make_word_rect([2000, 10, 2100, 50], line, self.IMAGE_W, self.IMAGE_H)
        self.assertFalse(native)
        self.assertEqual(rect, line)
        # word box 完全在 line 之外（虽在图片内）→ 明显不属于 line → fallback
        rect, native = make_word_rect([900, 500, 1000, 560], line, self.IMAGE_W, self.IMAGE_H)
        self.assertFalse(native)
        self.assertEqual(rect, line)

    def test_word_box_overlapping_line_is_intersected(self):
        line = {"x": 100, "y": 10, "w": 500, "h": 40}
        # word box 右侧超出 line → 与 line 取交集
        rect, native = make_word_rect([400, 0, 700, 80], line, self.IMAGE_W, self.IMAGE_H)
        self.assertTrue(native)
        self.assertEqual(rect, {"x": 400, "y": 10, "w": 200, "h": 40})
        # word box 同时越过 line 右边界与图片右边界 → 先裁剪图片再与 line 交集
        rect, native = make_word_rect([500, 0, 1300, 80], line, self.IMAGE_W, self.IMAGE_H)
        self.assertTrue(native)
        self.assertEqual(rect, {"x": 500, "y": 10, "w": 100, "h": 40})

    def test_word_fallback_with_invalid_box(self):
        line = {"x": 5, "y": 6, "w": 7, "h": 8}
        for bad in (None, [1, 2], ["a", "b", "c", "d"]):
            rect, native = make_word_rect(bad, line, self.IMAGE_W, self.IMAGE_H)
            self.assertFalse(native)
            self.assertEqual(rect, line)

    def test_extract_results_mixed_valid_and_invalid(self):
        page_data = {
            "res": {
                "rec_texts": ["valid line", "outside line", "", "partial oob"],
                "rec_scores": [0.9, 0.8, 0.7, 0.6],
                # line1: 完全在图片外（x 全部 > 1188）→ 跳过
                # line3: 右边界越界 → clamp
                "dt_polys": [
                    [[10, 10], [300, 10], [300, 50], [10, 50]],
                    [[1300, 10], [1500, 10], [1500, 50], [1300, 50]],
                    [[10, 10], [300, 10], [300, 50], [10, 50]],  # 空 text 占位
                    [[600, 100], [1259.7, 100], [1259.7, 140], [600, 140]],
                ],
                "rec_boxes": [],
                "text_word_boxes": [
                    [[10, 10, 150, 50], [400, 10, 500, 50]],  # 第二个 word 与 line 无交集
                    [],
                    [],
                    [[600, 90, 1188, 150]],  # word box 越过 line 底部与图片右界
                ],
                "text_word": [
                    ["hello", "ghost"],
                    ["x"],
                    [],
                    ["world"],
                ],
            }
        }
        lines = []
        words = []
        extract_results(page_data, lines, words, self.IMAGE_W, self.IMAGE_H, warn=lambda _m: None)

        # 空 text line 跳过；越界 line 跳过 → 剩 2 行
        self.assertEqual(len(lines), 2)
        self.assertEqual(lines[0]["text"], "valid line")
        self.assertEqual(lines[1]["text"], "partial oob")
        # line1 越界 clamp 后 x+w == 1188
        self.assertEqual(lines[1]["rect"]["x"] + lines[1]["rect"]["w"], self.IMAGE_W)

        # words：line0 两个（ghost fallback 到 line rect）+ line1 一个
        self.assertEqual(len(words), 3)
        self.assertEqual(words[0]["line_index"], 0)
        self.assertEqual(words[1]["rect"], lines[0]["rect"], "越界 word 应 fallback 到 line rect")
        self.assertEqual(words[2]["line_index"], 1)
        assert_rect_in_image(words[2]["rect"], self.IMAGE_W, self.IMAGE_H, "mixed-word")

        resp = build_response(page_data, self.IMAGE_W, self.IMAGE_H)
        assert_bidirectional_consistency(resp)


class FixtureContractTest(unittest.TestCase):
    """fixture 契约：raw → 生产映射 → normalized 与提交文件逐字段一致。"""

    def test_normalized_fixture_matches_production_mapping(self):
        with open(RAW_FIXTURE, "r", encoding="utf-8") as f:
            raw = json.load(f)
        resp = build_response(raw["page_data"], raw["image_width"], raw["image_height"])

        # 8. 归一化结果必须通过 Rust parse_rect_strict 同等严格规则
        for i, line in enumerate(resp["lines"]):
            assert_rect_in_image(line["rect"], resp["image_width"], resp["image_height"], f"line[{i}]")
        for i, word in enumerate(resp["words"]):
            assert_rect_in_image(word["rect"], resp["image_width"], resp["image_height"], f"word[{i}]")
        assert_bidirectional_consistency(resp)

        # 与提交到 testdata 的 fixture 一致（Rust 测试消费同一文件走
        # map_paddleocr_response 真实校验；两侧任一漂移都会使本断言失败）
        with open(NORMALIZED_FIXTURE, "r", encoding="utf-8") as f:
            committed = json.load(f)
        self.assertEqual(resp, committed)


if __name__ == "__main__":
    unittest.main(verbosity=2)
