#!/usr/bin/env python3
"""Check if any line inside the here-string starts with "@ (which would terminate it)."""

data = open("xtask/spikes/ppocrv6/install.ps1", "rb").read()
text = data.decode("utf-8")
lines = text.split("\r\n")

in_herestring = False
for i, line in enumerate(lines, 1):
    stripped = line.rstrip()
    if not in_herestring and '@"' in line:
        # Check if @" is at the end of the line
        if stripped.endswith('@"') or stripped.endswith('@"\r'):
            in_herestring = True
            print(f"L{i}: START here-string")
            continue
    if in_herestring:
        if stripped.startswith('"@'):
            in_herestring = False
            print(f"L{i}: END here-string")
            continue
        # Check if line starts with " (might be confused with end marker)
        if line.startswith('"@'):
            print(f"L{i}: POTENTIAL EARLY TERMINATION: |{repr(line)}|")
        # Check for backtick
        if "`" in line:
            print(f"L{i}: Has backtick: |{repr(line)}|")
