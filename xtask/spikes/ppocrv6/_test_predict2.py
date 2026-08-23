#!/usr/bin/env python3
"""Quick test: what does PaddleOCR 3.7 predict() actually return (deep dive)?"""
import sys
import os
import json

os.environ["GLOG_minloglevel"] = "3"

from paddleocr import PaddleOCR

img_path = sys.argv[1] if len(sys.argv) > 1 else r"testdata\ocr\ppocrv6\chinese\basic-1.png"

print(f"Image: {img_path}")
print("Creating engine (tiny)...")

engine = PaddleOCR(
    ocr_version="PP-OCRv6",
    text_detection_model_name="PP-OCRv6_tiny_det",
    text_recognition_model_name="PP-OCRv6_tiny_rec",
    use_doc_orientation_classify=False,
    use_doc_unwarping=False,
    use_textline_orientation=False,
    return_word_box=True,
    device="cpu",
    enable_mkldnn=True,
)

print("Calling predict()...")
result = engine.predict(input=img_path, return_word_box=True)

for i, page_result in enumerate(result):
    print(f"\n--- Page {i} ---")
    j = page_result.json
    if isinstance(j, dict) and "res" in j:
        res = j["res"]
        print(f"res keys: {list(res.keys())}")
        for k, v in res.items():
            if isinstance(v, list):
                print(f"  {k}: type=list, len={len(v)}")
                if len(v) > 0:
                    print(f"    first: {v[0]!r:.300}")
            elif isinstance(v, dict):
                print(f"  {k}: type=dict, keys={list(v.keys())}")
            elif isinstance(v, str) and len(v) > 200:
                print(f"  {k}: type=str, len={len(v)} (truncated)")
                print(f"    first 200: {v[:200]!r}")
            else:
                print(f"  {k}: {v!r:.300}")
    else:
        print(f"json: {j!r:.500}")
    break
