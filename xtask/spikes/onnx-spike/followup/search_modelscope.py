#!/usr/bin/env python3
"""Search ModelScope for ParaformerOnline model."""
import sys

# Try to search for the model
try:
    from modelscope.hub.api import HubApi
    api = HubApi()
    # List models matching paraformer online
    results = api.list_models(query="paraformer")
    print(f"Found {len(results)} models matching 'paraformer'")
    for m in results[:20]:
        print(f"  {m}")
    
    print()
    results2 = api.list_models(query="paraformer-online")
    print(f"Found {len(results2)} models matching 'paraformer-online'")
    for m in results2[:10]:
        print(f"  {m}")
except Exception as e:
    print(f"Error: {e}")

# Also try common model IDs
from modelscope import snapshot_download
test_ids = [
    "speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx",
    "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx",
    "damo/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online-onnx",
]
for mid in test_ids:
    try:
        path = snapshot_download(mid, cache_dir="d:/Projects/Coding/blink/xtask/spikes/onnx-spike/models/test_cache")
        print(f"SUCCESS: {mid} -> {path}")
        break
    except Exception as e:
        print(f"FAIL: {mid} -> {e}")
