#!/usr/bin/env python3
"""Add UTF-8 BOM to all PS1 files that need to run on PS 5.1."""
import os

files = [
    "install.ps1",
    "run_benchmark.ps1",
    "evaluate.ps1",
    "cache_tests.ps1",
    "winrt_baseline.ps1",
    "_check_ps1.ps1",
    "_parse_all.ps1",
]

base = os.path.dirname(os.path.abspath(__file__))

for f in files:
    path = os.path.join(base, f)
    if not os.path.exists(path):
        print(f"SKIP (not found): {f}")
        continue
    data = open(path, "rb").read()
    if data[:3] != b"\xef\xbb\xbf":
        open(path, "wb").write(b"\xef\xbb\xbf" + data)
        print(f"Added BOM: {f}")
    else:
        print(f"Already has BOM: {f}")
