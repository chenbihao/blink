#!/usr/bin/env python3
"""Check here-string boundaries in install.ps1."""
data = open("xtask/spikes/ppocrv6/install.ps1", "rb").read()

# Find all occurrences of the end marker
marker = b'"@ 2>&1'
idx = 0
while True:
    idx = data.find(marker, idx)
    if idx == -1:
        break
    chunk = data[max(0, idx - 10):idx + 20]
    print(f"Found end marker at byte {idx}: {repr(chunk)}")
    idx += 1

# Find all occurrences of the start marker
marker2 = b'-c @"'
idx = 0
while True:
    idx = data.find(marker2, idx)
    if idx == -1:
        break
    chunk = data[max(0, idx - 5):idx + 20]
    print(f"Found start marker at byte {idx}: {repr(chunk)}")
    idx += 1
