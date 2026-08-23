#!/usr/bin/env python3
"""Test predict() with bytes vs file path."""
import os, sys
os.environ["GLOG_minloglevel"] = "3"
from paddleocr import PaddleOCR

img_path = r"testdata\ocr\ppocrv6\chinese\basic-1.png"

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

# Test 1: file path
print("=== Test 1: file path ===")
result1 = engine.predict(input=img_path, return_word_box=True)
print(f"Type: {type(result1)}, len: {len(result1)}")
for p in result1:
    j = p.json
    if isinstance(j, dict) and "res" in j:
        res = j["res"]
        print(f"  rec_texts: {res.get('rec_texts', [])}")
    break

# Test 2: bytes
print("\n=== Test 2: bytes ===")
with open(img_path, "rb") as f:
    img_bytes = f.read()
result2 = engine.predict(input=img_bytes, return_word_box=True)
print(f"Type: {type(result2)}, len: {len(result2)}")
for p in result2:
    j = p.json
    if isinstance(j, dict):
        if "res" in j:
            res = j["res"]
            print(f"  res keys: {list(res.keys())}")
            print(f"  rec_texts: {res.get('rec_texts', [])}")
        else:
            print(f"  keys: {list(j.keys())}")
    else:
        print(f"  json type: {type(j)}")
    break

# Test 3: PIL Image
print("\n=== Test 3: numpy array ===")
import numpy as np
from PIL import Image
import io

img = Image.open(img_path)
arr = np.array(img)
result3 = engine.predict(input=arr, return_word_box=True)
print(f"Type: {type(result3)}, len: {len(result3)}")
for p in result3:
    j = p.json
    if isinstance(j, dict) and "res" in j:
        res = j["res"]
        print(f"  rec_texts: {res.get('rec_texts', [])}")
    break
