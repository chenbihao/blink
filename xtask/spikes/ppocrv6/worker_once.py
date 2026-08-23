#!/usr/bin/env python3
"""PP-OCRv6 单次 worker 实验 — 拓扑 C。

不启动常驻 HTTP 服务，而是每次请求：
1. 加载模型（或复用已加载）
2. 执行 OCR（PaddleOCR 3.7 predict() API）
3. 输出结果 JSON 到 stdout

这模拟 Blink 直接在进程内调用 OCR 的场景，用于比较常驻服务 vs 单次启动的延迟差异。

用法：
    python worker_once.py --image <png_path> --model small --model-cache ./model-cache
    python worker_once.py --image <png_path> --model small --warmup  # 预热后测单次
"""

import argparse
import json
import os
import sys
import time

# UTF-8 安全
sys.stdin.reconfigure(encoding="utf-8", errors="replace")
sys.stdout.reconfigure(encoding="utf-8", errors="replace", line_buffering=True)
sys.stderr.reconfigure(encoding="utf-8", errors="replace")

# ── 模型档位 → 官方模型名映射 ──
MODEL_MAP = {
    "tiny": {
        "det": "PP-OCRv6_tiny_det",
        "rec": "PP-OCRv6_tiny_rec",
    },
    "small": {
        "det": "PP-OCRv6_small_det",
        "rec": "PP-OCRv6_small_rec",
    },
    "medium": {
        "det": "PP-OCRv6_medium_det",
        "rec": "PP-OCRv6_medium_rec",
    },
}


