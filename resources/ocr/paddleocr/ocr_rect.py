#!/usr/bin/env python3
"""OCR rect 归一化 seam（0.22.6.1）。

**生产实现，不是测试副本**——blink_ocr_server.py 从本模块 import 同一套
映射函数；测试（test_ocr_rect.py）直接 import 本模块，不加载 FastAPI /
PIL / numpy 等重型依赖。

归一化规则（与 Rust 侧 `parse_rect_strict` 严格校验契约对齐）：

- 所有输出 rect 满足：``0 <= x``、``0 <= y``、``w > 0``、``h > 0``、
  ``x + w <= image_width``、``y + h <= image_height``。
- 浮点坐标先按**边**独立取整为整数边框 ``(x1, y1, x2, y2)``，
  再与图片边界取交集——先取整后裁剪，避免 ``round(min) + round(max-min)``
  使 ``x + w`` 越界 1 像素的取整漂移。
- 交集为空（完全在图片外 / 零宽高）返回 ``None``，调用方跳过该元素，
  绝不输出 Rust 必然拒绝的数据。
"""

import sys


def _round_edge(value):
    """单个边坐标取整。非有限数值返回 None（非法输入）。"""
    try:
        f = float(value)
    except (TypeError, ValueError):
        return None
    # NaN / inf 边界非法
    if f != f or f in (float("inf"), float("-inf")):
        return None
    return int(round(f))


def _poly_edges(points):
    """从点集（[[x, y], ...]）提取 (x1, y1, x2, y2) 整数边框。

    任一边无法解析为有限数值时返回 None。
    """
    if not points:
        return None
    try:
        xs = [float(p[0]) for p in points]
        ys = [float(p[1]) for p in points]
    except (TypeError, IndexError, ValueError):
        return None
    if not xs or not ys:
        return None
    x1 = _round_edge(min(xs))
    y1 = _round_edge(min(ys))
    x2 = _round_edge(max(xs))
    y2 = _round_edge(max(ys))
    if x1 is None or y1 is None or x2 is None or y2 is None:
        return None
    return (x1, y1, x2, y2)


def _box_edges(box):
    """从 [x1, y1, x2, y2] 提取规范化整数边框（min/max 消除方向性）。"""
    if box is None:
        return None
    try:
        if len(box) < 4:
            return None
        x1f, y1f, x2f, y2f = float(box[0]), float(box[1]), float(box[2]), float(box[3])
    except (TypeError, IndexError, ValueError):
        return None
    x1 = _round_edge(min(x1f, x2f))
    y1 = _round_edge(min(y1f, y2f))
    x2 = _round_edge(max(x1f, x2f))
    y2 = _round_edge(max(y1f, y2f))
    if x1 is None or y1 is None or x2 is None or y2 is None:
        return None
    return (x1, y1, x2, y2)


def clamp_edges_to_image(edges, image_width, image_height):
    """整数边框与图片边界取交集。

    返回裁剪后的 (x1, y1, x2, y2)，交集为空（x2 <= x1 或 y2 <= y1）返回 None。
    """
    if edges is None:
        return None
    x1, y1, x2, y2 = edges
    # 先取整、后裁剪——x2 被 clamp 到 image_width，杜绝取整导致的 x+w 越界
    x1 = max(x1, 0)
    y1 = max(y1, 0)
    x2 = min(x2, int(image_width))
    y2 = min(y2, int(image_height))
    if x2 <= x1 or y2 <= y1:
        return None
    return (x1, y1, x2, y2)


def intersect_edges(a, b):
    """两个整数边框求交集；空交集返回 None。"""
    if a is None or b is None:
        return None
    x1 = max(a[0], b[0])
    y1 = max(a[1], b[1])
    x2 = min(a[2], b[2])
    y2 = min(a[3], b[3])
    if x2 <= x1 or y2 <= y1:
        return None
    return (x1, y1, x2, y2)


def edges_to_rect(edges):
    """整数边框 → ``{"x", "y", "w", "h"}`` dict；空/退化边框返回 None。"""
    if edges is None:
        return None
    x1, y1, x2, y2 = edges
    w = x2 - x1
    h = y2 - y1
    if w <= 0 or h <= 0:
        return None
    return {"x": x1, "y": y1, "w": w, "h": h}


def make_rect_from_poly(poly_points, image_width, image_height):
    """从 PaddleOCR polygon 构造轴对齐 rect 并裁剪到图片边界。

    polygon 完全在图片外或交集为空时返回 None。
    """
    edges = clamp_edges_to_image(_poly_edges(poly_points), image_width, image_height)
    return edges_to_rect(edges)


def make_rect_from_box(box, image_width, image_height):
    """从 [x1, y1, x2, y2] 构造 rect 并裁剪到图片边界。"""
    edges = clamp_edges_to_image(_box_edges(box), image_width, image_height)
    return edges_to_rect(edges)


