#!/usr/bin/env python3
"""Run AST/compile checks on all Python scripts in the spike directory."""
import ast
import os
import sys

def check_python_scripts():
    scripts = [
        "xtask/spikes/ppocrv6/server_thin.py",
        "xtask/spikes/ppocrv6/server_paddlex.py",
        "xtask/spikes/ppocrv6/worker_once.py",
        "xtask/spikes/ppocrv6/generate_corpus.py",
        "xtask/spikes/ppocrv6/corpus_validator.py",
    ]
    all_ok = True
    for script in scripts:
        try:
            with open(script, encoding="utf-8") as f:
                source = f.read()
            ast.parse(source)
            # Also compile check
            compile(source, script, "exec")
            print(f"  {script}: OK")
        except SyntaxError as e:
            print(f"  {script}: FAIL - {e}")
            all_ok = False
        except FileNotFoundError:
            print(f"  {script}: NOT FOUND")
            all_ok = False
    return all_ok

if __name__ == "__main__":
    print("=== Python AST/compile checks ===")
    ok = check_python_scripts()
    if ok:
        print("\n[OK] All Python scripts pass AST/compile checks")
        sys.exit(0)
    else:
        print("\n[FAIL] Some scripts failed AST/compile checks")
        sys.exit(1)