def main():
    parser = argparse.ArgumentParser(description="PP-OCRv6 单次 worker")
    parser.add_argument("--image", required=True, help="PNG 图片路径")
    parser.add_argument("--model", default="small", choices=["tiny", "small", "medium"])
    parser.add_argument("--lang", default="ch")
    parser.add_argument("--model-cache", default="./model-cache")
    parser.add_argument("--warmup", action="store_true", help="预热后再测单次")
    parser.add_argument("--cpu-threads", type=int, default=2)
    parser.add_argument("--enable-mkldnn", action="store_true")
    args = parser.parse_args()

    if args.cpu_threads < 1:
        parser.error("--cpu-threads 必须 >= 1")

    thread_value = str(args.cpu_threads)
    for name in ("OMP_NUM_THREADS", "MKL_NUM_THREADS", "OPENBLAS_NUM_THREADS", "NUMEXPR_NUM_THREADS"):
        os.environ[name] = thread_value

    os.makedirs(args.model_cache, exist_ok=True)

    # ── 加载模型 ──
    t_load_start = time.perf_counter()
    try:
        from paddleocr import PaddleOCR  # type: ignore

        model_names = MODEL_MAP.get(args.model, MODEL_MAP["small"])

        # PaddleOCR 3.7 API：
        # - ocr_version="PP-OCRv6" 使用官方代际值
        # - text_detection/recognition_model_name 指定具体模型
        # - use_doc_orientation_classify=False（截图不需要）
        # - use_doc_unwarping=False
        # - use_textline_orientation=False
        # - return_word_box=True 获取 word 级 boxes
        # - device="cpu" 强制 CPU
        engine = PaddleOCR(
            ocr_version="PP-OCRv6",
            text_detection_model_name=model_names["det"],
            text_recognition_model_name=model_names["rec"],
            use_doc_orientation_classify=False,
            use_doc_unwarping=False,
            use_textline_orientation=False,
            return_word_box=True,
            device="cpu",
            cpu_threads=args.cpu_threads,
            enable_mkldnn=args.enable_mkldnn,
        )
    except Exception as e:
        print(json.dumps({"error": f"model_load_failed: {e}"}))
        sys.exit(1)
    t_load_end = time.perf_counter()
    load_ms = round((t_load_end - t_load_start) * 1000, 2)

    # ── 预热（可选）──
    if args.warmup:
        # 用 1x1 像素 PNG 预热
        warmup_png = bytes([
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
            0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41,
            0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
            0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
            0x42, 0x60, 0x82,
        ])
        try:
            # PaddleOCR 3.7 predict() 只接受 str 或 numpy.ndarray
            import numpy as _np
            from PIL import Image as _PILImage
            import io as _io
            _warmup_img = _np.array(_PILImage.open(_io.BytesIO(warmup_png)))
            engine.predict(input=_warmup_img, return_word_box=True)
        except Exception:
            pass  # 预热可能失败，不影响测量

    # ── 读取图片 ──
    # PaddleOCR 3.7 predict() 只接受 str(文件路径) 或 numpy.ndarray
    import numpy as _np2
    from PIL import Image as _PILImage2
    import io as _io2
    with open(args.image, "rb") as f:
        png_bytes = f.read()
    _img_array = _np2.array(_PILImage2.open(_io2.BytesIO(png_bytes)))

    # ── OCR 识别（PaddleOCR 3.7 predict() API）──
    t0 = time.perf_counter()
    try:
        result = engine.predict(input=_img_array, return_word_box=True)
    except Exception as e:
        print(json.dumps({"error": f"ocr_failed: {e}"}))
        sys.exit(1)
    t1 = time.perf_counter()
    ocr_ms = round((t1 - t0) * 1000, 2)

    # ── 映射输出（与 thin wrapper 格式一致）──
    lines = []
    words = []
    word_idx = 0
    native_word_boxes = 0
    fallback_word_boxes = 0

    if result:
        for page_result in result:
            try:
                if hasattr(page_result, "json"):
                    page_data = page_result.json
                    if isinstance(page_data, str):
                        page_data = json.loads(page_data)
                elif hasattr(page_result, "__dict__"):
                    page_data = vars(page_result)
                else:
                    page_data = page_result
            except Exception:
                page_data = page_result

            # PaddleOCR 3.7 OCRResult.json 结构（实测）：
            # { "res": { "rec_texts": [...], "rec_scores": [...], "dt_polys": [...],
            #            "rec_boxes": [...], "text_word_boxes": [...], "text_word": [...] } }
            if isinstance(page_data, dict) and "res" in page_data:
                res = page_data["res"]
            elif isinstance(page_data, dict):
                res = page_data
            else:
                continue

            if not isinstance(res, dict):
                continue

            rec_texts = res.get("rec_texts", [])
            rec_scores = res.get("rec_scores", [])
            dt_polys = res.get("dt_polys", [])
            rec_boxes = res.get("rec_boxes", [])
            text_word_boxes = res.get("text_word_boxes", [])
            text_word = res.get("text_word", [])

            line_idx = len(lines)

            for i, text in enumerate(rec_texts):
                if i < len(dt_polys) and dt_polys[i]:
                    box = dt_polys[i]
                    if box and len(box) >= 4:
                        xs = [p[0] for p in box]
                        ys = [p[1] for p in box]
                        line_rect = {
                            "x": round(min(xs)),
                            "y": round(min(ys)),
                            "w": round(max(xs) - min(xs)),
                            "h": round(max(ys) - min(ys)),
                        }
                    else:
                        line_rect = {"x": 0, "y": 0, "w": 0, "h": 0}
                elif i < len(rec_boxes) and rec_boxes[i]:
                    box = rec_boxes[i]
                    line_rect = {
                        "x": round(box[0]),
                        "y": round(box[1]),
                        "w": round(box[2] - box[0]),
                        "h": round(box[3] - box[1]),
                    }
                else:
                    line_rect = {"x": 0, "y": 0, "w": 0, "h": 0}

                conf = rec_scores[i] if i < len(rec_scores) else 0.0

                line_word_indices = []
                if i < len(text_word_boxes) and i < len(text_word):
                    word_boxes_i = text_word_boxes[i]
                    word_texts_i = text_word[i]
                    for j, w_box in enumerate(word_boxes_i):
                        w_text = word_texts_i[j] if j < len(word_texts_i) else ""
                        if isinstance(w_box, (list, tuple)) and len(w_box) >= 4:
                            word_rect = {
                                "x": round(w_box[0]),
                                "y": round(w_box[1]),
                                "w": round(w_box[2] - w_box[0]),
                                "h": round(w_box[3] - w_box[1]),
                            }
                        else:
                            word_rect = {"x": 0, "y": 0, "w": 0, "h": 0}

                        words.append({
                            "text": w_text,
                            "rect": word_rect,
                            "line_index": line_idx,
                        })
                        line_word_indices.append(word_idx)
                        word_idx += 1
                        native_word_boxes += 1
                else:
                    word_texts = text.split() if text else []
                    if not word_texts and text:
                        word_texts = [text]
                    for wt in word_texts:
                        words.append({
                            "text": wt,
                            "rect": line_rect,
                            "line_index": line_idx,
                        })
                        line_word_indices.append(word_idx)
                        word_idx += 1
                        fallback_word_boxes += 1

                lines.append({
                    "text": text,
                    "rect": line_rect,
                    "word_indices": line_word_indices,
                    "confidence": round(conf, 4),
                })

    # ── 内存测量 ──
    try:
        import psutil  # type: ignore
        proc = psutil.Process()
        mem_info = proc.memory_info()
        peak_ws_mb = round(mem_info.rss / 1024 / 1024, 1)
    except ImportError:
        peak_ws_mb = None

    output = {
        "engine": "ppocrv6-worker-once",
        "model": args.model,
        "ocr_version": "PP-OCRv6",
        "load_ms": load_ms,
        "ocr_ms": ocr_ms,
        "lines": lines,
        "words": words,
        "native_word_boxes": native_word_boxes,
        "fallback_word_boxes": fallback_word_boxes,
        "peak_working_set_mb": peak_ws_mb,
    }
    print(json.dumps(output, ensure_ascii=False))


if __name__ == "__main__":
    main()