def make_word_rect(word_box, line_rect, image_width, image_height):
    """构造 word rect：先裁剪到图片边界，再与所属 line rect 取交集。

    native word box 无效 / 完全越界 / 与 line rect 无交集（明显不属于该行）
    时 fallback 返回 ``(line_rect, False)``；有效交集返回
    ``(rect, True)``。line_rect 自身已保证在图片边界内，fallback 永远合法。
    """
    if line_rect is None:
        return (None, False)
    word_edges = _box_edges(word_box) if word_box is not None else None
    if word_edges is not None:
        clamped = clamp_edges_to_image(word_edges, image_width, image_height)
        line_edges = (
            line_rect["x"],
            line_rect["y"],
            line_rect["x"] + line_rect["w"],
            line_rect["y"] + line_rect["h"],
        )
        intersected = intersect_edges(clamped, line_edges)
        rect = edges_to_rect(intersected)
        if rect is not None:
            return (rect, True)
    # fallback：使用 line rect（native box 缺失/无效/明显不属于 line）
    return (dict(line_rect), False)


def extract_results(page_data, lines, words, image_width, image_height, warn=None):
    """从 PaddleOCR 3.7 predict() 结果提取 line/word 数据。

    铁则：
    - 过滤空 text line（不输出 Rust 必然拒绝的数据）。
    - 不得输出零宽高、负数、越界 rect（本模块统一归一化保证）。
    - line rect 不合法时跳过该 line 及其 words。
    - native word box 不合法 / 明显不属于 line 时使用 line rect 作为 fallback。
    - line.word_indices 与 word.line_index 必须双向一致。

    ``warn``：诊断回调（``fn(msg: str)``）；默认写 stderr。不记录图片内容。
    """
    if warn is None:
        def warn(msg):
            print(msg, file=sys.stderr, flush=True)

    if not page_data:
        return

    # PaddleOCR 3.7 数据嵌套在 "res" 字段下
    if isinstance(page_data, dict) and "res" in page_data:
        res = page_data["res"]
    elif isinstance(page_data, dict):
        res = page_data
    else:
        return

    if not isinstance(res, dict):
        return

    rec_texts = res.get("rec_texts", [])
    rec_scores = res.get("rec_scores", [])
    dt_polys = res.get("dt_polys", [])
    rec_boxes = res.get("rec_boxes", [])
    text_word_boxes = res.get("text_word_boxes", [])
    text_word = res.get("text_word", [])

    for i, text in enumerate(rec_texts):
        # ── 过滤空 text line ──
        if not text or not text.strip():
            continue

        # ── 构造合法 line rect（先 polygon 后 box，均裁剪到图片边界）──
        line_rect = None
        if i < len(dt_polys) and dt_polys[i]:
            line_rect = make_rect_from_poly(dt_polys[i], image_width, image_height)
        if line_rect is None and i < len(rec_boxes) and rec_boxes[i] is not None:
            line_rect = make_rect_from_box(rec_boxes[i], image_width, image_height)

        # line rect 不合法（完全越界/空交集）时跳过该 line
        if line_rect is None:
            warn(f"[WARN] line[{i}] rect 不合法，跳过")
            continue

        line_idx = len(lines)
        conf = rec_scores[i] if i < len(rec_scores) else 0.0

        # ── Word 级数据 ──
        line_word_indices = []
        if i < len(text_word_boxes) and i < len(text_word):
            word_boxes_i = text_word_boxes[i]
            word_texts_i = text_word[i]
            for j, w_box in enumerate(word_boxes_i):
                w_text = word_texts_i[j] if j < len(word_texts_i) else ""
                # 过滤空 word text
                if not w_text or not w_text.strip():
                    continue

                # native box 有效且与 line 有交集 → 使用交集 rect；
                # 否则 fallback 到 line rect（单个坏 word box 不影响请求）
                word_rect, native = make_word_rect(
                    w_box, line_rect, image_width, image_height
                )
                if word_rect is None:
                    # line_rect 非空时 make_word_rect 不会返回 None（防御）
                    continue

                words.append({
                    "text": w_text,
                    "rect": word_rect,
                    "line_index": line_idx,
                    "_native": native,
                })
                line_word_indices.append(len(words) - 1)
        else:
            # Fallback: 用文本拆分作为 word
            word_texts = text.split()
            if not word_texts:
                word_texts = [text]
            for wt in word_texts:
                words.append({
                    "text": wt,
                    "rect": dict(line_rect),
                    "line_index": line_idx,
                    "_native": False,
                })
                line_word_indices.append(len(words) - 1)

        lines.append({
            "text": text,
            "rect": line_rect,
            "word_indices": line_word_indices,
            "confidence": round(float(conf), 4),
        })
