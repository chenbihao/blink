#!/usr/bin/env python3
"""Find here-string boundaries in PowerShell scripts."""
import sys

path = sys.argv[1] if len(sys.argv) > 1 else "xtask/spikes/ppocrv6/install.ps1"
with open(path, "r", encoding="utf-8") as f:
    lines = f.readlines()

for i, line in enumerate(lines, 1):
    stripped = line.rstrip("\r\n")
    if '@"' in stripped and not stripped.strip().startswith("#"):
        print(f"L{i}: START |{repr(stripped)}|")
    if stripped == '"@' or stripped.startswith('"@'):
        print(f"L{i}: END   |{repr(stripped)}|")
