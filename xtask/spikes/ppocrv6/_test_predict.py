#!/usr/bin/env python3
"""Quick test: what does PaddleOCR 3.7 predict() actually return?"""
import sys
import os

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

print(f"Type of result: {type(result)}")
print(f"Result is iterable: {hasattr(result, '__iter__')}")

if hasattr(result, "__len__"):
    print(f"Length: {len(result)}")

for i, page_result in enumerate(result):
    print(f"\n--- Page {i} ---")
    print(f"Type: {type(page_result)}")
    print(f"Dir: {[x for x in dir(page_result) if not x.startswith('_')]}")

    # Check .json
    if hasattr(page_result, "json"):
        j = page_result.json
        print(f".json type: {type(j)}")
        if isinstance(j, str):
            import json
            parsed = json.loads(j)
            print(f".json parsed keys: {list(parsed.keys()) if isinstance(parsed, dict) else 'not a dict'}")
            if isinstance(parsed, dict):
                for k, v in parsed.items():
                    print(f"  {k}: type={type(v).__name__}, len={len(v) if hasattr(v, '__len__') else 'N/A'}")
                    if isinstance(v, list) and len(v) > 0:
                        print(f"    first: {v[0]!r:.200}")
        elif isinstance(j, dict):
            print(f".json keys: {list(j.keys())}")
            for k, v in j.items():
                print(f"  {k}: type={type(v).__name__}, len={len(v) if hasattr(v, '__len__') else 'N/A'}")
                if isinstance(v, list) and len(v) > 0:
                    print(f"    first: {v[0]!r:.200}")
        else:
            print(f".json value: {j!r:.500}")

    # Check __dict__
    if hasattr(page_result, "__dict__"):
        d = vars(page_result)
        print(f"__dict__ keys: {list(d.keys())}")
        for k, v in d.items():
            if k.startswith("_"):
                continue
            print(f"  {k}: type={type(v).__name__}, len={len(v) if hasattr(v, '__len__') else 'N/A'}")
            if isinstance(v, (list, dict)) and len(v) > 0:
                print(f"    first: {v[0]!r:.200}")

    break  # only first page
